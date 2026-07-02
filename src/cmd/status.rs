use crate::cmd::error::{IndexError, LockfileError, RepoError, WorkspaceError};
use crate::cmd::index::{Index, StatNode};
use crate::cmd::lockfile::Lockfile;
use crate::cmd::workspace::Workspace;
use crate::cmd::{Repository, db, index, os};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

#[derive(Debug)]
pub(super) enum StatusError {
    Repository(RepoError),
    Index(IndexError),
    Workspace(WorkspaceError),
    Lockfile(LockfileError),
}

// green for new, orange for modified, red for deleted, something else for untracked
enum HeadIndexChange {
    ADDED,    // exists in the index but not in HEAD
    MODIFIED, // exists in both but modified in the index
    DELETED,  // exists in HEAD but not in the index
}

enum WorkspaceIndexChange {
    MODIFIED, // exists in both but modified in workspace
    DELETED,  // exists in index but not in workspace
}

struct Change {
    head_index: Option<HeadIndexChange>,
    workspace_index: Option<WorkspaceIndexChange>,
}

#[derive(Default)]
pub(crate) struct Status {
    stats: BTreeMap<Vec<u8>, StatNode>,
    untracked: BTreeSet<Vec<u8>>,
    changes: BTreeMap<Vec<u8>, Change>,
    refreshes: HashMap<usize, StatNode>,
}

impl Status {
    pub(crate) fn new() -> Self {
        Self::default()
    }
    
    pub(super) fn execute(&mut self) -> Result<(), StatusError> {
        let repo = Repository::discover()?;
        let mut index = Index::new(repo.index_path());
        let workspace = Workspace { root: repo.root };

        let lock = match Lockfile::acquire(&index.path) {
            Ok(lock) => Some(lock),
            Err(LockfileError::LockDenied { .. }) => None,
            Err(err) => return Err(err.into()),
        };

        index.load()?;
        self.scan_workspace(&workspace, &index, Path::new(""))?;
        self.scan_index(&index, &workspace);

        if let Some(mut lockfile) = lock {
            if !self.refreshes.is_empty() {
                for (&i, &stat) in self.refreshes.iter() {
                    index.refresh_entry_stat(i, stat);
                }
                lockfile.write(&index.serialize())?;
                lockfile.commit()?;
            }
        }
        Ok(())
    }

    fn scan_workspace(
        &mut self,
        workspace: &Workspace,
        index: &Index,
        prefix: &Path,
    ) -> Result<(), WorkspaceError> {
        for (path, stat) in workspace.dir_entries(prefix)? {
            // internally is_tracked() converts the OS specific path to an index path
            if index.is_tracked(&path) {
                if stat.mode == os::DIR {
                    // found a dir that contains at least 1 index entry, we recurse
                    self.scan_workspace(workspace, index, &path)?;
                } else {
                    // tracked file, pending to see if it is actually different from the Index Entry
                    self.stats.insert(index::path_as_bytes(&path), stat);
                }
            // for an untracked path to be shown in the report it must be either a file or a
            // non-empty directory. We will show untracked directories that actually contain
            // at least 1 file. An empty-directory contains no file to actually add in the index.
            } else if workspace.contains_trackable_file(&path, stat.mode)? {
                let mut name = os::name_as_bytes(path.as_ref()).to_vec();
                // for the a/b relative to root path we display it as a/b/ only if it is a dir
                if stat.mode == os::DIR {
                    name.push(b'/');
                }
                self.untracked.insert(name);
            }
            // we never descend into an empty directory
        }
        Ok(())
    }

    fn scan_index(&mut self, index: &Index, workspace: &Workspace) {
        for (index, entry) in index.entries.iter().enumerate() {
            match self.stats.get(&entry.path) {
                None => {
                    self.changes.insert(
                        entry.path.clone(),
                        Change {
                            head_index: None,
                            workspace_index: Some(WorkspaceIndexChange::DELETED),
                        },
                    );
                }
                // At this point the entry is found in both workspace and index, we need to check
                // if it has changed and if it did to refresh the index
                Some(&ws_stat) => {
                    // different size/mode -> modified
                    if entry.stat.file_size != ws_stat.file_size || entry.stat.mode != ws_stat.mode
                    {
                        self.changes.insert(
                            entry.path.clone(),
                            Change {
                                head_index: None,
                                workspace_index: Some(WorkspaceIndexChange::MODIFIED),
                            }
                        );
                        // this is the tricky part: Coglan mentions in 9.2.4 that a timestamp mismatch
                        // does not automatically mean modified, it means maybe changed, need to
                        // verify by reading and hashing the file. Because we can touch a file and
                        // change its mtime without changing its contents
                    } else if !times_match(&entry.stat, &ws_stat) {
                        let path = os::bytes_to_path(&entry.path);
                        // Note: pretty much every other call to read_file() used a path that was
                        // generated from the OS, but not now. Now the entry.path is the normalized
                        // vec that Index uses, and it is not platform specific, it is a sequence of
                        // components separated by '/'. foo/bar/baz is a valid index path, but if we
                        // tried to search on the workspace for the file pointed by that path we might
                        // miss it because workspace uses the underlying file systems and on Windows
                        // it should be foo\bar\baz. We have to create a platform specific Path from
                        // those bytes.
                        // TODO: the prefixed \\?\ paths on Windows
                        let content = workspace.read_file(&path).unwrap();
                        if db::hash(b"blob", &content) != entry.oid {
                            self.changes.insert(
                                entry.path.clone(),
                                Change {
                                    head_index: None,
                                    workspace_index: Some(WorkspaceIndexChange::MODIFIED),
                                }
                            );
                        } else {
                            // content is the same -> update index to refresh entry stat
                            self.refreshes.insert(index, ws_stat);
                        }
                    } // else: metadata match -> no changes, no I/O
                }
            }
        }
    }
    // TODO: make sure that when we track the paths we have / as file separator the same way Index
    // does it. Revisit when printing.
}

fn times_match(this: &StatNode, other: &StatNode) -> bool {
    this.ctime == other.ctime
        && this.ctime_nsec == other.ctime_nsec
        && this.mtime == other.mtime
        && this.mtime_nsec == other.mtime_nsec
}

impl From<RepoError> for StatusError {
    fn from(err: RepoError) -> Self {
        StatusError::Repository(err)
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

impl From<WorkspaceError> for StatusError {
    fn from(err: WorkspaceError) -> Self {
        StatusError::Workspace(err)
    }
}
