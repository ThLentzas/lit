use crate::repo::db::{Database, DbError};
use crate::repo::index::{Index, IndexEntry, IndexError};
use crate::repo::lockfile::{Lockfile, LockfileError};
use crate::repo::object::Object;
use crate::repo::object::mode::Mode;
use crate::repo::os::{FileKind, StatNode};
use crate::repo::path::RepoPath;
use crate::repo::pathspec::{Pathspec, PathspecError};
use crate::repo::workspace::{Workspace, WorkspaceError};
use crate::repo::{DiscoverError, Repository};
use std::error::Error;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::{fmt, io};

#[derive(Debug)]
pub(crate) struct Add {
    // user provided path
    pub(crate) paths: Vec<PathBuf>,
}

// TODO: when we add .litignore support, if a file is already tracked by lit and then added to .litignore
// we still update it. .litignore rules apply for untracked files.
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

            let node = match workspace.stat(&pathspec.pattern) {
                Ok(node) => Some(node),
                Err(WorkspaceError::Io { source, .. })
                    if source.kind() == io::ErrorKind::NotFound =>
                {
                    None
                }
                Err(err) => return Err(err.into()),
            };

            match node {
                Some(node) => {
                    let mut collector =
                        EntryCollector::new(&workspace, &db, &pathspec.pattern, &index, &node);
                    collector.collect()?;
                    index.add_entries(collector.finish())?;
                }
                // Case: a deleted file that index used to keep track of
                //
                // this is referred to as a stage deletion because index no longer contains the specific
                // file next time we ran status or commit HEAD will have the file, but index will not
                None if index.is_tracked(&pathspec.pattern) => {
                    index.remove(&pathspec.pattern);
                }
                None => {
                    return Err(AddError::FoundNoMatch {
                        path: pathspec.original,
                    });
                }
            }
        }
        if index.modified {
            lockfile.write(&index.serialize())?;
            lockfile.commit()?;
        }
        Ok(())
    }
}

struct EntryCollector<'a> {
    workspace: &'a Workspace,
    db: &'a Database,
    path: &'a RepoPath,
    index: &'a Index,
    node: &'a StatNode,
    entries: Vec<IndexEntry>,
}

impl<'a> EntryCollector<'a> {
    fn new(
        workspace: &'a Workspace,
        db: &'a Database,
        path: &'a RepoPath,
        index: &'a Index,
        node: &'a StatNode,
    ) -> Self {
        Self {
            workspace,
            db,
            path,
            index,
            node,
            entries: Vec::new(),
        }
    }

    // standard recursive approach, collect is the function that triggers the recursion with some
    // initial state
    fn collect(&mut self) -> Result<(), AddError> {
        self.collect_entries(self.path, self.node)
    }

    fn collect_entries(&mut self, path: &RepoPath, node: &StatNode) -> Result<(), AddError> {
        match node.kind {
            FileKind::Regular(_) => {
                let content = self.workspace.read_file(path)?;
                let oid = self.db.store(Object::Blob(content))?;
                // Safe to unwarp because file kind is regular and has a corresponding Mode
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
                let content = self.workspace.read_link(path)?;
                let oid = self.db.store(Object::Blob(content))?;
                // Safe to unwarp because file kind is symlink and has a corresponding Mode
                let mode = Mode::try_from(node.kind).unwrap();
                self.entries
                    .push(IndexEntry::new(path.clone(), oid, mode, node.stat));
            }
            FileKind::Directory => {
                // TODO: check if the mode returned by stat() contains permission bits
                // TODO: look at --ignore-errors flag
                for (path, node) in self.workspace.dir_entries(path)? {
                    self.collect_entries(&path, &node)?;
                }
            }
            FileKind::Other => {
                // this is the case where a tracked file gets deleted and a new file with the same
                // name but unsupported type is created. Because the old tracked file and the new
                // unsupported one have the same name(path) Git refuses to update the Index entry.
                //
                // src/foo was a regular tracked file that was deleted and now src/foo is socket, and
                // we try lit add src/foo it must fail
                if self.index.contains(path) {
                    return Err(AddError::UnsupportedFileType { path: path.clone() });
                }
                // Untracked unsupported files are silently ignored
            }
        }
        Ok(())
    }

    fn finish(self) -> Vec<IndexEntry> {
        self.entries
    }
}

#[derive(Debug)]
pub(super) enum AddError {
    Repository(DiscoverError),
    Index(IndexError),
    Database(DbError),
    Workspace(WorkspaceError),
    Pathspec(PathspecError),
    Lockfile(LockfileError),
    UnsupportedFileType { path: RepoPath },
    // user provided path verbatim
    FoundNoMatch { path: OsString },
}

impl Error for AddError {}

impl fmt::Display for AddError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AddError::Repository(err) => write!(f, "{err}"),
            // the actual index error is kept internally but no specific information is provided
            // the user can't really do much with an error like bad padding at ...
            AddError::Index(err) => {
                if err.is_format_error() {
                    write!(f, "invalid index format")
                } else {
                    // Io errors are reported
                    write!(f, "{err}")
                }
            }
            // TODO: maybe same behavior as Index?
            AddError::Database(err) => write!(f, "{err}"),
            AddError::Workspace(err) => write!(f, "{err}"),
            AddError::Pathspec(err) => write!(f, "{err}"),
            AddError::Lockfile(err) => write!(f, "{err}"),
            AddError::UnsupportedFileType { path } => {
                write!(f, "unsupported file type at '{}'", path.display())
            }
            AddError::FoundNoMatch { path } => {
                write!(
                    f,
                    "path '{}' did not match any files",
                    path.to_string_lossy()
                )
            }
        }
    }
}

impl From<DiscoverError> for AddError {
    fn from(err: DiscoverError) -> Self {
        AddError::Repository(err)
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
        AddError::Database(err)
    }
}

impl From<WorkspaceError> for AddError {
    fn from(err: WorkspaceError) -> Self {
        AddError::Workspace(err)
    }
}

impl From<LockfileError> for AddError {
    fn from(err: LockfileError) -> Self {
        AddError::Lockfile(err)
    }
}
