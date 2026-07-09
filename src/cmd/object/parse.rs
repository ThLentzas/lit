use crate::cmd::object::{Commit, Entry, Object, Signature};
use crate::cmd::os;
use crate::cmd::timestamp::{Timestamp, TimestampError};
use crate::hex;
use std::str::Utf8Error;

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    fn rest(&self) -> &'a [u8] {
        &self.buf[self.pos..]
    }

    fn advance(&mut self, n: usize) {
        self.pos += n;
    }

    fn take<const N: usize>(&mut self) -> Result<&'a [u8; N], CursorError> {
        let available = self.buf.len().saturating_sub(self.pos);

        let bytes: &[u8; N] = self.buf[self.pos..self.pos + N].try_into().map_err(|_| {
            CursorError::new(
                self.pos,
                CursorErrorKind::Truncated {
                    needed: N,
                    available,
                },
            )
        })?;
        self.advance(N);

        Ok(bytes)
    }

    // read_until does not know whether it's scanning for a header nul, an entry name or a mode terminator
    // the caller does, so we return an error that the delim was not found, the offset, where we started
    // searching for it and let the caller map it as they wish.
    fn read_until(&mut self, delimiter: u8) -> Result<&'a [u8], CursorError> {
        let start = self.pos;
        let end = match memchr::memchr(delimiter, &self.buf[start..]) {
            Some(index) => self.pos + index,
            None => {
                return Err(CursorError::new(
                    start,
                    CursorErrorKind::MissingDelimiter { delimiter },
                ));
            }
        };
        // one past the delimiter, we could stop at end but that would mean that the caller every
        // time has to call advance(), we essentially consume the separator in our case
        self.pos = end + 1;
        // delimiter is exclusive
        Ok(&self.buf[start..end])
    }
}

#[derive(Debug, PartialEq, Eq)]
enum OidError {
    WrongLen { offset: usize, len: usize },
    BadDigit { offset: usize, digit: u8 },
}

impl OidError {
    fn offset(&self) -> usize {
        match self {
            OidError::WrongLen { offset, .. } => *offset,
            OidError::BadDigit { offset, .. } => *offset,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum SignatureError {
    MissingSpace { offset: usize },
    MissingAngleBracket { offset: usize },
    MissingName { offset: usize },
    MissingEmail { offset: usize },
    MissingTime { offset: usize },
    InvalidUtf8(Utf8Error),
    BadTimestamp { offset: usize, err: TimestampError },
}

#[derive(Debug, PartialEq, Eq)]
enum TreeEntryError {
    MissingSpace,
    MissingNul,
    TruncatedOid,
    UnknownMode { mode: Vec<u8> },
}

#[derive(Debug, PartialEq, Eq)]
enum CursorErrorKind {
    /// read_until: delimiter absent between `offset` and end of input
    MissingDelimiter { delimiter: u8 },
    /// expect: next byte wasn't the required one (None = at end of input)
    Expected { byte: u8, found: Option<u8> },
    /// take::<N>: fewer than `needed` bytes remained
    Truncated { needed: usize, available: usize },
}

struct CursorError {
    offset: usize,
    kind: CursorErrorKind,
}

impl CursorError {
    fn new(offset: usize, kind: CursorErrorKind) -> Self {
        Self { offset, kind }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ParseErrorKind {
    DuplicateHeader { name: &'static str },
    MissingHeaderSpace,
    MissingHeaderNul,
    InvalidTreeEntry(TreeEntryError),
    InvalidSizeHeader,
    SizeMisMatch { expected: usize, actual: usize },
    MissingDelimiter { delimiter: u8 },
    MissingNewLine,
    UnknownType { got: Vec<u8> },
    // bad hex digit
    BadOid(OidError),
    BadTimestamp(TimestampError),
    UnexpectedHeader { header: Vec<u8> }
}

pub(super) struct ParseError {
    pub(super) offset: usize,
    pub(super) kind: ParseErrorKind,
}

impl ParseError {
    fn new(offset: usize, kind: ParseErrorKind) -> Self {
        Self { offset, kind }
    }
}

// object type, a space, the size of the object and a nul byte
pub(super) fn parse(buf: &[u8]) -> Result<Object, ParseError> {
    let mut cursor = Cursor::new(buf);
    let kind = cursor
        .read_until(b' ')
        .map_err(|err| ParseError::new(err.offset, ParseErrorKind::MissingHeaderSpace))?;

    let start = cursor.pos;
    let size = parse_size(&mut cursor)?;
    let rest = cursor.rest();
    if size != rest.len() {
        return Err(ParseError::new(
            start,
            ParseErrorKind::SizeMisMatch {
                expected: size,
                actual: rest.len(),
            },
        ));
    }

    match kind {
        b"blob" => Ok(Object::Blob(rest.to_vec())),
        b"tree" => {
            // we have 2 choices: we either create a new cursor that holds rest and need to adjust
            // offsets for absolute position since it will return slice relative offsets, or we can
            // pass our initial cursor that holds the entire buffer and its internal pointer we moved
            // to verify the structure so far and not adjust the offsets because it is already absolute
            let mut entries = Vec::new();
            while !cursor.is_empty() {
                entries.push(parse_tree_entry(&mut cursor)?);
            }
            Ok(Object::Tree(entries))
        }
        b"commit" => {}
        _ => Err(ParseError::new(
            cursor.pos,
            ParseErrorKind::UnknownType { got: kind.to_vec() },
        )),
    }
}

fn parse_tree_entry(cursor: &mut Cursor) -> Result<Entry, ParseError> {
    let start = cursor.pos;
    let buf = cursor.read_until(b' ').map_err(|err| {
        ParseError::new(
            err.offset,
            ParseErrorKind::InvalidTreeEntry(TreeEntryError::MissingSpace),
        )
    })?;
    let mut mode = 0u32;

    for &b in buf {
        if !b.is_ascii_digit() {
            return Err(ParseError::new(
                start,
                ParseErrorKind::InvalidTreeEntry(TreeEntryError::UnknownMode {
                    mode: buf.to_vec(),
                }),
            ));
        }
        mode = mode
            .checked_mul(10)
            .and_then(|n| n.checked_add((b - b'0') as u32))
            .ok_or(ParseError::new(
                start,
                ParseErrorKind::InvalidTreeEntry(TreeEntryError::UnknownMode {
                    mode: buf.to_vec(),
                }),
            ))?;
    }

    if !matches!(mode, os::EXECUTABLE | os::REGULAR | os::SYMLINK | os::DIR) {
        return Err(ParseError::new(
            start,
            ParseErrorKind::InvalidTreeEntry(TreeEntryError::UnknownMode { mode: buf.to_vec() }),
        ));
    }

    let name = cursor
        .read_until(0)
        .map_err(|err| {
            ParseError::new(
                err.offset,
                ParseErrorKind::InvalidTreeEntry(TreeEntryError::MissingNul),
            )
        })?
        .to_vec();
    let oid = cursor.take::<20>().map_err(|err| {
        ParseError::new(
            err.offset,
            ParseErrorKind::InvalidTreeEntry(TreeEntryError::TruncatedOid),
        )
    })?;

    Ok(Entry {
        mode,
        name,
        oid: *oid,
    })
}

fn parse_size(cursor: &mut Cursor) -> Result<usize, ParseError> {
    let start = cursor.pos;
    let size_buf = cursor
        .read_until(0)
        .map_err(|err| ParseError::new(err.offset, ParseErrorKind::MissingHeaderNul))?;

    if size_buf.is_empty() {
        return Err(ParseError::new(start, ParseErrorKind::InvalidSizeHeader));
    }
    cursor.advance(1);

    let mut size = 0usize;
    // we could also try to parse as usize from std
    for (index, &byte) in size_buf.iter().enumerate() {
        if !byte.is_ascii_digit() {
            return Err(ParseError::new(
                start + index,
                ParseErrorKind::InvalidSizeHeader,
            ));
        }

        // similar to the atoi!() macro in jolt
        size = size
            .checked_mul(10)
            .and_then(|n| n.checked_add((byte - b'0') as usize))
            .ok_or(ParseError::new(start, ParseErrorKind::InvalidSizeHeader))?;
    }
    Ok(size)
}

fn parse_commit(cursor: &mut Cursor) -> Result<Commit, ParseError> {
    let mut tree: Option<[u8; 20]> = None;
    // TODO: when we support merge, this needs to change to a Vec<[u8; 20]>, an empty vec means initial commit
    let mut parent: Option<[u8; 20]> = None;
    let mut author: Option<Signature> = None;
    let mut committer: Option<Signature> = None;

    loop {
        let start = cursor.pos;
        let line = cursor
            .read_until(b'\n')
            .map_err(|err| ParseError::new(err.offset, ParseErrorKind::MissingNewLine))?;
        // new line was found but the line is empty, we found the divider between headers and message
        if line.is_empty() {
            break;
        }
        let index = memchr::memchr(b' ', line)
            .ok_or(ParseError::new(start, ParseErrorKind::MissingHeaderSpace))?;
        let header_name = &line[..index];
        let header_value = &line[index + 1..];

        match header_name {
            b"tree" => {
                if tree.is_some() {
                    return Err(ParseError::new(
                        start,
                        ParseErrorKind::DuplicateHeader { name: "tree" },
                    ));
                }
                tree = Some(parse_oid(header_value).map_err(|err| {
                    // OidError returns the offset relative to the input, and we adjust for the
                    // absolute pos
                    ParseError::new(start + err.offset(), ParseErrorKind::BadOid(err))
                })?);
            }
            // TODO: read parent's declaration
            b"parent" => {
                if parent.is_some() {
                    return Err(ParseError::new(
                        start,
                        ParseErrorKind::DuplicateHeader { name: "parent" },
                    ));
                }
                parent = Some(parse_oid(header_value).map_err(|err| {
                    // OidError returns the offset relative to the input, and we adjust for the
                    // absolute pos
                    ParseError::new(start + err.offset(), ParseErrorKind::BadOid(err))
                })?);
            }
            b"author" => {
                if author.is_some() {
                    return Err(ParseError::new(
                        start,
                        ParseErrorKind::DuplicateHeader { name: "author" },
                    ));
                }
                author = Some(parse_signature(header_value).unwrap())
            }
            b"committer" => {
                if committer.is_some() {
                    return Err(ParseError::new(
                        start,
                        ParseErrorKind::DuplicateHeader { name: "committer" },
                    ));
                }
                committer = Some(parse_signature(header_value).unwrap())
            }
            other => return Err(ParseError::new(start, ParseErrorKind::UnexpectedHeader { header: other.to_vec() })),
        }
    }

    todo!()
}

fn parse_oid(buf: &[u8]) -> Result<[u8; 20], OidError> {
    let hex: &[u8; 40] = buf.try_into().map_err(|_| OidError::WrongLen {
        offset: 0,
        len: buf.len(),
    })?;

    let mut oid = [0u8; 20];
    // same logic as:
    //  let j = i * 2;
    //  let pair: &[u8; 2] = hex[j..j + 2].try_into().unwrap();
    for (i, pair) in hex.chunks_exact(2).enumerate() {
        oid[i] = hex::pair_to_u8(pair.try_into().unwrap())
            // i * 2 is the index we create
            .map_err(|e| OidError::BadDigit {
                offset: i * 2 + e.pos,
                digit: e.digit,
            })?;
    }
    Ok(oid)
}

fn parse_signature(buf: &[u8]) -> Result<Signature, SignatureError> {
    let mut pos = 0;
    let index =
        memchr::memchr(b'<', buf).ok_or(SignatureError::MissingAngleBracket { offset: pos })?;
    let name = buf[..index]
        .strip_suffix(b" ")
        .ok_or(SignatureError::MissingSpace { offset: 0 })?
        .to_vec();
    if name.is_empty() {
        return Err(SignatureError::MissingName { offset: 0 });
    }
    let name =
        String::from_utf8(name).map_err(|err| SignatureError::InvalidUtf8(err.utf8_error()))?;

    pos = index + 1;
    let index = memchr::memchr(b'>', &buf[pos..])
        .ok_or(SignatureError::MissingAngleBracket { offset: pos })?;
    let email = buf[pos..index].to_vec();
    if email.is_empty() {
        return Err(SignatureError::MissingEmail { offset: pos });
    }
    let email =
        String::from_utf8(email).map_err(|err| SignatureError::InvalidUtf8(err.utf8_error()))?;
    pos = index + 1;
    if buf.get(pos) != Some(&b' ') {
        return Err(SignatureError::MissingSpace { offset: index + 1 });
    }
    pos += 1;
    let index =
        memchr::memchr(b' ', &buf[pos..]).ok_or(SignatureError::MissingSpace { offset: pos })?;
    let unix = &buf[pos..index];
    let timezone = &buf[index + 1..];
    let timestamp = Timestamp::from_bytes(unix, timezone)
        .map_err(|err| SignatureError::BadTimestamp { offset: pos, err })?;

    Ok(Signature {
        name,
        email,
        timestamp,
    })
}
