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
    MissingRepoFile { path: PathBuf },
}

#[derive(Debug)]
pub(super) struct DbError {
    pub(super) path: PathBuf,
    pub(super) source: io::Error,
}

#[derive(Debug)]
pub(super) enum PathError {
    Empty,
    LeadingSlash,
    TrailingSlash,
    EmptyComponent,
    ReservedComponent,
    ContainsNul,
}

#[derive(Debug)]
pub(super) enum LockfileError {
    Io { path: PathBuf, source: io::Error, },
    LockDenied { path: PathBuf, },
}
// an error that can occur when creating IndexEntry or parsing .lit/index
// Git when format is corrupted reports: Unknown Index Format
// No more information is provided to the user because they can't do much if the format is invalid
#[derive(Debug)]
pub(super) enum IndexError {
    InvalidChecksum,
    UnsupportedVersion(u32),
    InvalidFormat(FormatError),
    Io { path: PathBuf, source: io::Error },
    Lockfile(LockfileError),
}

#[derive(Debug)]
pub(super) enum RefError {
    Io { path: PathBuf, source: io::Error },
    Lockfile(LockfileError),
    DbError(DbError)
}

#[derive(Debug)]
pub(super) struct FormatError {
    pub(super) offset: usize,
    pub(super) kind: FormatErrorKind,
}

impl FormatError {
    pub(super) fn at(offset: usize, kind: FormatErrorKind) -> Self {
        Self { offset, kind }
    }
}

#[derive(Debug)]
pub(super) enum FormatErrorKind {
    Eof { needed: usize, remaining: usize },
    InvalidChecksum,
    InvalidSignature,
    EntriesNotSorted,
    EntriesCountMissMatch { actual: usize, expected: usize },
    InvalidMode(u32),
    InvalidNanoseconds,
    MissingNulTerminator,
    InvalidPadding,
    LongPathLenMissMatch,
    InvalidPathSyntax(PathError),
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
    Index(IndexError),
    DbError(DbError),
    InvalidPath(PathError),
}

#[derive(Debug)]
pub(super) enum CommitError {
    Repo(RepoError),
    Io { path: PathBuf, source: io::Error },
    Index(IndexError),
    DbError(DbError),
    Lockfile(LockfileError),
    RefError(RefError)
}

impl From<RepoError> for AddError {
    fn from(err: RepoError) -> Self {
        AddError::Repo(err)
    }
}

impl From<RepoError> for CommitError {
    fn from(err: RepoError) -> Self { CommitError::Repo(err) }
}

impl From<PathError> for AddError {
    fn from(err: PathError) -> Self {
        AddError::InvalidPath(err)
    }
}

impl From<IndexError> for AddError {
    fn from(err: IndexError) -> Self {
        AddError::Index(err)
    }
}

impl From<DbError> for AddError {
    fn from(err: DbError) -> Self {
        AddError::DbError(err)
    }
}

impl From<IndexError> for CommitError {
    fn from(err: IndexError) -> Self {
        CommitError::Index(err)
    }
}

impl From<DbError> for CommitError {
    fn from(err: DbError) -> Self {
        CommitError::DbError(err)
    }
}

impl From<FormatError> for IndexError {
    fn from(err: FormatError) -> Self {
        IndexError::InvalidFormat(err)
    }
}

impl From<PathError> for FormatErrorKind {
    fn from(err: PathError) -> Self {
        FormatErrorKind::InvalidPathSyntax(err)
    }
}

impl From<LockfileError> for RefError {
    fn from(err: LockfileError) -> Self {
        RefError::Lockfile(err)
    }
}

impl From<DbError> for RefError {
    fn from(err: DbError) -> Self {
        RefError::DbError(err)
    }
}

impl From<LockfileError> for CommitError {
    fn from(err: LockfileError) -> Self {
        CommitError::Lockfile(err)
    }
}

impl From<RefError> for CommitError {
    fn from(err: RefError) -> Self {
        CommitError::RefError(err)
    }
}

impl From<LockfileError> for IndexError {
    fn from(err: LockfileError) -> Self {
        IndexError::Lockfile(err)
    }
}


