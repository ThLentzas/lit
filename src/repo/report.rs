use crate::repo::db::{Database, DbError};
use crate::repo::index::{Index, IndexEntry};
use crate::repo::object::mode::Mode;
use crate::repo::object::oid::Oid;
use crate::repo::object::{Object, OidError};
use crate::repo::os::{FileKind, StatNode};
use crate::repo::path::RepoPath;
use crate::repo::refs::{RefError, Refs};
use crate::repo::workspace::{Workspace, WorkspaceError};
use crate::repo::{Repository, db};
use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::fmt;

// green for new, orange for modified, red for deleted, something else for untracked
pub(crate) enum HeadIndexChange {
    Added,    // exists in the index but not in HEAD
    Modified, // exists in both but modified in the index
    Deleted,  // exists in HEAD but not in the index
}

pub(crate) enum WorkspaceIndexChange {
    //   UNTRACKED, // exists in the workspac but not in index
    Modified, // exists in both but modified in workspace
    Deleted,  // exists in index but not in workspace
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
pub(crate) struct Change {
    pub(crate) head_index: Option<HeadIndexChange>,
    pub(crate) workspace_index: Option<WorkspaceIndexChange>,
}

#[derive(Default)]
pub(crate) struct Report {
    head_entries: HashMap<RepoPath, (Oid, Mode)>,
    // HashMap is enough for stats, we never care about order, we use it when we check index against
    // workspace by making get() calls
    stats: HashMap<RepoPath, StatNode>,
    // we need to know the type of file because we want to display untracked dirs with a trailing
    // slash, a/b -> a/b/.
    // If we used a Map because this is the only place where we actually include directories in the
    // output we need to follow Git's rule with the trailing slash. When we add the RepoPath, the
    // relative root to dir path does not include the trailing slash, which means we need to walk
    // the map create a new iterator where for dir's we append the trailing slash, sort that and then
    // use the result to print. Map does nothing. Instead we use a vec apply the rules before printing
    // and we sort once.
    pub(crate) untracked: Vec<(RepoPath, FileKind)>,
    // changes always report files, so we never have to consider Git's rules about the trailing slash
    // for directories. Whatever order we get from, is later used to print the results.
    pub(crate) changes: BTreeMap<RepoPath, Change>,
    pub(crate) refreshes: Vec<(usize, StatNode)>,
}

impl Report {
    fn new() -> Self {
        Self::default()
    }

    pub(crate) fn generate(repo: &Repository, index: &Index) -> Result<Self, ReportError> {
        let mut report = Self::new();
        let db = Database {
            path: repo.db_path(),
        };
        let workspace = Workspace {
            root: repo.root.clone(),
        };
        let refs = Refs::new(&repo.root);
        report.load_head_entries(&refs, &db)?;
        report.scan_workspace(&workspace, index, &RepoPath::new())?;
        report.check_index_against(index, &workspace)?;
        report.check_staged_deletions(index);

        Ok(report)
    }

    // TODO: make sure that when we track the paths we have / as file separator the same way Index
    // does it. Revisit when printing.
    fn load_head_entries(&mut self, refs: &Refs, db: &Database) -> Result<(), ReportError> {
        // tried to read the HEAD and got nothing back -> first commit
        let Some(head_oid) = refs.read_head()? else {
            return Ok(());
        };

        let commit = match db.load(&Oid::from_hex(&head_oid)?)? {
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
        prefix: &RepoPath,
    ) -> Result<(), WorkspaceError> {
        for (path, node) in workspace.dir_entries(prefix)? {
            if index.is_tracked(&path) {
                if node.kind == FileKind::Directory {
                    // found a dir that contains at least 1 index entry, we recurse
                    self.scan_workspace(workspace, index, &path)?;
                } else {
                    // tracked file, pending to see if it is actually different from the Index Entry
                    self.stats.insert(path, node);
                }
            // for an untracked path to be shown in the report it must be either a file or a
            // non-empty directory. We will show untracked directories that actually contain
            // at least 1 file. An empty-directory contains no file to actually add in the index.
            } else if workspace.contains_trackable_file(&path, &node.kind)? {
                self.untracked.push((path, node.kind));
            }
            // we never descend into an empty directory
        }
        Ok(())
    }

    fn check_against_workspace(
        &mut self,
        workspace: &Workspace,
        pos: usize,
        entry: &IndexEntry,
    ) -> Result<(), WorkspaceError> {
        match self.stats.get(&entry.path) {
            None => {
                self.changes
                    .entry(entry.path.clone())
                    .or_default()
                    .workspace_index = Some(WorkspaceIndexChange::Deleted);
            }
            // At this point the entry is found in both workspace and index, we need to check
            // if it has changed and refresh the index.
            Some(&ws_stat) => {
                let mode_changed = Mode::try_from(ws_stat.kind)
                    // for a tracked regular file like src/foo we might have changed its type to
                    // some unsupported one like dev, or socket. We will report the change and that's
                    // it, Index will never hold a version of foo or any file that is unsupported.
                    // the good case is when the type is actually supported, but we still need to check
                    // if it is differernt
                    //
                    // didn't know about map_or()
                    // if err, it returns the default value, otherwise it apply the closure, default
                    // and the return value of the closure must be of the same type
                    .map_or(true, |mode| entry.mode != mode);
                // different size/mode -> modified
                if entry.stat.file_size != ws_stat.stat.file_size || mode_changed {
                    self.changes
                        .entry(entry.path.clone())
                        .or_default()
                        .workspace_index = Some(WorkspaceIndexChange::Modified);
                    // this is the tricky part: Coglan mentions in 9.2.4 that a timestamp mismatch
                    // does not automatically mean modified, it means maybe changed, need to
                    // verify by reading and hashing the file. Because we can touch a file and
                    // change its mtime without changing its contents
                } else if !entry.times_match(&ws_stat.stat) {
                    // Note: pretty much every other call to read_file() used a path that was
                    // generated from the OS, but not now. Now the entry.path is the normalized
                    // vec that Index uses, and it is not platform specific, it is a sequence of
                    // components separated by '/'. foo/bar/baz is a valid index path, but if we
                    // tried to search on the workspace for the file pointed by that path we might
                    // miss it because workspace uses the underlying file systems and on Windows
                    // it should be foo\bar\baz. We have to create a platform specific Path from
                    // those bytes.
                    // TODO: the prefixed \\?\ paths on Windows, handle this unwrap()
                    // if we blindly called fs::read_file(), for symlinks we would follow the path and return
                    // the target's content which is not what we store in the blob.
                    let content = if entry.mode.is_symlink() {
                        workspace.read_link(&entry.path)?
                    } else {
                        workspace.read_file(&entry.path)?
                    };
                    if db::hash(b"blob", &content) != entry.oid {
                        self.changes
                            .entry(entry.path.clone())
                            .or_default()
                            .workspace_index = Some(WorkspaceIndexChange::Modified);
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
        Ok(())
    }

    fn check_against_head(&mut self, entry: &IndexEntry) {
        match self.head_entries.get(&entry.path) {
            Some(&(oid, mode)) if entry.mode != mode || entry.oid != oid => {
                self.changes
                    .entry(entry.path.clone())
                    .or_default()
                    .head_index = Some(HeadIndexChange::Modified);
            }
            None => {
                self.changes
                    .entry(entry.path.clone())
                    .or_default()
                    .head_index = Some(HeadIndexChange::Added);
            }
            // no changes
            _ => {}
        }
    }

    // Initially I had a call outside the for loop : if self.head_entries().is_empty() where if true
    // I would iterate over all index entries and mark them as ADDED to avoid computing the hash when
    // .get() was guaranteed would return None, but the impl of map.get() internally computes the
    // hash only if the table is not empty so we can skip it.
    fn check_index_against(
        &mut self,
        index: &Index,
        workspace: &Workspace,
    ) -> Result<(), ReportError> {
        for (i, entry) in index.entries.iter().enumerate() {
            self.check_against_workspace(workspace, i, entry)?;
            self.check_against_head(entry);
        }
        Ok(())
    }

    // a path exists in HEAD, but not in the index, we need to stage that deletion
    fn check_staged_deletions(&mut self, index: &Index) {
        if self.head_entries.is_empty() {
            return;
        }

        for (key, _) in self.head_entries.iter() {
            if !index.contains(key) {
                self.changes.entry(key.clone()).or_default().head_index =
                    Some(HeadIndexChange::Deleted);
            }
        }
    }
}

#[derive(Debug)]
pub(crate) enum ReportError {
    Workspace(WorkspaceError),
    Database(DbError),
    Ref(RefError),
    HeadNotACommit { oid: String },
    HeadCommitNotFound { oid: String },
    HeadBadOid(OidError),
}

impl Error for ReportError {}

impl fmt::Display for ReportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReportError::Workspace(err) => write!(f, "{err}"),
            ReportError::Database(err) => write!(f, "{err}"),
            ReportError::Ref(err) => write!(f, "{err}"),
            ReportError::HeadNotACommit { oid } => {
                write!(f, "HEAD points to object {oid}, which is not a commit")
            }
            ReportError::HeadCommitNotFound { oid } => {
                write!(f, "HEAD points to missing commit {oid}")
            }
            ReportError::HeadBadOid(err) => {
                write!(f, "invalid object id in HEAD: {err}")
            }
        }
    }
}

impl From<WorkspaceError> for ReportError {
    fn from(err: WorkspaceError) -> Self {
        ReportError::Workspace(err)
    }
}

impl From<DbError> for ReportError {
    fn from(err: DbError) -> Self {
        ReportError::Database(err)
    }
}

impl From<RefError> for ReportError {
    fn from(err: RefError) -> Self {
        ReportError::Ref(err)
    }
}

impl From<OidError> for ReportError {
    fn from(err: OidError) -> Self {
        ReportError::HeadBadOid(err)
    }
}
