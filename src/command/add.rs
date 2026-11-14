mod print;

use crate::command::add::print::AddPrinter;
use crate::command::print::{Printer, ReadableBytes};
use crate::repo::config::{ConfigFile, ConfigFileError};
use crate::repo::db::{self, Database, DatabaseError};
use crate::repo::ignore::IgnoreFile;
use crate::repo::index::entry::{IndexEntry, IndexEntryMetadata};
use crate::repo::index::{Index, IndexError};
use crate::repo::lockfile::{Lockfile, LockfileError};
use crate::repo::object::Object;
use crate::repo::object::mode::Mode;
use crate::repo::os::{FileKind, IoError, IoErrorContext, OsPath, StatNode};
use crate::repo::pathspec::{Pathspec, PathspecError};
use crate::repo::repopath::RepoPath;
use crate::repo::workspace::Workspace;
use crate::repo::{Repository, environment, ignore};
use clap::Args;
use clap::ValueEnum;
use std::collections::{BTreeSet, HashSet};
use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

// Conflicts: Git accepts `-u`, `-A` with `refresh` or `interactive`. For example, `refresh -A` and
// `refresh -u` execute just `refresh`. `refresh -u -A` is a conflict error.
// Cases like `lit add -u -no-all -A` fail. `-A` wins over `no-all` and then it conflicts with `u`.
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
    // TODO: drop that?
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
    //  git accepts `-u -no-all` but rejects `-u -A` I couldn't understand why, so for now I
    //  reject both
    //  `update` includes stage deletions which contradicts the `--no-all/ignore-removal`
    #[arg(long, visible_alias = "ignore-removal", conflicts_with = "update")]
    no_all: bool,
    #[arg(short = 'u', long)]
    update: bool,
    #[arg(long)]
    refresh: bool,
    #[arg(long, value_enum)]
    chmod: Option<Chmod>,
    // empty = not provided, could also wrap it in an Option
    paths: Vec<PathBuf>,
    // TODO: --intend-to-add
}

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
                match workspace.path_to_root_prefix(&cwd) {
                    Some(prefix) => Some(prefix),
                    None => {
                        return Err(AddError::OutsideRepository {
                            candidate: cwd,
                            root: workspace.root().clone(),
                        });
                    }
                }
            };
            let pathspec = Pathspec::new(path.as_os_str(), prefix, workspace.root())?;
            pathspecs.push(pathspec);
        }

        // TODO: defer that until we are sure that `add` is valid
        let mut lockfile = Lockfile::acquire(index.path())?;
        index.load()?;
        let op = self.resolve_operation()?;
        match op {
            Operation::Interactive => {}
            Operation::Refresh => {
                if !pathspecs.is_empty() {
                    // `lit add refresh` with no pathspec is a no-op
                    // https://github.com/git/git/blob/0ffa3e35c7c82ab8d0de226149fb03b3f7674d05/builtin/add.c#L559-L564
                    self.refresh(&cfg, &workspace, &mut index, &pathspecs)?;
                }
            }
            Operation::Stage(selection) => {
                match selection {
                    // read `resolve_operation()` for the default case `lit add foo.rs`
                    Selection::AllChanges => {
                        self.stage_worktree_changes(&cfg, &workspace, &db, &mut index, &pathspecs)?;
                    }
                    // there is a flag check in `updated_tracked_entry()` that does not stage
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
                    // TODO: dry-run works with update
                    Selection::TrackedChanges => {
                        self.update(&cfg, &workspace, &db, &mut index, &pathspecs)?;
                    }
                }
                // Note: `chmod` works after staging
                //
                // `add --chmod=+x <pathspec>`, stage changes, then set the index executable bit
                // chmod and dry-run work together: `add --chmod=+x -n <pathspec>`,
                //  preview stage changes, validate chmod without changing the index entry
                if let Some(chmod) = self.chmod {
                    self.chmod(chmod, &mut index, &pathspecs)?;
                }
            }
        }
        if index.is_modified() {
            lockfile.write(&index.serialize())?;
            lockfile.commit()?;
        }
        Ok(())
    }

    // in terms of precedence interactive > refresh > any stage flag
    fn resolve_operation(&self) -> Result<Operation, AddError> {
        if self.interactive {
            return Ok(Operation::Interactive);
        }
        // The following cases are all treated by git as `no op`
        //  - git add --refresh
        //  - git add --refresh -n -v
        if self.refresh {
            return Ok(Operation::Refresh);
        }

        if self.update {
            Ok(Operation::Stage(Selection::TrackedChanges))
        } else if self.no_all {
            Ok(Operation::Stage(Selection::AdditionsAndModifications))
            // the default staging case `lit add foo.rs` behaves the same way as the `all` flag
            // so as long as a path is provided we want the same behavior
        } else if self.all || !self.paths.is_empty() {
            Ok(Operation::Stage(Selection::AllChanges))
        } else {
            // The following cases are all treated by git as `nothing specified, nothing added`
            //  - git add
            //  - git add -v
            //  - git add -n
            //  - git add -f
            Err(AddError::NothingSpecified)
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
        // may contain duplicates if the user provided the same path multiple times but we filter
        // them out later when we choose which of those are reported
        let mut ignored_paths = Vec::new();

        // this branch runs only for `lit add -A`. For `lit add --dry-run -A`, there is no pathspec
        // to report as `ignored` and the return value is ignored
        if pathspecs.is_empty() {
            let mut ignore_files = Vec::new();
            // we start from the root and initially the stack is empty
            // the first path we are going to compute is `{root}/.litignore` which is what we want
            let _ = self.collect_untracked_paths(
                workspace,
                index,
                &RepoPath::new(),
                &mut untracked,
                &mut seen,
                &mut ignore_files,
            )?;
        } else {
            // this took me a while to understand what Git actually does for each provided path
            // for each pathspec that we invoke `collect_untracked_paths()`, the stack must contain
            // the ignore files from `pathspec.pattern()` ancestor directories.
            for pathspec in pathspecs {
                let pattern = pathspec.pattern();
                // if `force` is set bypass any ignore handling
                let mut ignore_files = if self.force {
                    Vec::new()
                } else {
                    let Some(files) = ignore::find_ignore_files_from_ancestors(workspace, pattern)?
                    else {
                        // the path was ignored by an ancestor, the specific path is ignored as an
                        // untracked path, we can still process it if it is an already tracked index
                        // path
                        ignored_paths.push(pattern);
                        continue;
                    };
                    files
                };
                let outcome = self.collect_untracked_paths(
                    workspace,
                    index,
                    pattern,
                    &mut untracked,
                    &mut seen,
                    &mut ignore_files,
                )?;
                if matches!(outcome, CollectionOutcome::Ignored) {
                    ignored_paths.push(pattern);
                }
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
                // doesn't exist on the workspace and is not a staged deletion
                Err(err) if err.is_not_found() => {
                    return Err(AddError::FoundNoMatch(pathspec.original().to_os_string()));
                }
                Err(err) => return Err(AddError::Io(err)),
            }
        }
        // .litignore -> contains build/
        // build/tracked.txt, tracked
        // build/new.txt -> untracked
        //
        // running `lit add build/` processses the `build/tracked.txt`, ignores `build/new.txt` and
        // reports `build` as ignored. This is the behavior we established, we could call `is_tracked()`
        // instead of contains and not report `build/`.
        // TODO: review this if `build` should be reported as ignored or not. There is no golden rule
        let mut reported_paths = HashSet::new();
        ignored_paths.retain(|&path| {
            // path is &RepoPath and the comparison for `HashSet` is by value, there is no cloning
            // extra allocation apart from the set
            !index.contains(path) && reported_paths.insert(path)
        });

        let filemode = filemode(cfg)?;
        let symlinks = symlinks(cfg)?;
        let mut printer = AddPrinter::lock();
        for position in positions.into_iter().rev() {
            let update =
                self.update_tracked_entry(workspace, index, db, filemode, symlinks, position)?;
            if !self.verbose && !self.dry_run {
                // nothing to print
                continue;
            }
            // TODO: extract this in a method, it is duplicated twice
            match update {
                // in those 2 cases, git prints nothing
                EntryUpdate::Unchanged | EntryUpdate::Refreshed => {}
                EntryUpdate::Updated => {
                    // https://github.com/git/git/blob/50fe5cc63f9cd6e89b87a2c9ccf96a444f4cfdb7/read-cache.c#L815-L816
                    // it is safe to index because `update` never modifies the position or the path'
                    // of the entry. This is a new borrow and we avoid the issue mentioned above.
                    let entry = unsafe { index.get_unchecked(position) };
                    printer
                        .print(&AddEvent::Added(entry.path()))
                        .with_context("write stdout", None::<&Path>)?;
                }
                // https://github.com/git/git/blob/50fe5cc63f9cd6e89b87a2c9ccf96a444f4cfdb7/read-cache.c#L636-L648
                EntryUpdate::Removed(entry) => printer
                    .print(&AddEvent::Removed(entry.path()))
                    .with_context("write stdout", None::<&Path>)?,
                EntryUpdate::PendingRemoval => {
                    let entry = unsafe { index.get_unchecked(position) };
                    printer
                        .print(&AddEvent::Removed(entry.path()))
                        .with_context("write stdout", None::<&Path>)?;
                }
            }
        }

        // TODO: `ignored_paths` are reported in stderr, I dont know why
        // git reports explicitly requested ignored paths by default, without `dry-run` or `verbose`
        for path in ignored_paths {
            printer
                .print(&AddEvent::Ignored(path))
                .with_context("write stderr", None::<&Path>)?;
        }

        for (path, node) in untracked {
            // check first if readable then report it if `dry-run` is set
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
            // no changes
            if self.dry_run {
                printer
                    .print(&AddEvent::Added(&path))
                    .with_context("write stdout", None::<&Path>)?;
                continue;
            }
            let oid = db.store(Object::Blob(content))?;
            let entry = IndexEntry::new(path, oid, mode, node.stat);
            let pos = index.insert(entry)?;

            if self.verbose {
                let entry = unsafe { index.get_unchecked(pos) };
                printer
                    .print(&AddEvent::Added(entry.path()))
                    .with_context("write stdout", None::<&Path>)?;
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
        // collect_index_pos_for_update_or_refresh(&*index, pathspecs) would also work by explicitly
        // reborrowing as `&Index`
        let positions = collect_index_pos_for_update_or_refresh(index, pathspecs)?;
        let filemode = filemode(cfg)?;
        let symlinks = symlinks(cfg)?;
        // `positions` is in ascending order. Removing an entry shifts everything to its right one
        // slot left, so removing from the front would leave every remaining position stale after
        // the first removal.
        // Removing from the back avoid this. A removal only shifts entries to the right of
        // it, which we have already handled, so the positions still to be processed(all to
        // the left) remain valid.
        let mut printer = AddPrinter::lock();
        for position in positions.into_iter().rev() {
            let update =
                self.update_tracked_entry(workspace, index, db, filemode, symlinks, position)?;
            if !self.verbose && !self.dry_run {
                // nothing to print
                continue;
            }
            match update {
                // in those 2 cases, git prints nothing
                EntryUpdate::Unchanged | EntryUpdate::Refreshed => {}
                EntryUpdate::Updated => {
                    // https://github.com/git/git/blob/50fe5cc63f9cd6e89b87a2c9ccf96a444f4cfdb7/read-cache.c#L815-L816
                    // it is safe to index because `update` never modifies the position or the path'
                    // of the entry.
                    let entry = unsafe { index.get_unchecked(position) };
                    printer
                        .print(&AddEvent::Added(entry.path()))
                        .with_context("write stdout", None::<&Path>)?;
                }
                // https://github.com/git/git/blob/50fe5cc63f9cd6e89b87a2c9ccf96a444f4cfdb7/read-cache.c#L636-L648
                EntryUpdate::Removed(entry) => printer
                    .print(&AddEvent::Removed(entry.path()))
                    .with_context("write stdout", None::<&Path>)?,
                EntryUpdate::PendingRemoval => {
                    let entry = unsafe { index.get_unchecked(position) };
                    printer
                        .print(&AddEvent::Removed(entry.path()))
                        .with_context("write stdout", None::<&Path>)?;
                }
            }
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
    ) -> Result<EntryUpdate, AddError> {
        let entry = unsafe { index.get_unchecked(pos) };
        let node = match workspace.stat(entry.path()) {
            Ok(node) => node,
            // exists in the index but not in the workspace => stage deletion
            Err(err) if err.is_not_found() => return Ok(self.stage_deletion(pos, index)),
            Err(err) => return Err(AddError::Io(err)),
        };

        match node.kind {
            FileKind::Regular(exec) => {
                let mode = mode_for_regular_file(exec, filemode, symlinks, entry);
                self.finalize_blob_update(workspace, db, index, pos, mode, &node)
            }
            FileKind::Symlink => {
                self.finalize_blob_update(workspace, db, index, pos, Mode::Symlink, &node)
            }
            // this is the case where a tracked entry, a blob or a symlink, is now a directory, we
            // stage the deletion
            FileKind::Directory => Ok(self.stage_deletion(pos, index)),
            FileKind::Other => Err(AddError::UnsupportedFileType(entry.path().clone())),
        }
    }

    // https://github.com/git/git/blob/750f5de7dfe9a4062f95088212b063f189d50e91/read-cache.c#L1350-L1459
    // if `verbose` is true, refresh has the following output:
    //  `M`: means content changed or a trusted executable bit changed(assume `core.filemode = true`)
    //  `D`: tracked path is missing
    //  `T`: type changed
    // Each letter describes how a tracked path in the working tree differs from its index entry
    // `dry-run`, `chmod` are ignored if set alongise,
    fn refresh(
        &self,
        cfg: &ConfigFile,
        workspace: &Workspace,
        index: &mut Index,
        pathspecs: &[Pathspec],
    ) -> Result<(), AddError> {
        let positions = collect_index_pos_for_update_or_refresh(index, pathspecs)?;
        let filemode = filemode(cfg)?;
        let symlinks = symlinks(cfg)?;
        let mut changes = Vec::with_capacity(positions.len());

        for position in positions {
            let entry = unsafe { index.get_unchecked(position) };
            let node = match workspace.stat(entry.path()) {
                Ok(node) => node,
                // skip
                Err(err) if err.is_not_found() => {
                    if self.verbose {
                        changes.push((position, RefreshChange::Deleted));
                    }
                    continue;
                }
                Err(err) => return Err(AddError::Io(err)),
            };
            let mode = match node.kind {
                FileKind::Regular(exec) => mode_for_regular_file(exec, filemode, symlinks, entry),
                FileKind::Symlink => Mode::Symlink,
                // for a tracked entry that is now a directory or an unsupported file we skip it
                FileKind::Directory | FileKind::Other => {
                    if self.verbose {
                        changes.push((position, RefreshChange::TypeChanged));
                    }
                    continue;
                }
            };
            // mode is different, can't refresh
            // Mode::Regular <-> Executable'M'
            // file <-> symlink 'T'
            if mode != *entry.mode() {
                let change = match (mode, *entry.mode()) {
                    (Mode::Regular, Mode::Executable) | (Mode::Executable, Mode::Regular) => {
                        (position, RefreshChange::Modified)
                    }
                    // (Mode::Regular | Mode::Executable, Mode::Symlink) | (Mode::Symlink, Mode::Regular | Mode::Executable)
                    // can't be anything else at this point
                    _ => (position, RefreshChange::TypeChanged),
                };
                if self.verbose {
                    changes.push(change);
                }
                continue;
            }
            // this is interesting, `chmod` always updates `ctime` so checking early for stat and
            // returning if no change would have been fine but we have to consider the following case:
            // we stage a file when `core.filemode = false` and record it with `Mode::Regular` and
            // then we change `core.filemode = true` and then call `add --refresh -v`. The file
            // itself has not change at all, `FileStat`field matches but we now trust the executable
            // bit and the workspace mode is `Mode::Executable`. Refresh should report `M` for `-v`
            if *entry.stat() == node.stat {
                continue;
            }

            let content = if matches!(node.kind, FileKind::Regular(_)) {
                workspace.read_file(entry.path())?
            } else {
                workspace.read_link(entry.path())?
            };
            // TODO: should we consider collision here?
            if *entry.oid() == db::hash(b"blob", &content) {
                // at this point only `stat` is different so we refresh
                unsafe {
                    index.refresh_stat_unchecked(position, node.stat);
                }
            } else {
                if self.verbose {
                    // different content
                    changes.push((position, RefreshChange::Modified));
                }
            }
        }

        // only when `verbose` is set, changes are not empty
        if !changes.is_empty() {
            let mut printer = AddPrinter::lock();
            printer
                .print(&AddEvent::RefreshHeader)
                .with_context("write stdout", None::<&Path>)?;
            for change in changes {
                let entry = unsafe { index.get_unchecked(change.0) };
                printer
                    .print(&AddEvent::Refresh(entry.path(), change.1))
                    .with_context("write stdout", None::<&Path>)?;
            }
        }
        Ok(())
    }

    // ignore rules are only applied for untracked files
    //
    // we want to report if the pathspec that triggered the collection is ignored, the one that the
    // user provided
    fn collect_untracked_paths(
        &self,
        workspace: &Workspace,
        index: &Index,
        path: &RepoPath,
        untracked: &mut Vec<(RepoPath, StatNode)>,
        seen: &mut HashSet<RepoPath>,
        ignore_files: &mut Vec<IgnoreFile>,
    ) -> Result<CollectionOutcome, IoError> {
        let node = match workspace.stat(path) {
            Ok(node) => node,
            // for a missing path will later determine what to do when we do our tracked processing
            //  - it might still exist as an index entry(stage the deletion)
            //  - if it does not exist in the index too, then a `NotFound` error
            Err(err) if err.is_not_found() => return Ok(CollectionOutcome::Completed),
            Err(err) => return Err(err),
        };
        match node.kind {
            // we can't just call `index.is_tracked()` for dir because it might still have entries that
            // are untracked, we have to recurse
            FileKind::Directory => {
                // check dir against its ancestors, the 2nd check is for root's relative path
                // if it is excluded by some parent dir don't descend
                if !self.force
                    && !path.as_bytes().is_empty()
                    && ignore::is_ignored(ignore_files, path, true)
                {
                    return Ok(CollectionOutcome::Ignored);
                }
                // prune
                // for directories if the user provided the parent and 1 or many of its subdirectories, we don't
                // have to visit any of them because the parent walk already does.
                // `lit add -A src src/command`
                // we don't walk `command` when we process `src/command`. It has already been visited when we
                // processed `src`.
                if !seen.insert(path.clone()) {
                    return Ok(CollectionOutcome::Completed);
                }

                let len = ignore_files.len();
                if !self.force {
                    // we look for `.litignore` file at the current directory
                    if let Some(ignore_file) = IgnoreFile::load(workspace, path)? {
                        ignore_files.push(ignore_file);
                    }
                }

                for (path, _) in workspace.dir_entries(path)? {
                    self.collect_untracked_paths(
                        workspace,
                        index,
                        &path,
                        untracked,
                        seen,
                        ignore_files,
                    )?;
                }
                // By truncating we restore the stack to its state before walking the directory, so
                // the rules(any ignore file added during that walk) does not apply when we walk
                // sibllings
                ignore_files.truncate(len);
            }
            FileKind::Regular(_) | FileKind::Symlink => {
                if index.contains(path) {
                    return Ok(CollectionOutcome::Completed);
                }
                if !self.force && ignore::is_ignored(ignore_files, path, false) {
                    return Ok(CollectionOutcome::Ignored);
                }
                if !seen.insert(path.clone()) {
                    return Ok(CollectionOutcome::Completed);
                }
                untracked.push((path.clone(), node));
            }
            // an unsupported file explicitly supplied is silently skipped, git returns exit code 0
            FileKind::Other => {}
        }

        Ok(CollectionOutcome::Completed)
    }

    // TODO: the current impl does not address racy-git
    //  https://git-scm.com/docs/racy-git
    //  https://www.youtube.com/watch?v=m44cAozuLNI
    fn finalize_blob_update(
        &self,
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

        if self.dry_run {
            let oid = db::hash(b"blob", &content);
            return Ok(if oid == *entry.oid() && *entry.mode() == mode {
                EntryUpdate::Unchanged
            } else {
                EntryUpdate::Updated
            });
        }

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

    fn stage_deletion(&self, pos: usize, index: &mut Index) -> EntryUpdate {
        if self.no_all {
            return EntryUpdate::Unchanged;
        }
        if self.dry_run {
            return EntryUpdate::PendingRemoval;
        }
        let entry = unsafe { index.remove_unchecked(pos) };
        EntryUpdate::Removed(entry)
    }

    // https://github.com/git/git/blob/1891de46f79dc444a47db1c87bd0c8c76b5463f2/builtin/add.c#L43
    fn chmod(
        &self,
        chmod: Chmod,
        index: &mut Index,
        pathspecs: &[Pathspec],
    ) -> Result<(), AddError> {
        let mode = match chmod {
            Chmod::Executable => Mode::Executable,
            Chmod::NonExecutable => Mode::Regular,
        };
        let positions = collect_chmod_positions(&index, &pathspecs);
        for position in positions {
            let entry = unsafe { index.get_unchecked(position) };
            if !matches!(entry.mode(), Mode::Regular | Mode::Executable) {
                return Err(AddError::NothingSpecified);
            }
            if !self.dry_run {
                unsafe {
                    index.set_mode_unchecked(position, mode);
                }
            }
        }
        Ok(())
    }
}

enum Selection {
    AllChanges,
    TrackedChanges,
    AdditionsAndModifications,
}

// refresh updates the index stat information without staging any changes, and it is an operation
// rather than a different `Selection` variant
//
// This is similar to `Target` in `init` when based on what the user provided we determine worktree
// and metadata dirs. The rest of the flags are independent and will check them after we made a
// `Selection`
enum Operation {
    Interactive,
    Refresh,
    Stage(Selection),
}

#[derive(Debug, Copy, Clone, ValueEnum)]
enum Chmod {
    #[value(name = "+x")]
    Executable,
    #[value(name = "-x")]
    NonExecutable,
}

impl Chmod {
    fn flag(&self) -> &'static str {
        match self {
            Self::Executable => "+x",
            Self::NonExecutable => "-x",
        }
    }
}

enum EntryUpdate {
    Unchanged,
    Refreshed,
    Updated,
    Removed(IndexEntry),
    // this variant is used when a staged deletion is detected but a flag like `dry-run` is set,
    // report it but not modify the index
    PendingRemoval,
}

enum AddEvent<'a> {
    Added(&'a RepoPath),
    Removed(&'a RepoPath),
    Ignored(&'a RepoPath),
    RefreshHeader,
    Refresh(&'a RepoPath, RefreshChange),
}

enum RefreshChange {
    Modified,
    Deleted,
    TypeChanged,
}

enum CollectionOutcome {
    Completed,
    Ignored,
}

fn collect_index_pos_for_update_or_refresh(
    index: &Index,
    pathspecs: &[Pathspec],
) -> Result<BTreeSet<usize>, AddError> {
    let mut positions = BTreeSet::new();
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
    Ok(positions)
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

fn mode_for_regular_file(exec: bool, filemode: bool, symlinks: bool, entry: &IndexEntry) -> Mode {
    // On fs without symlink support, a tracked symlink appears as a regular file containing
    // its target text. If `core.symlinks = false` and the existing entry's mode is `Symlink`
    // we treat the regular file as a symlink, and later we call `read_link()` instead of
    // `read_file()`
    // https://github.com/git/git/blob/50fe5cc63f9cd6e89b87a2c9ccf96a444f4cfdb7/read-cache.h#L9-L29
    if !symlinks && *entry.mode() == Mode::Symlink {
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
    }
}

fn collect_chmod_positions(index: &Index, pathspecs: &[Pathspec]) -> Vec<usize> {
    index
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            // `any()` because we don't want to call chmod more than once per entry
            // for `foo.rs` and pathspecs = ['.', 'foo.rs'] we change the mod once on the first match
            let matched = pathspecs.iter().any(|pathspec| {
                let pattern = pathspec.pattern();
                pattern.as_bytes().is_empty()
                    || entry.path() == pattern
                    || pattern.is_parent_of(entry.path())
            });
            matched
        })
        .map(|(position, _)| position)
        .collect()
}

#[derive(Debug)]
pub(crate) enum AddError {
    NothingSpecified,
    OutsideRepository { candidate: OsPath, root: OsPath },
    Config(ConfigFileError),
    Index(IndexError),
    Database(DatabaseError),
    Pathspec(PathspecError),
    Lockfile(LockfileError),
    Io(IoError),
    UnsupportedFileType(RepoPath),
    // user provided path verbatim
    FoundNoMatch(OsString),
    CannotChmod(RepoPath, Chmod),
}

impl Error for AddError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::NothingSpecified => None,
            Self::OutsideRepository { .. } => None,
            Self::Config(err) => Some(err),
            Self::Index(err) if err.is_format_error() => None,
            Self::Index(err) => Some(err),
            Self::Database(err) => Some(err),
            Self::Pathspec(err) => Some(err),
            Self::Lockfile(err) => Some(err),
            Self::Io(err) => Some(err),
            Self::UnsupportedFileType(_) => None,
            Self::FoundNoMatch(_) => None,
            Self::CannotChmod(_, _) => None,
        }
    }
}

impl fmt::Display for AddError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutsideRepository { candidate, root } => write!(
                f,
                "'{}' is outside repository at '{}'",
                ReadableBytes(candidate.as_bytes()),
                ReadableBytes(root.as_bytes())
            ),
            Self::NothingSpecified => write!(f, "Nothing specified, nothing added"),
            Self::Config(_) => write!(f, "could not read repository configuration"),
            // the actual index error is kept internally but no specific information is provided
            // the user can't really do much with an error like bad padding at ...
            Self::Index(err) if err.is_format_error() => write!(f, "invalid index format"),
            Self::Index(_) => write!(f, "could not process repository index"),
            Self::Database(_) => write!(f, "could not store staged content"),
            Self::Pathspec(_) => write!(f, "could not resolve pathspec"),
            Self::Lockfile(_) => write!(f, "could not update index"),
            Self::UnsupportedFileType(path) => {
                write!(
                    f,
                    "unsupported file type at '{}'",
                    ReadableBytes(path.as_bytes())
                )
            }
            Self::FoundNoMatch(path) => {
                write!(
                    f,
                    "pathspec '{}' did not match any files",
                    path.to_string_lossy()
                )
            }
            Self::Io(_) => write!(f, "I/O error during add"),
            Self::CannotChmod(path, mode) => write!(
                f,
                "cannot chmod {} '{}'",
                mode.flag(),
                ReadableBytes(path.as_bytes())
            ),
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

impl From<LockfileError> for AddError {
    fn from(err: LockfileError) -> Self {
        Self::Lockfile(err)
    }
}
