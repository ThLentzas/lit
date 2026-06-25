use crate::cmd::config::parse::{LineParser, ParseError};
use crate::cmd::error::ConfigError;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

pub(super) mod parse;

mod utf8;

struct Span {
    start: usize,
    end: usize, // exclusive
}

struct Config {
    path: PathBuf,
    // the buffer we read into
    buf: Vec<u8>,
    lines: Vec<Line>,
}

enum LineKind {
    Blank,
    Comment,
    Section { name: Span, subsection: Option<Span>, },
    Variable { name: Span, value: Option<Span>, },
}

impl LineKind {
    fn offset(self, base: usize) -> LineKind {
        let shift = |s: Span| Span { start: s.start + base, end: s.end + base };
        match self {
            LineKind::Section { name, subsection } => {
                LineKind::Section { name: shift(name), subsection: subsection.map(shift) }
            },
            LineKind::Variable { name, value } => {
                LineKind::Variable { name: shift(name), value: value.map(shift) }
            },
            // Blank / Comment
            other => other,
        }
    }
}

struct Line {
    raw: Span,
    kind: LineKind,
}

impl Config {
    // In theory .lit/config is just a list of sections where each section has a list of variables
    // If we try to use this as our parsing rule we lose the trivia and our CST is no more lossless.
    // We have nowhere to store the trivia. Where does a blank between two variables go? A comment
    // after the last variable but before the next session header? Instead, we can model it as a flat
    // Vec<Line> where each line holds the raw bytes and is classified as SECTION, COMMENT etc.
    // CST becomes our source of truth for writing.
    //
    // This is a zero-copy approach. The name of a variable is a sub-slice of its line, which is a
    // sub-slice of the file.
    fn load(&mut self) -> Result<(), ConfigError> {
        let file = File::open(&self.path).unwrap();
        let mut reader = BufReader::new(&file);
        let mut physical_lines = 0;

        loop {
            let start = self.buf.len();
            loop {
                let n = reader.read_until(b'\n', &mut self.buf).unwrap();
                physical_lines += 1;
                if n == 0 { // EOF
                    break;
                }
                // From the different kinds of line we can have only 1 is allowed to be multiline,
                // the variable, and the \<newline> continuation rule is only applied to the value
                // part of the variable. We can't just join the next physical line before we know
                // what kind of line it is. The solution is to scan the part of the buffer we just
                // read and only continue if the line is actually a variable. Variables starts with
                // name and name's 1st character must be alphabetic. If we didn't scan we could end
                // in a case where we have a comment that spans into multiple lines which is not allowed
                if self.first_line_is_variable(start) {
                    while self.is_multiline() {
                        if reader.read_until(b'\n', &mut self.buf).unwrap() == 0 {
                            break;
                        }
                    }
                }
            }
            let end = self.buf.len();
            if start == end {
                break;
            }
            // each span represents the boundaries of each logical line which is the result of applying
            // the continuation rules in a physical line.
            let span = Span { start, end };
            let line = self.classify(span)
                // we can't pass self.lines.len() + 1 because that would result in the logical lines
                // physical lines != logical lines
                // the user sees the physical lines of the file
                .map_err(|err| ConfigError::InvalidFormat {
                    line: physical_lines, source: err
                })?;
            self.lines.push(line);
        }
        Ok(())
    }

    // if we wanted to return a ConfigError here we would need to pass the physical line count
    fn classify(&self, span: Span) -> Result<Line, ParseError> {
        let parser = LineParser::new(&self.buf, span);
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
    fn first_line_is_variable(&self, start: usize) -> bool {
        let mut i = start;
        while matches!(self.buf.get(i), Some(b' ' | b'\t')) {
            i += 1;
        }
        matches!(self.buf.get(i), Some(b) if b.is_ascii_alphabetic())
    }
}
