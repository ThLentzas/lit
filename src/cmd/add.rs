use crate::repo::db::{Database, DbError};
use crate::repo::index::{Index, IndexEntry, IndexError};
use crate::repo::lockfile::{Lockfile, LockfileError};
use crate::repo::object::Object;
use crate::repo::object::mode::Mode;
use crate::repo::os::{FileKind, StatNode};
use crate::repo::path::RepoPath;
use crate::repo::pathspec::{Pathspec, PathspecError};
use crate::repo::workspace::{Workspace, WorkspaceError};
use crate::repo::{RepoError, Repository};
use std::path::{Path, PathBuf};

pub(crate) struct Add {
    // user provided path
    pub(crate) paths: Vec<PathBuf>,
}

impl Add {
    // cwd -> where the user ran the command from
    // root -> directory that owns .lit
    //
    // Git commands can be run from inside the repo, not only the root
    // cd jolt/src/parser
    // git add lexer.rs
    // git commit
    //
    // Git still knows the repository root is jolt and the index path should be src/parser/lexer.rs
    // not just lexer.rs(what the user provided, self.path in our case)
    pub(super) fn execute(&self) -> Result<(), AddError> {
        let repo = Repository::discover()?;
        let db = Database {
            path: repo.db_path(),
        };
        let workspace = Workspace {
            root: repo.root.clone(),
        };
        let mut index = Index::new(repo.index_path());
        let mut lockfile = Lockfile::acquire(&index.path)?;
        index.load()?;

        for path in self.paths.iter() {
            // if the user called add . from root then the prefix is "" and the pathspec.pattern is
            // also  "". This is fine because in collect_entries() for the dir case we call dir_entries()
            // which does self.to_absolute() so absolute root + "" give us the absolute root path
            // which is what we want.
            let pathspec = if path.is_absolute() {
                Pathspec::new(path.as_os_str(), Path::new(""), &repo.root)?
            } else {
                let prefix = workspace.prefix()?;
                Pathspec::new(path.as_os_str(), &prefix, &repo.root)?
            };
            let mut collector = EntryCollector::new(&workspace, &db, &pathspec.pattern);
            collector.collect()?;
            index.add_entries(collector.finish())?;
        }
        if index.modified {
            lockfile.write(&index.serialize())?;
        }
        lockfile.commit()?;
        Ok(())
    }
}

// it was created to replace the initial approach where we had a single collect() method that did
// all the work, but we had to pass 5 arguments:
//  collect_entries(ws: &Workspace, rel: &Path, stat: StatNode, db: &Database, out: &mut Vec<IndexEntry>)
struct EntryCollector<'a> {
    workspace: &'a Workspace,
    db: &'a Database,
    path: &'a RepoPath,
    entries: Vec<IndexEntry>,
}

impl<'a> EntryCollector<'a> {
    fn new(workspace: &'a Workspace, db: &'a Database, path: &'a RepoPath) -> Self {
        Self {
            workspace,
            db,
            path,
            entries: Vec::new(),
        }
    }

    // standard recursive approach, collect is the function that triggers the recursion with some initial
    // state
    fn collect(&mut self) -> Result<(), AddError> {
        let stat = self.workspace.stat(&self.path)?;
        self.collect_entries(&self.path, stat)?;

        Ok(())
    }

    fn collect_entries(&mut self, path: &RepoPath, node: StatNode) -> Result<(), AddError> {
        match node.kind {
            FileKind::Regular(_) => {
                let content = self.workspace.read_file(&path)?;
                let oid = self.db.store(Object::Blob(content))?;
                let mode = Mode::try_from(node.kind).unwrap();
                self.entries
                    .push(IndexEntry::new(path.clone(), oid, mode, node.stat));
            }
            FileKind::Symlink => {
                // https://stackoverflow.com/questions/954560/how-does-git-handle-symbolic-links
                // the content of the blob is the target path as bytes
                // the file size is the length of the above sequence
                //
                // if the user deletes target, then we have a dangling reference which is allowed. 
                // It's up to the user to remove the symlink
                //
                // TODO: Test the following cases
                //
                // lstat's st_size for a symlink is the target's length in bytes, so node.stat.file_size
                // already equals the blob length, we store those bytes verbatim without normalizing
                // anything.
                //
                // For Windows, symlink metadata reports size 0, not target length. If we try to set
                // size = content.len() when we call status, the stat call there will return 0,
                // different size, we have to rehash and see that the oid are unchanged which is not
                // correct, can't have different size but identical hashes.
                let content = self.workspace.read_link(&path)?;
                let oid = self.db.store(Object::Blob(content))?;
                let mode = Mode::try_from(node.kind).unwrap();
                self.entries
                    .push(IndexEntry::new(path.clone(), oid, mode, node.stat));
            }
            FileKind::Directory => {
                // TODO: check if the mode returned by stat() contains permission bits
                // TODO: look at --ignore-errors flag
                for (path, stat) in self.workspace.dir_entries(&path)? {
                    self.collect_entries(&path, stat)?;
                }
            }
            // Unsupported file are silently ignored
            FileKind::Other => {}
        }
        Ok(())
    }

    fn finish(self) -> Vec<IndexEntry> {
        self.entries
    }
}

#[derive(Debug)]
pub(super) enum AddError {
    Repo(RepoError),
    Index(IndexError),
    DbError(DbError),
    WsError(WorkspaceError),
    Pathspec(PathspecError),
    Lockfile(LockfileError),
}

impl From<RepoError> for AddError {
    fn from(err: RepoError) -> Self {
        AddError::Repo(err)
    }
}

impl From<PathspecError> for AddError {
    fn from(err: PathspecError) -> Self {
        AddError::Pathspec(err)
    }
}

impl From<IndexError> for AddError {
    fn from(err: IndexError) -> Self {
        AddError::Index(err)
    }
}

impl From<DbError> for AddError {
    fn from(err: DbError) -> Self {
        AddError::DbError(err)
    }
}

impl From<WorkspaceError> for AddError {
    fn from(err: WorkspaceError) -> Self {
        AddError::WsError(err)
    }
}

impl From<LockfileError> for AddError {
    fn from(err: LockfileError) -> Self {
        AddError::Lockfile(err)
    }
}
