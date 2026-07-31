use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use crate::repo::db::{Database, DbError};
use crate::repo::index::{Index, IndexEntry, StatNode};
use crate::repo::object::Object;
use crate::repo::refs::{RefError, Refs};
use crate::repo::{index, Repository, db};
use crate::repo::workspace::{Workspace, WorkspaceError};

// green for new, orange for modified, red for deleted, something else for untracked
pub(super) enum HeadIndexChange {
    ADDED,    // exists in the index but not in HEAD
    MODIFIED, // exists in both but modified in the index
    DELETED,  // exists in HEAD but not in the index
}

pub(super) enum WorkspaceIndexChange {
    //   UNTRACKED, // exists in the workspac but not in index
    MODIFIED, // exists in both but modified in workspace
    DELETED,  // exists in index but not in workspace
}

// A file can participate in both comparisons, Index <-> HEAD, Workspace <-> Index.
//
// main.rs exists in HEAD, the version stored in the most recent commit
// main.rs exists in Index, the version that would be placed in the next commit
// main.rs also exists in the Workspace, the version currently on disc
//
// HEAD != Index: the file has staged changes.
// Index != Workspace: the file also has unstaged changes.
//
// We have main.rs in Workspace, we call add, now Index and Workspace has the same version, then
// we modify main.rs, now the version of Index and Workspace is different. commit, now HEAD <->
// Index have the same version. add now Workspace <-> Index have the same version and modifying main.rs
// again will give us 3 different versions of main.rs. status will report both transitions.
#[derive(Default)]
pub(super) struct Change {
    pub(super) head_index: Option<HeadIndexChange>,
    pub(super) workspace_index: Option<WorkspaceIndexChange>,
}

#[derive(Default)]
pub(super) struct Report {
    head_entries: HashMap<Vec<u8>, ([u8; 20], u32)>,
    stats: BTreeMap<Vec<u8>, StatNode>,
    pub(super) untracked: BTreeSet<Vec<u8>>,
    pub(super) changes: BTreeMap<Vec<u8>, Change>,
    pub(super) refreshes: Vec<(usize, StatNode)>,
}

impl Report {
    fn new() -> Self {
        Self::default()
    }

    pub(super) fn generate(repo: &Repository, index: &Index) -> Result<Self,ReportError> {
        let mut report = Self::new();
        let db = Database {
            path: repo.db_path(),
        };
        let workspace = Workspace {
            root: repo.root.clone(),
        };
        let refs = Refs {
            path: repo.refs_path(),
        };
        report.load_head_entries(&refs, &db)?;
        report.scan_workspace(&workspace, &index, Path::new(""))?;
        report.scan_index(&index, &workspace);
        report.check_staged_deletions(&index);

        Ok(report)
    }

    // TODO: make sure that when we track the paths we have / as file separator the same way Index
    // does it. Revisit when printing.
    fn load_head_entries(&mut self, refs: &Refs, db: &Database) -> Result<(), ReportError> {
        // tried to read the HEAD and got nothing back -> first commit
        let Some(head_oid) = refs.read_head()? else {
            return Ok(());
        };

        let commit = match db.load(&head_oid)? {
            Some(Object::Commit(commit)) => commit,
            Some(_) => return Err(ReportError::HeadNotACommit { oid: head_oid }),
            // retrieved the oid of HEAD but is missing from db.
            None => return Err(ReportError::HeadCommitNotFound { oid: head_oid }),
        };
        self.head_entries = db.load_tree_files(&commit.root_id)?;

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
            // TODO: pass an empty RepoPath to trigger the recursion
            if index.is_tracked(&path) {
                if stat.mode == os::DIR {
                    // found a dir that contains at least 1 index entry, we recurse
                    self.scan_workspace(workspace, index, &path)?;
                } else {
                    // tracked file, pending to see if it is actually different from the Index Entry
                    self.stats.insert(index::path_to_bytes(&path), stat);
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

    fn check_against_workspace(&mut self, workspace: &Workspace, pos: usize, entry: &IndexEntry) {
        match self.stats.get(&entry.path) {
            None => {
                self.changes
                    .entry(entry.path.clone())
                    .or_default()
                    .workspace_index = Some(WorkspaceIndexChange::DELETED);
            }
            // At this point the entry is found in both workspace and index, we need to check
            // if it has changed and refresh the index.
            Some(&ws_stat) => {
                // different size/mode -> modified
                if entry.stat.file_size != ws_stat.file_size || entry.stat.mode != ws_stat.mode {
                    self.changes
                        .entry(entry.path.clone())
                        .or_default()
                        .workspace_index = Some(WorkspaceIndexChange::MODIFIED);
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
                    // TODO: the prefixed \\?\ paths on Windows, handle this unwrap()
                    let content = workspace.read_file(&path).unwrap();
                    if db::hash(b"blob", &content) != entry.oid {
                        self.changes
                            .entry(entry.path.clone())
                            .or_default()
                            .workspace_index = Some(WorkspaceIndexChange::MODIFIED);
                    } else {
                        // this is the only moment where we want to record the change to update
                        // the index entry based on if we acquired the lock.
                        // in the previous 2 cases, size and oid mismatch initially I did
                        // entry.oid = oid where oid was the result of hash(). It was wrong because
                        // hash() computed an oid for an object that was never stored and the index
                        // now references a non-existing object.
                        self.refreshes.push((pos, ws_stat));
                    }
                } // else: metadata match -> no changes, no I/O
            }
        }
    }

    fn check_against_head(&mut self, entry: &IndexEntry) {
        match self.head_entries.get(&entry.path) {
            Some(&(oid, mode)) if entry.stat.mode != mode || entry.oid != oid => {
                self.changes
                    .entry(entry.path.clone())
                    .or_default()
                    .head_index = Some(HeadIndexChange::MODIFIED);
            }
            None => {
                self.changes
                    .entry(entry.path.clone())
                    .or_default()
                    .head_index = Some(HeadIndexChange::ADDED);
            }
            // no changes
            _ => {}
        }
    }

    // Initially I had a call outside the for loop : if self.head_entries().is_empty() where if true
    // I would iterate over all index entries and mark them as ADDED to avoid computing the hash when
    // .get() was guaranteed would return None, but the impl of map.get() internally computes the
    // hash only if the table is not empty so we can skip it.
    fn scan_index(&mut self, index: &Index, workspace: &Workspace) {
        for (i, entry) in index.entries.iter().enumerate() {
            self.check_against_workspace(workspace, i, &entry);
            self.check_against_head(entry);
        }
    }

    // a path exists in HEAD, but not in the index, we need to stage that deletion
    fn check_staged_deletions(&mut self, index: &Index) {
        if self.head_entries.is_empty() {
            return;
        }

        for (key, _) in self.head_entries.iter() {
            if !index.contains(&key) {
                self.changes
                    .entry(key.clone())
                    .or_default()
                    .head_index = Some(HeadIndexChange::DELETED);
            }
        }
    }
}

pub(crate) fn times_match(this: &StatNode, other: &StatNode) -> bool {
    this.ctime == other.ctime
        && this.ctime_nsec == other.ctime_nsec
        && this.mtime == other.mtime
        && this.mtime_nsec == other.mtime_nsec
}

#[derive(Debug)]
pub(super) enum ReportError {
    Workspace(WorkspaceError),
    DbError(DbError),
    RefError(RefError),
    HeadNotACommit { oid: String },
    HeadCommitNotFound { oid: String },
}

impl From<WorkspaceError> for ReportError {
    fn from(err: WorkspaceError) -> Self {
        ReportError::Workspace(err)
    }
}

impl From<DbError> for ReportError {
    fn from(err: DbError) -> Self {
        ReportError::DbError(err)
    }
}

impl From<RefError> for ReportError {
    fn from(err: RefError) -> Self {
        ReportError::RefError(err)
    }
}