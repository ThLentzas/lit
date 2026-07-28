use crate::command::object::{Commit, Entry, Object, Signature};
use crate::command::os;
use crate::command::timestamp::{Timestamp, TimestampError};
use crate::hex::{self, HexError};

// Every parser that I wrote so far is a struct and all the parse_* methods are implemented in its
// impl block. Now we pass Cursor a struct that has some generic methods that are used to walk the
// buffer. It is helps a lot approaching the parsing like this because of commit's structure.
// Read parse_commit(). Every method that does not take &mut Cursor as arg, returns error indices
// relative to the slice passed, but they get adjusted by the caller(parse_signature(), parse_oid()).
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

    fn take<const N: usize>(&mut self) -> Result<&'a [u8; N], CursorError> {
        let start = self.pos;
        let available = self.buf.len().saturating_sub(start);

        let end = start.checked_add(N).ok_or(CursorError::new(
            start,
            CursorErrorKind::Truncated {
                needed: N,
                available,
            },
        ))?;

        // we can't just call self.buf[self.pos..self.pos + N], it can panic
        let bytes = self.buf.get(start..end).ok_or(CursorError::new(
            start,
            CursorErrorKind::Truncated {
                needed: N,
                available,
            },
        ))?;

        // safe to unwrap since end - start is N
        let bytes: &[u8; N] = bytes.try_into().unwrap();
        self.pos = end;

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
enum CursorErrorKind {
    /// read_until: delimiter absent between `offset` and end of input
    MissingDelimiter { delimiter: u8 },
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
pub(crate) enum OidError {
    WrongLen { offset: usize, len: usize },
    BadDigit(HexError),
}

impl OidError {
    fn offset(&self) -> usize {
        match self {
            OidError::WrongLen { offset, .. } => *offset,
            OidError::BadDigit(err) => err.pos,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct SignatureError {
    pub(super) offset: usize,
    pub(super) kind: SignatureErrorKind,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SignatureErrorKind {
    MissingSpace,
    MissingAngleBracket,
    MissingName,
    MissingEmail,
    MissingTime,
    InvalidUtf8,
    BadTimestamp(TimestampError),
}

impl SignatureError {
    fn new(offset: usize, kind: SignatureErrorKind) -> Self {
        Self { offset, kind }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TreeEntryErrorKind {
    MissingSpace,
    MissingNul,
    TruncatedOid,
    BadPathName { name: Vec<u8> },
    UnknownMode { mode: Vec<u8> },
}

#[derive(Debug, PartialEq, Eq)]
struct TreeEntryError {
    offset: usize,
    kind: TreeEntryErrorKind,
}

impl TreeEntryError {
    fn new(offset: usize, kind: TreeEntryErrorKind) -> Self {
        Self { offset, kind }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ParseErrorKind {
    DuplicateHeader { name: &'static str },
    MissingHeaderSpace,
    MissingHeaderNul,
    InvalidTreeEntry(TreeEntryErrorKind),
    InvalidSizeHeader,
    SizeMisMatch { expected: usize, actual: usize },
    MissingDelimiter { delimiter: u8 },
    MissingNewLine,
    UnknownType { got: Vec<u8> },
    BadSignature(SignatureErrorKind),
    BadOid(OidError),
    BadTimestamp(TimestampError),
    MissingBlankLine,
    UnexpectedHeader { expected: Vec<u8>, got: Vec<u8> },
    InvalidUtf8,
}

pub(crate) struct ParseError {
    offset: usize,
    kind: ParseErrorKind,
}

impl ParseError {
    fn new(offset: usize, kind: ParseErrorKind) -> Self {
        Self { offset, kind }
    }
}

struct Line<'a> {
    // where the line starts with respect to the entire buffer
    start: usize,
    key: &'a [u8],
    value: &'a [u8],
    // absolute index of value within the entire buffer
    vbase: usize,
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
                entries.push(parse_tree_entry(&mut cursor).map_err(|err| {
                    ParseError::new(err.offset, ParseErrorKind::InvalidTreeEntry(err.kind))
                })?);
            }
            Ok(Object::Tree(entries))
        }
        b"commit" => Ok(Object::Commit(parse_commit(&mut cursor)?)),
        _ => Err(ParseError::new(
            0,
            ParseErrorKind::UnknownType { got: kind.to_vec() },
        )),
    }
}

fn parse_tree_entry(cursor: &mut Cursor) -> Result<Entry, TreeEntryError> {
    let start = cursor.pos;
    let buf = cursor
        .read_until(b' ')
        .map_err(|err| TreeEntryError::new(err.offset, TreeEntryErrorKind::MissingSpace))?;
    let mut mode = 0u32;

    // this part is tricky, we have the mode as ASCII, something like '100644', we can't just convert
    // it to 100644 in base 10, we need base 8
    // "100644" octal -> 0o100644(33188 decimal)
    for &b in buf {
        if !(b'0'..=b'7').contains(&b) {
            return Err(TreeEntryError::new(
                start,
                TreeEntryErrorKind::UnknownMode { mode: buf.to_vec() },
            ));
        }
        mode = mode
            .checked_mul(8)
            .and_then(|n| n.checked_add((b - b'0') as u32))
            .ok_or(TreeEntryError::new(
                start,
                TreeEntryErrorKind::UnknownMode { mode: buf.to_vec() },
            ))?;
    }

    if !matches!(mode, os::EXECUTABLE | os::REGULAR | os::SYMLINK | os::DIR) {
        return Err(TreeEntryError::new(
            start,
            TreeEntryErrorKind::UnknownMode { mode: buf.to_vec() },
        ));
    }

    let start = cursor.pos;
    let name = cursor
        .read_until(0)
        .map_err(|err| TreeEntryError::new(err.offset, TreeEntryErrorKind::MissingNul))?;
    // the name of the tree entry is the name as it was set by index::Tree::write()
    // it is flat and represents one object level at the current tree
    if matches!(name, b"." | b".." | b".lit")
        || name.contains(&0)
        || name.contains(&b'/')
        || name.is_empty()
    {
        return Err(TreeEntryError::new(
            start,
            TreeEntryErrorKind::BadPathName {
                name: name.to_vec(),
            },
        ));
    }

    let oid = cursor
        .take::<20>()
        .map_err(|err| TreeEntryError::new(err.offset, TreeEntryErrorKind::TruncatedOid))?;

    Ok(Entry {
        mode,
        name: name.to_vec(),
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

// tree tree_oid_hex\n
// parent parent_oid_hex\n // repeated once per parent, omitted for root commit
// author name <email> timestamp timezone\n
// committer name <email> timestamp timezone\n
// \n
// message
//
// To parse the commit instead of reading up to 1st space and then to \n etc, we can try to parse it
// line by line. Read until \n and that slice is now a Line, where everything up to the 1st space is
// the header name/key and everything after is the value. This way we can create helper functions to
// parse value without bloating commit. tree and parent headers are followed by the same value a
// 40-character hex string. After tree, parent header is optional so in the mismatch case we don't
// error but pass line to the next check.
fn parse_commit(cursor: &mut Cursor) -> Result<Commit, ParseError> {
    let line = parse_header(cursor)?;
    if line.key != b"tree" {
        return Err(ParseError::new(
            line.start,
            ParseErrorKind::UnexpectedHeader {
                expected: b"tree".to_vec(),
                got: line.key.to_vec(),
            },
        ));
    }
    let tree = parse_oid(line.value)
        .map_err(|err| ParseError::new(line.vbase + err.offset(), ParseErrorKind::BadOid(err)))?;
    let mut parents = Vec::new();
    let line = loop {
        let line = parse_header(cursor)?;
        if line.key != b"parent" {
            break line;
        }
        parents.push(parse_oid(line.value).map_err(|err| {
            ParseError::new(line.vbase + err.offset(), ParseErrorKind::BadOid(err))
        })?);
    };
    if line.key != b"author" {
        return Err(ParseError::new(
            line.start,
            ParseErrorKind::UnexpectedHeader {
                expected: b"author".to_vec(),
                got: line.key.to_vec(),
            },
        ));
    }
    let author = parse_signature(line.value).map_err(|err| {
        ParseError::new(
            line.vbase + err.offset,
            ParseErrorKind::BadSignature(err.kind),
        )
    })?;

    let line = parse_header(cursor)?;
    if line.key != b"committer" {
        return Err(ParseError::new(
            line.start,
            ParseErrorKind::UnexpectedHeader {
                expected: b"committer".to_vec(),
                got: line.key.to_vec(),
            },
        ));
    }
    let committer = parse_signature(line.value).map_err(|err| {
        ParseError::new(
            line.vbase + err.offset,
            ParseErrorKind::BadSignature(err.kind),
        )
    })?;
    let start = cursor.pos;
    let line = cursor
        .read_until(b'\n')
        .map_err(|err| ParseError::new(err.offset, ParseErrorKind::MissingNewLine))?;
    if !line.is_empty() {
        return Err(ParseError::new(start, ParseErrorKind::MissingBlankLine));
    }
    let start = cursor.pos;
    // after the blank line, everything that is left is the message
    let message = String::from_utf8(cursor.rest().to_vec()).map_err(|err| {
        ParseError::new(
            start + err.utf8_error().valid_up_to(),
            ParseErrorKind::InvalidUtf8,
        )
    })?;

    Ok(Commit {
        root_id: tree,
        parent: parents,
        author,
        committer,
        message,
    })
}

// line.key and line.value live in cursor.buf so they share the same lifetime
fn parse_header<'a>(cursor: &mut Cursor<'a>) -> Result<Line<'a>, ParseError> {
    let start = cursor.pos;
    let line = cursor
        .read_until(b'\n')
        .map_err(|err| ParseError::new(err.offset, ParseErrorKind::MissingNewLine))?;
    let index = memchr::memchr(b' ', line)
        .ok_or(ParseError::new(start, ParseErrorKind::MissingHeaderSpace))?;
    // everything up to space is the key and everything after is the value
    Ok(Line {
        start,
        key: &line[..index],
        value: &line[index + 1..],
        vbase: start + index + 1,
    })
}

fn parse_oid(buf: &[u8]) -> Result<String, OidError> {
    let bytes: &[u8; 40] = buf.try_into().map_err(|_| OidError::WrongLen {
        offset: 0,
        len: buf.len(),
    })?;

    Ok(hex::parse_hex(bytes).map_err(OidError::BadDigit)?)
}

fn parse_signature(buf: &[u8]) -> Result<Signature, SignatureError> {
    let mut pos = 0;
    let index = memchr::memchr(b'<', buf).ok_or(SignatureError::new(
        pos,
        SignatureErrorKind::MissingAngleBracket,
    ))?;
    let name = buf[..index]
        .strip_suffix(b" ")
        .ok_or(SignatureError::new(pos, SignatureErrorKind::MissingSpace))?
        .to_vec();
    if name.is_empty() {
        return Err(SignatureError::new(pos, SignatureErrorKind::MissingName));
    }
    let name = String::from_utf8(name).map_err(|err| {
        SignatureError::new(
            pos + err.utf8_error().valid_up_to(),
            SignatureErrorKind::InvalidUtf8,
        )
    })?;

    pos += index + 1;
    let index = memchr::memchr(b'>', &buf[pos..]).ok_or(SignatureError::new(
        pos,
        SignatureErrorKind::MissingAngleBracket,
    ))?;
    let email = buf[pos..index].to_vec();
    if email.is_empty() {
        return Err(SignatureError::new(pos, SignatureErrorKind::MissingEmail));
    }
    let email = String::from_utf8(email).map_err(|err| {
        SignatureError::new(
            pos + err.utf8_error().valid_up_to(),
            SignatureErrorKind::InvalidUtf8,
        )
    })?;
    pos += index + 1;
    if buf.get(pos) != Some(&b' ') {
        return Err(SignatureError::new(pos, SignatureErrorKind::MissingSpace));
    }
    pos += 1;
    let index = memchr::memchr(b' ', &buf[pos..])
        .ok_or(SignatureError::new(pos, SignatureErrorKind::MissingSpace))?;
    let unix = &buf[pos..pos + index];
    let timezone = &buf[pos + index + 1..];
    let timestamp = Timestamp::from_bytes(unix, timezone)
        .map_err(|err| SignatureError::new(pos, SignatureErrorKind::BadTimestamp(err)))?;

    Ok(Signature {
        name,
        email,
        timestamp,
    })
}
