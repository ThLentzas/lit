// Represents the internal on disk path format. Paths are always root relative. No leading/trailing
// slash, components can't be empty, can't contain NUL and components are always joined by '/' regardless
// of the platform. This is what the index stores, what tree objects store, what report uses as map
// keys etc. Previously I would pass &[u8] which does not hold any invariant, but now RepoPath has
// all the above guarantees.
#[derive(Debug, Default, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct RepoPath {
    inner: Vec<u8>,
}

impl RepoPath {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn join(&self, bytes: &[u8]) -> Self {
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

    pub(super) fn as_bytes(&self) -> &[u8] {
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
            && other[self.len()] == b'/'
    }

    pub(super) fn push(&mut self, byte: u8) {
        self.inner.push(byte);
    }

    pub(super) fn components(&self) -> impl Iterator<Item = &[u8]> {
        // split actually returns a SplitIterator and next() returns slices up to the index that the
        // predicate returned true, this is why we can call componenets().peekable() and components.next()
        self.inner.split(|&byte| byte == b'/')
    }
}
