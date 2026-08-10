use crate::repo::config::parse::{Header, LineKind, LineParser, ParseError};
use crate::repo::os;
use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::PathBuf;

pub(super) mod parse;

// BufferSpan is the span of the line relative to the buffer which includes the trivia
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct BufferSpan {
    start: usize,
    end: usize,
}

// LineSpan refers to the span that name and optionally value has for a variable line, they are relative
// to the line itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct LineSpan {
    start: usize,
    end: usize, // exclusive
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
        // +1 for the '\n'
        let mut line = Vec::with_capacity(value_end + 1);
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
}

// the .gitconfig which the global file Git looks for any configuration is created lazily on first
// write, unlike .git/config which is created when we call init
// TODO: global, system, Read Chapter 25.2.3 and 25.3
pub(crate) struct Config {
    local: PathBuf,
    // the buffer we read into
    buf: Vec<u8>,
    lines: Vec<Line>,
    // Initially thought about an IndexMap to preserve the order, but it is already there in Vec<Line>
    index: HashMap<ConfigKey, Vec<usize>>,
    // TODO: finish this comment, currently is wrong
    // We can't use Header as key because it holds spans into the buffer which incorrectly identifies
    // duplicate sections. If a section like [user] appears twice then Header will hold two different
    // spans for it, since it exists in 2 different places on the buffer
    sections: HashMap<SectionKey, Vec<LineSpan>>,
    modified: bool,
}

impl Config {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            local: path,
            buf: Vec::new(),
            lines: Vec::new(),
            index: HashMap::new(),
            sections: HashMap::new(),
            modified: false,
        }
    }

    // TODO: for now we return a slice which is tied to self which is fine for the case where we
    // retrieve the use info since it will be stored to commit and commit owns the data. In the future,
    // when we make this method returns values based on the --config we need to decode the actual
    // variable value and return owned data.
    // key = "a" b "c" -> should map to a b c
    //
    // The api for retrieving values is designed as follows:
    //  - when config is invoked as a command with get what we return is always a byte slice. The
    //  returned value is then displayed with the same logic as status.
    //  - when other commands need values from config it is up to the caller to invoke one of the
    //  typed functions based on their requirements. For example, commit needs the user's information
    //  which it can get from .config. In this case, the caller invokes get_str() because name/email
    //  are human readable.
    //
    // name is an &OsStr because subsection can contain pretty much anything
    // TODO: we need to decode the value before we return, should be Cow the return type,
    pub(crate) fn get(&self, name: &OsStr) -> Option<Option<&[u8]>> {
        let key = ConfigKey::from_name(name)?;
        let line_index = self.index.get(&key)?.last()?;
        let line = &self.lines[*line_index];

        let LineKind::Variable { value, .. } = &line.kind else {
            unreachable!("config index should only point to variable lines");
        };

        match value {
            Some(span) => Some(Some(&self.buf[span.start..span.end])),
            // valueless boolean
            None => Some(None),
        }
    }

    // TODO: when config is invoked as a command, impl Printer
    pub(crate) fn get_str(&self, name: &OsStr) -> Result<Option<&str>, ConfigError> {
        match self.get(name) {
            // valueless boolean
            Some(None) => Err(ConfigError::MissingValue {
                name: name.to_os_string(),
            }),
            Some(Some(bytes)) => {
                str::from_utf8(bytes)
                    .map(Some)
                    .map_err(|_| ConfigError::NotUnicode {
                        key: name.to_os_string(),
                        value: bytes.to_vec(),
                    })
            }
            None => Ok(None),
        }
    }

    pub(super) fn set(&mut self, name: &OsStr, value: &OsStr) {
        let value = os::os_str_as_bytes(value).unwrap();
        // TODO: this part should be done by the caller.
        // let lock = Lockfile::acquire(&self.local).unwrap();
        // self.load().unwrap();
        let key = ConfigKey::from_name(name).unwrap();
        // this is idiomatic, instead of what my dumbass did, where I matched against the actual vector
        // Some(lines) if lines.len() > 1 => {}
        // Some(&lines) => ...
        match self.index.get(&key).map(|v| v.as_slice()) {
            Some(&[index]) => {
                self.replace_value(index, &value);
            }
            // By default, set will not write multi value keys
            Some([..]) => return,
            None => {
                let section = key.section;
                let line = Line::new_variable(&key.name, &value);
                if let Some(index) = self
                    .sections
                    .get(&section)
                    .and_then(|block| block.last())
                    .map(|block| block.end)
                {
                    self.lines.insert(index, line);
                } else {
                    let header = Line::new_header(&section);
                    self.lines.push(header);
                    self.lines.push(line);
                }
            }
        }
    }

    // In theory .lit/config is just a list of sections where each section has a list of variables
    // If we try to use this as our parsing rule we lose the trivia and our CST is no more lossless.
    // We have nowhere to store the trivia. Where does a blank between two variables go? A comment
    // after the last variable but before the next session header? Instead, we can model it as a flat
    // Vec<Line> where each line holds the raw bytes and is classified as SECTION, COMMENT etc.
    // CST becomes our source of truth for writing.
    //
    // This is a zero-copy approach. The name of a variable is a sub-slice of its line, which is a
    // sub-slice of the file.
    pub(crate) fn load(&mut self) -> Result<(), ConfigError> {
        let file = File::open(&self.local).map_err(|err| ConfigError::Io {
            path: self.local.clone(),
            source: err,
        })?;
        let mut reader = BufReader::new(&file);
        let mut physical_lines = 0;

        loop {
            let start = self.buf.len();
            let n = reader
                .read_until(b'\n', &mut self.buf)
                .map_err(|err| ConfigError::Io {
                    path: self.local.clone(),
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
            if self.should_fold(start) {
                while self.is_multiline() {
                    if reader
                        .read_until(b'\n', &mut self.buf)
                        .map_err(|err| ConfigError::Io {
                            path: self.local.clone(),
                            source: err,
                        })?
                        == 0
                    {
                        break;
                    }
                }
            }
            let end = self.buf.len();
            // n was 0, inner loop break, outer must too
            if start == end {
                break;
            }
            // each span represents the boundaries of each logical line which is the result of applying
            // the continuation rules in a physical line.
            let span = BufferSpan { start, end };
            let kind = self
                .classify(&span)
                // we can't pass self.lines.len() + 1 because that would result in the logical lines
                // physical lines != logical lines
                // the user sees the physical lines of the file
                .map_err(|err| ConfigError::InvalidFormat {
                    line: physical_lines,
                    source: err,
                })?;
            self.lines.push(Line {
                content: LineContent::Slice(span),
                kind,
            });
        }
        self.build_index();
        Ok(())
    }

    pub(crate) fn serialize(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.buf.len());
        for line in &self.lines {
            match &line.content {
                LineContent::Slice(span) => {
                    bytes.extend_from_slice(&self.buf[span.start..span.end])
                }
                LineContent::Owned(vec) => bytes.extend_from_slice(&vec),
            }
        }
        bytes
    }

    // after loading the file in memory and creating the Vec<Line> we create the index. As mentioned
    // above we don't have a nested structure, but a flat list of logical lines. The way we group
    // lines togther is by section, and each section is positional, the most recent header seen above
    // it, current_section keeps track of that.
    fn build_index(&mut self) {
        let mut current_section: Option<(SectionKey, usize)> = None;

        for (i, line) in self.lines.iter().enumerate() {
            match &line.kind {
                // Note: both self.sections and ConfigKey::new() need to take ownership fo SectionKey
                // we have to move it, now we either move it to sections and call clone() for ConfigKey
                // or the other way around. It is quite possible to have less headers than variables
                // so cloning on Header will happen less. We still have to test it but that is the
                // reason for the current approach
                LineKind::Header(header) => {
                    if let Some((key, start)) = current_section.as_ref() {
                        self.sections
                            .entry(key.clone())
                            .or_default()
                            .push(LineSpan {
                                start: *start,
                                end: i,
                            });
                    }
                    let key = unsafe { SectionKey::from_header_unchecked(&self.buf, header) };
                    current_section = Some((key, i));
                }
                LineKind::Variable { name, .. } => {
                    // this is the case where a variable appears before a section which is not possible
                    // in a valid config format. Parser would have caught it so unwrap is safe
                    let (section, _) = current_section.take().unwrap();
                    let key = ConfigKey::new(&self.buf, section, name);
                    self.index.entry(key).or_default().push(i);
                }
                LineKind::Blank | LineKind::Comment => {}
            }
        }

        // last section
        if let Some((key, start)) = current_section {
            self.sections.entry(key).or_default().push(LineSpan {
                start,
                end: self.lines.len(),
            });
        }
    }

    // if we wanted to return a ConfigError here we would need to pass the physical line count
    fn classify(&self, span: &BufferSpan) -> Result<LineKind, ParseError> {
        let parser = LineParser::new(&self.buf[span.start..span.end]);
        parser.parse()
    }

    fn is_multiline(&self) -> bool {
        if self.buf.last() != Some(&b'\n') {
            return false; // the last chunk does not end with a new line
        }

        let mut i = self.buf.len() - 1;
        let mut backslashes = 0;
        while i > 0 && self.buf[i - 1] == b'\\' {
            backslashes += 1;
            i -= 1;
        }
        backslashes % 2 == 1
    }

    fn should_fold(&self, start: usize) -> bool {
        let mut i = start;
        while matches!(self.buf.get(i), Some(b' ' | b'\t')) {
            i += 1;
        }
        matches!(self.buf.get(i), Some(b) if b.is_ascii_alphabetic())
    }

    // TODO: dont force the write, change this and pass the value that exists
    fn replace_value(&mut self, pos: usize, value: &[u8]) {
        let line = &mut self.lines[pos];
        let LineContent::Slice(raw) = &line.content else {
            // already Owned from an earlier edit this session
            unreachable!("replacing a freshly-loaded line");
        };
        let LineKind::Variable { value: Some(v), .. } = &line.kind else {
            // valueless boolean `flag`
            todo!("boolean -> value");
        };

        let pre = &self.buf[raw.start..v.start];
        let post = &self.buf[v.end..raw.end];
        let mut content = Vec::new();
        content.extend_from_slice(pre);
        content.extend_from_slice(&encode_value(value));
        content.extend_from_slice(post);

        self.lines[pos].content = LineContent::Owned(content);
        self.modified = true;
    }
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
        || value.iter().any(|&b| b == b'#' || b == b';');

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

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SectionKey {
    name: String,
    subsection: Option<Vec<u8>>,
}

impl SectionKey {
    // Read Config::new()
    unsafe fn from_header_unchecked(buf: &[u8], header: &Header) -> Self {
        // SAFETY: header name can contain only alphanumeric, or '-' and it is guaranteed by the parser
        let name = unsafe {
            String::from_utf8_unchecked(downcase(&buf[header.name.start..header.name.end]))
        };
        let subsection = header
            .subsection
            .as_ref()
            .map(|span| buf[span.start..span.end].to_vec());

        Self { name, subsection }
    }
}

#[derive(Debug, PartialEq, Eq, Hash)]
struct ConfigKey {
    section: SectionKey,
    name: String,
}

impl ConfigKey {
    // TODO: should this fn be unsafe?
    // This fn should only be called during build_index() where we know that the name is guaranteed
    // to be alphanumeric(plus '-') by the parser so calls like from_utf8_unchecked or downcase are
    // safe
    fn new(buf: &[u8], section: SectionKey, name: &LineSpan) -> Self {
        ConfigKey {
            section,
            // SAFETY: name can contain only alphanumeric, '-' and it is guaranteed by the parser
            name: unsafe { String::from_utf8_unchecked(downcase(&buf[name.start..name.end])) },
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
    fn from_name(name: &OsStr) -> Option<Self> {
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
            section: section,
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

#[derive(Debug)]
pub(crate) enum ConfigError {
    Io { path: PathBuf, source: io::Error },
    // TODO: display the unexpected byte value as hex, git shows bad config line 1. We can provide
    // TODO: a message with more information such as the actual reason and the offset within the line
    InvalidFormat { line: usize, source: ParseError },
    MissingValue { name: OsString },
    NotUnicode { key: OsString, value: Vec<u8> },
}
