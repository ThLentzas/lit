use crate::repo::config::parse::{Header, LineKind, LineParser, ParseError};
use crate::repo::os;
use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct BufferSpan {
    start: usize,
    end: usize,
}

// LineSpan refers to the span that name and optionally value has for a variable line, they are relative
// to the line itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct LineSpan {
    pub(super) start: usize,
    pub(super) end: usize,
}

// TODO: duplicate headers can exist each with their own block
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct SectionBlock {
    start: usize,
    end: usize,
}

enum LineContent {
    Slice(BufferSpan),
    Owned(Vec<u8>),
}

struct Line {
    content: LineContent,
    kind: LineKind,
}

impl Line {
    fn new_variable(name: &str, value: &[u8]) -> Self {
        let value = encode_value(value);
        // starts after '\t'
        let name_start = 1;
        let name_end = name_start + name.len();
        // " = "
        let value_start = name_end + 3;
        let value_end = value_start + value.len();
        let mut line = Vec::with_capacity(value_end);
        line.push(b'\t');
        line.extend_from_slice(name.as_bytes());
        line.extend_from_slice(b" = ");
        line.extend_from_slice(&value);
        line.push(b'\n');

        Self {
            content: LineContent::Owned(line),
            kind: LineKind::Variable {
                name: LineSpan {
                    start: name_start,
                    end: name_end,
                },
                value: Some(LineSpan {
                    start: value_start,
                    end: value_end,
                }),
            },
        }
    }

    fn new_header(key: &SectionKey) -> Self {
        let mut line = Vec::new();
        let name_start = 1;
        let name_end = name_start + key.name.len();

        line.push(b'[');
        line.extend_from_slice(key.name.as_bytes());
        let subsection = key.subsection.as_ref().map(|sub| {
            let start = name_end + 2;
            line.extend_from_slice(b" \"");
            line.extend_from_slice(&sub);
            line.push(b'"');
            let end = start + sub.len();
            LineSpan { start, end }
        });

        line.extend_from_slice(b"]\n");

        Self {
            content: LineContent::Owned(line),
            kind: LineKind::Header(Header {
                name: LineSpan {
                    start: name_start,
                    end: name_end,
                },
                subsection,
            }),
        }
    }

    // returns the slice of the line in the buffer
    //
    // Bad Design: Initially I would pass around the buffer and the span and let the callee create
    // the slice which could cause problems with indexing. Now there is only source of truth,
    // line.slice() needs the buffer and the span of the thing we are interested in within the line
    // This was the old constructor of ConfigKey::new()
    //      fn new(buf: &[u8], section: &Span, subsection: &Option<Span>, name: &Span) -> Self
    // Now types are created and slices have been resolved by the caller
    //      fn new(section: SectionKey, name: &[u8])
    fn bytes<'a>(&'a self, buf: &'a [u8]) -> &'a [u8] {
        match &self.content {
            LineContent::Slice(span) => &buf[span.start..span.end],
            LineContent::Owned(bytes) => &bytes,
        }
    }

    // a slice within the line, it is used to extract the values of name or value in a case of a
    // variable
    fn slice<'a>(&'a self, buf: &'a [u8], span: &LineSpan) -> &'a [u8] {
        &self.bytes(buf)[span.start..span.end]
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SectionKey {
    name: String,
    subsection: Option<Vec<u8>>,
}

impl SectionKey {
    // Read Config::new()
    unsafe fn new_unchecked(name: &[u8], subsection: Option<&[u8]>) -> Self {
        // SAFETY: header name can contain only alphanumeric, or '-' and it is guaranteed by the parser
        let name = unsafe { String::from_utf8_unchecked(downcase(name)) };
        let subsection = subsection.map(Vec::from);

        Self { name, subsection }
    }
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub(super) struct ConfigKey {
    section: SectionKey,
    name: String,
}

impl ConfigKey {
    // This fn should only be called during build_index() where we know that the name is guaranteed
    // to be alphanumeric(plus '-') by the parser so calls like from_utf8_unchecked or downcase are
    // safe
    unsafe fn new_unchecked(section: SectionKey, name: &[u8]) -> Self {
        ConfigKey {
            section,
            // SAFETY: name can contain only alphanumeric, '-' and it is guaranteed by the parser
            name: unsafe { String::from_utf8_unchecked(downcase(name)) },
        }
    }

    // Git’s flat config key is ambiguous because: section.subsection.variable uses . as a separator,
    // but section names themselves may also contain . The docs say the fully qualified variable name
    // treats the last dot-separated segment as the variable name, and "everything before the last dot"
    // as the section header. https://github.com/git/git/blob/master/config.c
    //
    // This is where the implementation diverges. In real Git I couldn't find how they handle the
    // ambiguity of having '.' being a valid character of section and the separator for section/
    // subsection. foo.bar.baz is ambiguous
    //
    // It can mean [foo.bar] and baz = ...
    // It can also mean [foo "bar"] and baz = ...
    // It matters which part we consider as section because the section part needs to be downcased
    // while the subsection must remain as is.
    //
    // For now, we mimic the behavior where name is everything past the last dot. Section is everything
    // up to the first dot, the rest is the subsection. If there is only one '.', it's always
    // section-name
    pub(super) fn from_name(name: &OsStr) -> Option<Self> {
        let bytes = os::os_str_as_bytes(name).ok()?;
        let pos = bytes.iter().rposition(|&b| b == b'.')?;
        let header = &bytes[..pos];
        let name = &bytes[pos + 1..];

        if header.is_empty() || name.is_empty() {
            return None;
        }

        let (section, subsection): (&[u8], Option<&[u8]>) =
            match header.iter().position(|&b| b == b'.') {
                Some(i) => {
                    let section = &header[..i];
                    let subsection = &header[i + 1..];
                    if section.is_empty() || subsection.is_empty() {
                        return None;
                    }
                    (section, Some(subsection))
                }
                None => (header, None),
            };
        if !name[0].is_ascii_alphabetic() {
            return None;
        }

        let section = SectionKey {
            // SAFETY: will only return Some if section contains alphanumeric or '-'
            name: unsafe { String::from_utf8_unchecked(try_downcase(section)?.to_vec()) },
            subsection: subsection.map(|slice| slice.to_vec()),
        };

        Some(ConfigKey {
            section,
            name: unsafe { String::from_utf8_unchecked(try_downcase(name)?) },
        })
    }
}

fn try_downcase(buf: &[u8]) -> Option<Vec<u8>> {
    let mut bytes = Vec::with_capacity(buf.len());

    for &byte in buf {
        if byte.is_ascii_alphanumeric() || byte == b'-' {
            bytes.push(byte.to_ascii_lowercase());
        } else {
            return None;
        }
    }
    Some(bytes)
}

fn downcase(buf: &[u8]) -> Vec<u8> {
    buf.iter().map(|&byte| byte.to_ascii_lowercase()).collect()
}

pub(crate) enum Value<'a> {
    ImplicitlyTrue,
    Bytes(Cow<'a, [u8]>)
}

struct ConfigFileIndex {
    // Initially thought about an IndexMap to preserve the order, but it is already there in Vec<Line>
    keys: HashMap<ConfigKey, Vec<usize>>,
    sections: HashMap<SectionKey, Vec<SectionBlock>>,
}

impl ConfigFileIndex {
    // after loading the file in memory and creating the Vec<Line> we create the index. As mentioned
    // above we don't have a nested structure, but a flat list of logical lines. The way we group
    // lines togther is by section, and each section is positional, the most recent header seen above
    // it, current_section keeps track of that.
    fn new(buf: &[u8], lines: &[Line]) -> Self {
        let mut variables: HashMap<ConfigKey, Vec<usize>> = HashMap::new();
        let mut sections: HashMap<SectionKey, Vec<SectionBlock>> = HashMap::new();

        let mut current_section: Option<(SectionKey, usize)> = None;

        for (i, line) in lines.iter().enumerate() {
            match &line.kind {
                LineKind::Header(header) => {
                    current_section.take().map(|(key, start)| {
                        sections
                            .entry(key.clone())
                            .or_default()
                            .push(SectionBlock { start, end: i });
                    });
                    let name = line.slice(buf, &header.name);
                    let subsection = header.subsection.as_ref().map(|span| line.slice(buf, span));
                    let section = unsafe { SectionKey::new_unchecked(name, subsection) };
                    current_section = Some((section, i));
                }
                LineKind::Variable { name, .. } => {
                    // this is the case where a variable appears before a section which is not possible
                    // in a valid config format. Parser would have caught it so unwrap is safe
                    let (section, _) = current_section.as_ref().unwrap();
                    let name = line.slice(buf, name);
                    let key = unsafe { ConfigKey::new_unchecked(section.clone(), name) };
                    variables.entry(key).or_default().push(i);
                }
                LineKind::Blank | LineKind::Comment => {}
            }
        }

        // last section
        if let Some((key, start)) = current_section {
            sections.entry(key).or_default().push(SectionBlock {
                start,
                end: lines.len(),
            });
        }

        Self {
            keys: variables,
            sections,
        }
    }

    fn last_pos(&self, key: &ConfigKey) -> Option<usize> {
        self.key_positions(key).and_then(|pos| pos.last()).copied()
    }

    // returns the indices of the lines that contain the key
    // returning &[usize] where [] means no lines found is incorrect because it does not represent
    // a valid internal. a key entry always corresponds to at least one line, [] never occurs.
    fn key_positions(&self, key: &ConfigKey) -> Option<&[usize]> {
        self.keys.get(key).map(Vec::as_slice)
    }

    // returns the last section block to find where should the new line be inserted
    fn last_block(&self, section: &SectionKey) -> Option<&SectionBlock> {
        self.sections.get(section).and_then(|block| block.last())
    }
}

pub(super) struct ConfigFile {
    // the buffer we read into
    buf: Vec<u8>,
    lines: Vec<Line>,
    index: ConfigFileIndex,
    modified: bool,
}

impl ConfigFile {
    // In theory .lit/config is just a list of sections where each section has a list of variables
    // If we try to use this as our parsing rule we lose the trivia and our CST is no more lossless.
    // We have nowhere to store the trivia. Where does a blank between two variables go? A comment
    // after the last variable but before the next session header? Instead, we can model it as a flat
    // Vec<Line> where each line holds the raw bytes and is classified as SECTION, COMMENT etc.
    // CST becomes our source of truth for writing.
    //
    // This is a zero-copy approach. The name of a variable is a sub-slice of its line, which is a
    // sub-slice of the file.
    pub(crate) fn load(path: &Path) -> Result<Self, ConfigFileError> {
        let mut buf = Vec::new();
        let lines = read_lines(&mut buf, path)?;
        let index = ConfigFileIndex::new(&buf, &lines);

        Ok(Self {
            buf,
            lines,
            index,
            modified: false,
        })
    }

    pub(crate) fn serialize(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.buf.len());

        for line in &self.lines {
            bytes.extend_from_slice(line.bytes(&self.buf));
        }

        bytes
    }

    pub(super) fn key_last_pos(&self, key: &ConfigKey) -> Option<usize> {
        self.index.last_pos(key)
    }

    pub(super) fn value_at(&self, pos: usize) -> Value<'_> {
        let LineKind::Variable { value, .. } = &self.lines[pos].kind else {
            unreachable!("positions only point at variable lines");
        };
        match value {
            // valueless boolean, always true
            None => Value::ImplicitlyTrue,
            Some(span) => Value::Bytes(interpret_value(self.lines[pos].slice(&self.buf, span))),
        }
    }

    // returns the indices of the lines that contain the key
    pub(super) fn key_positions(&self, key: &ConfigKey) -> Option<&[usize]> {
        self.index.key_positions(key)
    }

    // this method is called when the look-up for the exact config key returned None
    // it means that they exact key provided by the user does not exist, and we have to check if
    // the section of the key exists, if so we append, otherwise, we create the section and insert
    // the value
    //
    // if foo.bar.baz does not exist, we shouldn't naively try to create foo.bar and then insert
    // we first check if foo.bar exists, we append, otherwise we create the whole section
    pub(super) fn insert_variable(&mut self, key: &ConfigKey, value: &[u8]) {
        let section = &key.section;
        let variable = Line::new_variable(&key.name, value);

        // if the header exists and has multiple blocks the new variable is added always to the last
        // one
        match self.index.last_block(section) {
            Some(block) => {
                let at = block.end;
                self.ensure_newline(at - 1);
                self.lines.insert(at, variable);
            }
            None => {
                if let Some(last) = self.lines.len().checked_sub(1) {
                    self.ensure_newline(last);
                }
                let header = Line::new_header(section);
                self.lines.push(header);
                self.lines.push(variable);
            }
        }
    }

    // TODO: dont force the write, change this and pass the value that exists
    pub(super) fn replace_value(&mut self, pos: usize, value: &[u8]) {
        let line = &mut self.lines[pos];
        let LineContent::Slice(raw) = &line.content else {
            // already Owned from an earlier edit this session
            unreachable!("replacing a freshly-loaded line");
        };
        let LineKind::Variable { value: Some(v), .. } = &line.kind else {
            // valueless boolean `flag`
            todo!("boolean -> value");
        };

        let pre = &self.buf[raw.start..raw.start + v.start];
        let post = &self.buf[raw.start + v.end..raw.end];
        let mut content = Vec::new();
        content.extend_from_slice(pre);
        content.extend_from_slice(&encode_value(value));
        content.extend_from_slice(post);

        self.lines[pos].content = LineContent::Owned(content);
        self.modified = true;
    }

    // when we want to insert the new line(either header or variable) we have 1 edge case to consider
    // the last line of the file might not be '\n' terminated, our writer does add '\n' even for
    // the last line but the file could have been changed
    //
    // if the '\n' is missing, and we try to write our new line it fuses onto the prior last line
    // the last line can either belong to the same section we want to write our new line, or if
    // the section is missing then it is the last line of the previous section
    fn ensure_newline(&mut self, pos: usize) {
        let line = &self.lines[pos];
        let ends_with_nl = match &line.content {
            LineContent::Slice(s) => self.buf.get(s.end - 1) == Some(&b'\n'),
            LineContent::Owned(v) => v.last() == Some(&b'\n'),
        };
        if ends_with_nl {
            return;
        }
        // rebuild as Owned with a trailing '\n' (kind spans are line-relative, still valid)
        let mut bytes = line.bytes(&self.buf).to_vec();
        bytes.push(b'\n');
        self.lines[pos].content = LineContent::Owned(bytes);
    }
}

fn read_lines(mut buf: &mut Vec<u8>, path: &Path) -> Result<Vec<Line>, ConfigFileError> {
    let file = File::open(path).map_err(|err| ConfigFileError::Io {
        path: path.to_path_buf(),
        source: err,
    })?;
    let mut lines = Vec::new();
    let mut reader = BufReader::new(&file);
    let mut physical_lines = 0;

    loop {
        let start = buf.len();
        let n = reader
            .read_until(b'\n', &mut buf)
            .map_err(|err| ConfigFileError::Io {
                path: path.to_path_buf(),
                source: err,
            })?;
        if n == 0 {
            // EOF
            break;
        }
        physical_lines += 1;
        // From the different kinds of line we can have only 1 is allowed to be multiline,
        // the variable, and the \<newline> continuation rule is only applied to the value
        // part of the variable. We can't just join the next physical line before we know
        // what kind of line it is. The solution is to scan the part of the buffer we just
        // read and only continue if the line is actually a variable. Variables starts with
        // name and name's 1st character must be alphabetic. If we didn't scan we could end
        // in a case where we have a comment that spans into multiple lines which is not allowed
        if should_fold(&buf, start) {
            while ends_with_continuation(&buf) {
                if reader
                    .read_until(b'\n', &mut buf)
                    .map_err(|err| ConfigFileError::Io {
                        path: path.to_path_buf(),
                        source: err,
                    })?
                    == 0
                {
                    break;
                }
            }
        }
        let end = buf.len();
        // n was 0, inner loop break, outer must too
        if start == end {
            break;
        }
        // each span represents the boundaries of each logical line which is the result of applying
        // the continuation rules in a physical line.
        let span = BufferSpan { start, end };
        let kind = classify(&buf, &span)
            // we can't pass self.lines.len() + 1 because that would result in the logical lines
            // physical lines != logical lines
            // the user sees the physical lines of the file
            .map_err(|err| ConfigFileError::InvalidFormat {
                line: physical_lines,
                source: err,
            })?;
        lines.push(Line {
            content: LineContent::Slice(span),
            kind,
        });
    }
    Ok(lines)
}

// if we wanted to return a ConfigError here we would need to pass the physical line count
fn classify(buf: &[u8], span: &BufferSpan) -> Result<LineKind, ParseError> {
    let parser = LineParser::new(&buf[span.start..span.end]);
    parser.parse()
}

fn ends_with_continuation(buf: &[u8]) -> bool {
    if buf.last() != Some(&b'\n') {
        return false; // the last chunk does not end with a new line
    }

    let mut i = buf.len() - 1;
    let mut backslashes = 0;
    while i > 0 && buf[i - 1] == b'\\' {
        backslashes += 1;
        i -= 1;
    }
    backslashes % 2 == 1
}

fn should_fold(buf: &[u8], start: usize) -> bool {
    let mut i = start;
    while matches!(buf.get(i), Some(b' ' | b'\t')) {
        i += 1;
    }
    matches!(buf.get(i), Some(b) if b.is_ascii_alphabetic())
}

fn interpret_value(value: &[u8]) -> Cow<'_, [u8]> {
    // fast path if nothing needs handling we return the value verbatim
    if memchr::memchr2(b'"', b'\\', value).is_some() {
        return Cow::Borrowed(value);
    }

    let mut i = 0;
    let mut bytes = Vec::with_capacity(value.len());
    while i < value.len() {
        match value[i] {
            // quotes are syntax, they aren't part of the logical value.
            b'"' => i += 1,
            b'\\' => {
                i += 1;
                // safe parse_value() has already validated the syntax.
                match value[i] {
                    b'"' => bytes.push(b'"'),
                    b'\\' => bytes.push(b'\\'),
                    b'n' => bytes.push(b'\n'),
                    b't' => bytes.push(b'\t'),
                    b'b' => bytes.push(0x08),
                    // logical-line continuation, \<newline>
                    b'\n' => {}
                    b'\r' => {
                        // TODO: how do we handle this gracefully? degug_assert_eq!() is for unoptimized builds
                        debug_assert_eq!(value.get(i + 1), Some(&b'\n'));
                        i += 1;
                    }
                    _ => unreachable!("interpret_value() called with a bad escape sequence"),
                }
                i += 1;
            }
            // TODO: if those individuals push() calls have a performance cost as we have seen in
            // jolt with parse_string() I think we can use the same technique and push slices that
            // do not need special handling.
            byte => {
                bytes.push(byte);
                i += 1;
            }
        }
    }
    Cow::Owned(bytes)
}

// the logic of decoding the value lives in parse.rs::parse_value(). There is a key distinction that
// confused me for a while, encode_value() takes a buffer that represents the value, but parse_value()
// takes a buffer that contains the value and all the trivia until the end of the line. parse_value()
// stops when it encounters LF, CRLF or any comment. All encode_value() does is mapping, it never
// has to consider comments or new lines. It knows everything within the buffer is the value. It needs
// to do special handling if certain characters are present. The caller, parse_value() for example
// is responsible for writing the trivia after the value including the new line.
// hello # world as value can't be passed verbatim, because the parser will see '#' and treat is as
// a comment, but # world is part of the value, we need to quote in such cases. This is true for
// leading/trailing ws. Read set() for how terminals pass values, very important.
// TODO: we need to see how to handle '/r' if we should reject it or not
fn encode_value(value: &[u8]) -> Cow<'_, [u8]> {
    let needs_quotes = matches!(value.first(), Some(b' ' | b'\t'))
        || matches!(value.last(), Some(b' ' | b'\t'))
        || memchr::memchr2(b'#', b';', value).is_some();

    if !needs_quotes
        && value
            .iter()
            .any(|&b| matches!(b, b'\"' | b'\\' | b'\n' | b'\t' | 0x08))
    {
        return Cow::Borrowed(value);
    }

    let mut bytes = Vec::with_capacity(value.len() + 2);
    if needs_quotes {
        bytes.push(b'"');
    }
    for &b in value {
        match b {
            b'"' => bytes.extend_from_slice(b"\\\""),
            b'\\' => bytes.extend_from_slice(b"\\\\"),
            b'\n' => bytes.extend_from_slice(b"\\n"),
            b'\t' => bytes.extend_from_slice(b"\\t"),
            0x08 => bytes.extend_from_slice(b"\\b"),
            _ => bytes.push(b),
        }
    }
    if needs_quotes {
        bytes.push(b'"');
    }

    Cow::Owned(bytes)
}

#[derive(Debug)]
pub(crate) enum ConfigFileError {
    Io { path: PathBuf, source: io::Error },
    // TODO: display the unexpected byte value as hex, git shows bad config line 1. We can provide
    // TODO: a message with more information such as the actual reason and the offset within the line
    InvalidFormat { line: usize, source: ParseError },
    MissingValue { name: OsString },
    NotUnicode { key: OsString, value: Vec<u8> },
    MultipleValues,
    BadKey,
}
