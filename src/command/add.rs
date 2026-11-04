use crate::repo::config::{ConfigFile, ConfigFileError};
use crate::repo::db::{Database, DatabaseError};
use crate::repo::index::entry::{IndexEntry, IndexEntryMetadata};
use crate::repo::index::{Index, IndexError};
use crate::repo::lockfile::{Lockfile, LockfileError};
use crate::repo::object::Object;
use crate::repo::object::mode::Mode;
use crate::repo::os::{FileKind, IoError, OsPathError, StatNode};
use crate::repo::pathspec::{Pathspec, PathspecError};
use crate::repo::repopath::RepoPath;
use crate::repo::workspace::{Workspace, WorkspaceError};
use crate::repo::{Repository, environment};
use clap::Args;
use std::collections::{BTreeSet, HashSet};
use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;

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
    //  git accepts `-u -no-all` while rejects `-u -A` I couldn't understand why, so for now I
    //  reject both
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
// we still update it. .litignore rules apply for untracked files.
//
// `Removal` in this context means removing an entry from the index because its file was deleted
// from the working tree.
// `ignore-removal` leaves deleted files' index entries intact, same as `--no-all`, `no-ignore-removal`
// does not. `git add <pathspec>` includes them. These options control whether `add` stages deletions
//
// staged deletion: `add` removes the file's entry from the index, so the next commit will emit that
// file.
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
        // always safe for commands that require a working tree
        let workspace = repository.workspace().unwrap();
        let mut index = repository.index();
        let db = repository.database();
        let cfg = repository.config()?;
        let mut pathspecs = Vec::with_capacity(self.paths.len());
        let cwd = environment::cwd()?;

        for path in self.paths.iter() {
            let prefix = if path.is_absolute() {
                None
            } else {
                // if the user called `add .` from root, the prefix is empty and the `pathspec.pattern`
                // is also empty. This is intended because when workspace makes any fs calls will
                // first do `self.to_abolute(), which for an empty path results to the absolute
                // path to root.
                Some(workspace.path_to_root_prefix(&cwd)?)
            };
            let pathspec = Pathspec::new(path.as_os_str(), prefix, workspace.root())?;
            pathspecs.push(pathspec);
        }

        // TODO: defer that until we are sure that `add` is valid
        let mut lockfile = Lockfile::acquire(index.path())?;
        index.load()?;
        let op = self.resolve_operation();
        if let Some(op) = op {
            match op {
                Operation::Interactive => {}
                Operation::Refresh => {}
                Operation::Stage(selection) => match selection {
                    // TODO: add support for ignoring files, we already have a `.litignore` parser
                    Selection::AllChanges => {
                        self.stage_worktree_changes(&cfg, &workspace, &db, &mut index, &pathspecs)?;
                    }
                    // there is a flag check in `updated_tracked_entry()` that does not stages
                    // deletions if `no_all` is set. The rest of the work for `all`, `no_all` is
                    // identical
                    Selection::AdditionsAndModifications => {
                        // if no paths are provided, this option is a no-op
                        if !pathspecs.is_empty() {
                            self.stage_worktree_changes(
                                &cfg, &workspace, &db, &mut index, &pathspecs,
                            )?;
                        }
                    }
                    Selection::TrackedChanges => {
                        self.update(&cfg, &workspace, &db, &mut index, &pathspecs)?;
                    }
                },
            }
        } else {
        }

        if index.is_modified() {
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

    fn stage_worktree_changes(
        &self,
        cfg: &ConfigFile,
        workspace: &Workspace,
        db: &Database,
        index: &mut Index,
        pathspecs: &[Pathspec],
    ) -> Result<(), AddError> {
        let mut untracked = Vec::new();
        let mut seen = HashSet::new();

        if pathspecs.is_empty() {
            collect_untracked_paths(
                workspace,
                index,
                &RepoPath::new(),
                &mut untracked,
                &mut seen,
            )?;
        } else {
            for pathspec in pathspecs {
                collect_untracked_paths(
                    workspace,
                    index,
                    pathspec.pattern(),
                    &mut untracked,
                    &mut seen,
                )?;
            }
        };

        let mut positions = BTreeSet::new();
        let mut matches = vec![false; pathspecs.len()];
        // If no `pathspec` was provided, the entire index is updated(all tracked files in the entire
        // working tree are updated)
        if pathspecs.is_empty() {
            // let iter = index.iter().enumerate().map(|(pos, _)| pos);
            // positions.extend(iter);
            positions.extend(0..index.len());
        } else {
            for (i, pathspec) in pathspecs.iter().enumerate() {
                let pattern = pathspec.pattern();
                let mut matched = false;
                for (j, entry) in index.iter().enumerate() {
                    // user provided `.`
                    // exact match any regular file, symlinks
                    // if the provided path is a directory check if any of its entries are tracked
                    if pattern.as_bytes().is_empty()
                        || pattern == entry.path()
                        || pattern.is_parent_of(entry.path())
                    {
                        matched = true;
                        positions.insert(j);
                    }
                }
                // in `update` when we had no match we would return `NotFound` but now we refer to
                // an untracked file/non-existing path
                matches[i] = matched;
            }
        }

        // `pathspces` and `matches` have the same length
        // `zip` gives us the one to one mapping, pathspec[i], matches[i]
        // instead we could call enumerate on `pathspecs.iter()` and access the pair via index
        for (pathspec, &tracked_match) in pathspecs.iter().zip(&matches) {
            // tracked entry, might not exist in the working tree(staged deletion)
            if tracked_match {
                continue;
            }
            match workspace.stat(pathspec.pattern()) {
                // new existing file
                Ok(_) => {}
                // doesn't exist on the workspace and is not a staged deletion(index would return
                // true)
                Err(err) if err.is_not_found() => {
                    return Err(AddError::FoundNoMatch(pathspec.original().to_os_string()));
                }
                Err(err) => return Err(AddError::Io(err)),
            }
        }

        let filemode = filemode(cfg)?;
        let symlinks = symlinks(cfg)?;
        for position in positions.into_iter().rev() {
            self.update_tracked_entry(workspace, index, db, filemode, symlinks, position)?;
        }

        for (path, node) in untracked {
            let (content, mode) = match node.kind {
                FileKind::Regular(exec) => {
                    let content = workspace.read_file(&path)?;
                    // trust the executable bit, otherwise fallback to regular
                    let mode = if filemode && exec {
                        Mode::Executable
                    } else {
                        Mode::Regular
                    };
                    (content, mode)
                }
                _ => (workspace.read_link(&path)?, Mode::Symlink),
            };
            let oid = db.store(Object::Blob(content))?;
            let entry = IndexEntry::new(path, oid, mode, node.stat);
            let pos = index.insert(entry)?;

            if self.verbose {
                // TODO: we need ReadableBytes here from print.rs
                let entry = unsafe { index.get_unchecked(pos) };
                println!("add '{}'", entry.path().display())
            }
        }
        Ok(())
    }

    // index: [src/a.rs, src/b.rs]
    // `lit add -u src src/a.rs`
    //
    //  if the caller iterated through `pathspecs` and invoked `update` for each we have the
    //  following issue.
    //      We process `src` first which removes from the index both entries. Next, when we try to
    //      process `src/a.rs` it fails because we have no matching entries, and we return a `NotFound`
    //      error. This is incorrect according to git, because when the command was invoked
    //      `src/a.rs` was tracked.
    //  Instead, for every `pathspec` we check against index and track the positions where they
    //  match. If a path has no match that is a `NotFound` error. We use a `BtreeSet` to deduplicate
    //  from cases like the above where we track `0` the position of `src/a.rs` twice.
    fn update(
        &self,
        cfg: &ConfigFile,
        workspace: &Workspace,
        db: &Database,
        index: &mut Index,
        pathspecs: &[Pathspec],
    ) -> Result<(), AddError> {
        let mut positions = BTreeSet::new();
        // If no `pathspec` was provided, the entire index is updated(all tracked files in the entire
        // working tree are updated)
        if pathspecs.is_empty() {
            // let iter = index.iter().enumerate().map(|(pos, _)| pos);
            // positions.extend(iter);
            positions.extend(0..index.len());
        } else {
            for pathspec in pathspecs {
                let pattern = pathspec.pattern();
                let mut matched = false;
                for (position, entry) in index.iter().enumerate() {
                    // user provided `.`
                    // exact match any regular file, symlinks
                    // if the provided path is a directory check if any of its entries are tracked
                    if pattern.as_bytes().is_empty()
                        || pattern == entry.path()
                        || pattern.is_parent_of(entry.path())
                    {
                        matched = true;
                        positions.insert(position);
                    }
                }
                // a provided path with no match is an error
                if !matched {
                    return Err(AddError::FoundNoMatch(pathspec.original().to_os_string()));
                }
            }
        }

        let filemode = filemode(cfg)?;
        let symlinks = symlinks(cfg)?;
        // `positions` is in ascending order. Removing an entry shifts everything to its
        // right one slot left, so removing from the front would leave every remaining
        // position stale after the first removal.
        // Removing from the back avoid this. A removal only shifts entries to the right of
        // it, which we have already handled, so the positions still to be processed(all to
        // the left) remain valid.
        for position in positions.into_iter().rev() {
            self.update_tracked_entry(workspace, index, db, filemode, symlinks, position)?;
        }
        Ok(())
    }

    fn update_tracked_entry(
        &self,
        workspace: &Workspace,
        index: &mut Index,
        db: &Database,
        filemode: bool,
        symlinks: bool,
        pos: usize,
    ) -> Result<(), AddError> {
        let entry = unsafe { index.get_unchecked(pos) };
        let node = match workspace.stat(entry.path()) {
            Ok(node) => node,
            // exists in the index but not in the workspace => staged deletion
            Err(err) if err.is_not_found() => {
                if self.no_all {
                    return Ok(());
                }
                // SAFETY: every position points to an existing index entry
                let entry = unsafe { index.remove_unchecked(pos) };
                if self.verbose {
                    println!("remove '{}'", entry.path().display())
                }
                return Ok(());
            }
            // TODO: this should be an error? and ignored if `ignore-errors` is set?
            Err(err) => return Err(AddError::Io(err)),
        };

        let update = match node.kind {
            FileKind::Regular(exec) => {
                // On fs without symlink support, a tracked symlink appears as a regular file containing
                // its target text. If `core.symlinks = false` and the existing entry's mode is `Symlink`
                // we treat the regular file as a symlink, and later we call `read_link()` instead of
                // `read_file()`
                // https://github.com/git/git/blob/50fe5cc63f9cd6e89b87a2c9ccf96a444f4cfdb7/read-cache.h#L9-L29
                let mode = if !symlinks && *entry.mode() == Mode::Symlink {
                    Mode::Symlink
                // if filemode is true, trust the current mode, otherwise keep the existing one
                } else if filemode {
                    if exec {
                        Mode::Executable
                    } else {
                        Mode::Regular
                    }
                } else {
                    match entry.mode() {
                        Mode::Executable => Mode::Executable,
                        _ => Mode::Regular,
                    }
                };
                finalize_blob_update(workspace, db, index, pos, mode, &node)?
            }
            FileKind::Symlink => {
                finalize_blob_update(workspace, db, index, pos, Mode::Symlink, &node)?
            }
            // this is the case where a tracked entry, a blob or a symlink, is now a directory, we
            // stage the deletion
            FileKind::Directory => {
                if self.no_all {
                    return Ok(());
                }
                let entry = unsafe { index.remove_unchecked(pos) };
                EntryUpdate::Removed(entry)
            }
            FileKind::Other => {
                // TODO: adjust based on `ignore-errors`
                return Err(AddError::UnsupportedFileType(entry.path().clone()));
            }
        };

        // let entry = index.get_unchecked(pos); // we borrow from index immutably
        // finalize_blob_update();
        // println(...entry.path().diplay()); // keeps the immutable borrow alive since we use it
        // after mutating the index
        //
        // Also the above example won't work for another reason, which is that the removed `entry`
        // from the vector gets deallocated, and we can no longer access the path via `entry.path()`
        if self.verbose {
            match update {
                // in those 2 cases, git prints nothing
                EntryUpdate::Unchanged | EntryUpdate::Refreshed => {}
                EntryUpdate::Updated => {
                    // https://github.com/git/git/blob/50fe5cc63f9cd6e89b87a2c9ccf96a444f4cfdb7/read-cache.c#L815-L816
                    // it is safe to index because `update` never modifies the position or the path'
                    // of the entry. This is a new borrow and we avoid the issue mentioned above.
                    // TODO: use ReadableBytes from print.rs
                    let entry = unsafe { index.get_unchecked(pos) };
                    println!("add '{}'", entry.path().display())
                }
                // https://github.com/git/git/blob/50fe5cc63f9cd6e89b87a2c9ccf96a444f4cfdb7/read-cache.c#L636-L648
                EntryUpdate::Removed(entry) => println!("remove '{}'", entry.path().display()),
            }
        }
        Ok(())
    }

    // https://github.com/git/git/blob/750f5de7dfe9a4062f95088212b063f189d50e91/read-cache.c#L1350-L1459
    fn refresh(
        &self,
        workspace: &Workspace,
        index: &mut Index,
        db: &Database,
    ) -> Result<(), AddError> {
        Ok(())
    }
}

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

enum EntryUpdate {
    Unchanged,
    Refreshed,
    Updated,
    Removed(IndexEntry),
}

fn collect_untracked_paths(
    workspace: &Workspace,
    index: &Index,
    path: &RepoPath,
    untracked: &mut Vec<(RepoPath, StatNode)>,
    seen: &mut HashSet<RepoPath>,
) -> Result<(), IoError> {
    // prune
    // for directories if the user provided the parent and 1 or many of its subdirectories, we don't
    // have to visit any of them because the parent walk already does.
    // `lit add -A src src/command`
    // we don't walk `command` when we process `src/command`. It has already been visited when we
    // processed `src`.
    if !seen.insert(path.clone()) {
        return Ok(());
    }

    let node = match workspace.stat(path) {
        Ok(node) => node,
        // for a missing path will later determine what to do when we do our tracked processing
        //  - it might still exist as an index entry(staged deletion)
        //  - if it does not exist in the index too then a `NotFound` error
        Err(err) if err.is_not_found() => return Ok(()),
        Err(err) => return Err(err),
    };
    match node.kind {
        // we can't just call `index.is_tracked()` for dir because it might still have entries that
        // are untracked, we have to recurse
        FileKind::Directory => {
            for (path, _) in workspace.dir_entries(path)? {
                collect_untracked_paths(workspace, index, &path, untracked, seen)?;
            }
        }
        FileKind::Regular(_) | FileKind::Symlink => {
            if !index.contains(path) {
                untracked.push((path.clone(), node));
            }
        }
        // an unsupported file explicitly supplied is silently skipped, git returns exit code 0
        FileKind::Other => {}
    }

    Ok(())
}

// TODO: the current impl does not address racy-git
//  https://git-scm.com/docs/racy-git
//  https://www.youtube.com/watch?v=m44cAozuLNI
fn finalize_blob_update(
    workspace: &Workspace,
    db: &Database,
    index: &mut Index,
    pos: usize,
    mode: Mode,
    node: &StatNode,
) -> Result<EntryUpdate, AddError> {
    let entry = unsafe { index.get_unchecked(pos) };
    // no changes detected, nothing to update
    // https://github.com/git/git/blob/50fe5cc63f9cd6e89b87a2c9ccf96a444f4cfdb7/statinfo.c#L64-L105
    if *entry.stat() == node.stat && *entry.mode() == mode {
        return Ok(EntryUpdate::Unchanged);
    }

    // if the file is a symlink in a fs that does not support symlinks, and it is reported as regular
    // we use `read_file()`, `read_link()` would expect a symlink. We perform the read based on the
    // `FileKind` which is what the os reported and not our recorded `Mode`
    let content = if matches!(node.kind, FileKind::Regular(_)) {
        workspace.read_file(entry.path())?
    } else {
        workspace.read_link(entry.path())?
    };
    let oid = db.store(Object::Blob(content))?;
    if oid == *entry.oid() && *entry.mode() == mode {
        unsafe {
            index.refresh_stat_unchecked(pos, node.stat);
        }
        Ok(EntryUpdate::Refreshed)
    } else {
        let metadata = IndexEntryMetadata {
            stat: node.stat,
            mode,
            oid,
            /*
             * TODO: this is temporary, same logic as `Index::new()` for now, when we add support
             * for multiple index version it will change
             */
            flags: entry.path().as_bytes().len().min(0xfff) as u16,
        };
        unsafe {
            index.update_entry_unchecked(pos, metadata);
        }
        Ok(EntryUpdate::Updated)
    }
}

fn filemode(cfg: &ConfigFile) -> Result<bool, ConfigFileError> {
    match cfg.get_bool("core.filemode") {
        Ok(filemode) => Ok(filemode),
        // https://git-scm.com/docs/git-config#Documentation/git-config.txt-corefileMode
        Err(err) if err.is_key_not_found() => Ok(true),
        Err(err) => Err(err),
    }
}

fn symlinks(cfg: &ConfigFile) -> Result<bool, ConfigFileError> {
    match cfg.get_bool("core.symlinks") {
        Ok(symlinks) => Ok(symlinks),
        // https://git-scm.com/docs/git-config#Documentation/git-config.txt-coresymlinks
        Err(err) if err.is_key_not_found() => Ok(true),
        Err(err) => Err(err),
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
    UnsupportedFileType(RepoPath),
    // user provided path verbatim
    FoundNoMatch(OsString),
}

impl Error for AddError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Config(err) => Some(err),
            Self::Index(err) => Some(err),
            Self::Database(err) => Some(err),
            Self::OsPath(err) => Some(err),
            Self::Workspace(err) => Some(err),
            Self::Pathspec(err) => Some(err),
            Self::Lockfile(err) => Some(err),
            Self::Io(err) => Some(err),
            Self::UnsupportedFileType(_) => None,
            Self::FoundNoMatch(_) => None,
        }
    }
}

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
        Self::Pathspec(err)
    }
}

impl From<IoError> for AddError {
    fn from(err: IoError) -> Self {
        Self::Io(err)
    }
}

impl From<IndexError> for AddError {
    fn from(err: IndexError) -> Self {
        Self::Index(err)
    }
}

impl From<DatabaseError> for AddError {
    fn from(err: DatabaseError) -> Self {
        Self::Database(err)
    }
}

impl From<WorkspaceError> for AddError {
    fn from(err: WorkspaceError) -> Self {
        Self::Workspace(err)
    }
}

impl From<LockfileError> for AddError {
    fn from(err: LockfileError) -> Self {
        Self::Lockfile(err)
    }
}
