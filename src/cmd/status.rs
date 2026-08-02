use std::io;
use crate::repo::index::{Index, IndexError};
use crate::repo::{RepoError, Repository};
use crate::repo::lockfile::{Lockfile, LockfileError};
use crate::repo::report::{Report, ReportError};

mod print;

// TODO:
// merge conflicts / unmerged index stages
// rename detection
// copy detection
// ignored files
// submodule states
// typechange as a separate status category

enum Format {
    Short,
    Long,
}

impl Default for Format {
    fn default() -> Self {
        Format::Long
    }
}

#[derive(Default)]
pub(crate) struct Status {}

impl Status {
    pub(super) fn execute(&mut self) -> Result<(), StatusError> {
        let repo = Repository::discover()?;
        let mut index = Index::new(repo.index_path());
        // When status is called Git tries to acquire the lock for index because it does something
        // called Background Refresh: https://git-scm.com/docs/git-status#_background_refresh
        //
        // If it fails to acquire the lock though, it does not error, it still reports changes but
        // never updates the index.
        let lock = match Lockfile::acquire(&index.path) {
            Ok(lock) => Some(lock),
            // don't try to do Err(e) if e == LockfileError::LockDenied .. won't work because io::Error
            // does not impl PartialEq. LockfileError is an Enum not a struct like io::Error where
            // we had to check against err.kind
            Err(LockfileError::LockDenied { .. }) => None,
            // TODO: should we keep going if we fail to acquire the lock due to io::Error?
            Err(err) => return Err(StatusError::from(err)),
        };
        index.load()?;
        let report = Report::generate(&repo, &mut index)?;

        if let Some(mut lockfile) = lock {
            if !report.refreshes.is_empty() {
                for (i, node) in report.refreshes.iter() {
                    index.refresh_entry_stat(*i, node.stat);
                }
                lockfile.write(&index.serialize())?;
                lockfile.commit()?;
            }
        }
        print::print(&report, Format::default()).map_err(StatusError::Io)?;
        Ok(())
    }
}

#[derive(Debug)]
pub(super) enum StatusError {
    Index(IndexError),
    BadReport(ReportError),
    Lockfile(LockfileError),
    Repository(RepoError),
    // occurs when trying to print to the terminal, compared to the other Io variants we had there
    // is no path
    Io(io::Error),
}


impl From<RepoError> for StatusError {
    fn from(err: RepoError) -> Self {
        StatusError::Repository(err)
    }
}

impl From<ReportError> for StatusError {
    fn from(err: ReportError) -> Self {
        StatusError::BadReport(err)
    }
}

impl From<LockfileError> for StatusError {
    fn from(err: LockfileError) -> Self {
        StatusError::Lockfile(err)
    }
}

impl From<IndexError> for StatusError {
    fn from(err: IndexError) -> Self {
        StatusError::Index(err)
    }
}
