use std::io;
use std::path::PathBuf;

// toDo: maybe instead of PathBuf we could &'static Path to avoid cloning on the call site?

// toDo: A function does not need to be able to return every variant of the enum. That is not
// automatically bad design. What matters is whether the enum represents the error domain of the layer/function.
// that is the case for find_root() where it can return 2/3 variants
#[derive(Debug)]
pub(super) enum RepoError {
    CurrentDir(io::Error),
    // "not a lit repository (or any of the parent directories)"
    NotRepository,
    MissingRepoFile { path: PathBuf }
}

// an error that can occur when creating IndexEntry or parsing .lit/index
// Git when format is corrupted reports: Unknown Index Format
// No more information is provided to the user because they can't do much if the format is invalid

#[derive(Debug)]
pub(super) enum  IndexError {
    InvalidIndexFormat(IndexFormatError),
    Io { path: PathBuf, source: io::Error },
    LockDenied { path: PathBuf }
}


#[derive(Debug)]
pub(super) struct IndexFormatError {
    pub(super) offset: usize,
    pub(super) kind: IndexFormatErrorKind
}

impl IndexFormatError {
    pub(super) fn at(offset: usize, kind: IndexFormatErrorKind) -> Self {
        Self {
            offset,
            kind
        }
    }
}

#[derive(Debug)]
pub(super) enum IndexFormatErrorKind {
    Eof { needed: usize, remaining: usize },
    InvalidChecksum

}

#[derive(Debug)]
pub(super) enum AddError {
    Repo(RepoError),
    OutsideRepository { path: PathBuf, root: PathBuf },
    // The path resolved successfully, but when add tried to inspect its metadata, the OS failed
    // This can happen because the filesystem can change between operations:
    //      let (absolute, relative) = self.resolve_path(&root)?;
    //      let stat = os::stat(&absolute)?;
    //
    // Even if resolve_path() succeeded, another process might delete the file before stat() runs.
    // Or permissions might change. Or the path might become inaccessible.
    // fatal: unable to stat 'path': <source>
    StatFile { path: PathBuf, source: io::Error },
    Io { path: PathBuf, source: io::Error },
    InvalidIndexFormat,
}

impl From<RepoError> for AddError {
    fn from(err: RepoError) -> Self {
        AddError::Repo(err)
    }
}

impl From<IndexFormatError> for AddError {
    fn from(_err: IndexFormatError) -> Self {
        AddError::InvalidIndexFormat
    }
}