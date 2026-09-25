use crate::repo::config::{ConfigFile, ConfigFileError, ConfigFileErrorKind};
use crate::repo::db::{self, DatabaseError};
use crate::repo::index::{Index, IndexError};
use crate::repo::lockfile::{Lockfile, LockfileError};
use crate::repo::object::mode::Mode;
use crate::repo::object::{self, Object, Signature, SignatureError};
use crate::repo::refs::RefError;
use crate::repo::tree::Tree;
use crate::repo::workspace::{Workspace, WorkspaceError};
use crate::repo::{Repository, RepositoryError};
use crate::repo::os::IoError;

#[derive(Debug)]
pub(crate) struct Commit;

impl Commit {
    // TODO: [master 1b9a196] Done with status, need to test it 3 files changed, 92 insertions(+),
    //  46 deletions(-)
    pub(super) fn execute(&self) -> Result<(), CommitError> {
        let repo = Repository::discover()?;
        let db = repo.database();
        let workspace = repo.workspace().unwrap();
        let refs = repo.refs();
        let mut index = repo.index();

        // this is the same optimistic approach Git follows with status. For a tracked file by index
        // in the workspace if any metadata have changed, it updates the corresponding index entry.
        // it is explained in more detailed in report::Report::scan_against_workspace()
        //
        // the refresh is optional if for whatever reason we fail to acquire the lock we still want
        // to commit our changes.
        let lock = Lockfile::acquire(index.path()).ok();

        index.load()?;
        if let Some(lock) = lock {
            index_background_refreshing(&mut index, lock, &workspace)?;
        }
        // TODO: need to rethink how this load() is called because now load() is called without
        //  knowing if Signature will actually read the info from config or env
        let cfg = repo.config()?;
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
    Repository(RepositoryError),
    Workspace(WorkspaceError),
    Index(IndexError),
    DbError(DatabaseError),
    Lockfile(LockfileError),
    RefError(RefError),
    Signature(SignatureError),
    Config(ConfigFileError),
    Io(IoError),
}

impl From<IoError> for CommitError {
    fn from(err: IoError) -> Self {
        Self::Io(err)
    }
}

impl From<RepositoryError> for CommitError {
    fn from(err: RepositoryError) -> Self {
        Self::Repository(err)
    }
}

impl From<WorkspaceError> for CommitError {
    fn from(err: WorkspaceError) -> Self {
        Self::Workspace(err)
    }
}

impl From<IndexError> for CommitError {
    fn from(err: IndexError) -> Self {
        Self::Index(err)
    }
}

impl From<DatabaseError> for CommitError {
    fn from(err: DatabaseError) -> Self {
        Self::DbError(err)
    }
}

impl From<LockfileError> for CommitError {
    fn from(err: LockfileError) -> Self {
        Self::Lockfile(err)
    }
}

impl From<RefError> for CommitError {
    fn from(err: RefError) -> Self {
        Self::RefError(err)
    }
}

impl From<SignatureError> for CommitError {
    fn from(err: SignatureError) -> Self {
        Self::Signature(err)
    }
}

impl From<ConfigFileError> for CommitError {
    fn from(err: ConfigFileError) -> Self {
        Self::Config(err)
    }
}
