use std::io;

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
        let repo = crate::command::Repository::discover()?;
        let mut index = crate::command::index::Index::new(repo.index_path());
        // When status is called Git tries to acquire the lock for index because it does something
        // called Background Refresh: https://git-scm.com/docs/git-status#_background_refresh
        //
        // If it fails to acquire the lock though, it does not error, it still reports changes but
        // never updates the index.
        let lock = match crate::command::lockfile::Lockfile::acquire(&index.path) {
            Ok(lock) => Some(lock),
            // don't try to do Err(e) if e == LockfileError::LockDenied .. won't work because io::Error
            // does not impl PartialEq. LockfileError is an Enum not a struct like io::Error where
            // we had to check against err.kind
            Err(crate::command::error::LockfileError::LockDenied { .. }) => None,
            Err(err) => return Err(StatusError::from(err)),
        };
        index.load()?;
        let report = crate::command::status::report::Report::generate(&repo, &mut index)?;

        if let Some(mut lockfile) = lock {
            if !report.refreshes.is_empty() {
                for (i, stat) in report.refreshes.iter() {
                    index.refresh_entry_stat(*i, *stat);
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
    Index(crate::command::error::IndexError),
    BadReport(crate::command::status::report::ReportError),
    Repository(crate::command::error::RepoError),
    Lockfile(crate::command::error::LockfileError),
    // occurs when trying to print to the terminal, so compared to the other Io variants we had there
    // is no path
    Io(io::Error),
}


impl From<crate::command::error::RepoError> for StatusError {
    fn from(err: crate::command::error::RepoError) -> Self {
        StatusError::Repository(err)
    }
}

impl From<crate::command::status::report::ReportError> for StatusError {
    fn from(err: crate::command::status::report::ReportError) -> Self {
        StatusError::BadReport(err)
    }
}

impl From<crate::command::error::LockfileError> for StatusError {
    fn from(err: crate::command::error::LockfileError) -> Self {
        StatusError::Lockfile(err)
    }
}

impl From<crate::command::error::IndexError> for StatusError {
    fn from(err: crate::command::error::IndexError) -> Self {
        StatusError::Index(err)
    }
}
