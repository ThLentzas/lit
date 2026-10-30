use crate::repo::object::mode::Mode;
use crate::repo::object::oid::Oid;
use crate::repo::os::FileStat;
use crate::repo::repopath::RepoPath;
use std::borrow::Borrow;
use std::cmp::Ordering;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct IndexEntryMetadata {
    pub(crate) stat: FileStat,
    pub(crate) mode: Mode,
    pub(crate) oid: Oid,
    pub(crate) flags: u16,
}

impl IndexEntryMetadata {
    pub(crate) fn times_match(&self, other: &FileStat) -> bool {
        self.stat.ctime == other.ctime
            && self.stat.ctime_nsec == other.ctime_nsec
            && self.stat.mtime == other.mtime
            && self.stat.mtime_nsec == other.mtime_nsec
    }
}

#[derive(Debug)]
pub(crate) struct IndexEntry {
    metadata: IndexEntryMetadata,
    pub(crate) path: RepoPath,
}

impl PartialEq for IndexEntry {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
    }
}

impl Eq for IndexEntry {}

impl PartialOrd for IndexEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for IndexEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.path.cmp(other.path())
    }
}

impl Borrow<RepoPath> for IndexEntry {
    fn borrow(&self) -> &RepoPath {
        self.path()
    }
}

impl Borrow<[u8]> for IndexEntry {
    fn borrow(&self) -> &[u8] {
        self.path.as_bytes()
    }
}

impl IndexEntry {
    pub(crate) fn new(path: RepoPath, oid: Oid, mode: Mode, stat: FileStat) -> Self {
        // https://git-scm.com/docs/index-format
        // the lowest 12 bits store the name length
        // if the length is less than 0xFFF; otherwise 0xFFF is stored in this field.
        // newly added entries have the remaining bits set to 0
        let flags = path.len().min(super::PATH_MAX_SIZE as usize) as u16;
        let metadata = IndexEntryMetadata {
            stat,
            mode,
            oid,
            flags,
        };

        Self { metadata, path }
    }

    pub(crate) fn path(&self) -> &RepoPath {
        &self.path
    }

    pub(crate) fn oid(&self) -> &Oid {
        &self.metadata.oid
    }

    pub(crate) fn stat(&self) -> &FileStat {
        &self.metadata.stat
    }

    pub(crate) fn stat_mut(&mut self) -> &mut FileStat {
        &mut self.metadata.stat
    }

    pub(crate) fn metadata(&self) -> &IndexEntryMetadata {
        &self.metadata
    }

    pub(super) fn metadata_mut(&mut self) -> &mut IndexEntryMetadata {
        &mut self.metadata
    }

    pub(crate) fn mode(&self) -> &Mode {
        &self.metadata.mode
    }

    pub(crate) fn file_size(&self) -> u32 {
        self.metadata.stat.file_size
    }

    // used when we construct entries from fields validated by the index parser
    // unlike `new()`, which computes the path-length bits and clears the upper flag bits, we preserve
    // the flags read from disk
    pub(super) fn from_parsed_parts(
        stat: FileStat,
        mode: Mode,
        oid: Oid,
        flags: u16,
        path: RepoPath,
    ) -> Self {
        let metadata = IndexEntryMetadata {
            stat,
            mode,
            oid,
            flags,
        };
        Self { metadata, path }
    }

    pub(super) fn serialize(&self) -> Vec<u8> {
        // 10 fields, 4 bytes each
        // 20 bytes for the oid
        // 2 bytes for flags
        // at least 1 NUL byte, at this point at least 63
        // path bytes
        // padding
        let mut bytes = Vec::with_capacity(64);
        bytes.extend_from_slice(&self.metadata.stat.ctime.to_be_bytes());
        bytes.extend_from_slice(&self.metadata.stat.ctime_nsec.to_be_bytes());
        bytes.extend_from_slice(&self.metadata.stat.mtime.to_be_bytes());
        bytes.extend_from_slice(&self.metadata.stat.mtime_nsec.to_be_bytes());
        bytes.extend_from_slice(&self.metadata.stat.dev.to_be_bytes());
        bytes.extend_from_slice(&self.metadata.stat.ino.to_be_bytes());
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
        bytes.extend_from_slice(&(self.metadata.mode as u32).to_be_bytes());
        bytes.extend_from_slice(&self.metadata.stat.uid.to_be_bytes());
        bytes.extend_from_slice(&self.metadata.stat.gid.to_be_bytes());
        bytes.extend_from_slice(&self.metadata.stat.file_size.to_be_bytes());
        bytes.extend_from_slice(self.oid().as_bytes()); // in the docs this is called Object name
        bytes.extend_from_slice(&self.metadata.flags.to_be_bytes());
        bytes.extend_from_slice(self.path.as_bytes()); // in the docs, is called Entry path name
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
