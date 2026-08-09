use super::{FormatError, FormatErrorKind, IndexEntry};
use crate::repo::index;
use crate::repo::object::mode::Mode;
use crate::repo::object::oid::Oid;
use crate::repo::os::FileStat;
use crate::repo::path::RepoPath;

pub(super) struct Parser<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    pub(super) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    // next always returns a valid IndexEntry
    pub(super) fn next(&mut self) -> Result<IndexEntry, FormatError> {
        let entry_offset = self.offset();

        let [ctime, ctime_nsec, mtime, mtime_nsec, dev, ino] = self.u32_array::<6>()?;
        let mode = self.read_u32()?;
        let mode = Mode::from_raw(mode).ok_or(FormatError::at(
            self.offset(),
            FormatErrorKind::InvalidMode(mode),
        ))?;
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

        // TODO: we need to rethink for DIR when we support sparse
        if !matches!(mode, Mode::Regular | Mode::Executable | Mode::Symlink) {
            return Err(FormatError::at(
                entry_offset,
                FormatErrorKind::InvalidMode(mode as u32),
            ));
        }
        // The nanoseconds field represents the fractional part of a second, so it lives in the range
        // [0, 10^9). Anything above that is a second.
        if stat.ctime_nsec >= 1_000_000_000 || stat.mtime_nsec >= 1_000_000_000 {
            return Err(FormatError::at(
                entry_offset,
                FormatErrorKind::InvalidNanoseconds,
            ));
        }

        let bytes = self.take::<20>()?;
        // Any [u8; 20] is structurally a valid SHA-1 object id. We actually have to check that each
        // entry’s oid matches the current contents at entry.path but this integrity check happens at
        // other levels. Rehashing the workspace during load would incorrectly reject valid index states
        // The index records the staged version of a path, while the workspace file may have changed
        // or been deleted since git add. db.load() is such case where the object might be missing, or
        // we can have a hash mismatch, the object stored under that OID is corrupt or misplaced.
        let oid = Oid::from_bytes(*bytes);
        let flags = u16::from_be_bytes(*self.take::<2>()?);
        // we extract the lowest 12 bits which is where we stored the path len
        let path_len = flags & 0xfff;
        let path = if path_len == index::PATH_MAX_SIZE {
            self.read_path_until_nul()?
        } else {
            self.read_path(path_len as usize)?
        };
        let size = self.pos - entry_offset;
        self.skip_padding(size)?;

        Ok(IndexEntry::new(path, oid, mode, stat))
    }

    // one of the things that we have to validate is that the path length from flags matches the
    // actual path length.
    //
    // if the real path is longer than path_len, the next byte will not be NUL, and we reject it
    // If the real path is shorter than path_len, then the path slice will include the NUL byte
    // inside it, and validate_index_path() will reject it
    fn read_path(&mut self, path_len: usize) -> Result<RepoPath, FormatError> {
        // need path_len bytes for the path, plus 1 byte for the required NUL terminator.
        if self.pos + path_len >= self.buf.len() {
            return Err(FormatError::at(
                self.pos,
                FormatErrorKind::UnexpectedEof {
                    needed: path_len,
                    remaining: self.remaining(),
                },
            ));
        }

        let start = self.pos;
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

        // position() returns relative to the slice we searched not the entire buffer
        // the position of the NUL with respect to the whole buffer is start + relative
        let nul_pos = start + relative_pos;
        if nul_pos - start < index::PATH_MAX_SIZE as usize {
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
        let mut padding = (8 - (entry_size % 8)) % 8;

        while padding > 0 {
            // if it is not null or exhausted the buffer too early it's an invalid format
            match self.buf.get(self.pos) {
                Some(0) => {
                    self.advance(1);
                    padding -= 1;
                }
                Some(_) => return Err(FormatError::at(self.pos, FormatErrorKind::InvalidPadding)),
                None => {
                    // self.pos is exactly the byte offset where we expected the next byte to exist
                    // self.pos - 1 points to the last valid byte, which didn't actually cause the
                    // error
                    return Err(FormatError::at(
                        self.pos,
                        FormatErrorKind::UnexpectedEof {
                            needed: 1,
                            remaining: 0,
                        },
                    ));
                }
            }
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

    // how far we read in the buffer
    pub(super) fn offset(&self) -> usize {
        self.pos
    }

    fn advance(&mut self, n: usize) {
        self.pos += n;
    }

    pub(super) fn take<const N: usize>(&mut self) -> Result<&'a [u8; N], FormatError> {
        let remaining = self.remaining();

        let bytes: &[u8; N] =
            self.buf[self.pos..self.pos + N]
                .try_into()
                .map_err(|_| FormatError {
                    offset: self.pos,
                    kind: FormatErrorKind::UnexpectedEof {
                        needed: N,
                        remaining,
                    },
                })?;
        self.advance(N);

        Ok(bytes)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    // This would also work if our parser was bug-free self.buf.len().saturating_sub(self.pos) but
    // using saturating_sub could hide a parser bug where pos accidentally moves beyond the buffer
    pub(super) fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }
}
