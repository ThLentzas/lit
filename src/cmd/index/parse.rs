use crate::cmd::error::{FormatError, FormatErrorKind};
use crate::cmd::index::{validate_path, IndexEntry, StatNode, PATH_MAX_SIZE};
use crate::cmd::os;

pub(super) struct Parser<'a> {
    buffer: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    pub(super) fn new(buffer: &'a [u8]) -> Self {
        Self { buffer, pos: 0 }
    }

    // next always returns a valid IndexEntry
    pub(super) fn next(&mut self) -> Result<IndexEntry, FormatError> {
        let entry_offset = self.offset();

        let stat = StatNode {
            ctime: u32::from_be_bytes(*self.take::<4>()?),
            ctime_nsec: u32::from_be_bytes(*self.take::<4>()?),
            mtime: u32::from_be_bytes(*self.take::<4>()?),
            mtime_nsec: u32::from_be_bytes(*self.take::<4>()?),
            dev: u32::from_be_bytes(*self.take::<4>()?),
            ino: u32::from_be_bytes(*self.take::<4>()?),
            mode: u32::from_be_bytes(*self.take::<4>()?),
            uid: u32::from_be_bytes(*self.take::<4>()?),
            gid: u32::from_be_bytes(*self.take::<4>()?),
            file_size: u32::from_be_bytes(*self.take::<4>()?),
        };
        if !matches!(stat.mode, os::REGULAR | os::EXECUTABLE | os::SYMLINK) {
            return Err(FormatError::at(
                entry_offset,
                FormatErrorKind::InvalidMode(stat.mode),
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

        let oid = self.take::<20>()?;
        let flags = u16::from_be_bytes(*self.take::<2>()?);
        let mut path_bytes = Vec::new();
        // we extract the lowest 12 bits which is where we stored the path len
        let path_len = flags & 0xfff;
        if path_len == PATH_MAX_SIZE {
            path_bytes.extend(self.read_path_until_nul()?);
        } else {
            path_bytes.extend(self.read_path(path_len as usize)?);
        }

        let size = self.pos - entry_offset;
        self.skip_padding(size)?;

        Ok(IndexEntry::new(path_bytes, *oid, stat))
    }

    // one of the things that we have to validate is that the path length from flags matches the
    // actual path length.
    //
    // if the real path is longer than path_len, the next byte will not be NUL, and we reject it
    // If the real path is shorter than path_len, then the path slice will include the NUL byte
    // inside it, and validate_index_path() will reject it
    fn read_path(&mut self, path_len: usize) -> Result<Vec<u8>, FormatError> {
        // need path_len bytes for the path, plus 1 byte for the required NUL terminator.
        if self.pos + path_len >= self.buffer.len() {
            return Err(FormatError::at(
                self.pos,
                FormatErrorKind::Eof {
                    needed: path_len,
                    remaining: self.buffer.len().saturating_sub(self.pos),
                },
            ));
        }

        let start = self.pos;
        let path_bytes = &self.buffer[start..start + path_len];
        // path name needs to follow the rules of the index format
        validate_path(path_bytes)
            .map_err(|err| FormatError::at(start, FormatErrorKind::InvalidPathSyntax(err)))?;
        // move to the next byte after the path that according to the format should be NUL
        self.pos = start + path_len;

        match self.buffer.get(self.pos) {
            // move past NUL
            Some(0) => self.advance(1),
            Some(_) => {
                return Err(FormatError::at(
                    self.pos,
                    FormatErrorKind::MissingNulTerminator,
                ));
            }
            None => {
                return Err(FormatError::at(
                    self.pos,
                    FormatErrorKind::Eof {
                        needed: 1,
                        remaining: self.buffer.len().saturating_sub(self.pos),
                    },
                ));
            }
        }

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
    fn read_path_until_nul(&mut self) -> Result<Vec<u8>, FormatError> {
        let start = self.pos;
        let relative_pos = self.buffer[start..]
            .iter()
            .position(|b| *b == 0)
            .ok_or_else(|| {
                FormatError::at(
                    self.buffer.len(),
                    FormatErrorKind::Eof {
                        needed: 1,
                        remaining: 0,
                    },
                )
            })?;

        // position() returns relative to the slice we searched not the entire buffer
        // the position of the NUL with respect to the whole buffer is start + relative
        let nul_pos = start + relative_pos;
        if nul_pos - start < PATH_MAX_SIZE as usize {
            return Err(FormatError::at(
                start,
                FormatErrorKind::LongPathLenMissMatch,
            ));
        }

        let path_bytes = &self.buffer[start..nul_pos];
        validate_path(path_bytes)
            .map_err(|err| FormatError::at(start, FormatErrorKind::InvalidPathSyntax(err)))?;
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
    fn skip_padding(&mut self, entry_size: usize) -> Result<(), FormatError> {
        let mut padding = (8 - (entry_size % 8)) % 8;

        while padding > 0 {
            // if it is not null or exhausted the buffer too early it's an invalid format
            match self.buffer.get(self.pos) {
                Some(0) => {
                    self.advance(1);
                    padding -= 1;
                }
                Some(_) => return Err(FormatError::at(self.pos, FormatErrorKind::InvalidPadding)),
                None => {
                    return Err(FormatError::at(
                        self.pos - 1,
                        FormatErrorKind::Eof {
                            needed: 1,
                            remaining: 0,
                        },
                    ));
                }
            }
        }
        Ok(())
    }

    // how far we read in the buffer
    pub(super) fn offset(&self) -> usize {
        self.pos
    }

    fn advance(&mut self, n: usize) {
        self.pos += n;
    }

    pub(super) fn take<const N: usize>(&mut self) -> Result<&'a [u8; N], FormatError> {
        let remaining = self.buffer.len().saturating_sub(self.pos);

        let bytes: &[u8; N] = self.buffer[self.pos..self.pos + N]
            .try_into()
            .map_err(|_| FormatError {
                offset: self.pos,
                kind: FormatErrorKind::Eof {
                    needed: N,
                    remaining,
                },
            })?;
        self.advance(N);

        Ok(bytes)
    }
}