use super::IndexEntry;
use crate::repo::object::mode::Mode;
use crate::repo::object::oid::Oid;
use crate::repo::os::FileStat;
use crate::repo::repopath::{RepoPath, RepoPathError};
use sha1::{Digest, Sha1};
use std::error::Error;
use std::fmt;

const CHECKSUM_LEN: usize = 20;

pub(super) struct Parser<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    pub(super) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub(super) fn parse(&mut self) -> Result<Vec<IndexEntry>, FormatError> {
        let (content, checksum): (&[u8], &[u8; CHECKSUM_LEN]) =
            self.buf.split_last_chunk::<CHECKSUM_LEN>().ok_or_else(|| {
                FormatError::at(
                    self.offset(),
                    FormatErrorKind::UnexpectedEof {
                        needed: CHECKSUM_LEN,
                        remaining: self.buf.len(),
                    },
                )
            })?;
        let hash: [u8; 20] = Sha1::digest(content).into();
        // checking the SHA-1 before parsing anything is a boundary we set,
        // we don't yet have reason to trust the bytes enough to report on their internal structure.
        if hash != *checksum {
            // as offset we use the starting position of the stored checkcsum
            return Err(FormatError::at(
                content.len(),
                FormatErrorKind::InvalidChecksum,
            ));
        }

        self.buf = content;
        let start = self.offset();
        if self.take::<4>()? != super::SIGNATURE.as_bytes() {
            return Err(FormatError::at(start, FormatErrorKind::InvalidSignature));
        }

        let start = self.pos;
        let version = u32::from_be_bytes(*self.take::<4>()?);
        // TODO: when we support multiple index formats, we match on this
        let version = match IndexVersion::try_from(version) {
            Some(version) => version,
            None => return Err(FormatError::at(start, FormatErrorKind::UnsupportedVersion(version))),
        };
        
        let count = u32::from_be_bytes(*self.take::<4>()?);
        // we need to be explicit with the type because the check for `last.path` happens before the
        // entries.push(entry) where the type can be inferred.
        let mut entries: Vec<IndexEntry> = Vec::new();
        for _ in 0..count {
            let entry_offset = self.offset();
            let entry = self.next()?;
            if let Some(last) = entries.last() {
                // if the entries are not in ascending order the format is invalid
                // in a sorted vec duplicates appear next to each other, and we check each entry
                // with the previous one
                //
                // if entry.path == last.path, the duplicate is adjacent and caught here
                // if entry.path < last.path, sort order is broken, any earlier duplicate would also
                // surface as a non-ascending pair somewhere in the sequence
                /*
                 * TODO: address sorting order when we add stage support during merge, when paths
                 * are equal compare based on stage
                 */
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
        if !self.is_empty() {
            return Err(FormatError::at(
                self.offset(),
                FormatErrorKind::TrailingData {
                    remaining: self.remaining(),
                },
            ))?;
        }
        Ok(entries)
    }

    fn next(&mut self) -> Result<IndexEntry, FormatError> {
        let entry_offset = self.offset();
        let [ctime, ctime_nsec, mtime, mtime_nsec, dev, ino] = self.u32_array::<6>()?;
        // The nanoseconds field represents the fractional part of a second, so it lives in the range
        // [0, 10^9). Anything above that is a second.
        if ctime_nsec >= 1_000_000_000 {
            return Err(FormatError::at(
                entry_offset + 4,
                FormatErrorKind::InvalidNanoseconds,
            ));
        }
        if mtime_nsec >= 1_000_000_000 {
            return Err(FormatError::at(
                entry_offset + 12,
                FormatErrorKind::InvalidNanoseconds,
            ));
        }

        let offset = self.offset();
        let mode = self.read_u32()?;
        let mode = Mode::try_from_raw(mode)
            .ok_or_else(|| FormatError::at(offset, FormatErrorKind::InvalidMode(mode)))?;
        // TODO: we need to rethink for DIR when we support sparse
        if !matches!(mode, Mode::Regular | Mode::Executable | Mode::Symlink) {
            return Err(FormatError::at(
                offset,
                FormatErrorKind::InvalidMode(mode as u32),
            ));
        }

        let [uid, gid, file_size] = self.u32_array::<3>()?;
        let stat = FileStat {
            ctime,
            ctime_nsec,
            mtime,
            mtime_nsec,
            dev,
            ino,
            uid,
            gid,
            file_size,
        };
        let bytes = self.take::<20>()?;
        // Any [u8; 20] is structurally a valid SHA-1 object id. We actually have to check that each
        // entry’s oid matches the current contents at entry.path but this integrity check happens at
        // other levels
        let oid = Oid::from_bytes(*bytes);
        let offset = self.offset();
        let flags = u16::from_be_bytes(*self.take::<2>()?);
        // https://git-scm.com/docs/index-format
        // 0 - 11 is path length
        // 12 - 13 merge state
        // 14 extended flags follow, must be zero for v2
        // 15 assume-valid
        //
        // the 14th bit(0-indexed) must not be set for v2
        if flags & 0x4000 != 0 {
            return Err(FormatError::at(offset, FormatErrorKind::ExtendedFlagInV2));
        }
        // we isolate the 2 stage bits
        // 12 and 13th bit are set, rest are zeros, shifting them moves those 2 bits into positions
        // 1, 0
        // TODO: update when we add `merge` support
        let stage = (flags & 0x3000) >> 12;
        // we extract the lowest 12 bits which is where we stored the path len
        let path_len = flags & 0xfff;
        let path = if path_len == super::PATH_MAX_SIZE {
            self.read_path_until_nul()?
        } else {
            self.read_path(path_len as usize)?
        };
        let size = self.pos - entry_offset;
        self.skip_padding(size)?;

        Ok(IndexEntry::from_parsed_parts(stat, mode, oid, flags, path))
    }

    // path length from flags must match the actual path length.
    //
    // if the real path is longer than path_len, the next byte will not be NUL, and we reject it
    // if the real path is shorter than path_len, then the path slice will include the NUL byte
    // inside it, and validate_index_path() will reject it
    fn read_path(&mut self, path_len: usize) -> Result<RepoPath, FormatError> {
        // need path_len bytes for the path, plus 1 byte for the required NUL terminator.
        if self.pos + path_len >= self.buf.len() {
            return Err(FormatError::at(
                self.pos,
                FormatErrorKind::UnexpectedEof {
                    // including the terminator
                    needed: path_len + 1,
                    remaining: self.remaining(),
                },
            ));
        }

        let start = self.pos;
        /*
         * NOTE: the slice is built from `path_len` alone so, `RepoPath::from_bytes()` only validates
         * those bytes. If `path_len` is smaller than the real path, the mismatch is caught below by
         * the nul terminator check
         */
        let path_bytes = &self.buf[start..start + path_len];
        // path name needs to follow the rules of the index format
        let path = RepoPath::from_bytes(path_bytes)
            .map_err(|err| FormatError::at(start, FormatErrorKind::InvalidPathSyntax(err)))?;
        // move to the next byte after the path that according to the format should be NUL
        self.pos = start + path_len;

        if let Some(0) = self.buf.get(self.pos) {
            self.advance(1);
        } else {
            return Err(FormatError::at(
                self.pos,
                FormatErrorKind::MissingNulTerminator,
            ));
        }
        Ok(path)
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
    // exact length, but we use a sentinel value
    fn read_path_until_nul(&mut self) -> Result<RepoPath, FormatError> {
        let start = self.pos;
        let Some(relative_pos) = memchr::memchr(0, &self.buf[start..]) else {
            return Err(FormatError::at(
                self.buf.len(),
                FormatErrorKind::MissingNulTerminator,
            ));
        };

        // the position of the NUL with respect to the whole buffer is start + relative
        let nul_pos = start + relative_pos;
        if nul_pos - start < super::PATH_MAX_SIZE as usize {
            return Err(FormatError::at(start, FormatErrorKind::LongPathLenMisMatch));
        }

        let path_bytes = &self.buf[start..nul_pos];
        let path = RepoPath::from_bytes(path_bytes)
            .map_err(|err| FormatError::at(start, FormatErrorKind::InvalidPathSyntax(err)))?;
        // skip NUL
        self.pos = nul_pos + 1;

        Ok(path)
    }

    // padding formula: size % 8 tells us how far past the last multiple of 8 we are. 67 % 8 = 3
    // 8 - 3 is our padding. 1 edge case to consider, what if it is already multiple of 8 then no
    // padding should be added
    // 64 % 8 = 0, 8 - 0 = 8 which is incorrect we don't need 8 bytes of padding
    // to address that we mod the result with 8.
    // if we need any padding n % 8 = n when n < 8 and 0 when n = 8
    fn skip_padding(&mut self, entry_size: usize) -> Result<(), FormatError> {
        let padding = (8 - (entry_size % 8)) % 8;
        if self.remaining() < padding {
            // self.pos is exactly the byte offset where we expected the next byte to exist
            // self.pos - 1 points to the last valid byte, which didn't actually cause the
            // error
            return Err(FormatError::at(
                self.pos,
                FormatErrorKind::UnexpectedEof {
                    needed: padding,
                    remaining: self.remaining(),
                },
            ));
        }

        for _ in 0..padding {
            if self.buf[self.pos] != 0 {
                return Err(FormatError::at(
                    self.offset(),
                    FormatErrorKind::NonZeroPadding,
                ));
            }
            self.advance(1);
        }
        Ok(())
    }

    fn read_u32(&mut self) -> Result<u32, FormatError> {
        Ok(u32::from_be_bytes(*self.take::<4>()?))
    }

    fn u32_array<const N: usize>(&mut self) -> Result<[u32; N], FormatError> {
        let mut elements = [0u32; N];

        for element in &mut elements {
            *element = self.read_u32()?;
        }
        Ok(elements)
    }

    fn offset(&self) -> usize {
        self.pos
    }

    fn advance(&mut self, n: usize) {
        self.pos += n;
    }

    fn take<const N: usize>(&mut self) -> Result<&'a [u8; N], FormatError> {
        if self.remaining() < N {
            return Err(FormatError {
                offset: self.pos,
                kind: FormatErrorKind::UnexpectedEof {
                    needed: N,
                    remaining: self.remaining(),
                },
            });
        }
        let bytes = &self.buf[self.pos..self.pos + N];
        self.advance(N);
        // SAFETY: we know our slice contains exactly `N` bytes
        Ok(unsafe { bytes.as_array::<N>().unwrap_unchecked() })
    }

    fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    // if our parser was bug-free `self.buf.len().saturating_sub(self.pos)` would also work but using 
    // `saturating_sub` could hide a parser bug where pos accidentally moves beyond the buffer
    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }
}

enum IndexVersion {
    V2,
    V3,
    V4
}

impl IndexVersion {
    fn try_from(value: u32) -> Option<IndexVersion> {
        match value {
            2 => Some(IndexVersion::V2),
            3 => Some(IndexVersion::V3),
            4 => Some(IndexVersion::V4),
            _ => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum FormatErrorKind {
    InvalidChecksum,
    UnsupportedVersion(u32),
    UnexpectedEof { needed: usize, remaining: usize },
    InvalidSignature,
    EntriesOutOfSorted,
    InvalidMode(u32),
    InvalidNanoseconds,
    MissingNulTerminator,
    NonZeroPadding,
    LongPathLenMisMatch,
    InvalidPathSyntax(RepoPathError),
    ExtendedFlagInV2,
    TrailingData { remaining: usize },
}

impl Error for FormatErrorKind {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidChecksum => None,
            Self::UnsupportedVersion(_) => None,
            Self::UnexpectedEof { .. } => None,
            Self::InvalidSignature => None,
            Self::EntriesOutOfSorted => None,
            Self::InvalidMode(_) => None,
            Self::InvalidNanoseconds => None,
            Self::MissingNulTerminator => None,
            Self::NonZeroPadding => None,
            Self::LongPathLenMisMatch => None,
            FormatErrorKind::InvalidPathSyntax(source) => Some(source),
            FormatErrorKind::ExtendedFlagInV2 => None,
            FormatErrorKind::TrailingData { .. } => None,
        }
    }
}

impl fmt::Display for FormatErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidChecksum => write!(f, "invalid index checksum"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported index version {version}")
            }
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
            Self::NonZeroPadding => write!(f, "expected a zero byte in index entry padding"),
            Self::LongPathLenMisMatch => {
                write!(f, "path length does not match the index flags")
            }
            Self::InvalidPathSyntax(_) => {
                write!(f, "invalid index path: ")
            }
            Self::ExtendedFlagInV2 => write!(f, "extended flag is not allowed in index version 2"),
            Self::TrailingData { remaining } => {
                write!(f, "unexpected trailing data: {remaining} bytes remaining")
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FormatError {
    offset: usize,
    kind: FormatErrorKind,
}

impl FormatError {
    fn at(offset: usize, kind: FormatErrorKind) -> Self {
        Self { offset, kind }
    }
}

impl Error for FormatError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.kind.source()
    }
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid index at byte {}: {}", self.offset, self.kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::{index::PATH_MAX_SIZE, repopath::RepoPathErrorKind};

    struct EntryMetadata {
        stat: FileStat,
        mode: u32,
        oid: [u8; 20],
        flags: Option<u16>,
    }

    impl Default for EntryMetadata {
        fn default() -> Self {
            Self {
                stat: FileStat {
                    ctime: 1_700_000_000,
                    ctime_nsec: 123_456_789,
                    mtime: 1_700_000_001,
                    mtime_nsec: 987_654_321,
                    dev: 11,
                    ino: 22,
                    uid: 1000,
                    gid: 1001,
                    file_size: 5,
                },
                mode: 0o100644,
                oid: Sha1::digest(b"blob 5\0hello").into(),
                flags: None,
            }
        }
    }

    fn append_entry(buf: &mut Vec<u8>, path: &[u8], metadata: EntryMetadata) {
        let entry_offset = buf.len();
        let flags = metadata
            .flags
            .unwrap_or(path.len().min(PATH_MAX_SIZE as usize) as u16);
        buf.extend_from_slice(&metadata.stat.ctime.to_be_bytes());
        buf.extend_from_slice(&metadata.stat.ctime_nsec.to_be_bytes());
        buf.extend_from_slice(&metadata.stat.mtime.to_be_bytes());
        buf.extend_from_slice(&metadata.stat.mtime_nsec.to_be_bytes());
        buf.extend_from_slice(&metadata.stat.dev.to_be_bytes());
        buf.extend_from_slice(&metadata.stat.ino.to_be_bytes());
        buf.extend_from_slice(&(metadata.mode as u32).to_be_bytes());
        buf.extend_from_slice(&metadata.stat.uid.to_be_bytes());
        buf.extend_from_slice(&metadata.stat.gid.to_be_bytes());
        buf.extend_from_slice(&metadata.stat.file_size.to_be_bytes());
        buf.extend_from_slice(&metadata.oid);
        buf.extend_from_slice(&flags.to_be_bytes());
        buf.extend_from_slice(path);
        buf.push(0);

        // pab based on the entry's size
        while (buf.len() - entry_offset) % 8 != 0 {
            buf.push(0);
        }
    }

    #[test]
    fn reject_checksum_mismatch() {
        let bytes = [
            0x44, 0x49, 0x52, 0x43, 
            0x00, 0x00, 0x00, 0x02, 
            0x00, 0x00, 0x00, 0x00,
            // checksum
            0x00, 0x00, 0x00, 0x00, 
            0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 
            0x00, 0x00, 0x00, 0x00, 
            0x00, 0x00, 0x00, 0x00,
        ];
        let mut parser = Parser::new(&bytes);
        let err = parser.parse().unwrap_err();

        assert_eq!(err, FormatError::at(12, FormatErrorKind::InvalidChecksum));
    }

    #[test]
    fn reject_input_shorter_than_checksum() {
        // 19
        let bytes = [
            0x00, 0x00, 0x00, 0x00, 
            0x00, 0x00, 0x00, 0x00, 
            0x00, 0x00, 0x00, 0x00, 
            0x00, 0x00, 0x00, 0x00, 
            0x00, 0x00, 0x00,
        ];
        let mut parser = Parser::new(&bytes);
        let err = parser.parse().unwrap_err();

        assert_eq!(
            err,
            FormatError::at(
                0,
                FormatErrorKind::UnexpectedEof {
                    needed: 20,
                    remaining: 19
                }
            )
        );
    }

    #[test]
    fn reject_unknown_singature() {
        let bytes = [
            // 'F''A''I''L'
            0x46, 0x41, 0x49, 0x4c, 
            0x00, 0x00, 0x00, 0x02, 
            0x00, 0x00, 0x00, 0x00,
            // checksum of the preceding 12 bytes
            0x38, 0x1b, 0x4d, 0xdc, 
            0xe0, 0xb4, 0xc2, 0x4a, 
            0xa6, 0x4a, 0xb0, 0x21, 
            0x80, 0x51, 0x2c, 0xcf, 
            0x39, 0xd6, 0x6d, 0xfc,
        ];
        let mut parser = Parser::new(&bytes);
        let err = parser.parse().unwrap_err();

        assert_eq!(err, FormatError::at(0, FormatErrorKind::InvalidSignature));
    }

    #[test]
    fn reject_unsupported_version() {
        let bytes = [
            0x44, 0x49, 0x52, 0x43,
            0x00, 0x00, 0x00, 0x05,
            0x00, 0x00, 0x00, 0x00,
            // checksum of the preceding 12 bytes
            0x15, 0xb0, 0x8b, 0x67, 
            0x12, 0xc6, 0x49, 0x92, 
            0x86, 0x5c, 0xb2, 0xfe, 
            0x3b, 0x0b, 0xe6, 0xaa, 
            0x98, 0x22, 0x66, 0x90,
        ];
        let mut parser = Parser::new(&bytes);
        let err = parser.parse().unwrap_err();

        assert_eq!(
            err,
            FormatError::at(4, FormatErrorKind::UnsupportedVersion(5))
        );
    }

    #[test]
    fn reject_out_of_order_entries() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"DIRC");
        bytes.extend_from_slice(&2u32.to_be_bytes());
        bytes.extend_from_slice(&2u32.to_be_bytes());
        append_entry(&mut bytes, b"b", EntryMetadata::default());
        append_entry(&mut bytes, b"a", EntryMetadata::default());
        let checksum: [u8; 20] = Sha1::digest(&bytes).into();
        bytes.extend_from_slice(&checksum);

        let mut parser = Parser::new(&bytes);
        let err = parser.parse().unwrap_err();

        assert_eq!(
            err,
            // 12 for header(signature, version, count)
            // entry "b": 64 bytes(62 fixed, 1 for path, 1 for nul)
            // entry "a": start at 64 + 12 = 76
            FormatError::at(76, FormatErrorKind::EntriesOutOfSorted)
        );
    }

    #[test]
    fn reject_trailing_data_after_entries() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"DIRC");
        bytes.extend_from_slice(&2u32.to_be_bytes());
        bytes.extend_from_slice(&1u32.to_be_bytes());

        append_entry(&mut bytes, b"a", EntryMetadata::default());
        // extra bytes after the last entry
        bytes.extend_from_slice(&[0x66, 0x6f, 0x6f]);
        let checksum: [u8; 20] = Sha1::digest(&bytes).into();
        bytes.extend_from_slice(&checksum);

        let mut parser = Parser::new(&bytes);
        let err = parser.parse().unwrap_err();

        assert_eq!(
            err,
            FormatError::at(76, FormatErrorKind::TrailingData { remaining: 3 })
        );
    }

    #[test]
    fn reject_invalid_ctime_nsec() {
        let mut bytes = Vec::new();
        let mut metadata = EntryMetadata::default();
        metadata.stat.ctime_nsec = 1_000_000_000;
        append_entry(&mut bytes, b"a", metadata);

        let mut parser = Parser::new(&bytes);
        let err = parser.next().unwrap_err();

        assert_eq!(err, FormatError::at(4, FormatErrorKind::InvalidNanoseconds));
    }

    #[test]
    fn reject_invalid_mtime_nsec() {
        let mut bytes = Vec::new();
        let mut metadata = EntryMetadata::default();
        metadata.stat.mtime_nsec = 1_000_000_000;
        append_entry(&mut bytes, b"a", metadata);

        let mut parser = Parser::new(&bytes);
        let err = parser.next().unwrap_err();

        assert_eq!(
            err,
            FormatError::at(12, FormatErrorKind::InvalidNanoseconds)
        );
    }

    #[test]
    fn reject_invalid_mode() {
        let mut bytes = Vec::new();
        let mut metadata = EntryMetadata::default();
        metadata.mode = 0;
        append_entry(&mut bytes, b"a", metadata);

        let mut parser = Parser::new(&bytes);
        let err = parser.next().unwrap_err();

        assert_eq!(err, FormatError::at(24, FormatErrorKind::InvalidMode(0)));
    }

    #[test]
    fn reject_extended_flag_in_v2() {
        let mut bytes = Vec::new();
        let mut metadata = EntryMetadata::default();
        metadata.flags = Some(0x4001);
        append_entry(&mut bytes, b"a", metadata);

        let mut parser = Parser::new(&bytes);
        let err = parser.next().unwrap_err();

        assert_eq!(err, FormatError::at(60, FormatErrorKind::ExtendedFlagInV2));
    }

    #[test]
    fn read_path_rejects_insufficient_path_bytes() {
        let bytes = Vec::new();
        let mut parser = Parser::new(&bytes);
        let err = parser.read_path(4).unwrap_err();

        assert_eq!(
            err,
            FormatError::at(
                0,
                FormatErrorKind::UnexpectedEof {
                    // include the nul terminator
                    needed: 5,
                    remaining: 0
                }
            )
        );
    }

    #[test]
    fn read_path_rejects_missing_nul_terminator() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"abcd");
        let mut parser = Parser::new(&bytes);
        let err = parser.read_path(3).unwrap_err();

        assert_eq!(
            err,
            FormatError::at(3, FormatErrorKind::MissingNulTerminator)
        );
    }

    // we don't really test `RepoPath::from_bytes()`, just making sure we report the correct offset
    #[test]
    fn read_path_rejects_invalid_path_syntax() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"/abcd");
        let mut parser = Parser::new(&bytes);
        let err = parser.read_path(3).unwrap_err();

        assert_eq!(
            err,
            FormatError::at(
                0,
                FormatErrorKind::InvalidPathSyntax(RepoPathError {
                    path: bytes[..3].to_vec(),
                    kind: RepoPathErrorKind::LeadingSlash
                })
            )
        );
    }

    #[test]
    fn read_path_until_nul_rejects_missing_nul_terminator() {
        let bytes = vec![b'a'; 5000];
        let mut parser = Parser::new(&bytes);
        let err = parser.read_path_until_nul().unwrap_err();

        assert_eq!(
            err,
            FormatError::at(5000, FormatErrorKind::MissingNulTerminator)
        );
    }

    // the method was invoked because the extracted `path_len` was equal to PATH_MAX_SIZE(0xfff) but
    // the actual path length was not
    #[test]
    fn read_path_until_nul_rejects_paths_below_sentinel_length() {
        let bytes = vec![0];
        let mut parser = Parser::new(&bytes);
        let err = parser.read_path_until_nul().unwrap_err();

        assert_eq!(
            err,
            FormatError::at(0, FormatErrorKind::LongPathLenMisMatch)
        );
    }

    // we don't really test `RepoPath::from_bytes()`, just making sure we report the correct offset
    #[test]
    fn read_path_until_nul_rejects_invalid_path_syntax() {
        let mut bytes = vec![b'/'; 5000];
        bytes.extend_from_slice(b"\0");
        let mut parser = Parser::new(&bytes);
        let err = parser.read_path_until_nul().unwrap_err();

        assert_eq!(
            err,
            FormatError::at(
                0,
                FormatErrorKind::InvalidPathSyntax(RepoPathError {
                    path: bytes[..5000].to_vec(),
                    kind: RepoPathErrorKind::LeadingSlash
                })
            )
        );
    }

    #[test]
    fn reject_truncated_padding() {
        let bytes = vec![0; 3];
        let mut parser = Parser::new(&bytes);
        let err = parser.skip_padding(75).unwrap_err();

        assert_eq!(
            err,
            FormatError::at(
                0,
                FormatErrorKind::UnexpectedEof {
                    needed: 5,
                    remaining: 3
                }
            )
        );
    }

    #[test]
    fn reject_non_zero_padding() {
        let bytes = vec![1; 10];
        let mut parser = Parser::new(&bytes);
        let err = parser.skip_padding(75).unwrap_err();

        assert_eq!(err, FormatError::at(0, FormatErrorKind::NonZeroPadding));
    }

    #[test]
    fn reject_insufficient_bytes() {
        let bytes = vec![0; 10];
        let mut parser = Parser::new(&bytes);
        let err = parser.take::<20>().unwrap_err();

        assert_eq!(
            err,
            FormatError::at(
                0,
                FormatErrorKind::UnexpectedEof {
                    needed: 20,
                    remaining: 10
                }
            )
        );
    }
}
