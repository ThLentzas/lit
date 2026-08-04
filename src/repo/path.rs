use crate::repo::index::PathError;

// Represents the internal on disk path format. Paths are always root relative. No leading/trailing
// slash, components can't be empty, can't contain NUL and components are always joined by '/' regardless
// of the platform. This is what the index stores, what tree objects store, what report uses as map
// keys etc. Previously I would pass &[u8] which does not hold any invariant, but now RepoPath has
// all the above guarantees. This is why inner is private, and join() checks for any violation in
// the provided bytes.
//
// RepoPath is constructed in 3 different cases so far.
//
// - It is created for Pathspec after resolving the user provided path. There is a call to name_as_bytes()
// which for Windows guarantees that inner will always hold valid UTF8 bytes.
// - When we walk a directory(workspace.dir_entries()) builds the RepoPath for each entry by joining
// the parent RepoPath with the entry's name. The entry's name is an OsString, and it has to valid
// UTF8 from the reasons mentioned in os::name_to_bytes() for Windows.
// - During parsing for Index, this is where even on Windows the bytes for RepoPath's are not guaranteed
// to be UTF8.
//
// Note: There is no encoding enforced on the underlying vector, just in the 2/3 cases mentioned above
// for Windows, they will be valid UTF8 bytes.
#[derive(Debug, Default, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct RepoPath {
    inner: Vec<u8>,
}

impl RepoPath {
    // could also call it empty()
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(super) fn from_bytes(bytes: &[u8]) -> Result<Self, PathError> {
        if bytes.is_empty() {
            return Err(PathError::Empty);
        }
        // paths are relative to the repository root, so no leading slash.
        if bytes[0] == b'/' {
            return Err(PathError::LeadingSlash);
        }
        // trailing slash is not allowed.
        if bytes[bytes.len() - 1] == b'/' {
            return Err(PathError::TrailingSlash);
        }

        for component in bytes.split(|&b| b == b'/') {
            // empty components: "src//main.rs"
            if component.is_empty() {
                return Err(PathError::EmptyComponent);
            }
            // ".", "..", and ".lit" as path components are not allowed
            // src/./main.rs: stays in the current directory, redundant and not a real subdirectory
            // src/../etc/passwd: escapes upward, would let a crafted index reference files outside the repo
            // .lit/config: points into Lit's own metadata, never legitimate as a tracked file.
            if matches!(component, b"." | b".." | b".lit") {
                return Err(PathError::ReservedComponent);
            }
            // NUL cannot appear inside the path.
            if memchr::memchr(0, component).is_some() {
                return Err(PathError::ContainsNul);
            }
        }
        let mut inner = Vec::with_capacity(bytes.len());
        inner.extend_from_slice(bytes);

        Ok(RepoPath { inner })
    }

    // we need to assert that bytes are not empty, do not contain '/' at the start or end and no NUL
    // byte
    pub(super) fn join(&self, component: &[u8]) -> Self {
        let mut inner = Vec::with_capacity(self.inner.len() + 1 + component.len());
        inner.extend_from_slice(&self.inner);
        if !self.inner.is_empty() {
            inner.push(b'/');
        }
        inner.extend_from_slice(component);

        RepoPath { inner }
    }

    pub(super) fn len(&self) -> usize {
        self.inner.len()
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.inner
    }

    // read Index::resolve_conflicts() first
    //
    // in order to have a conflict we need them to share the same parent directories, lib/index/main.rs
    // and src/index/main.rs are fine. We need to answer the question: Does the child start with the
    // parent path, and is the next byte a / ?
    //
    // the length of the child must be greater than the parent because parent is part of the child
    // for a conflict to exist the parent must be a parent dir of child
    // the 3rd condition is to avoid a false match, like parent: lib, child: library/file.rs it is not
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
}
