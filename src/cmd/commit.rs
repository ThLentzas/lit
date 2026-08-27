use crate::repo::config::{ConfigFile, ConfigFileError};
use crate::repo::db::{self, Database, DbError};
use crate::repo::index::{Index, IndexError};
use crate::repo::lockfile::{Lockfile, LockfileError};
use crate::repo::object::mode::Mode;
use crate::repo::object::{self, Object, Signature, SignatureError};
use crate::repo::refs::{RefError, Refs};
use crate::repo::tree::Tree;
use crate::repo::workspace::{Workspace, WorkspaceError};
use crate::repo::{DiscoverError, Repository};

#[derive(Debug)]
pub(crate) struct Commit;

impl Commit {
    // TODO: [master 1b9a196] Done with status, need to test it 3 files changed, 92 insertions(+),
    // 46 deletions(-)
    pub(super) fn execute(&self) -> Result<(), CommitError> {
        let repo = Repository::discover()?;
        let db = Database {
            path: repo.db_path(),
        };
        let workspace = Workspace {
            root: repo.root.clone(),
        };
        let refs = Refs {
            path: repo.refs_path(),
        };
        let mut index = Index::new(repo.index_path());
        // this is the same optimistic approach Git follows with status. For a tracked file by index
        // in the workspace if any metadata have changed, it updates the corresponding index entry.
        // it is explained in more detailed in report::Report::scan_against_workspace()
        //
        // the refresh is optional if for whatever reason we fail to acquire the lock we still want
        // to commit our changes.
        let lock = Lockfile::acquire(&index.path).ok();

        index.load()?;
        if let Some(lock) = lock {
            index_background_refreshing(&mut index, lock, &workspace)?;
        }
        // TODO: need to rethink how this load() is called because now load() is called without
        // knowing if Signature will actually read the info from config or env
        let cfg = ConfigFile::new(&repo.config_path())?;
        let author = Signature::author(&cfg)?;
        let committer = Signature::committer(&cfg)?;
        let tree_id = Tree::from_index(index).write(&db)?;

        refs.update_head(|parents| {
            let commit = object::Commit {
                author,
                parents,
                committer,
                message: "hey".to_string(),
                root_id: tree_id,
            };
            db.store(Object::Commit(commit))
        })?;
        Ok(())
    }
}

fn index_background_refreshing(
    index: &mut Index,
    mut lock: Lockfile,
    workspace: &Workspace,
) -> Result<(), CommitError> {
    let mut refreshes = Vec::new();

    for (i, entry) in index.entries.iter().enumerate() {
        let node = match workspace.stat(&entry.path) {
            Ok(node) => node,
            Err(_) => continue,
        };
        let mode = Mode::try_from(node.kind).map_or(true, |mode| entry.mode != mode);
        // different size/mode -> modified
        if entry.stat.file_size != node.stat.file_size || mode {
            continue;
        }

        if !entry.times_match(&node.stat) {
            // if we blindly called fs::read_file(), for symlinks we would follow the path and return
            // the target's content which is not what we store in the blob.
            let content = if entry.mode.is_symlink() {
                workspace.read_link(&entry.path)?
            } else {
                workspace.read_file(&entry.path)?
            };
            if db::hash(b"blob", &content) == entry.oid {
                refreshes.push((i, node.stat));
            }
        }
    }

    if refreshes.is_empty() {
        drop(lock);
    } else {
        for (i, stat) in refreshes {
            index.refresh_entry_stat(i, stat);
        }
        lock.write(&index.serialize())?;
        lock.commit()?;
    }

    Ok(())
}

#[derive(Debug)]
pub(super) enum CommitError {
    Repo(DiscoverError),
    Workspace(WorkspaceError),
    Index(IndexError),
    DbError(DbError),
    Lockfile(LockfileError),
    RefError(RefError),
    Signature(SignatureError),
    Config(ConfigFileError),
}

impl From<DiscoverError> for CommitError {
    fn from(err: DiscoverError) -> Self {
        CommitError::Repo(err)
    }
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

impl From<ConfigFileError> for CommitError {
    fn from(err: ConfigFileError) -> Self {
        CommitError::Config(err)
    }
}
