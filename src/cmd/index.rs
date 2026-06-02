use crate::cmd::error::{IndexError, IndexFormatError, IndexFormatErrorKind};
use crate::cmd::lockfile::Lockfile;
use crate::cmd::os;
use sha1::{Digest, Sha1};
use std::fs;
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

#[derive(Default)]
pub(super) struct Index {
    entries: Vec<IndexEntry>,
    modified: bool,
}

impl Index {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn add_entry(&mut self, entry: IndexEntry) {
        self.resolve_conflicts(&entry.path);

        match self.entries.binary_search_by(|e| e.path.cmp(&entry.path)) {
            Ok(pos) => {
                // don't compare the oid because two different files can have the same exact content
                // or it is the same file and some metadata changed(mode, mtime)
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

    // updating index is the same as updating head
    // we have to address competing writes and make sure that when someone tries to read .lit/index
    // they never read half-written data, both are guarantees by lockfile
    //
    pub(super) fn update(
        &mut self,
        path: &Path,
        entries: Vec<IndexEntry>,
    ) -> Result<(), IndexError> {
        let mut lockfile = match Lockfile::acquire(path) {
            Ok(lock) => match lock {
                Some(lockfile) => lockfile,
                None => {
                    return Err(IndexError::LockDenied {
                        path: path.to_path_buf(),
                    });
                }
            },
            Err(err) => {
                return Err(IndexError::Io {
                    path: path.to_path_buf(),
                    source: err,
                });
            }
        };
        if path.exists() {
            self.load(path)?;
        }

        for entry in entries {
            self.add_entry(entry);
        }

        if !self.modified {
            // if the contents were never modified, we don't need to overwrite, but we must return
            // the lock. Lockfile implements Drop which removes the .lock file. In the book Coglan
            // has a function rollback which does the same thing
            return Ok(());
        }
        let content = self.serialize();
        lockfile.write(&content);
        lockfile.commit();

        Ok(())
    }

    pub(super) fn serialize(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"DIRC");
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
    fn load(&mut self, path: &Path) -> Result<(), IndexError> {
        let bytes = fs::read(path).map_err(|err| IndexError::Io {
            path: path.to_path_buf(),
            source: err,
        })?;

        let (content, checksum): (&[u8], &[u8; 20]) =
            bytes
                .split_last_chunk::<20>()
                .ok_or(IndexError::InvalidIndexFormat(IndexFormatError::at(
                    bytes.len(),
                    IndexFormatErrorKind::Eof {
                        needed: 20,
                        remaining: bytes.len(),
                    },
                )))?;
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
            return Err(IndexError::InvalidIndexFormat(IndexFormatError::at(
                3,
                IndexFormatErrorKind::InvalidChecksum,
            )));
        }

        if &content[..4] != b"DIRC" {
            return Err(IndexFormatError {});
        }

        let version = u32::from_be_bytes(content[4..8].try_into().unwrap());
        if version != INDEX_VERSION {
            return Err(IndexFormatError {});
        }

        let count = u32::from_be_bytes(content[8..12].try_into().unwrap());
        let mut parser = Parser::new(&content[12..]);

        for _ in 0..count {
            let entry = parser.next()?;
            if !self.entries.is_empty() {
                let last = self.entries.last().unwrap();
                // if the entries are not in ascending order the format is invalid
                // in a sorted list duplicates must appear next to each other, and we check each entry
                // with the previous one
                //
                // if entry.path == last.path, the duplicate is adjacent and caught here
                // if entry.path < last.path, sort order is broken; any earlier duplicate would also
                // surface as a non-ascending pair somewhere in the sequence
                // toDo: address sorting order when we add stage support
                if last.path >= entry.path {
                    return Err(IndexFormatError {});
                }
            }
            self.entries.push(entry);
            if self.entries.len() > count as usize {
                return Err(IndexFormatError {});
            }
        }

        if self.entries.len() != count as usize {
            return Err(IndexFormatError {});
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
}

// toDo: add GitLink support.
#[derive(PartialEq, Eq)]
pub(super) struct StatNode {
    pub(super) ctime: u32,
    pub(super) ctime_nsec: u32,
    pub(super) mtime: u32,
    pub(super) mtime_nsec: u32,
    pub(super) dev: u32,
    pub(super) ino: u32,
    pub(super) mode: u32,
    pub(super) uid: u32,
    pub(super) gid: u32,
    pub(super) file_size: u32,
}

#[derive(PartialEq, Eq)]
pub(super) struct IndexEntry {
    stat: StatNode,
    oid: [u8; 20],
    flags: u16,
    // always relative to root
    path: Vec<u8>,
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
        //
        // 64 because even if we had 0 path bytes(invalid case) we would still need 1 NUL byte for padding
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

struct Parser<'a> {
    buffer: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(buffer: &'a [u8]) -> Self {
        Self { buffer, pos: 0 }
    }

    // next always returns a valid IndexEntry
    fn next(&mut self) -> Result<IndexEntry, IndexFormatError> {
        let start = self.pos;

        let stat = StatNode {
            ctime: self.read_u32_be(),
            ctime_nsec: self.read_u32_be(),
            mtime: self.read_u32_be(),
            mtime_nsec: self.read_u32_be(),
            dev: self.read_u32_be(),
            ino: self.read_u32_be(),
            mode: self.read_u32_be(),
            uid: self.read_u32_be(),
            gid: self.read_u32_be(),
            file_size: self.read_u32_be(),
        };
        if !matches!(stat.mode, os::REGULAR | os::EXECUTABLE | os::SYMLINK) {
            return Err(IndexFormatError {});
        }
        // The nanoseconds field represents the fractional part of a second, so it lives in the range
        // [0, 10^9). Anything above that is a second.
        if stat.ctime_nsec >= 1_000_000_00 || stat.mtime_nsec >= 1_000_000_000 {
            return Err(IndexFormatError {});
        }

        let oid = self.take::<20>();
        let flags = self.read_u16_be();
        let mut path_bytes = Vec::new();
        // we extract the lowest 12 bits which is where we stored the path len
        let path_len = flags & 0xfff;
        if path_len == PATH_MAX_SIZE {
            path_bytes.extend(self.read_path_until_nul()?);
        } else {
            path_bytes.extend(self.read_path(path_len as usize)?);
        }

        let size = self.pos - start;
        self.skip_padding(size)?;

        Ok(IndexEntry::new(path_bytes, oid, stat))
    }

    fn read_u32_be(&mut self) -> u32 {
        let val = u32::from_be_bytes(self.buffer[self.pos..self.pos + 4].try_into().unwrap());
        self.advance(4);

        val
    }

    fn read_u16_be(&mut self) -> u16 {
        let val = u16::from_be_bytes(self.buffer[self.pos..self.pos + 2].try_into().unwrap());
        self.advance(2);

        val
    }

    // one of the things that we have to validate is that the path length from flags matches the
    // actual path length.
    //
    // if the real path is longer than path_len, the next byte will not be NUL, and we reject it
    // If the real path is shorter than path_len, then the path slice will include the NUL byte
    // inside it, and validate_index_path() will reject it
    fn read_path(&mut self, path_len: usize) -> Result<Vec<u8>, IndexFormatError> {
        // need path_len bytes for the path, plus 1 byte for the required NUL terminator.
        if self.pos + path_len >= self.buffer.len() {
            return Err(IndexFormatError {});
        }

        let start = self.pos;
        let path_bytes = &self.buffer[start..start + path_len];
        // path name needs to follow the rules of the index format
        validate_index_path(path_bytes)?;

        match self.buffer.get(self.pos) {
            Some(0) => {}
            _ => return Err(IndexFormatError {}),
        }
        self.pos = start + path_len + 1;

        Ok(path_bytes.to_vec())
    }

    // we use this method read the path when it's length exceeds PATH_MAX_SIZE
    //
    // we read until we find NUL, if not the format is invalid
    //
    // if the length of the path we just walked(up to NUL) is less than PATH_MAX_SIZE the format is
    // invalid. At this point, we know the path spans the range [start, nul_pos), so we can validate
    // its name.
    //
    // in read_path() we have to validate that the path length from flags matches the actual path
    // length. One caveat is that we can no longer verify that because we don't keep track of the
    // exact length but we use a sentinel value
    fn read_path_until_nul(&mut self) -> Result<Vec<u8>, IndexFormatError> {
        let start = self.pos;
        let relative_pos = self.buffer[start..]
            .iter()
            .position(|b| *b == 0)
            .ok_or(IndexFormatError {})?;
        // position() returns relative to the slice we searched not the entire buffer
        // the position of the NUL with respect to the whole buffer is start + relative
        let nul_pos = start + relative_pos;
        if nul_pos - start < PATH_MAX_SIZE as usize {
            return Err(IndexFormatError {});
        }

        let path_bytes = &self.buffer[start..nul_pos];
        validate_index_path(path_bytes)?;
        // skip NUL
        self.pos = nul_pos + 1;

        Ok(path_bytes.to_vec())
    }

    // padding formula: size % 8 tells us how far past the last multiple of 8 we are. 67 % 8 = 3
    // 8 - 3 is our padding. 1 edge case to consider, what if it is already multiple of 8 then no
    // padding should be added
    // 64 % 8 = 0, 8 - 0 = 8 which is incorrect we don't need 8 bytes of padding
    // to address that we mod the result with 8.
    // if we need any padding n % 8 = n when n < 8 and 0 when n = 8
    fn skip_padding(&mut self, entry_size: usize) -> Result<(), IndexFormatError> {
        let mut padding = (8 - (entry_size % 8)) % 8;

        while padding > 0 {
            // if it is not null or exhausted the buffer too early it's an invalid format
            match self.buffer.get(self.pos) {
                Some(b) if *b == 0 => {
                    self.advance(1);
                    padding -= 1;
                }
                _ => return Err(IndexFormatError {}),
            }
        }
        Ok(())
    }

    fn advance(&mut self, n: usize) {
        self.pos += n;
    }

    fn take(&mut self, n: usize) -> Result<&[u8], IndexFormatError> {
        let remaining = self.buffer.len().saturating_sub(self.pos);

        if remaining < n {
            return Err(IndexFormatError::at(
                self.pos,
                IndexFormatErrorKind::Eof {
                    needed: n,
                    remaining,
                },
            ));
        }

        let start = self.pos;
        self.advance(n);

        Ok(&self.buffer[start..self.pos])
    }
}

pub(super) fn validate_index_path(path: &[u8]) -> Result<(), IndexFormatError> {
    if path.is_empty() {
        return Err(IndexFormatError {});
    }
    // paths are relative to the repository root, so no leading slash.
    if path[0] == b'/' {
        return Err(IndexFormatError {});
    }
    // trailing slash is not allowed.
    if path[path.len() - 1] == b'/' {
        return Err(IndexFormatError {});
    }

    for component in path.split(|&b| b == b'/') {
        // empty components: "src//main.rs"
        if component.is_empty() {
            return Err(IndexFormatError {});
        }
        // ".", "..", and ".lit" as path components are not allowed
        // src/./main.rs: stays in the current directory, redundant and not a real subdirectory
        // src/../etc/passwd: escapes upward, would let a crafted index reference files outside the repo
        // .lit/config: points into Lit's own metadata, never legitimate as a tracked file.
        if component == b"." || component == b".." || component == b".lit" {
            return Err(IndexFormatError {});
        }
        // NUL cannot appear inside the path.
        if component.contains(&0) {
            return Err(IndexFormatError {});
        }
    }
    Ok(())
}

pub(super) fn to_path_bytes(path: &Path) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut components = path.iter().peekable();

    while let Some(component) = components.next() {
        bytes.extend(os::name_to_bytes(component));
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
