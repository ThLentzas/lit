use std::ffi::OsString;
use std::io;
use std::path::PathBuf;

// toDo: A function does not need to be able to return every variant of the enum. That is not
// automatically bad design. What matters is whether the enum represents the error domain of the layer/function.
// that is the case for find_root() where it can return 2/3 variants

use crate::cmd::config::parse::ParseError;
use crate::cmd::object::SignatureError;

#[derive(Debug)]
pub(super) enum RepoError {
    CurrentDir(io::Error),
    // "not a lit repository (or any of the parent directories)"
    NotRepository,
    Io{ path: PathBuf, source: io::Error},
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum PathspecError {
    OutsideRepository { path: PathBuf },
    ReservedComponent { path: PathBuf, component: OsString }
}

#[derive(Debug)]
pub(super) struct DbError {
    pub(super) path: PathBuf,
    pub(super) source: io::Error,
}

#[derive(Debug)]
pub(super) enum WsError {
    CurrentDir{ source: io::Error },
    Io { path: PathBuf, source: io::Error },
    OutsideRepository { path: PathBuf }
}

#[derive(Debug)]
pub(super) enum ConfigError {
    Io { path: PathBuf, source: io::Error },
    Lockfile(LockfileError),
    // toDo: display the unexpected byte value as hex, git shows bad config line 1. We can provide
    // toDo: a message with more information such as the actual reason and the offset within the line
    InvalidFormat { line: usize, source: ParseError}
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
pub(super) enum InitError {
    Io { path: PathBuf, source: io::Error },
}

#[derive(Debug)]
pub(super) enum AddError {
    Repo(RepoError),
    Index(IndexError),
    DbError(DbError),
    WsError(WsError),
    Pathspec(PathspecError),
}

#[derive(Debug)]
pub(super) enum CommitError {
    Repo(RepoError),
    Index(IndexError),
    DbError(DbError),
    Lockfile(LockfileError),
    RefError(RefError),
    Signature(SignatureError),
    Config(ConfigError)
}

impl From<RepoError> for AddError {
    fn from(err: RepoError) -> Self {
        AddError::Repo(err)
    }
}

impl From<RepoError> for CommitError {
    fn from(err: RepoError) -> Self { CommitError::Repo(err) }
}

impl From<PathspecError> for AddError {
    fn from(err: PathspecError) -> Self {
        AddError::Pathspec(err)
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

impl From<WsError> for AddError {
    fn from(err: WsError) -> Self {
        AddError::WsError(err)
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

impl From<SignatureError> for CommitError {
    fn from(err: SignatureError) -> Self {
        CommitError::Signature(err)
    }
}

impl From<ConfigError> for CommitError {
    fn from(err: ConfigError) -> Self {
        CommitError::Config(err)
    }
}

