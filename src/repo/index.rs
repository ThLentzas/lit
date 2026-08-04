mod parse;

use crate::repo::index::parse::Parser;
use crate::repo::object::mode::Mode;
use crate::repo::os::FileStat;
use crate::repo::path::RepoPath;
use sha1::{Digest, Sha1};
use std::fs;
use std::io;
use std::path::PathBuf;
// toDo:
// Integrity validation:
//   Did the bytes survive unchanged?
//   -> checksum
//
// Structural validation:
//   Can these bytes be interpreted as an index file?
//   -> header/version/entry boundaries/path terminators/padding
//
// Semantic validation:
//   Do the parsed entries obey all Git rules?
//   -> sorting/modes/stages/path rules

// git has multiple versions(2, 3, 4) for the index format
const INDEX_VERSION: u32 = 2;
const PATH_MAX_SIZE: u16 = 0xfff;
const SIGNATURE: &str = "DIRC";

// toDo: caching tree
pub(crate) struct Index {
    // TODO: on the rewrite test if a BTreeMap could work
    pub(crate) entries: Vec<IndexEntry>,
    pub(crate) path: PathBuf,
    // a flag that is used to not unnecessary write if no changes detected
    pub(crate) modified: bool,
}

// .lit/index is created lazily on the first write
impl Index {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            entries: Vec::new(),
            modified: false,
        }
    }

    // This method is used by status to see if the entry pointed by `path` is tracked by the index.
    // If path points to a file, a single binary search call is enough, but directories have specific
    // behavior. As we already mentioned Index does not keep track of directories. If the path passed
    // to add is a directory Index and tracks its entries.
    // An optimization that Git does is that when a directory  contains no tracked files anywhere
    // inside it, status reports the directory itself, not every file below it. So if foo contains
    // bar.txt, and baz.txt, instead of listing both as untracked it lists foo/.
    //      workspace:
    //          a/b/inner.txt      tracked
    //          a/outer.txt        untracked
    //          a/b/c/file.txt     untracked
    //
    //      index:
    //          a/b/inner.txt
    //
    // The result of status for untracked files is: a/b/c, a/outer.txt. even though a/ is not literally
    // in the index, it is a tracked directory for status purposes because it contains a/b/inner.txt,
    // which is tracked. a/b is also tracked since it contains inner.txt.
    //
    // The rule is:
    //      A path is tracked for status if:
    //          1. the exact file path exists in the index, OR
    //          2. the path is a directory prefix of at least one index entry.
    //
    // The exact file is easy as mentioned. For the dir case, we already have the is_parent_path()
    // from resolve_conflicts() we need to find the child entry path. Read lower_bound()
    // This logic handles untracked(non-empty) directories.
    pub(crate) fn is_tracked(&self, path: &RepoPath) -> bool {
        if self.contains(&path) {
            return true;
        }
        // TODO: test it
        // Hard to see edge case spotted by AI. If the path is not found it means either that it is
        // a file and does not exist or that is a dir. In either case we append a trailing slash
        // a/b.txt      <- '.' (0x2E) sorts before '/' (0x2F)
        // a/b/c.txt
        // is_tracked("a/b"): exact search misses, lower_bound("a/b") lands on a/b.txt (the first
        // entry > "a/b"), is_parent_path("a/b", "a/b.txt") is false -> we return false, even though
        // a/b/c.txt makes a/b tracked. The real descendants live past the sibling file. Searching
        // for a/b/ instead skips over a/b.txt and lands exactly on the first descendant if one exists
        let mut path_bytes = Vec::with_capacity(path.as_bytes().len() + 1);
        path_bytes.extend_from_slice(path.as_bytes());
        path_bytes.push(b'/');
        let pos = self.lower_bound(&path_bytes);
        self.entries
            .get(pos)
            .is_some_and(|entry| entry.path.as_bytes().starts_with(&path_bytes))
    }

    // Git requires index entries to be sorted by filename. It is a requirement for deterministic
    // hashing. When we try to hash a tree with 2 entries a and b, we need a and b to have fixed
    // order otherwise we will compute a different hash for the same tree which breaks Git's invariance
    // same content -> same hash
    //
    // We encofre sorted, unique index entries to achieve the same behavior. Maintaing the sorted
    // order prevents the above issue where we could produce different hash for the same tree. By
    // sorting based on the path we maintain the order accross all componenets of the path.
    // foo/src/a, foo/src/b and foo/README . In the final order README appears before src as the entries
    // of foo and a appears before b as the entries of src. Inserting in ascending order allows us
    // to do BS for retrieveing entries instead of appending + sorting. Read more Tree::write()
    pub(crate) fn add_entries(&mut self, entries: Vec<IndexEntry>) -> Result<(), IndexError> {
        for entry in entries {
            self.resolve_conflicts(&entry.path);

            match self.entries.binary_search_by(|e| e.path.cmp(&entry.path)) {
                Ok(pos) => {
                    if self.entries[pos] != entry {
                        self.entries[pos] = entry;
                        self.modified = true;
                    }
                }
                Err(pos) => {
                    self.entries.insert(pos, entry);
                    self.modified = true;
                }
            }
        }
        Ok(())
    }
    
    pub(crate) fn remove(&mut self, path: &RepoPath) -> Option<IndexEntry> {
       match self.entries.binary_search_by(|entry| entry.path.cmp(path)) {
           Ok(index) => Some(self.entries.remove(index)),
           Err(_) => None,
       }
    }

    pub(crate) fn refresh_entry_stat(&mut self, index: usize, stat: FileStat) {
        self.entries[index].stat = stat;
    }
    

    pub(crate) fn serialize(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(SIGNATURE.as_bytes());
        bytes.extend_from_slice(&INDEX_VERSION.to_be_bytes());
        bytes.extend_from_slice(&(self.entries.len() as u32).to_be_bytes());
        // toDo: at this point according to the format we are supposed to add Extensions
        for entry in self.entries.iter() {
            bytes.extend(entry.serialize());
        }
        let checksum: [u8; 20] = Sha1::digest(&bytes).into();
        bytes.extend_from_slice(&checksum);

        bytes
    }

    // loads the context of .lit/index in memory
    pub(crate) fn load(&mut self) -> Result<(), IndexError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                self.entries = Vec::new();
                return Ok(());
            }
            Err(err) => {
                return Err(IndexError::Io {
                    path: self.path.clone(),
                    source: err,
                });
            }
        };

        let (content, checksum): (&[u8], &[u8; 20]) =
            bytes.split_last_chunk::<20>().ok_or_else(|| {
                FormatError::at(
                    0,
                    FormatErrorKind::Eof {
                        needed: 20,
                        remaining: bytes.len(),
                    },
                )
            })?;
        let hash: [u8; 20] = Sha1::digest(content).into();
        // The checksum tells us the bytes we read are the same bytes that were written. It does
        // not prove those bytes form a valid index file. We still need to do validation.
        //
        // checksum matches
        // signature is DIRC
        // content has enough bytes for the header
        // each entry has enough bytes for the fixed 62-byte part
        // each path has a NUL terminator
        // entry padding does not run past the content
        // entries are sorted correctly
        // path length in flags matches actual path length
        // paths do not contain weird/disallowed components
        // duplicate paths are rejected
        // mode is one of the allowed Git modes
        // after reading entry_count entries, the parser ends exactly where expected
        if hash != *checksum {
            // it is not InvalidFormat, we don't know that yet, the contents have been tempered with
            return Err(IndexError::InvalidChecksum);
        }

        let mut parser = Parser::new(content);
        if parser.take::<4>()? != SIGNATURE.as_bytes() {
            return Err(FormatError::at(0, FormatErrorKind::InvalidSignature))?;
        }

        let version = u32::from_be_bytes(*parser.take::<4>()?);
        if version != INDEX_VERSION {
            return Err(IndexError::UnsupportedVersion(version));
        }

        let count = u32::from_be_bytes(*parser.take::<4>()?);
        for _ in 0..count {
            let entry_offset = parser.offset();
            let entry = parser.next()?;

            // don't do the C++/Java way of
            //  if !self.entries.is_empty() {
            //  let last = self.entries.last().unwrap();
            if let Some(last) = self.entries.last() {
                // if the entries are not in ascending order the format is invalid
                // in a sorted list duplicates must appear next to each other, and we check each entry
                // with the previous one
                //
                // if entry.path == last.path, the duplicate is adjacent and caught here
                // if entry.path < last.path, sort order is broken; any earlier duplicate would also
                // surface as a non-ascending pair somewhere in the sequence
                // toDo: address sorting order when we add stage support
                if last.path >= entry.path {
                    return Err(FormatError::at(
                        entry_offset,
                        FormatErrorKind::EntriesNotSorted,
                    ))?;
                }
            }
            self.entries.push(entry);
            if self.entries.len() > count as usize {
                return Err(FormatError::at(
                    parser.offset(),
                    FormatErrorKind::EntriesCountMissMatch {
                        actual: self.entries.len(),
                        expected: count as usize,
                    },
                ))?;
            }
        }

        if self.entries.len() != count as usize {
            return Err(FormatError::at(
                parser.offset(),
                FormatErrorKind::EntriesCountMissMatch {
                    actual: self.entries.len(),
                    expected: count as usize,
                },
            ))?;
        }
        Ok(())
    }

    // we need to consider 2 edge cases:
    //  -adding a file whose parent directory has the same name as an existing file in the index. We
    //  have an index entry, foo.txt then we remove foo.txt from our filesystem, create
    //  foo.txt/bar.rs, and we call add for foo.txt/bar.rs. Now in our index we have both foo.txt and
    //  foo.txt/bar.rs which is not possible because no filesystem will allow a file and a directory
    //  to have the same name. Consider we want to add lib/index/entry.rs while have lib/index and
    //  lib. Both those entries must be removed before adding our new entry, we will have the same
    //  name for a file/directory at the same level violation otherwise.
    //  is_parent_path(b"lib", b"lib/index/entry.rb") returns true
    //  is_parent_path(b"lib/index", b"lib/index/entry.rb") this also returns true so both will be
    //  removed. is_parent_path() takes parent as first arg and child as 2nd, in this case the existing
    //  entry must be the parent dir so is_parent_path(&entry.path, path) returns true
    //  -in the previous case an existing entry was parent of the new entry, now it is the reversed,
    //  the new entry is parent of existing entries. New entry: lib, existing entry: lib/index/entry.rs
    //  we need to delete all the lib/ entries. This is why we make the call to is_parent_path() twice
    fn resolve_conflicts(&mut self, path: &RepoPath) {
        self.entries
            .retain(|entry| !entry.path.is_parent_of(path) || path.is_parent_of(&entry.path))
    }

    // This method we have seen before on Leetcode is the next_greater() adaptation of binary search,
    // or it is treated as such. To determine if `path` is parent path of any of the entries, we need
    // to find the entry that is lexicographically greater than `path`. The entries that we are actually
    // going to return true are entries that their path length is always greater than length of the
    // provided path because child_path.len() > parent_path.len(). Sure foo/bar.txt is one potential
    // candidate as is lexicographically greater than a/b/ but so is a/b/foo.txt.
    fn lower_bound(&self, path_bytes: &[u8]) -> usize {
        self.entries
            .binary_search_by(|entry| entry.path.as_bytes().cmp(path_bytes))
            .unwrap_or_else(|pos| pos)
    }

    pub(crate) fn contains(&self, path: &RepoPath) -> bool {
        self.entries
            .binary_search_by(|entry| entry.path.cmp(path))
            .is_ok()
    }
}

#[derive(PartialEq, Eq)]
pub(crate) struct IndexEntry {
    pub(crate) stat: FileStat,
    pub(crate) mode: Mode,
    pub(crate) oid: [u8; 20],
    flags: u16,
    pub(crate) path: RepoPath,
}

impl IndexEntry {
    pub(crate) fn new(
        path: RepoPath,
        oid: [u8; 20],
        mode: Mode,
        stat: FileStat,
    ) -> Self {
        // https://git-scm.com/docs/index-format
        // the lowest 12 bits store the name length
        // if the length is less than 0xFFF; otherwise 0xFFF is stored in this field.
        let flags = path.len().min(PATH_MAX_SIZE as usize) as u16;

        Self {
            stat,
            oid,
            flags,
            path,
            mode,
        }
    }

    pub(crate) fn times_match(&self, other: &FileStat) -> bool {
        self.stat.ctime == other.ctime
            && self.stat.ctime_nsec == other.ctime_nsec
            && self.stat.mtime == other.mtime
            && self.stat.mtime_nsec == other.mtime_nsec
    }

    fn serialize(&self) -> Vec<u8> {
        // 10 fields, 4 bytes each
        // 20 bytes for the oid
        // 2 bytes for flags
        // at least 1 NUL byte, at this point at least 63
        // path bytes
        // padding
        let mut bytes = Vec::with_capacity(64);
        bytes.extend_from_slice(&self.stat.ctime.to_be_bytes());
        bytes.extend_from_slice(&self.stat.ctime_nsec.to_be_bytes());
        bytes.extend_from_slice(&self.stat.mtime.to_be_bytes());
        bytes.extend_from_slice(&self.stat.mtime_nsec.to_be_bytes());
        bytes.extend_from_slice(&self.stat.dev.to_be_bytes());
        bytes.extend_from_slice(&self.stat.ino.to_be_bytes());
        // the next part is confusing in the docs
        // it says: 32-bit mode, 4-bit type, 3 unused 9-bit permissions(3 per group)
        //
        // 31                                                0
        //  ┌────────────┬─────────┬─────────────────────────┐
        //  │ 4-bit type │ 3 unused│ 9-bit Unix permissions  │
        //  └────────────┴─────────┴─────────────────────────┘
        //
        // bits 31..16: zero
        // bits 15..12: object type
        // bits 11..9:  unused
        // bits 8..0:   Unix permission bits
        //
        // 100644 octal
        // =
        // 1000 000 110100100 binary (16bits the rest are padded with zeros)
        // │    │   │
        // │    │   └── 9 permission bits: 0644
        // │    └────── 3 unused bits: 000
        // └─────────── 4 object-type bits: 1000
        //
        // let mode: u32 = 0o100644;
        //
        // let object_type = (mode >> 12) & 0b1111;
        // let unused = (mode >> 9) & 0b111;
        // let permissions = mode & 0o777;
        //
        // we get the exact same behavior by calling Mode::from_raw() when parsing those bytes, for
        // storing is just we write those in BE.
        bytes.extend_from_slice(&(self.mode as u32).to_be_bytes());
        bytes.extend_from_slice(&self.stat.uid.to_be_bytes());
        bytes.extend_from_slice(&self.stat.gid.to_be_bytes());
        bytes.extend_from_slice(&self.stat.file_size.to_be_bytes());
        bytes.extend_from_slice(&self.oid); // in the docs this is called Object name
        bytes.extend_from_slice(&self.flags.to_be_bytes());
        bytes.extend_from_slice(&self.path.as_bytes()); // in the docs, is called Entry path name
        // the path bytes are always followed by 1 NULL byte, and then we do padding until we have a
        // multiple of 8
        //
        // For any path 4095 bytes or longer, the format stores 0xFFF as a sentinel meaning "too long"
        // For those paths, the length field gives us nothing, we still need to find where the path
        // ends is by scanning for the NUL terminator.
        bytes.push(0);

        // The size of each record(entry) according to the format rules has to be divisible by 8
        // Since the path has variable length, the total size of an entry is not fixed.
        //
        // Why did they choose padding on v2/v3?
        //
        // I couldn't find the exact answer to that question. Maybe it has to with mmap. At first,
        // I thought it will be some sort of 8-alignment, but not sure. Padding is dropped in v4.
        // Maybe it is a rule that was set that each entry size must be a multiple of 8 bytes. Maybe
        // it is to make the parsing process following a simple boundary rule where next_entry_offset
        // = current_entry_offset + padded_entry_size
        //
        // next_entry_offset = current_entry_offset + padded_entry_size
        //
        // header
        // offset 12:  [ fixed entry fields ... 62 bytes ... ]
        // offset 74:  f
        // offset 75:  o
        // offset 76:  o
        // offset 77:  \0   <- path terminator
        // offset 78:  \0   <- padding
        // offset 79:  \0   <- padding
        // offset 80:  \0   <- padding
        // offset 81:  \0   <- padding
        // offset 82:  \0   <- padding
        // offset 83:  \0   <- padding
        // offset 84:  next entry starts here
        //
        // current_entry_offset = 12
        // padded_entry_size = 72
        //
        // next_entry_offset = 12 + 72 = 84
        while bytes.len() % 8 != 0 {
            bytes.push(0);
        }
        bytes
    }
}

// an error that can occur when creating IndexEntry or parsing .lit/index
// Git when format is corrupted reports: Unknown Index Format
// No more information is provided to the user because they can't do much if the format is invalid
#[derive(Debug)]
pub(crate) enum IndexError {
    InvalidChecksum,
    UnsupportedVersion(u32),
    InvalidFormat(FormatError),
    Io { path: PathBuf, source: io::Error },
}

// TODO: we need to refactor this to include the actual bad path
#[derive(Debug)]
pub(super) enum PathError {
    Empty,
    LeadingSlash,
    TrailingSlash,
    EmptyComponent,
    ReservedComponent,
    ContainsNul,
}

#[derive(Debug)]
pub(super) struct FormatError {
    pub(super) offset: usize,
    pub(super) kind: FormatErrorKind,
}

impl FormatError {
    pub(super) fn at(offset: usize, kind: FormatErrorKind) -> Self {
        Self { offset, kind }
    }
}

#[derive(Debug)]
pub(super) enum FormatErrorKind {
    // TODO: rethink this Eof
    Eof { needed: usize, remaining: usize },
    InvalidSignature,
    EntriesNotSorted,
    EntriesCountMissMatch { actual: usize, expected: usize },
    InvalidMode(u32),
    InvalidNanoseconds,
    MissingNulTerminator,
    InvalidPadding,
    LongPathLenMissMatch,
    InvalidPathSyntax(PathError),
}

impl From<FormatError> for IndexError {
    fn from(err: FormatError) -> Self {
        IndexError::InvalidFormat(err)
    }
}