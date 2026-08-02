use crate::hex;
use crate::repo::config::{Config, ConfigError};
use crate::repo::db::{Database, DbError};
use crate::repo::index::{Index, IndexError};
use crate::repo::lockfile::{Lockfile, LockfileError};
use crate::repo::object::{Object, Signature, SignatureError};
use crate::repo::refs::{RefError, Refs};
use crate::repo::{object, Repository, RepoError, db};
use crate::repo::tree::Tree;
use crate::repo::workspace::{Workspace, WorkspaceError};

pub(crate) struct Commit;

impl Commit {
    // [master 1b9a196] Done with status, need to test it
    //  3 files changed, 92 insertions(+), 46 deletions(-)
    pub(super) fn execute(&self) -> Result<(), CommitError> {
        let repo = Repository::discover()?;
        let db = Database {
            path: repo.db_path(),
        };
        let workspace = Workspace { root: repo.root.clone() };
        let refs = Refs { path: repo.refs_path() };
        let mut index = Index::new(repo.index_path());
        // this is the same optimistic approach Git follows with status. For a tracked file by index
        // in the workspace if any metadata have changed, it updates the corresponding index entry.
        // it is explained in more detailed in report::Report::scan_against_workspace()
        // TODO: when we implement the cache tree for Index we need to check this again.
        let lock = match Lockfile::acquire(&index.path) {
            Ok(lock) => Some(lock),
            Err(LockfileError::LockDenied { .. }) => None,
            Err(err) => return Err(CommitError::Lockfile(err)),
        };
        index.load()?;
        if let Some(lock) = lock {
            Self::index_background_refreshing(&mut index, lock, &workspace)?;
        }

        // need to rethink how this load() is called because now load() is called without
        // knowing if Signature will actually read the info from config or env
        let mut config = Config::new(repo.config_path());
        config.load()?;
        let author = Signature::author(&config)?;
        let committer = Signature::committer(&config)?;
        let tree_id = Tree::from_index(index).write(&db)?;

        refs.update_head(|parent| {
            let commit = object::Commit {
                author,
                parent,
                committer,
                message: "".to_string(),
                // convert the [u8, 20] to its hex value
                root_id: hex::bytes_as_hex(&tree_id),
            };
            db.store(Object::Commit(commit))
        })?;
        Ok(())
    }

    fn index_background_refreshing(
        index: &mut Index,
        mut lock: Lockfile,
        workspace: &Workspace) -> Result<(), CommitError> {
        let mut refreshes = Vec::new();

        for(i, entry) in index.entries.iter().enumerate() {
            let ws_stat = workspace.stat(&entry.path)?;
            if !entry.times_match(&ws_stat.stat) {
                let content = workspace.read_file(&entry.path)?;
                if db::hash(b"blob", &content) == entry.oid {
                    refreshes.push((i, ws_stat));
                }
            }
        }

        if refreshes.is_empty() {
            drop(lock);
        } else {
            for(i, node) in refreshes {
                index.refresh_entry_stat(i, node.stat);
            }
            lock.write(&index.serialize())?;
            lock.commit()?;
        }

        Ok(())
    }
}

#[derive(Debug)]
pub(super) enum CommitError {
    Repo(RepoError),
    Workspace(WorkspaceError),
    Index(IndexError),
    DbError(DbError),
    Lockfile(LockfileError),
    RefError(RefError),
    Signature(SignatureError),
    Config(ConfigError)
}


impl From<RepoError> for CommitError {
    fn from(err: RepoError) -> Self { CommitError::Repo(err) }
}

impl From<WorkspaceError> for CommitError {
    fn from(err: WorkspaceError) -> Self {
        CommitError::Workspace(err)
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