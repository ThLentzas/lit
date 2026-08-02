use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use crate::repo::config::parse::{LineKind, LineParser, ParseError};
use crate::repo::os;

pub(super) mod parse;

#[derive(Debug, PartialEq, Eq)]
struct Span {
    start: usize,
    end: usize, // exclusive
}

struct Line {
    raw: Span,
    kind: LineKind,
}

// toDo: global, system, Read Chapter 25.2.3 and 25.3
pub(crate) struct Config {
    local: PathBuf,
    // the buffer we read into
    buf: Vec<u8>,
    lines: Vec<Line>,
    // Initially thought about an IndexMap to preserve the order, but it is already there in Vec<Line>
    index: HashMap<ConfigKey, Vec<usize>>,
}

impl Config {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            local: path,
            buf: Vec::new(),
            lines: Vec::new(),
            index: HashMap::new(),
        }
    }

    // toDo: for now we return a slice which is tied to self which is fine for the case where we
    // retrieve the use info since it will be stored to commit and commit owns the data. In the future,
    // when we make this method returns values based on the --config we need to decode the actual
    // variable value and return owned data.
    // key = "a" b "c" -> should map to a b c
    //
    // name is an &OsStr because subsection can contain pretty much anything
    // TODO: maybe we need to apply some stricter rules for what is allowed in subsection
    pub(crate) fn get(&self, name: &OsStr) -> Option<&[u8]> {
        let key = ConfigKey::from_name(name)?;
        let line_index = self.index.get(&key)?.last()?;
        let line = &self.lines[*line_index];

        let LineKind::Variable { value, .. } = &line.kind else {
            unreachable!("config index should only point to variable lines");
        };

        match value {
            Some(span) => Some(&self.buf[span.start..span.end]),
            // valueless boolean defaults to true
            None => Some(b"true"),
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
            let span = Span { start, end };
            let kind = self.classify(&span)
                // we can't pass self.lines.len() + 1 because that would result in the logical lines
                // physical lines != logical lines
                // the user sees the physical lines of the file
                .map_err(|err| ConfigError::InvalidFormat {
                    line: physical_lines,
                    source: err,
                })?
                .offset(span.start);
            self.lines.push(Line { raw: span, kind });
        }
        self.build_index();
        Ok(())
    }

    // after loading the file in memory and creating the Vec<Line> we create the index. As mentioned
    // above we don't have a nested structure, but a flat list of logical lines. The way we group
    // lines togther is by section, and each section is positional, the most recent header seen above
    // it, current_section keeps track of that.
    fn build_index(&mut self) {
        let mut current_section: Option<(&Span, &Option<Span>)> = None;

        for (i, line) in self.lines.iter().enumerate() {
            match &line.kind {
                LineKind::Section { name, subsection } => {
                    current_section = Some((name, subsection));
                }
                LineKind::Variable { name, .. } => {
                    // this is the case where a variable appears before a section which is not possible
                    // in a valid config format. Parser would have caught it so unwrap is safe
                    let current_section = current_section.unwrap();
                    let key = ConfigKey::new(&self.buf, current_section.0, current_section.1, name);
                    self.index.entry(key).or_default().push(i);
                }
                LineKind::Blank | LineKind::Comment => {}
            }
        }
    }

    // if we wanted to return a ConfigError here we would need to pass the physical line count
    fn classify(&self, span: &Span) -> Result<LineKind, ParseError> {
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
}

#[derive(Debug, PartialEq, Eq, Hash)]
struct ConfigKey {
    section: String,
    subsection: Option<Vec<u8>>,
    name: String,
}

impl ConfigKey {
    fn new(buf: &[u8], section: &Span, subsection: &Option<Span>, name: &Span) -> Self {
        ConfigKey {
            // SAFETY: section can contain only alphanumeric, '-' or '.' and it is guaranteed by
            // the parser
            section: unsafe {
                String::from_utf8_unchecked(downcase(&buf[section.start..section.end]))
            },
            subsection: subsection.as_ref().map(|s| buf[s.start..s.end].to_vec()),
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
    // up to the first dot, the rest is the subsection. If there is only one '.', it's always section-name
    fn from_name(name: &OsStr) -> Option<Self> {
        let bytes = os::name_as_bytes(name).unwrap();
        // rposition finds '.' starting from the back
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
                },
                None => (header, None),
            };

        if !name[0].is_ascii_alphabetic() {
            return None;
        }

        Some(ConfigKey {
            section: unsafe { String::from_utf8_unchecked(try_downcase(section)?) },
            subsection: subsection.map(|s| s.to_vec()),
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
    // toDo: display the unexpected byte value as hex, git shows bad config line 1. We can provide
    // toDo: a message with more information such as the actual reason and the offset within the line
    InvalidFormat { line: usize, source: ParseError}
}