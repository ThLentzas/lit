use crate::command::print::Printer;
use crate::command::status::print::StatusPrinter;
use crate::repo::index::IndexError;
use crate::repo::lockfile::{Lockfile, LockfileError};
use crate::repo::os::{IoError, IoErrorContext};
use crate::repo::report::{Report, ReportError};
use crate::repo::{Repository, RepositoryError};
use std::error::Error;
use std::fmt;
use std::path::Path;

mod print;

// TODO:
//  merge conflicts / unmerged index stages
//  rename detection
//  copy detection
//  ignored files
//  submodule states
//  typechange as a separate status category

#[derive(Default)]
enum Format {
    Short,
    #[default]
    Long,
}

#[derive(Default, Debug)]
pub(crate) struct Status;

impl Status {
    pub(super) fn execute(&self) -> Result<(), StatusError> {
        let repo = Repository::discover()?;
        let mut index = repo.index();
        // When status is called Git tries to acquire the lock for index because it does something
        // called Background Refresh: https://git-scm.com/docs/git-status#_background_refresh
        //
        // the refresh is optional if for whatever reason we fail to acquire the lock we still want
        // to report the changes.
        let lock = Lockfile::acquire(index.path()).ok();
        index.load()?;
        let report = Report::generate(&repo, &index)?;

        if let Some(mut lockfile) = lock
            && !report.refreshes.is_empty()
        {
            for (i, node) in report.refreshes.iter() {
                index.refresh_entry_stat(*i, node.stat);
            }
            lockfile.write(&index.serialize())?;
            lockfile.commit()?;
        }
        let printer = StatusPrinter {
            format: Format::default(),
        };
        printer
            .print(&report)
            .with_context::<&Path>("write", None)?;

        Ok(())
    }
}

#[derive(Debug)]
pub(super) enum StatusError {
    Index(IndexError),
    BadReport(ReportError),
    Lockfile(LockfileError),
    Repository(RepositoryError),
    Io(IoError),
}

impl Error for StatusError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Index(source) => Some(source),
            Self::BadReport(source) => Some(source),
            Self::Lockfile(source) => Some(source),
            Self::Repository(source) => Some(source),
            Self::Io(source) => Some(source),
        }
    }
}

impl fmt::Display for StatusError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Index(_) => write!(f, "could not load index"),
            Self::BadReport(_) => write!(f, "could not determine report status"),
            Self::Lockfile(_) => write!(f, "could not load index"),
            Self::Repository(_) => write!(f, "could not discover repository"),
            Self::Io(_) => write!(f, "could not write to stdout"),
        }
    }
}

impl From<IndexError> for StatusError {
    fn from(err: IndexError) -> Self {
        Self::Index(err)
    }
}

impl From<ReportError> for StatusError {
    fn from(err: ReportError) -> Self {
        Self::BadReport(err)
    }
}

impl From<LockfileError> for StatusError {
    fn from(err: LockfileError) -> Self {
        Self::Lockfile(err)
    }
}

impl From<RepositoryError> for StatusError {
    fn from(err: RepositoryError) -> Self {
        Self::Repository(err)
    }
}

impl From<IoError> for StatusError {
    fn from(err: IoError) -> Self {
        Self::Io(err)
    }
}
