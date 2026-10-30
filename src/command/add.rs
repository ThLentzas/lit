use crate::repo::config::{ConfigFile, ConfigFileError};
use crate::repo::db::{Database, DatabaseError};
use crate::repo::index::entry::{IndexEntry, IndexEntryMetadata};
use crate::repo::index::{Index, IndexError};
use crate::repo::lockfile::{Lockfile, LockfileError};
use crate::repo::object::Object;
use crate::repo::object::mode::Mode;
use crate::repo::os::{FileKind, IoError, OsPath, OsPathError, StatNode};
use crate::repo::pathspec::{Pathspec, PathspecError};
use crate::repo::repopath::RepoPath;
use crate::repo::workspace::{Workspace, WorkspaceError};
use crate::repo::{Repository, db};
use clap::Args;
use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;

// refresh updates the index stat information without staging any changes, and it is an operation
// rather than a different `Selection` variant
//
// scope is determined by pathspec. If present, the scope is the supplied paths, otherwise entire
// worktree.
//
// This is similar to `Target` in `init` when based on what the user provided we determine worktree
// and metadata dirs. The rest of the flags are independent and will check them after we made a
// `Selection`
enum Selection {
    AllChanges,
    TrackedChanges,
    AdditionsAndModifications,
}

enum Operation {
    Interactive,
    Refresh,
    Stage(Selection),
}

// Conflicts: Git accepts `-u`, `-A` with `refresh` or `interactive`. For example, `refresh -A` and
// `refresh -u` execute just `refresh`. `refresh -u -A` is a conflict error.
// Cases like `lit add -u -no-all -A` fail. `-A` cancels `no-all` and then it conflicts with `u`.
// Clap handles this by first checking for overrides and then for conflicts.
//
// `lit add -i --refresh` and `lit add --refresh -i` both enter interactive mode because argument
// order does not affect operation precedence
#[derive(Debug, Args)]
pub(crate) struct Add {
    #[arg(short = 'v', long)]
    verbose: bool,
    #[arg(short = 'n', long)]
    dry_run: bool,
    #[arg(short = 'f', long)]
    force: bool,
    #[arg(short = 'i', long, conflicts_with = "dry_run")]
    interactive: bool,
    #[arg(
        short = 'A',
        long,
        visible_alias = "no-ignore-removal",
        // when both `no_all` and `all` are provided last one wins
        // https://docs.rs/clap/latest/clap/struct.Arg.html#method.overrides_with
        overrides_with = "no_all",
        // both specify which changes to stage
        // update stages tracked modifications and deletions but not untracked files, all includes
        // those too
        conflicts_with = "update"
    )]
    // equivalent to `git add .` when called from root
    all: bool,
    // TODO: review
    //  git accepts `-u -no-all` while rejects `-u -A` I couldn't understand why so for now I reject
    //  `update` includes stage deletions which contradicts the `--no-all/ignore-removal`
    #[arg(long, visible_alias = "ignore-removal", conflicts_with = "update")]
    no_all: bool,
    #[arg(short = 'u', long)]
    update: bool,
    refresh: bool,
    ignore_errors: bool,
    // empty = not provided, could also wrap it in an Option
    paths: Vec<PathBuf>,
}

// TODO: when we add .litignore support, if a file is already tracked by lit and then added to .litignore
//  we still update it. .litignore rules apply for untracked files.
//
// `Removal` in this context means removing an entry from the index because its file was deleted
// from the working tree.
// `ignore-removal` leaves deleted files' index entries intact, same as `--no-all`, `no-ignore-removal`
// does not. `git add <pathspec>` includes them. These options control whether `add` stages deletions
// `add` removes the file's entry from the index, so the next commit will emit that file.
//
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
    pub(super) fn execute(&self, repository: &Repository) -> Result<(), AddError> {
        let db = repository.database();
        // always safe for commands that require a working tree
        let workspace = repository.workspace().unwrap();
        let mut index = repository.index();
        let config = repository.config()?;
        let mut lockfile = Lockfile::acquire(index.path())?;
        index.load()?;
        let op = self.resolve_operation();

        for path in self.paths.iter() {
            // if the user called add . from root then the prefix is "" and the pathspec.pattern is
            // also "". This is fine because in collect_entries() for the dir case we call dir_entries()
            // which does self.to_absolute() so absolute root + "" give us the absolute root path
            // which is what we want.
            let prefix = if path.is_absolute() {
                None
            } else {
                Some(workspace.prefix()?)
            };
            let pathspec = Pathspec::new(path.as_os_str(), prefix.as_deref(), workspace.root())?;
            let pattern = pathspec.pattern();
            let node = match workspace.stat(pattern) {
                Ok(node) => Some(node),
                Err(err) if err.is_not_found() => None,
                Err(err) => return Err(AddError::Io(err)),
            };

            match node {
                Some(node) => {
                    let mut collector =
                        EntryCollector::new(&workspace, &db, pattern, &index, &node);
                    collector.collect()?;
                    index.add_entries(collector.finish())?;
                }
                // Case: a deleted file that index used to keep track of
                //
                // this is referred to as a staged deletion because index no longer contains the specific
                // file next time we run status or commit HEAD will have the file, but index will not
                // TODO: what happens if the provided path is a deleted directory that index contained
                //  entries, it was tracked, what is the behavior in this case? remove() will fail
                //  because dir's path itself does not exist on the index.
                None if index.is_tracked(pattern) => {
                    index.remove(pattern);
                }
                None => {
                    return Err(AddError::FoundNoMatch {
                        path: pathspec.original().to_os_string(),
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

    // in terms of precedence interactive > refresh > any stage flag
    fn resolve_operation(&self) -> Option<Operation> {
        if self.interactive {
            return Some(Operation::Interactive);
        }

        if self.refresh {
            return Some(Operation::Refresh);
        }

        if self.update {
            Some(Operation::Stage(Selection::TrackedChanges))
        } else if self.no_all {
            Some(Operation::Stage(Selection::AdditionsAndModifications))
        } else if self.all {
            Some(Operation::Stage(Selection::AllChanges))
        } else {
            None
        }
    }

    fn setup_update(
        &self,
        filemode: bool,
        workspace: &Workspace,
        index: &mut Index,
        pathspec: &Pathspec,
    ) -> Result<(), AddError> {
        let pattern = pathspec.pattern();
        let node = match workspace.stat(pattern) {
            Ok(node) => Some(node),
            // TODO: this should be an error? and ignored if `ignore-errors` is set?
            Err(err) if err.is_not_found() => None,
            Err(err) => return Err(AddError::Io(err)),
        };
        let Some(entry) = index.get(pattern) else {
            // untracked, if `node` is `Some` and `node.kind` is other/unsupported, no error is returned
            // we silently skip it
            return Ok(());
        };

        let paths = index
            .iter()
            .filter(|&entry| {
                // user provided `.`
                pattern.as_bytes().is_empty()
                    // exact match any regular file, symlinks
                    || pattern == entry.path()
                    // if the provided path is a directory we include its entries
                    || pattern.is_parent_of(entry.path())
            })
            // TODO: review the design
            //  with the current design and approach clone is unavoidable. Without `map` we get back
            //  a `Vec<&IndexEntry>`, and the returned references keep the immutable borrow of
            //  `index.iter()` alive for as long as needed. Later when we call `update_tracked()` we
            //  have to mutate the index, and at this point we have 2 both borrows alive and we try
            //  to use them both which is not allowed.
            .map(|entry| entry.path().clone())
            .collect::<Vec<_>>();

        for path in paths {
            self.update_tracked(filemode, workspace, index, &path);
        }

        // let node = match node {
        //     Some(node) => node,
        //     // Case: a deleted file that index used to keep track of
        //     //
        //     // this is referred to as a staged deletion because index no longer contains the specific
        //     // file next time we run status or commit HEAD will have the file, but index will not
        //     // TODO: what happens if the provided path is a deleted directory that index contained
        //     //  entries, it was tracked, what is the behavior in this case? remove() will fail
        //     //  because dir's path itself does not exist on the index.
        //     None if index.is_tracked(path) => {
        //         index.remove(path);
        //         return Ok(());
        //     }
        //     None => {
        //         return Err(AddError::FoundNoMatch {
        //             path: pathspec.original().to_os_string(),
        //         });
        //     }
        // };
        //
        // let (mode, content) = match node.kind {
        //     FileKind::Regular(exe) => {
        //         // if filemode is true, trust the current mode, otherwise keep the existing
        //         // one
        //         let mode = if filemode {
        //             if exe { Mode::Executable } else { Mode::Regular }
        //         } else {
        //             match entry.mode() {
        //                 Mode::Regular => Mode::Regular,
        //                 Mode::Executable => Mode::Executable,
        //                 // this is the case where the entry previously represented a
        //                 // symlink, we can't keep the old mode because we would label
        //                 // the new regular file as a symlink
        //                 _ => Mode::Regular,
        //             }
        //         };
        //         let content = workspace.read_file(path)?;
        //         (mode, content)
        //     }
        //     FileKind::Symlink => (Mode::Symlink, workspace.read_link(path)?),
        //     FileKind::Directory => (Mode::Symlink, Vec::new()),
        //     FileKind::Other => {
        //         if index.contains(pathspec.pattern()) {
        //             return Err(AddError::UnsupportedFileType { path: path.clone() });
        //         }
        //         unreachable!("index does not contain unsupported files")
        //     }
        // };
        Ok(())
    }

    fn update_tracked(
        &self,
        filemode: bool,
        workspace: &Workspace,
        index: &mut Index,
        path: &RepoPath,
    ) {
    }

    // fn update(
    //     &self,
    //     filemode: bool,
    //     workspace: &Workspace,
    //     db: &Database,
    //     index: &mut Index,
    //     path: &RepoPath,
    // ) -> Result<(), AddError> {
    //     let node = match workspace.stat(path) {
    //         Ok(node) => Some(node),
    //         // TODO: this should be an error? and ignored if `ignore-errors` is set?
    //         Err(err) if err.is_not_found() => None,
    //         Err(err) => return Err(AddError::Io(err)),
    //     };
    //
    //     match node {
    //         Some(node) => {
    //             if let Some(existing_entry) = index.get(path) {
    //                 match node.kind {
    //                     FileKind::Regular(exe) => {
    //                         // if filemode is true, trust the current mode, otherwise keep the existing
    //                         // one
    //                         let mode = if filemode {
    //                             if exe { Mode::Executable } else { Mode::Regular }
    //                         } else {
    //                             match existing_entry.mode() {
    //                                 Mode::Regular => Mode::Regular,
    //                                 Mode::Executable => Mode::Executable,
    //                                 // this is the case where the entry previously represented a
    //                                 // symlink, we can't keep the old mode because we would label
    //                                 // the new regular file as a symlink
    //                                 _ => Mode::Regular,
    //                             }
    //                         };
    //
    //                         if existing_entry.metadata().times_match(&node.stat)
    //                             && *existing_entry.mode() == mode
    //                         {
    //                             return Ok(());
    //                         }
    //                         let content = workspace.read_file(path)?;
    //                         // A simple check `if *existing_entry.oid() != db::hash(b"blob", &content)`
    //                         // is not enough, it could bypass a collision? We let store handle it
    //                         let oid = db.store(Object::Blob(content))?;
    //                         let metadata = IndexEntryMetadata {
    //                             stat: node.stat,
    //                             mode,
    //                             oid,
    //                             flags: 0,
    //                         };
    //                         index.update_entry(path, metadata);
    //                     }
    //                     FileKind::Symlink => {}
    //                     FileKind::Directory => {}
    //                     FileKind::Other => {}
    //                 }
    //             }
    //         }
    //         // Case: a deleted file that index used to keep track of
    //         //
    //         // this is referred to as a staged deletion because index no longer contains the specific
    //         // file next time we run status or commit HEAD will have the file, but index will not
    //         // TODO: what happens if the provided path is a deleted directory that index contained
    //         //  entries, it was tracked, what is the behavior in this case? remove() will fail
    //         //  because dir's path itself does not exist on the index.
    //         None if index.is_tracked(path) => {
    //             index.remove(path);
    //         }
    //         None => {}
    //     }
    //
    //     Ok(())
    // }
}

// 'a: for how long Collector can borrow workspace database
// 'repo: for how long workspace/database borrows their paths from repository
// we could drop `path` and `node` since they are only used once to start the recursion and we could
// pass them to `collect()` instead of keeping them internally.
struct EntryCollector<'a, 'repo> {
    workspace: &'a Workspace<'repo>,
    database: &'a Database<'repo>,
    path: &'a RepoPath,
    index: &'a Index,
    node: &'a StatNode,
    entries: Vec<IndexEntry>,
}

impl<'a, 'repo> EntryCollector<'a, 'repo> {
    fn new(
        workspace: &'a Workspace<'repo>,
        db: &'a Database<'repo>,
        path: &'a RepoPath,
        index: &'a Index,
        node: &'a StatNode,
    ) -> Self {
        Self {
            workspace,
            database: db,
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
            FileKind::Regular(exe) => {
                let content = self.workspace.read_file(path)?;
                let oid = self.database.store(Object::Blob(content))?;
                let mode = if exe { Mode::Executable } else { Mode::Regular };
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
                let oid = self.database.store(Object::Blob(content))?;
                self.entries
                    .push(IndexEntry::new(path.clone(), oid, Mode::Symlink, node.stat));
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
pub(crate) enum AddError {
    Config(ConfigFileError),
    Index(IndexError),
    Database(DatabaseError),
    OsPath(OsPathError),
    Workspace(WorkspaceError),
    Pathspec(PathspecError),
    Lockfile(LockfileError),
    Io(IoError),
    UnsupportedFileType { path: RepoPath },
    // user provided path verbatim
    FoundNoMatch { path: OsString },
}

impl Error for AddError {}

impl fmt::Display for AddError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AddError::Config(err) => write!(f, "{err}"),
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
            AddError::Io(_) => write!(f, "I/O error during add"),
            _ => todo!(),
        }
    }
}

impl From<ConfigFileError> for AddError {
    fn from(err: ConfigFileError) -> Self {
        Self::Config(err)
    }
}

impl From<PathspecError> for AddError {
    fn from(err: PathspecError) -> Self {
        AddError::Pathspec(err)
    }
}

impl From<IoError> for AddError {
    fn from(err: IoError) -> Self {
        Self::Io(err)
    }
}

impl From<IndexError> for AddError {
    fn from(err: IndexError) -> Self {
        AddError::Index(err)
    }
}

impl From<DatabaseError> for AddError {
    fn from(err: DatabaseError) -> Self {
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
