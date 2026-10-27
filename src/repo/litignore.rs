use crate::repo::os::{IoError, IoErrorContext, OsPath};
use crate::repo::repo_path::RepoPath;
use std::fs::File;
use std::io::{BufRead, BufReader, ErrorKind};
use std::ops::Range;

// used by add, status, clean, and check-ignore
const LITIGNORE: &str = ".litignore";

// Note: we never handle any escape sequences when parsing, will do when we compare
pub(crate) struct Pattern {
    range: Range<usize>,
    negation: bool,
    match_only_dirs: bool,
    relative: bool,
}

pub(crate) struct IgnoreFile {
    path: RepoPath,
    buf: Vec<u8>,
    patterns: Vec<Pattern>,
}

fn parse(path: &OsPath) -> Result<Option<IgnoreFile>, IoError> {
    let mut buf = Vec::new();
    let mut patterns = Vec::new();
    let litignore_path = path.join_unchecked(LITIGNORE);
    let file = match File::open(&litignore_path) {
        Ok(file) => file,
        // not found at this level
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(IoError::new("open", Some(path), err)),
    };
    let mut reader = BufReader::new(&file);

    loop {
        let start = buf.len();
        let n = reader
            .read_until(b'\n', &mut buf)
            .with_context("read", Some(&litignore_path))?;
        if n == 0 {
            // EOF
            break;
        }
        let line = &buf[start..];
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        if let Some(mut pattern) = parse_pattern(line) {
            // shift the range from slice relative to buffer relative
            pattern.range.start += start;
            pattern.range.end += start;
            patterns.push(pattern);
        };
    }

    let file = IgnoreFile {
        path: RepoPath::from_bytes_unchecked(litignore_path.into_bytes()),
        buf,
        patterns,
    };

    Ok(Some(file))
}

// TODO: glob support
fn parse_pattern(buf: &[u8]) -> Option<Pattern> {
    let mut negation = false;
    let mut relative = false;
    let mut start = 0;
    let end = skip_trailing_spaces(buf);
    let buf = &buf[start..end];

    match buf[start] {
        // `#` is considered as a comment only at the start
        // leading spaces followed by `#` form a pattern
        b'#' => return None,
        // `!`, `/`, or any of the two with trailing spaces are considered as: no pattern was provided
        b'!' | b'/' if start + 1 == end => return None,
        b'!' => {
            match buf.get(start + 1) {
                // `!/`
                Some(b'\\') if start + 2 == end => return None,
                // can't be none we already checked
                _ => {
                    negation = true;
                    start += 1;
                }
            }
        }
        b'/' => {
            relative = true;
            start += 1;
        }
        _ => {}
    }

    let mut j = start;
    while j < end {
        // somewhere in the middle but not at the end
        if buf[j] == b'/' && j + 1 != end {
            relative = true;
        }
        j += 1;
    }

    let mut backslashes = 0;
    while j > 0 && buf[j - 1] == b'\\' {
        j -= 1;
        backslashes += 1;
    }
    // unpaired trailing '\' is an invalid pattern
    if backslashes % 2 == 1 {
        return None;
    }

    let range = start..end;
    let match_only_dirs = buf[end - 1] == b'/';

    Some(Pattern {
        range,
        negation,
        match_only_dirs,
        relative,
    })
}

// the returning index is exclusive
// TODO: max pattern len, exclude file len, do we need to record the line on the file of the pattern
fn skip_trailing_spaces(buf: &[u8]) -> usize {
    let mut end = buf.len();
    // Note: this is the correct way to scan from the end of the buffer. Setting `end = buf.len() - 1`
    // and checking `end >= 0 && buf[end] == b' ' will make a `usize` negative if the check at `0`
    // returns true.
    while end > 0 && buf[end - 1] == b' ' {
        end -= 1;
    }
    if end == buf.len() {
        return end; // no spaces were removed
    }

    // at this point `end` points to the last space we encounter
    // if that space is escaped we must include it
    let mut backslashes = 0;
    let mut j = end;
    while j > 0 && buf[j - 1] == b'\\' {
        j -= 1;
        backslashes += 1;
    }
    // the range for `ab\\ ` and for `ab\ `  is [0, 4). In the first case, we have a trailing space, 
    // in the other we have an escaped one.
    if backslashes % 2 == 0 { end } else { end + 1 }
}
