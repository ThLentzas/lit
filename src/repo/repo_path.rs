use crate::cmd::print::ReadableBytes;
use std::error::Error;
use std::fmt;
use std::path::Path;

// Represents the internal on disk path format. Paths are always root relative. No leading/trailing
// slash, components can't be empty, can't contain NUL and components are always joined by '/' regardless
// of the platform. This is what the index stores, what tree objects store, what report uses as map
// keys etc. Previously I would pass &[u8] which does not hold any invariant, but now RepoPath has
// all the above guarantees. This is why inner is private, and join() checks for any violation in
// the provided bytes.
//
// RepoPath is constructed in 3 different cases so far.
//
// - It is created for Pathspec after resolving the user provided path.
// - When we walk a directory(workspace.dir_entries()) builds the RepoPath for each entry by joining
// the parent RepoPath with the entry's name.
// - During Index parsing
//
// Note: There is no encoding enforced on the underlying vector
#[derive(Debug, Default, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct RepoPath {
    inner: Vec<u8>,
}

impl RepoPath {
    // could also call it empty()
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(super) fn from_bytes(bytes: &[u8]) -> Result<Self, RepoPathError> {
        check_bytes(bytes)?;
        let mut inner = Vec::with_capacity(bytes.len());
        inner.extend_from_slice(bytes);

        Ok(RepoPath { inner })
    }

    pub(super) fn join<P: AsRef<Path>>(&self, path: P) -> Result<Self, RepoPathError> {
        let bytes = path.as_ref().as_os_str().as_encoded_bytes();
        check_bytes(bytes)?;

        Ok(self.join_unchecked(path))
    }

    pub(super) fn join_unchecked<P: AsRef<Path>>(&self, path: P) -> Self {
        let path = path.as_ref().as_os_str().as_encoded_bytes();

        let mut inner = Vec::with_capacity(self.inner.len() + 1 + path.len());
        inner.extend_from_slice(&self.inner);
        if !self.inner.is_empty() {
            inner.push(b'/');
        }
        inner.extend_from_slice(path);

        RepoPath { inner }
    }

    pub(super) fn join_bytes(&self, bytes: &[u8]) -> Result<Self, RepoPathError> {
        check_bytes(bytes)?;

        Ok(self.join_bytes_unchecked(bytes))
    }

    pub(super) fn join_bytes_unchecked(&self, bytes: &[u8]) -> Self {
        let mut inner = Vec::with_capacity(self.inner.len() + 1 + bytes.len());
        inner.extend_from_slice(&self.inner);
        if !self.inner.is_empty() {
            inner.push(b'/');
        }
        inner.extend_from_slice(bytes);

        RepoPath { inner }
    }

    pub(super) fn len(&self) -> usize {
        self.inner.len()
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.inner
    }

    // in order to have a conflict we need them to share the same parent directories, lib/index/main.rs
    // and src/index/main.rs are fine. We need to answer the question: Does the child start with the
    // parent path, and is the next byte a / ?
    //
    // the length of the child must be greater than the parent because parent is part of the child
    // for a conflict to exist the parent must be a parent dir of child
    // the 3rd condition is to avoid a false match, like parent: lib, child: library/doc it is not
    // enough for it to be a prefix, it has to be a parent dir
    pub(super) fn is_parent_of(&self, other: &RepoPath) -> bool {
        other.len() > self.len()
            && other.inner.starts_with(self.as_bytes())
            && other.inner[self.len()] == b'/'
    }

    pub(super) fn components(&self) -> impl Iterator<Item = &[u8]> {
        // split actually returns a SplitIterator and next() returns slices up to the index that the
        // predicate returned true, this is why we can call componenets().peekable() and components.next()
        self.inner.split(|&byte| byte == b'/')
    }

    pub(crate) fn display(&self) -> String {
        String::from_utf8_lossy(&self.inner).to_string()
    }
}

fn check_bytes(bytes: &[u8]) -> Result<(), RepoPathError> {
    if bytes.is_empty() {
        return Err(RepoPathError {
            path: bytes.to_vec(),
            kind: RepoPathErrorKind::Empty,
        });
    }
    // paths are relative to the repository root, so no leading slash.
    if bytes.starts_with(b"/") {
        return Err(RepoPathError {
            path: bytes.to_vec(),
            kind: RepoPathErrorKind::LeadingSlash,
        });
    }
    // trailing slash is not allowed.
    if bytes.ends_with(b"/") {
        return Err(RepoPathError {
            path: bytes.to_vec(),
            kind: RepoPathErrorKind::TrailingSlash,
        });
    }

    for component in bytes.split(|&b| b == b'/') {
        // empty components: "src//main.rs"
        if component.is_empty() {
            return Err(RepoPathError {
                path: bytes.to_vec(),
                kind: RepoPathErrorKind::EmptyComponent,
            });
        }
        // ".", "..", and ".lit" as path components are not allowed
        // src/./main.rs: stays in the current directory, redundant and not a real subdirectory
        // src/../etc/passwd: escapes upward, would let a crafted index reference files outside the repo
        // .lit/config: points into Lit's own metadata, never legitimate as a tracked file.
        if matches!(component, b"." | b".." | b".lit") {
            return Err(RepoPathError {
                path: bytes.to_vec(),
                kind: RepoPathErrorKind::ReservedComponent,
            });
        }
        // NUL cannot appear inside the path.
        if memchr::memchr(0, component).is_some() {
            return Err(RepoPathError {
                path: bytes.to_vec(),
                kind: RepoPathErrorKind::ContainsNul,
            });
        }
    }

    Ok(())
}

#[derive(Debug)]
pub(super) enum RepoPathErrorKind {
    Empty,
    LeadingSlash,
    TrailingSlash,
    EmptyComponent,
    // TODO: add the component's name
    ReservedComponent,
    ContainsNul,
}

#[derive(Debug)]
pub(crate) struct RepoPathError {
    pub(super) path: Vec<u8>,
    pub(super) kind: RepoPathErrorKind,
}

impl Error for RepoPathError {}

impl fmt::Display for RepoPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: review if printing the path should use ReadableBytes or print.rs::stdout_path_bytes
        let path = ReadableBytes(&self.path);

        match &self.kind {
            RepoPathErrorKind::Empty => write!(f, "path is empty"),
            RepoPathErrorKind::LeadingSlash => write!(f, "path '{path}' begins with '/'"),
            RepoPathErrorKind::TrailingSlash => write!(f, "path '{path}' ends with '/'"),
            RepoPathErrorKind::EmptyComponent => {
                write!(f, "path '{path}' contains an empty component")
            }
            RepoPathErrorKind::ReservedComponent => {
                write!(f, "path '{path}' contains a reserved component")
            }
            RepoPathErrorKind::ContainsNul => write!(f, "path '{path}' contains a NUL byte"),
        }
    }
}
