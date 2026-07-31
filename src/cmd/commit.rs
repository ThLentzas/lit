use crate::command::{db, object};
use crate::command::status::report;
use crate::hex;

pub(super) struct Commit;

impl Commit {
    // [master 1b9a196] Done with status, need to test it
    //  3 files changed, 92 insertions(+), 46 deletions(-)
    fn execute(&self) -> Result<(), crate::command::error::CommitError> {
        let repo = crate::command::Repository::discover()?;
        let db = crate::command::db::Database {
            path: repo.db_path(),
        };
        let workspace = crate::command::workspace::Workspace { root: repo.root.clone() };
        let refs = crate::command::refs::Refs { path: repo.refs_path() };
        let mut index = crate::command::index::Index::new(repo.index_path());
        // this is the same optimistic approach Git follows with status. For a tracked file by index
        // in the workspace if any metadata have changed, it updates the corresponding index entry.
        // it is explained in more detailed in report::Report::scan_against_workspace()
        // TODO: when we implement the cache tree for Index we need to check this again.
        let lock = match crate::command::lockfile::Lockfile::acquire(&index.path) {
            Ok(lock) => Some(lock),
            Err(crate::command::error::LockfileError::LockDenied { .. }) => None,
            Err(err) => return Err(crate::command::error::CommitError::Lockfile(err)),
        };
        index.load()?;
        if let Some(lock) = lock {
            Self::index_background_refreshing(&mut index, lock, &workspace)?;
        }

        // need to rethink how this load() is called because now load() is called without
        // knowing if Signature will actually read the info from config or env
        let mut config = crate::command::config::Config::new(repo.config_path());
        config.load()?;
        let author = crate::command::object::Signature::author(&config)?;
        let committer = crate::command::object::Signature::committer(&config)?;
        let tree_id = crate::command::index::tree::Tree::from_index(index).write(&db)?;

        refs.update_head(|parent| {
            let commit = object::Commit {
                author,
                parent,
                committer,
                message: "".to_string(),
                // convert the [u8, 20] to its hex value
                root_id: hex::bytes_as_hex(&tree_id),
            };
            db.store(crate::command::object::Object::Commit(commit))
        })?;
        Ok(())
    }

    fn index_background_refreshing(
        index: &mut crate::command::index::Index,
        mut lock: crate::command::lockfile::Lockfile,
        workspace: &crate::command::workspace::Workspace) -> Result<(), crate::command::error::CommitError> {
        let mut refreshes = Vec::new();

        for(i, entry) in index.entries.iter().enumerate() {
            let path = crate::command::os::bytes_to_path(&entry.path);
            let ws_stat = workspace.stat(&path)?;

            if !report::times_match(&entry.stat, &ws_stat) {
                let content = workspace.read_file(&path)?;
                if db::hash(b"blob", &content) == entry.oid {
                    refreshes.push((i, ws_stat));
                }
            }
        }

        if refreshes.is_empty() {
            drop(lock);
        } else {
            for(i, stat) in refreshes {
                index.refresh_entry_stat(i, stat);
            }
            lock.write(&index.serialize())?;
            lock.commit()?;
        }

        Ok(())
    }
}

#[derive(Debug)]
pub(super) enum CommitError {
    Repo(crate::command::error::RepoError),
    Workspace(crate::command::error::WorkspaceError),
    Index(crate::command::error::IndexError),
    DbError(crate::command::error::DbError),
    Lockfile(crate::command::error::LockfileError),
    RefError(crate::command::error::RefError),
    Signature(crate::command::object::SignatureError),
    Config(crate::command::error::ConfigError)
}


impl From<crate::command::error::RepoError> for CommitError {
    fn from(err: crate::command::error::RepoError) -> Self { CommitError::Repo(err) }
}

impl From<crate::command::error::WorkspaceError> for CommitError {
    fn from(err: crate::command::error::WorkspaceError) -> Self {
        CommitError::Workspace(err)
    }
}

impl From<crate::command::error::IndexError> for CommitError {
    fn from(err: crate::command::error::IndexError) -> Self {
        CommitError::Index(err)
    }
}

impl From<crate::command::error::DbError> for CommitError {
    fn from(err: crate::command::error::DbError) -> Self {
        CommitError::DbError(err)
    }
}

impl From<crate::command::error::LockfileError> for CommitError {
    fn from(err: crate::command::error::LockfileError) -> Self {
        CommitError::Lockfile(err)
    }
}

impl From<crate::command::error::RefError> for CommitError {
    fn from(err: crate::command::error::RefError) -> Self {
        CommitError::RefError(err)
    }
}

impl From<crate::command::object::SignatureError> for CommitError {
    fn from(err: crate::command::object::SignatureError) -> Self {
        CommitError::Signature(err)
    }
}

impl From<crate::command::error::ConfigError> for CommitError {
    fn from(err: crate::command::error::ConfigError) -> Self {
        CommitError::Config(err)
    }
}