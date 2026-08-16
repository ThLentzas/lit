use crate::repo::config::parse::{Header, LineKind, LineParser, ParseError, Variable};
use crate::repo::os;
use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::iter::{self, Chain, Once};
use std::path::{Path, PathBuf};
use std::slice::Iter;
use std::vec::IntoIter;

// As of now there are two times that we try to index into lines using a VariablePos. By construction
// VariablePos is an index that points to a Variable in lines, but Variable is a kind of Line which
// forces 2 unreachable!() calls, in value_at() and replace_value(). We know that the VariablePos
// always points to a Variable but the compiler does not. A solution is to use a sparse index where
// sparse_index.len() = lines.len() and its entries will be only at indices where line[i] = Variable
// but things will get too complicated on how to represent the empty state. Vec<Option<Variable>>
// will still run into the issue where even if we know the index we still have to check for Some/None
// or just call unwrap().
//
// VariablePos can't be constructed outside this module. The only instances of such type are created
// during the index construction, `DocIndex::new()`. Based on that, we can safely index in
// lines and not call get(). When passed as argument, it prevents the caller from passing a random
// potentially out of range usize that could cause problems.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct VariablePos(usize);

impl VariablePos {
    pub(super) fn get(&self) -> usize {
        self.0
    }
}

// we need this struct to hold the invariant: when we insert a key to index, we also insert the
// index of the line we encountered it. If we use a Vec<usize> the following problem occurs when set
// is called.
//         match self.file.key_positions(&key) {
//             Some(&[index]) => Ok(self.file.replace_value(index, &value)),
//             Some([_, ..]) => Err(ConfigError::MultipleValues(name.to_os_string())),
//             Some(&[]) => unreachable!("key positions should be nonempty or None"),
//             None => Ok(self.file.insert_variable(&key, &value)),
//         }
// We have this unreachable case, that internally we know it is not possible but the compiler does not
pub(super) struct NonEmpty<T>
where
    T: Copy,
{
    head: T,
    rest: Vec<T>,
}

impl<T> NonEmpty<T>
where
    T: Copy,
{
    fn new(head: T) -> Self {
        Self {
            head,
            rest: Vec::new(),
        }
    }

    fn push(&mut self, val: T) {
        self.rest.push(val);
    }

    pub(super) fn first(&self) -> T {
        self.head
    }
    pub(super) fn len(&self) -> usize {
        1 + self.rest.len()
    }

    fn last(&self) -> T {
        self.rest.last().copied().unwrap_or(self.first())
    }

    pub(super) fn single(&self) -> bool {
        self.rest.is_empty()
    }
}

// If we wrote our own type then IntoIter would look like this:
//
// struct NonEmptyIter<T> {
//  // store `head` until it is returned by the first next() call
//  first: Option<T>
//  // iterate over the remaining
//  rest: IntoIter<T>
// }
//
// impl<T> IntoIterator for NonEmpty<T>
//  type Item = T
//  type IntoIter = NonEmptyIter<T>
//
//  fn into_iter(self) -> Self::IntoIter {
//      NonEmptyIter {
//          first: Some(self.head)
//          rest: self.rest.into_iter()
//      }
//  }
//
// impl<T> Iterator for NonEmptyIter<T> {
//  type Item = T
//
//  fn next(&mut self) -> Option<T> {
//      // returns first once and then delegates to the vector iterator
//      self.first.take().or_else(|| self.rest.next())
//  }
// }
//
// We can avoid all that by using a type from std called Chain.
// once(self.head) creates an iterator Once<T> whose next() returns T once and then None
// chain(..) creates a Chain<Once<T>, Vec::IntoIter<T>> whose next() reads from Once<T> until
// exhausted then reads from the vector iterator.
impl<T> IntoIterator for NonEmpty<T>
where
    T: Copy,
{
    type Item = T;
    type IntoIter = Chain<Once<T>, IntoIter<T>>;

    fn into_iter(self) -> Self::IntoIter {
        iter::once(self.head).chain(self.rest.into_iter())
    }
}

impl<'a, T> IntoIterator for &'a NonEmpty<T>
where
    T: Copy,
{
    type Item = &'a T;
    // https://doc.rust-lang.org/beta/std/iter/struct.Chain.html
    // https://doc.rust-lang.org/std/iter/fn.once.html
    type IntoIter = Chain<Once<&'a T>, Iter<'a, T>>;

    fn into_iter(self) -> Self::IntoIter {
        iter::once(&self.head).chain(self.rest.iter())
    }
}

// holds the trivia
// it is created when we parse a line and holds the span of the line relative to the buffer
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BufferSpan {
    start: usize,
    end: usize,
}

// Line relative spans used by Header for section, subesction and Variable for name and value
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct LineSpan {
    pub(super) start: usize,
    pub(super) end: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SectionBlock {
    start: usize,
    end: usize,
}

// we create Lines in two scenarios:
//  - parsed lines referring their original bytes in the input buffer
//  - lines created by `set()` own their bytes
enum LineContent {
    Slice(BufferSpan),
    Owned(Vec<u8>),
}

struct Line {
    content: LineContent,
    kind: LineKind,
}

impl Line {
    // it can also be just variable()
    // canonical tells that the generated variable is in Git's canonical representation
    fn canonical_variable(name: &str, value: &[u8]) -> Self {
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
            kind: LineKind::Variable(Variable {
                name: LineSpan {
                    start: name_start,
                    end: name_end,
                },
                value: Some(LineSpan {
                    start: value_start,
                    end: value_end,
                }),
            }),
        }
    }

    // it can also be just header()
    // canonical tells that the generated header is in Git's canonical representation
    fn canonical_header(key: &SectionKey) -> Self {
        let mut line = Vec::new();
        let name_start = 1;
        let name_end = name_start + key.name.len();

        line.push(b'[');
        line.extend_from_slice(key.name.as_bytes());
        let subsection = key.subsection.as_ref().map(|sub| {
            let start = name_end + 2;
            line.extend_from_slice(b" \"");
            line.extend_from_slice(sub);
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

    // returns the bytes of the line in the buffer
    //
    // Bad Design: Initially I would pass around the buffer and the span and let the callee create
    // the slice which could cause problems with indexing. Now there is only source of truth,
    // line.slice() needs the buffer and the span of the thing that lives in the line
    // This was the old constructor of ConfigKey::new()
    //      fn new(buf: &[u8], section: &Span, subsection: &Option<Span>, name: &Span) -> Self
    // Now types are created and slices have been resolved by the caller
    //      fn new(section: SectionKey, name: &[u8])
    fn bytes<'a>(&'a self, buf: &'a [u8]) -> &'a [u8] {
        match &self.content {
            LineContent::Slice(span) => &buf[span.start..span.end],
            LineContent::Owned(bytes) => bytes,
        }
    }

    // a slice within the line, it is used to extract the values of name or value in a case of a
    // variable, or section/subsection for Header
    fn slice<'a>(&'a self, buf: &'a [u8], span: &LineSpan) -> &'a [u8] {
        &self.bytes(buf)[span.start..span.end]
    }

    fn variable(&self) -> Option<&Variable> {
        match &self.kind {
            LineKind::Variable(variable) => Some(variable),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SectionKey {
    name: String,
    subsection: Option<Vec<u8>>,
}

impl SectionKey {
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
    // should only be called when we build `DocIndex` where we know that the name is guaranteed
    // to be alphanumeric(plus '-') by the parser so calls like from_utf8_unchecked or downcase are
    // safe
    // the safe version is from_name() where it breaks down the provided name to section.subsection.
    // key and does the validation
    unsafe fn new_unchecked(section: SectionKey, name: &[u8]) -> Self {
        ConfigKey {
            section,
            // SAFETY: name can contain only alphanumeric, '-' and it is guaranteed by the parser
            name: unsafe { String::from_utf8_unchecked(downcase(name)) },
        }
    }

    // Git’s flat config key is ambiguous because: section.subsection.variable uses '.'as a separator,
    // but section names themselves may also contain '.' The docs say the fully qualified variable name
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
            // SAFETY: `try_downcase` returns `Some` only when `name` contains alphanumeric, or '-'
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
    // valueless
    // not just boolean, because boolean can also mean false
    ImplicitlyTrue,
    Bytes(Cow<'a, [u8]>),
}

struct DocIndex {
    // ConfigKey refers to a variable of a section.
    // foo.bar maps to foo = section, subsection = None, bar = key. Multiple keys can exist in the
    // config file, and we have to keep track of all them. If we made the value as Vec<VariablePos>
    // that means whenever we call get and match on the returned value we would have to match against
    // Some(&[]), an empty Vec, which is not possible by construction. If a ConfigKey is found
    // it is defined to at least one line which makes an empty Vector an invalid state.
    keys: HashMap<ConfigKey, NonEmpty<VariablePos>>,
    // sections are used as a complement to keys
    // whenever we want to set a value for a key and the exact key is missing we don't have any
    // information if the section is also absent. If foo.bar is None, it can mean foo exists but
    // does not have a key named bar or that foo does not exist at all. This distinction is important
    // because we have to know if we should append on an existing section or create and insert.
    // we want the same invariant to hold, if a section exists it spans to at least 1 block.
    // sections without variables are allowed. In that case, the block is just the header's line.
    // Blocks are stored in file order. When inserting a new variable, we use the last block for the
    // matching section.
    sections: HashMap<SectionKey, NonEmpty<SectionBlock>>,
}

impl DocIndex {
    // after loading the file in memory and creating the Vec<Line> we create the index. The way we
    // group lines togther is by section, and each section is positional, the most recent header
    // seen above it, current_section keeps track of that. This includes in the current section's
    // block the blank lines.
    fn new(buf: &[u8], lines: &[Line]) -> Self {
        let mut keys: HashMap<ConfigKey, NonEmpty<VariablePos>> = HashMap::new();
        let mut sections: HashMap<SectionKey, NonEmpty<SectionBlock>> = HashMap::new();
        let mut current_section: Option<(SectionKey, usize)> = None;

        for (i, line) in lines.iter().enumerate() {
            match &line.kind {
                LineKind::Header(header) => {
                    // This was a suggestion by clippy. The code below is problematic because the
                    // purpose of map is to go from Option<A> to Option<B> but we return Option<()>
                    // which ignore/never bind. Calling let res = ... would have not triggerred that
                    // warning.
                    //
                    // current_section.take().map(|(key, start)| {
                    //     sections
                    //         .entry(key)
                    //         .and_modify(|blocks| blocks.push(SectionBlock { start, end: i }))
                    //         .or_insert_with(|| NonEmpty::new(SectionBlock { start, end: i }));
                    // });
                    if let Some((key, start)) = current_section.take() {
                        sections
                            .entry(key)
                            .and_modify(|blocks| blocks.push(SectionBlock { start, end: i }))
                            .or_insert_with(|| NonEmpty::new(SectionBlock { start, end: i }));
                    };
                    let name = line.slice(buf, &header.name);
                    let subsection = header.subsection.as_ref().map(|span| line.slice(buf, span));
                    // SAFETY: the name of the section was created by the parser which guaranteed
                    // that it contains only alphanumeric, or '-'
                    let section = unsafe { SectionKey::new_unchecked(name, subsection) };
                    current_section = Some((section, i));
                }
                LineKind::Variable(variable) => {
                    // this is the case where a variable appears before a section which is not possible
                    // in a valid config format. Parser would have caught it so unwrap is safe
                    let (section, _) = current_section.as_ref().unwrap();
                    let name = line.slice(buf, &variable.name);
                    // SAFETY: both section and name were created by the parser which would have
                    // rejected any invalid sequence
                    let key = unsafe { ConfigKey::new_unchecked(section.clone(), name) };
                    keys.entry(key)
                        .and_modify(|positions| positions.push(VariablePos(i)))
                        .or_insert_with(|| NonEmpty::new(VariablePos(i)));
                }
                LineKind::Blank | LineKind::Comment => {}
            }
        }

        // last section
        if let Some((key, start)) = current_section {
            sections
                .entry(key)
                .and_modify(|blocks| {
                    blocks.push(SectionBlock {
                        start,
                        end: lines.len(),
                    })
                })
                .or_insert_with(|| {
                    NonEmpty::new(SectionBlock {
                        start,
                        end: lines.len(),
                    })
                });
        }

        Self { keys, sections }
    }

    // returns the index of the last line that defining `key`
    fn key_last_pos(&self, key: &ConfigKey) -> Option<VariablePos> {
        self.key_positions(key).map(|pos| pos.last())
    }

    fn key_positions(&self, key: &ConfigKey) -> Option<&NonEmpty<VariablePos>> {
        self.keys.get(key)
    }

    // returns the last section block which determines where a new variable should be inserted
    fn last_block(&self, section: &SectionKey) -> Option<SectionBlock> {
        self.sections.get(section).map(|block| block.last())
    }
}

// CST + lookup
pub(super) struct ConfigDoc {
    buf: Vec<u8>,
    lines: Vec<Line>,
    index: DocIndex,
}

impl ConfigDoc {
    // .lit/config is just a list of sections where each section has a list of variables
    // If we try to use this as our parsing rule we lose the trivia and our CST is no more lossless.
    // We have nowhere to store the trivia. Where does a blank between two variables go? A comment
    // after the last variable but before the next session header? Instead, we can model it as a flat
    // Vec<Line> where each line holds the raw bytes and is classified as SECTION, COMMENT etc.
    // CST becomes our source of truth for writing. Read `DocIndex::new()`
    //
    // This is a zero-copy approach. The name of a variable is a sub-slice of its line, which is a
    // sub-slice of the file.
    pub(crate) fn load(path: &Path) -> Result<Self, ConfigDocError> {
        let mut buf = Vec::new();
        let lines = read_lines(&mut buf, path)?;
        let index = DocIndex::new(&buf, &lines);

        Ok(Self { buf, lines, index })
    }

    pub(crate) fn serialize(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.buf.len());

        for line in &self.lines {
            bytes.extend_from_slice(line.bytes(&self.buf));
        }

        bytes
    }

    pub(super) fn key_last_pos(&self, key: &ConfigKey) -> Option<VariablePos> {
        self.index.key_last_pos(key)
    }

    pub(super) fn value_at(&self, pos: VariablePos) -> Value<'_> {
        let LineKind::Variable(variable) = &self.lines[pos.0].kind else {
            unreachable!("position only point at variable lines");
        };
        match variable.value {
            // valueless boolean, always true
            None => Value::ImplicitlyTrue,
            Some(span) => Value::Bytes(interpret_value(self.lines[pos.0].slice(&self.buf, &span))),
        }
    }

    // returns the indices of all lines that define `key`
    pub(super) fn key_positions(&self, key: &ConfigKey) -> Option<&NonEmpty<VariablePos>> {
        self.index.key_positions(key)
    }

    // this method is called when the look-up for config key returned None
    // the exact key provided by the user does not exist, and we have to check if the section of the
    // key exists, if so we append, otherwise, we create the section and insert the variable
    //
    // if foo.bar.baz does not exist, we shouldn't naively try to create foo.bar header and then insert
    // we first check if foo.bar exists, we append, otherwise we create the whole section
    pub(super) fn insert_variable(&mut self, key: &ConfigKey, value: &[u8]) {
        let section = &key.section;
        let variable = Line::canonical_variable(&key.name, value);

        // if the header exists and has multiple blocks the new variable is added always to the last
        // one
        match self.index.last_block(section) {
            // the last line can either belong to the same section we want to write our new line, or
            // if the section is missing then it is the last line of the previous section
            Some(block) => {
                let at = block.end;
                self.ensure_newline(at - 1);
                self.lines.insert(at, variable);
            }
            None => {
                if let Some(last) = self.lines.len().checked_sub(1) {
                    self.ensure_newline(last);
                }
                let header = Line::canonical_header(section);
                self.lines.push(header);
                self.lines.push(variable);
            }
        }
    }

    pub(super) fn replace_value(&mut self, pos: VariablePos, value: &[u8]) {
        let line = &mut self.lines[pos.0];
        let LineContent::Slice(_) = &line.content else {
            // already Owned from an earlier edit this session
            unreachable!("replacing a freshly-loaded line");
        };

        // Git always writes a new variable line in the form of \t<name> = <encoded_value>\n
        // it does not matter if the variable is valueless
        // it discards all the trivia of the old line
        // it forces the write even if new_value = old_value
        let variable = line.variable().unwrap();
        let name = line.slice(&self.buf, &variable.name);
        let mut content = Vec::with_capacity(name.len() + value.len() + 5);
        content.push(b'\t');
        content.extend_from_slice(name);
        content.extend_from_slice(b" = ");
        content.extend_from_slice(&encode_value(value));
        content.push(b'\n');

        self.lines[pos.0].content = LineContent::Owned(content);
    }

    // when we want to insert the new line we have 1 edge case to consider
    // the last line of the file might not be '\n' terminated, our writer does add '\n' even
    // for the last line but the file could have been changed
    //
    // if the '\n' is missing, and we try to write our new line it fuses onto the prior last line
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

fn read_lines(buf: &mut Vec<u8>, path: &Path) -> Result<Vec<Line>, ConfigDocError> {
    let file = File::open(path).map_err(|err| ConfigDocError::Io {
        path: path.to_path_buf(),
        source: err,
    })?;
    let mut lines = Vec::new();
    let mut reader = BufReader::new(&file);
    let mut physical_lines = 0;

    loop {
        let start = buf.len();
        let n = reader
            .read_until(b'\n', buf)
            .map_err(|err| ConfigDocError::Io {
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
        if should_fold(buf, start) {
            while ends_with_continuation(buf) {
                if reader
                    .read_until(b'\n', buf)
                    .map_err(|err| ConfigDocError::Io {
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
        let kind = classify(buf, &span)
            // we can't pass self.lines.len() + 1 because that would result in the logical lines
            // physical lines != logical lines
            // the user sees the physical lines of the file
            .map_err(|err| ConfigDocError::InvalidFormat {
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
        return false; // the last chunk might not end with a new line
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

// interpret_value() converts config-file syntax into the actual logical value
// values like: a "b" c maps to a b c
// hello\nworld contains the config-file bytes h e l l o \ n w o r l d maps to the logical value
// h e l l o LF w o r l d
// it also drops continuation lines
// read encode_value()!!!
fn interpret_value(value: &[u8]) -> Cow<'_, [u8]> {
    // fast path if nothing needs handling we return the value verbatim
    let pos = match memchr::memchr2(b'"', b'\\', value) {
        Some(pos) => pos,
        None => return Cow::Borrowed(value),
    };

    let mut bytes = Vec::with_capacity(value.len());
    bytes.extend_from_slice(&value[..pos]);
    let mut i = pos;
    // we use the same batch write logic we did in jolt for strings
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
                    b'\r' if matches!(value.get(i + 1), Some(&b'\n')) => i += 1,
                    _ => unreachable!(
                        "interpret_value() called with a bad escape sequence, can only occur if there is a bug in the parsing logic"
                    ),
                }
                i += 1;
            }
            _ => {
                let start = i;
                while i < value.len() && matches!(value[i], b'"' | b'\\') {
                    i += 1;
                }
                bytes.extend_from_slice(&value[start..i]);
            }
        }
    }
    Cow::Owned(bytes)
}

// the logic of decoding the value lives in parse.rs::parse_value(). encode_value() takes a buffer
// that represents the value, but parse_value() takes a buffer that contains the value and all the
// trivia until the end of the line. parse_value() stops when it encounters LF, CRLF or any comment.
// All encode_value() does is mapping, it never has to consider comments or new lines. It knows
// everything within the buffer is the value. It needs to do special handling if certain characters
// are present.
//
// hello # world as value can't be passed verbatim, because the parser will see '#' and treat is as
// a comment, but # world is part of the value, we need to quote in such cases. This is true for
// leading/trailing ws. Read set() for how terminals pass values, very important.
//
// The key principle of the encode_value() is that whatever bytes are passed, these are the bytes
// the user wants as a value verbatim, untouched. What confused me is since whenever we encounter
// '\\' we always map it to '\\\\' how are we goning to handle escape sequences and continuation lines
// without looking ahead. The answer is we don't, because the continuation property is not part of the
// value, is part of the config's format. When the user provides for h e l l o \ q, these are
// the bytes of the value, all encode does is to make them follow the syntax rules, it does not do
// any validation it does not see '\' and says it must be followed by some known escape sequence.
// The encoder emits h e l l o \ \ q and then during retrival \\ will be mapped to \ and get back
// the original value. The same reasoning applies to the continuation rule. abcd\\ref -> a b c d \ \r e f
// it is backslash literal followed by CR, this is not a bad continuation line, it is an escape backslash
// followed by CR as raw byte. The encoder's job is to make sure that some arbitary payload cannot
// accidentally turn into malformed syntax. This is why we call encode_value() in set().
// This is the same logic with the quotes that are dropped during interpret value, they are information
// about the syntax not the actual value. "a" b "c" maps to a b c
// foo(\<newline>)
// bar
// maps to foobar. Very important to understand that those rules are information about the syntax and
// not the actual value.
//
// !!! The invariant that must hold true at all times: interpret_value(encode_value(value)) == value
// It means if we take any logical value, serialize into config syntax and then paser/decode it again
// we must get back the exact same logical bytes(the bytes the user/program provided). TODO: test this
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
pub(crate) enum ConfigDocError {
    Io { path: PathBuf, source: io::Error },
    // TODO: display the unexpected byte value as hex, git shows bad config line 1. We can provide
    // TODO: a message with more information such as the actual reason and the offset within the line
    InvalidFormat { line: usize, source: ParseError },
}
