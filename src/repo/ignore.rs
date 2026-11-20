use crate::repo::os::{IoError, IoErrorContext};
use crate::repo::repopath::RepoPath;
use crate::repo::workspace::Workspace;
use std::io::{BufRead, BufReader};
use std::ops::Range;

/*
 * TODO: we need to add support for reading patterns from `$GIT_COMMON_DIR/info/exclude` and a file
 * specified `core.excludesFile`
 */
// used by add, status, clean, and check-ignore
const LITIGNORE: &str = ".litignore";

// Note: we never handle any escape sequences when parsing, will do when we match
struct Pattern {
    range: Range<usize>,
    negation: bool,
    match_only_dirs: bool,
    relative: bool,
}

impl Pattern {
    // `debug.log` -> Match that exact name at any depth under `src/`(src/debug.log, src/net/debug.log)
    // `/debug.log` -> Match that name directly inside the directory containing `.litignore`(src/debug.log only)
    // `net/debug.log` -> A slash in the middle makes the whole path relative to `src/`. (src/net/debug.log only)
    // `cache/` -> A trailing slash restricts the match to directories. The name can still occur at any depth. (src/cache/, src/net/cache/)
    // `/cache/` -> Combine both rules. Match the `cache` directory directly under `src/`(src/cache/ only)
    fn matches(&self, buf: &[u8], candidate: &[u8], is_dir: bool) -> bool {
        if self.match_only_dirs && !is_dir {
            return false;
        }
        let pattern = &buf[self.range.clone()];
        // when the pattern is not relative, the path can match at any depth
        //  pattern: `debug.log`
        //  candidate: `src/net/debug.log`
        // we want to match pattern with the final component of the candidate, this is what at every
        // depth means.
        let candidate = if self.relative {
            candidate
        } else {
            candidate.rsplit(|&b| b == b'/').next().unwrap()
        };
        let mut pattern = pattern.iter();
        let mut candidate = candidate.iter();

        while let Some(mut expected) = pattern.next() {
            if *expected == b'\\' {
                let Some(escaped) = pattern.next() else {
                    return false;
                };
                expected = escaped;
            }
            if candidate.next() != Some(expected) {
                return false;
            }
        }
        candidate.next().is_none()
    }
}

pub(crate) struct IgnoreFile {
    // the directory containing the ignore file
    base_dir: RepoPath,
    buf: Vec<u8>,
    patterns: Vec<Pattern>,
}

impl IgnoreFile {
    pub(crate) fn load(
        workspace: &Workspace,
        directory: &RepoPath,
    ) -> Result<Option<Self>, IoError> {
        let litignore_path = directory.join_unchecked(LITIGNORE);
        let file = match workspace.open_file(&litignore_path) {
            Ok(file) => file,
            // not found at this level
            Err(err) if err.is_not_found() => return Ok(None),
            Err(err) => return Err(err),
        };

        let mut reader = BufReader::new(&file);
        let mut buf = Vec::new();
        let mut patterns = Vec::new();

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
            base_dir: directory.clone(),
            buf,
            patterns,
        };

        Ok(Some(file))
    }

    pub(crate) fn decision(&self, candidate: &[u8], is_dir: bool) -> Option<bool> {
        // we iterate backwards because last match wins for the same candidate
        //  `debug.log`
        //  `!debug.log`
        // returns Some(false)
        //
        // if we iterate forwards we would need to continue for every pattern and replace the
        // decision whenever another rule matched
        for pattern in self.patterns.iter().rev() {
            if pattern.matches(&self.buf, candidate, is_dir) {
                // if we have a match and negation is true, don't include it
                return Some(!pattern.negation);
            }
        }
        // no matches
        None
    }

    pub(crate) fn base_dir(&self) -> &RepoPath {
        &self.base_dir
    }
}

// we walk the path starting from root and we stop when we either reach the final component or an
// ancestor is ignored. An ignored ancestor means the target's untracked subtree is excluded. For
// example, path: `src/generated/result.txt`. Root's litignore does not exclude `src`, so we check
// for a `.litignore` file in src but let's assume we don't find it. Check the next ancestor
// `src/generated`, our stack only has root's litignore file and we have a match because it has an
// entry `/src/generated`, we stop and return `None`. Even if `src/generated/.litignore` existed with
// a rule `!results.txt`, we never check it, so we never include the actual path which is the expected
// behavior.
//  `None`: path is exlcuded from an ancestor's ignore file
//  `Empty Vec`: means no `.litignore` files found
//
// Note: we walk the parent directories from root and push ignore files as we encounter them.
// `is_ignored()` and `IgnoreFile::decision()` also walk the stack of ignore files but in reverse
// order. This method has not yet established the stack of ignore files. If we also tried in reverse
// and check a child before a parent, parent's ignore file or any of its ancestor ignore file might
// have excluded it. By walking downwards each directory is checked against rules that already apply
// to it.
pub(crate) fn find_ignore_files_from_ancestors(
    workspace: &Workspace,
    path: &RepoPath,
) -> Result<Option<Vec<IgnoreFile>>, IoError> {
    let mut stack = Vec::new();
    // no ancestors, the root relative path to root is always empty
    // the collector will load the root's `.litignore` file if any
    if path.as_bytes().is_empty() {
        return Ok(Some(stack));
    }

    let mut directory = RepoPath::empty();
    // the empty dir represents the repo root, this is the point where we look for root ignore file
    // if any to start the chain
    if let Some(ignore_file) = IgnoreFile::load(workspace, &directory)? {
        stack.push(ignore_file);
    }

    let mut components = path.components().peekable();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            break;
        }

        directory = directory.join_bytes_unchecked(component);
        if is_ignored(&mut stack, &directory, true) {
            return Ok(None);
        }
        if let Some(ignore_file) = IgnoreFile::load(workspace, &directory)? {
            stack.push(ignore_file);
        }
    }
    Ok(Some(stack))
}

// Note: that the stack of ignore files is established as applicable, each directory passed its
// ignore check before we loaded its own ignore file(was never excluded by an ancestor ignore file)
// We can then search the stack from closest to furthest and let the first matching decision wins.
pub(crate) fn is_ignored(ignore_files: &[IgnoreFile], path: &RepoPath, is_dir: bool) -> bool {
    // we iterate backwards because the closest ignore file has precedence over ancestor ignore files
    // https://git-scm.com/docs/gitignore#_description
    // the git rule:
    //  `Patterns read from a .gitignore file in the same directory as the path, or in any parent
    //  directory (up to the top-level of the working tree), with patterns in the higher level files
    //  being overridden by those in lower level files down to the directory containing the file.`
    for file in ignore_files.iter().rev() {
        // Two things must happen to consider a path
        //  1. the ignore file is appliable to the candidate
        //  2. the candidate must be relative to the ignore file's directory
        let Some(relative_path) = relative_to(file.base_dir(), path) else {
            continue;
        };
        if let Some(ignored) = file.decision(relative_path, is_dir) {
            return ignored;
        }
    }
    false
}

// every ignore file interprets patterns relative to its own directory, while `RepoPath` is relative
// to repo root.
//  `path: src/net/debug.log and an ignore file contains `/net/debug.log`
//  comparing them would just fail
//  we check if both live in the same dir under `src`, the candidate is inside the ignore files'
//  directory, and if so we remove the base
//
//  for `src/.litignore`
//      candidate `src/main.rs`: relative_to() returns `main.rs`,
//      candidate `src/net/debug.log`: relative_to() returns `net/debug.log`,
//      candate test/main.rs:  relative_to() returns None, ignore file is outside the candidate's
//      ancstry.
fn relative_to<'a>(base: &RepoPath, path: &'a RepoPath) -> Option<&'a [u8]> {
    if base.as_bytes().is_empty() {
        return Some(path.as_bytes());
    }

    if base.is_parent_of(path) {
        // at base.len() is the `/` so skip that
        Some(&path.as_bytes()[base.len() + 1..])
    } else {
        None
    }
}

// TODO: glob support
fn parse_pattern(buf: &[u8]) -> Option<Pattern> {
    let mut negation = false;
    let mut relative = false;
    let mut start = 0;
    let mut end = skip_trailing_spaces(buf);
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

    let match_only_dirs = buf[end - 1] == b'/';
    // pattern bytes will be compared with `RepoPath` bytes which don't include a trailing slash
    // if we kept the trailing slash in `Pattern::matches()` we would have to do something like:
    //  `let pattern = pattern.strip_suffix(b"/").unwrap_or(pattern);`
    // before the byte-by-byte comparison
    if match_only_dirs {
        end -= 1;
    }
    let range = start..end;

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
