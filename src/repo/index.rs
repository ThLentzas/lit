pub(crate) mod entry;
mod parse;

use crate::repo::index::entry::{IndexEntry, IndexEntryMetadata};
use crate::repo::index::parse::Parser;
use crate::repo::os::{FileStat, IoError, OsPath};
use crate::repo::repopath::{RepoPath, RepoPathError};
use sha1::{Digest, Sha1};
use std::{error::Error, fmt, fs, io, ptr};

// git has multiple versions(2, 3, 4) for the index format
const INDEX_VERSION: u32 = 2;
const PATH_MAX_SIZE: u16 = 0xfff;
const SIGNATURE: &str = "DIRC";

// TODO: cache tree is part of the extensions that Index supports
// TODO: we need to add support for `core.ignorecase`, it is probed and recorded during init, but not
//  considered during working-tree/index name matching.
// Things that I tried:
//  BTreeMap<RepoPath, IndexEntry>: we have to clone every time to store the path.
//  BTreeMap<RepoPath, IndexEntryMetadata>: works but then we lose `IndexEntry`. It gives us the
//  ability to mutate metadata while keys remain immutable. Required a lot of changes on the existing
//  index methods.
//  BTreeSet<IndexEntry>: this worked but with issues since it treats the entire entry as key.
//  Implemented `Ord` to only consider `entry.path`, and `Borrow<[u8]>` so we can search entries based
//  on the underlying path buffer. We can't get `&mut IndexEntry` because it would break the order,
//  so any modifications were remove, modify?, insert. Another issue was when we were doing the setup
//  for the `update` option, we would have to iterate over the existing entries and find which of
//  those we have to modify. The result was a `Vec<&RepoPath>. The operation then dies because we
//  would have 2 active references for index, 1 from the calling `index.iter()` to collect the paths
//  and the mutable one to perform the mutations. The solution to that was to clone each path. If the
//  user called `update .` from root and no new files were added just modifications of existing ones
//  we would have to clone everything.
//
// Index is represented by a sorted array. Entries are unique by raw repository-relative path bytes.
// No mutable entry iterator is exposed. Every mutation must go through Index so `modified` is
// maintained, and paths cannot be modified by accident. This invariant must hold no matter what is
// the underlying structure. DO NOT expose any methods that return `&mut IndexEntry`
pub(crate) struct Index {
    entries: Vec<IndexEntry>,
    path: OsPath,
    modified: bool,
}

// .lit/index is created lazily on the first write
impl Index {
    pub(crate) fn new(path: OsPath) -> Self {
        Self {
            path,
            entries: Vec::new(),
            modified: false,
        }
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &IndexEntry> {
        self.entries.iter()
    }

    pub(crate) fn into_entries(self) -> impl Iterator<Item = IndexEntry> {
        self.entries.into_iter()
    }

    pub(crate) fn path(&self) -> &OsPath {
        &self.path
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn is_modified(&self) -> bool {
        self.modified
    }

    pub(crate) fn get(&self, path: &RepoPath) -> Option<&IndexEntry> {
        self.search(path).ok().map(|pos| &self.entries[pos])
    }

    pub(crate) unsafe fn get_unchecked(&self, pos: usize) -> &IndexEntry {
        unsafe { self.entries.get_unchecked(pos) }
    }

    pub(crate) fn contains(&self, path: &RepoPath) -> bool {
        self.search(path).is_ok()
    }

    // This method is used by status to see if the entry pointed by `path` is tracked by the index.
    // If `path` points to a file, a single binary search call is enough. Directories though, since
    // are not tracked by Index are treated differently. Read below.
    // An optimization that Git does is that when a directory contains no tracked files anywhere
    // inside it, status reports the directory itself, not every file below it. So if `foo` contains
    // `bar.txt`, and `baz.txt`, instead of listing both as untracked it lists `foo/`.
    //      workspace:
    //          a/b/inner.txt      tracked
    //          a/outer.txt        untracked
    //          a/b/c/file.txt     untracked
    //
    //      index:
    //          a/b/inner.txt
    //
    // The result of status for untracked files is: `a/b/c`, `a/outer.txt`. even though `a/ is not
    // literally in the index, it is a 'tracked' directory for status purposes because it contains
    // `a/b/inner.txt` which is tracked. `a/b` is also tracked since it contains `inner.txt`.
    //
    // The rule is:
    //      A path is tracked for status if:
    //          1. the exact file path exists in the index, OR
    //          2. the path is a directory prefix of at least one index entry.
    //
    // The exact file is easy as mentioned. For the dir case, read lower_bound()
    // This logic handles untracked(non-empty) directories.
    pub(crate) fn is_tracked(&self, path: &RepoPath) -> bool {
        // path points to file
        if self.contains(path) {
            return true;
        }
        // TODO: test it
        // If the entry is not found it means either that it is an untracked file or that is a dir.
        // We need to see if it is a 'tracked' directory, a directory whose entries are tracked.
        // path: a/b
        //  [a/b.txt, a/b/c.txt], '.' (0x2E) sorts before '/c' (0x2F)
        //
        //  for path to be parent dir of any entry three properties must hold true.
        //      - child.len() > parent.len()
        //      - parent is a prefix of child
        //      - child[parent.len()] must be '/'
        //  It is the same logic as `RepoPath::is_parent_of()`. Appending `\` handles the 3rd condition
        //  and the `starts_with()` call checks the prefix. The 1st condition is guarantee by Index
        //  where an entry's path never has a trailing slash, so any entry that starts with
        // `path` + '/' is necessarily longer than `path`.
        let mut path_bytes = Vec::with_capacity(path.as_bytes().len() + 1);
        path_bytes.extend_from_slice(path.as_bytes());
        path_bytes.push(b'/');
        let entry = self.lower_bound(&path_bytes);
        entry.is_some_and(|entry| entry.path().as_bytes().starts_with(&path_bytes))
    }

    // Git requires index entries to be sorted by filename. It is a requirement for deterministic
    // hashing. When we try to hash a tree with 2 entries a and b, we need a and b to have fixed
    // order otherwise we will compute a different hash for the same tree which breaks Git's invariance
    // same content -> same hash
    //
    // We enforce sorted, unique index entries to achieve the same behavior. Maintaining the sorted
    // order prevents the above issue where we could produce different hash for the same tree. By
    // sorting based on the path we maintain the order across all components of the path.
    // `foo/src/a`, `foo/src/b` and `foo/README`. In the final order, `README` appears before `src`
    // as the entries of `foo` and `a` appears before `b` as the entries of `src`. Inserting in
    // ascending order allows us to do BS for retrieving entries instead of appending + sorting.
    // Read more Tree::write()
    pub(crate) fn insert(&mut self, entry: IndexEntry) -> Result<usize, IndexError> {
        self.resolve_conflicts(entry.path());

        let pos = match self.search(entry.path()) {
            Ok(pos) => {
                if self.entries[pos].metadata() != entry.metadata() {
                    self.entries[pos] = entry;
                    self.modified = true;
                }
                pos
            }
            Err(pos) => {
                self.entries.insert(pos, entry);
                self.modified = true;
                pos
            }
        };
        Ok(pos)
    }

    // `entry.path` remains unchanged so the order never breaks
    pub(crate) fn update_entry(&mut self, path: &RepoPath, metadata: IndexEntryMetadata) -> bool {
        let Ok(pos) = self.search(path) else {
            return false;
        };

        let entry = &mut self.entries[pos];
        if *entry.metadata() != metadata {
            *entry.metadata_mut() = metadata;
            self.modified = true;
            true
        } else {
            false
        }
    }

    pub(crate) unsafe fn update_entry_unchecked(
        &mut self,
        pos: usize,
        metadata: IndexEntryMetadata,
    ) -> bool {
        let entry = &mut self.entries[pos];
        if *entry.metadata() != metadata {
            *entry.metadata_mut() = metadata;
            self.modified = true;
            true
        } else {
            false
        }
    }

    pub(crate) fn remove(&mut self, path: &RepoPath) -> Option<IndexEntry> {
        let Ok(pos) = self.search(path) else {
            return None;
        };
        let entry = self.entries.remove(pos);
        self.modified = true;

        Some(entry)
    }

    // the code below is taken Rust's `try_remove()` impl
    // https://doc.rust-lang.org/beta/src/alloc/vec/mod.rs.html#2437-2452
    //
    // an alternative would be to call `try_remove().unwrap_unchecked()`
    pub(crate) unsafe fn remove_unchecked(&mut self, pos: usize) -> IndexEntry {
        let len = self.entries.len();

        unsafe {
            let entry;
            {
                // the place we are taking from.
                let ptr = self.entries.as_mut_ptr().add(pos);
                // copy it out, unsafely having a copy of the value on
                // the stack and in the vector at the same time.
                entry = ptr::read(ptr);
                // Shift everything down to fill in that spot.
                ptr::copy(ptr.add(1), ptr, len - pos - 1);
            }
            self.entries.set_len(len - 1);
            self.modified = true;

            entry
        }
    }

    pub(crate) fn refresh_stat(&mut self, path: &RepoPath, stat: FileStat) -> bool {
        let Ok(pos) = self.search(path) else {
            return false;
        };

        let entry = &mut self.entries[pos];
        if *entry.stat() != stat {
            *entry.stat_mut() = stat;
            self.modified = true;
            true
        } else {
            false
        }
    }

    pub(crate) unsafe fn refresh_stat_unchecked(&mut self, pos: usize, stat: FileStat) -> bool {
        let entry = &mut self.entries[pos];
        if *entry.stat() != stat {
            *entry.stat_mut() = stat;
            self.modified = true;
            true
        } else {
            false
        }
    }

    pub(crate) fn serialize(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(SIGNATURE.as_bytes());
        bytes.extend_from_slice(&INDEX_VERSION.to_be_bytes());
        bytes.extend_from_slice(&(self.entries.len() as u32).to_be_bytes());
        // TODO: at this point according to the format we add Extensions
        for entry in self.entries.iter() {
            bytes.extend(entry.serialize());
        }
        let checksum: [u8; 20] = Sha1::digest(&bytes).into();
        bytes.extend_from_slice(&checksum);

        bytes
    }

    // loads the contents of .lit/index
    pub(crate) fn load(&mut self) -> Result<(), IndexError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            // happens on a new repo, `Index` will be created lazily on the first write
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                self.entries.clear();
                self.modified = false;

                return Ok(());
            }
            Err(err) => {
                return Err(IndexError::Io(IoError::new(
                    "read",
                    Some(self.path.clone()),
                    err,
                )));
            }
        };

        let (content, checksum): (&[u8], &[u8; 20]) =
            bytes.split_last_chunk::<20>().ok_or_else(|| {
                FormatError::at(
                    0,
                    FormatErrorKind::UnexpectedEof {
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
        //
        // checking the SHA-1 before parsing anything is a boundary we set,
        // we don't yet have reason to trust the bytes enough to report on their internal structure.
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
        // we need to be explicit with the type because the check for last.path happens before the
        // entries.push(entry) where the type can be inferred.
        let mut entries: Vec<IndexEntry> = Vec::new();
        // if we exhausted the buffer before we reach count we get UnexpectedEof
        for _ in 0..count {
            let entry_offset = parser.offset();
            let entry = parser.next()?;
            // don't do the C++/Java way of
            //  if !self.entries.is_empty() {
            //  let last = self.entries.last().unwrap();
            if let Some(last) = entries.last() {
                // if the entries are not in ascending order the format is invalid
                // in a sorted list duplicates must appear next to each other, and we check each entry
                // with the previous one
                //
                // if entry.path == last.path, the duplicate is adjacent and caught here
                // if entry.path < last.path, sort order is broken; any earlier duplicate would also
                // surface as a non-ascending pair somewhere in the sequence
                // TODO: address sorting order when we add stage support during merge
                if last.path() >= entry.path() {
                    return Err(FormatError::at(
                        entry_offset,
                        FormatErrorKind::EntriesOutOfSorted,
                    ))?;
                }
            }
            entries.push(entry);
        }

        // there are trailing bytes between the last index entry and the checksum
        // this also handles the case where the actual number of entries was greater than count
        if !parser.is_empty() {
            return Err(FormatError::at(
                parser.offset(),
                FormatErrorKind::TrailingData {
                    remaining: parser.remaining(),
                },
            ))?;
        }
        self.entries = entries;

        Ok(())
    }

    // we need to consider 2 edge cases:
    // - an existing file becomes a directory. We have an entry called `foo` and we want to add
    // `foo/bar.rs`. We must remove the existing `foo` first before adding the new one.
    // - an existing directory becomes a file. Now we have the opposite, existing entry `foo/bar.rs`
    // and we want to add `foo`. In this case, all the entries that start with `foo/` will be removed
    //
    // In both examples, `foo` is the parent. In the first case, is the existing entry so we catch
    // it with `entry.path.is_parent_of(path)` and in the second, it is the argument,
    // `path.is_parent_of(&entry.path)` catches it. We can't have `||` here. If we look at the 2nd
    // case, we do `path.is_parent_of(&entry.path)` for `foo/bar.rs` and `foo` which returns true
    // so we keep the entry which is incorrect.
    //
    // Such cases represent invalid states because no fs allows 2 entries to have the same name
    fn resolve_conflicts(&mut self, path: &RepoPath) {
        let old_len = self.entries.len();
        // remove an existing entry if either path is a parent of the other
        self.entries
            .retain(|entry| !entry.path().is_parent_of(path) && !path.is_parent_of(entry.path()));
        // if the index is already modified keep it as is else is the result of attempting removal
        self.modified |= self.entries.len() != old_len
    }

    // This is the `next_greater()` adaptation of bs, or it is treated as such.
    // To determine if `path` is parent path of the entries, we need to find the entry that is
    // lexicographically greater than `path`. The entries that are actually going to return true are
    // entries that their path length is always greater than length of the provided path because
    // child_path.len() > parent_path.len(). Sure `foo/bar.txt` is one potential candidate as is
    // lexicographically greater than `a/b/` but so is `a/b/foo.txt`.
    fn lower_bound(&self, path_bytes: &[u8]) -> Option<&IndexEntry> {
        let position = self
            .entries
            .partition_point(|entry| entry.path().as_bytes() < path_bytes);
        self.entries.get(position)
    }

    fn search(&self, path: &RepoPath) -> Result<usize, usize> {
        self.entries
            .binary_search_by(|entry| entry.path().cmp(path))
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
    Io(IoError),
}

impl IndexError {
    pub(crate) fn is_format_error(&self) -> bool {
        matches!(
            self,
            IndexError::InvalidChecksum
                | IndexError::UnsupportedVersion(_)
                | IndexError::InvalidFormat(_)
        )
    }
}

impl Error for IndexError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidFormat(source) => Some(source),
            Self::Io(source) => Some(source),
            Self::InvalidChecksum => None,
            Self::UnsupportedVersion(_) => None,
        }
    }
}

// errors that have IndexError as a variant will include a generic error message and not carry the
// actual format error since the user can't do anything if the format is broken
impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidChecksum => write!(f, "invalid index checksum"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported index version {version}")
            }
            // TODO: this needs to change, index format errors should not be exposed to the user
            //  even if they know the issue how are they going to fix it?
            Self::InvalidFormat(err) => write!(f, "{err}"),
            Self::Io(_) => write!(f, "index I/O failed"),
        }
    }
}

#[derive(Debug)]
pub(crate) enum FormatErrorKind {
    UnexpectedEof { needed: usize, remaining: usize },
    InvalidSignature,
    EntriesOutOfSorted,
    InvalidMode(u32),
    InvalidNanoseconds,
    MissingNulTerminator,
    InvalidPadding,
    LongPathLenMisMatch,
    InvalidPathSyntax(RepoPathError),
    ExtendedFlagInV2,
    TrailingData { remaining: usize },
}

impl fmt::Display for FormatErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEof { needed, remaining } => {
                write!(
                    f,
                    "unexpected end of file: needed {needed} bytes, only {remaining} remaining"
                )
            }
            Self::InvalidSignature => write!(f, "invalid index signature"),
            Self::EntriesOutOfSorted => write!(f, "index entries are out of order"),
            Self::InvalidMode(mode) => write!(f, "invalid index entry mode {mode:#o}"),
            Self::InvalidNanoseconds => write!(f, "invalid nanosecond value"),
            Self::MissingNulTerminator => write!(f, "missing NUL terminator"),
            Self::InvalidPadding => write!(f, "invalid index entry padding"),
            Self::LongPathLenMisMatch => {
                write!(f, "path length does not match the index flags")
            }
            Self::InvalidPathSyntax(err) => {
                write!(f, "invalid index path: {err}")
            }
            Self::ExtendedFlagInV2 => write!(f, ""),
            Self::TrailingData { remaining } => {
                write!(f, "unexpected trailing data: {remaining} bytes remaining")
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct FormatError {
    pub(super) offset: usize,
    pub(super) kind: FormatErrorKind,
}

impl FormatError {
    pub(super) fn at(offset: usize, kind: FormatErrorKind) -> Self {
        Self { offset, kind }
    }
}

impl Error for FormatError {}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid index at byte {}: {}", self.offset, self.kind)
    }
}

impl From<FormatError> for IndexError {
    fn from(err: FormatError) -> Self {
        Self::InvalidFormat(err)
    }
}
