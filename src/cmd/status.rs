use std::io;
use crate::cmd::print::Printer;
use crate::cmd::status::print::StatusPrinter;
use crate::repo::index::{Index, IndexError};
use crate::repo::{DiscoverError, Repository};
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
        // the refresh is optional if for whatever reason we fail to acquire the lock we still want
        // to report the changes.
        let lock = match Lockfile::acquire(&index.path) {
            Ok(lock) => Some(lock),
            Err(_) => None,
        };
        index.load()?;
        let report = Report::generate(&repo, &index)?;

        if let Some(mut lockfile) = lock {
            if !report.refreshes.is_empty() {
                for (i, node) in report.refreshes.iter() {
                    index.refresh_entry_stat(*i, node.stat);
                }
                lockfile.write(&index.serialize())?;
                lockfile.commit()?;
            }
        }
        let printer = StatusPrinter { format: Format::default() };
        printer.print(&report).map_err(StatusError::Io)?;

        Ok(())
    }
}

#[derive(Debug)]
pub(super) enum StatusError {
    Index(IndexError),
    BadReport(ReportError),
    Lockfile(LockfileError),
    Repository(DiscoverError),
    // occurs when trying to print to the terminal, compared to the other Io variants we had there
    // is no path
    Io(io::Error),
}


impl From<DiscoverError> for StatusError {
    fn from(err: DiscoverError) -> Self {
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
