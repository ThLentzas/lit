mod parse;
pub(super) mod tree;

use crate::cmd::error::{FormatError, FormatErrorKind, IndexError, PathError};
use crate::cmd::index::parse::Parser;
use crate::cmd::os;
use sha1::{Digest, Sha1};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
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
const SIGNATURE: &'static str = "DIRC";

// toDo: caching tree
pub(super) struct Index {
    // TODO: on the rewrite test if a BTreeMap would work
    pub(super) entries: Vec<IndexEntry>,
    pub(super) path: PathBuf,
    // a flag that is used to not unnecessary write if no changes detected
    pub(super) modified: bool,
}

// .lit/index is created lazily on the first write
impl Index {
    pub(super) fn new(path: PathBuf) -> Self {
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
    pub(super) fn is_tracked(&self, path: &Path) -> bool {
        let path = path_to_bytes(path);

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
        let mut path = path.clone();
        path.push(b'/');
        let pos = self.lower_bound(path.as_slice());
        self.entries.get(pos)
            .is_some_and(|entry| is_parent_path(path.as_slice(), entry.path.as_slice()))
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
    pub(super) fn add_entries(&mut self, entries: Vec<IndexEntry>) -> Result<(), IndexError> {
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

    pub(super) fn refresh_entry_stat(&mut self, index: usize, stat: StatNode) {
        self.entries[index].stat = stat;
    }

    pub(super) fn serialize(&self) -> Vec<u8> {
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
    pub(super) fn load(&mut self) -> Result<(), IndexError> {
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
    fn resolve_conflicts(&mut self, path: &[u8]) {
        self.entries.retain(|entry| {
            !(is_parent_path(&entry.path, path) || is_parent_path(path, &entry.path))
        })
    }

    // This method we have seen before on Leetcode is the next_greater() adaptation of binary search,
    // or it is treated as such. To determine if `path` is parent path of any of the entries, we need
    // to find the entry that is lexicographically greater than `path`. The entries that we are actually
    // going to return true are entries that their path length is always greater than length of the
    // provided path because child_path.len() > parent_path.len(). Sure foo/bar.txt is one potential
    // candidate as is lexicographically greater than a/b/ but so is a/b/foo.txt.
    fn lower_bound(&self, path: &[u8]) -> usize {
        self.entries.binary_search_by(|entry| entry.path.as_slice().cmp(path))
            .unwrap_or_else(|pos| pos)
    }
    
    pub(super) fn contains(&self, path: &[u8]) -> bool {
        self.entries
            .binary_search_by(|entry| entry.path.as_slice().cmp(path))
            .is_ok()
    }
}

// toDo: add GitLink support.
// cheap copy only 40 bytes
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub(super) struct StatNode {
    // change time, most recent time a file's attributes changed(owner group, perm, etc)
    pub(super) ctime: u32,
    pub(super) ctime_nsec: u32,
    // modify time, most recent time a file's contents changed
    pub(super) mtime: u32,
    pub(super) mtime_nsec: u32,
    pub(super) dev: u32,
    pub(super) ino: u32,
    pub(super) mode: u32,
    pub(super) uid: u32,
    pub(super) gid: u32,
    // on disk size, truncated to 32-bit
    pub(super) file_size: u32,
}

#[derive(PartialEq, Eq)]
pub(super) struct IndexEntry {
    pub(super) stat: StatNode,
    pub(super) oid: [u8; 20],
    flags: u16,
    // always relative to root
    pub(super) path: Vec<u8>,
}

impl IndexEntry {
    pub(super) fn new(path: Vec<u8>, oid: [u8; 20], stat: StatNode) -> Self {
        // https://git-scm.com/docs/index-format
        // the lowest 12 bits store the name length
        // if the length is less than 0xFFF; otherwise 0xFFF is stored in this field.
        let flags = path.len().min(PATH_MAX_SIZE as usize) as u16;

        Self {
            stat,
            oid,
            flags,
            path,
        }
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
        bytes.extend_from_slice(&self.stat.mode.to_be_bytes());
        bytes.extend_from_slice(&self.stat.uid.to_be_bytes());
        bytes.extend_from_slice(&self.stat.gid.to_be_bytes());
        bytes.extend_from_slice(&self.stat.file_size.to_be_bytes());

        bytes.extend_from_slice(&self.oid);
        bytes.extend_from_slice(&self.flags.to_be_bytes());
        bytes.extend_from_slice(&self.path);
        // the path bytes are always followed by 1 NULL byte, and then we do padding until we have a
        // multiple of 8
        //
        // For any path 4095 bytes or longer, the format stores 0xFFF as a sentinel meaning "too long,
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

fn validate_path(path: &[u8]) -> Result<(), PathError> {
    if path.is_empty() {
        return Err(PathError::Empty);
    }
    // paths are relative to the repository root, so no leading slash.
    if path[0] == b'/' {
        return Err(PathError::LeadingSlash);
    }
    // trailing slash is not allowed.
    if path[path.len() - 1] == b'/' {
        return Err(PathError::TrailingSlash);
    }

    for component in path.split(|&b| b == b'/') {
        // empty components: "src//main.rs"
        if component.is_empty() {
            return Err(PathError::EmptyComponent);
        }
        // ".", "..", and ".lit" as path components are not allowed
        // src/./main.rs: stays in the current directory, redundant and not a real subdirectory
        // src/../etc/passwd: escapes upward, would let a crafted index reference files outside the repo
        // .lit/config: points into Lit's own metadata, never legitimate as a tracked file.
        if matches!(component, b"." | b".." | b".lit") {
            return Err(PathError::ReservedComponent);
        }
        // NUL cannot appear inside the path.
        if component.contains(&0) {
            return Err(PathError::ContainsNul);
        }
    }
    Ok(())
}

pub(super) fn path_to_bytes(path: &Path) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut components = path.iter().peekable();

    while let Some(component) = components.next() {
        // Git compares entry names as raw byte sequences(memcmp), no encoding, no locale, no
        // platform variation
        bytes.extend_from_slice(os::name_as_bytes(component));
        // no trailing slash
        if components.peek().is_some() {
            bytes.push(b'/');
        }
    }
    bytes
}

// read resolve_conflicts() first
//
// in order to have a conflict we need them to share the same parent directories, lib/index/main.rs
// and src/index/main.rs are fine. We need to answer the question: Does the child start with the
// parent path, and is the next byte a / ?
//
// the length of the child must be greater than the parent because parent is part of the child
// for a conflict to exist the parent must be a parent dir of child
// the 3rd condition is to avoid a false match, like parent: lib, child: library/file.rs it is not
// enough for it to be a prefix, it has to be a parent dir
fn is_parent_path(parent: &[u8], child: &[u8]) -> bool {
    child.len() > parent.len() && child.starts_with(parent) && child[parent.len()] == b'/'
}
