use crate::repo::litfile::{self, LitFileError};
use crate::repo::{self, MetadataDirError};
use clap::Args;
use std::error::Error;
use std::fs::{self, File, FileType, OpenOptions};
use std::io::{self};
use std::path::{Path, PathBuf};
use std::{env, fmt};
use crate::repo::config::{ConfigFile, ConfigFileError};

enum MetadataPlacement {
    // metadata dir is used directly
    Direct,
    // Ordinary `<worktree>/.lit` dir
    Embedded,
    // `<worktree>/.lit` is a pointer file to metadata
    Separate { link: PathBuf },
}

struct Layout {
    // directory containing the repository metadata (HEAD, config, objecets, ...)
    metadata: PathBuf,
    // root of the working tree for non-bare
    // None for bare
    worktree: Option<PathBuf>,
    // describes how the metadata is connected to the worktree
    placement: MetadataPlacement,
}

impl Layout {
    // we don't need to do any conversion here or any path validation, we follow the same logic as
    // we did before we will make the fs call and handle the error for a bad path
    //
    // There are 4 factors that determine the location of .lit dir when initializing a repo.
    // --bare, --separate_lit_dir, path and the LIT_DIR/LIT_WORK_TREE env var.
    // TODO: We need to set LIT_DIR after resolving the path
    //
    //  Resolves the metadata and worktree locations without touching the fs
    //
    //  Git's init impl will try to "guess" whether a repo should be bare from the value of GIT_DIR
    //  https://github.com/git/git/blob/18e66859d87fb4b76599f73460b54f0848c76b16/builtin/init-db.c#L17
    //  We avoid this behavior and set the following rules:
    //  - A repository is bare only when --bare is provided.
    //  - LIT_DIR has the same meaning as GIT_DIR. It names the metadata directory itself, not the
    //  directory in which an embedded `.lit` directory should be created, this is what the path
    //  positional arg refers to.
    //  - A positional path names the repository location and has precedence over LIT_DIR:
    //      - non-bare: <path> is the worktree and metadata is stored in <path>.lit
    //      - bare: <path> is the metadata directory and there is no worktree
    //  - If there is no positional path or --separate-lit-dir, LIT_DIR selects the metadata directory:
    //      - non-bare: LIT_WORK_TREE selects the worktree, falling back to cwd when unset
    //      - bare: no worktree, so LIT_WORK_TREE is invalid
    //  - LIT_WORK_TREE without LIT_DIR is invalid
    //  - With no explicit location, non-bare init uses cwd/.lit and bare init uses cwd directly
    /// Determines the repository layout
    fn resolve(
        path: Option<&Path>,
        bare: bool,
        separate_lit_dir: Option<&Path>,
    ) -> Result<Self, LayoutError> {
        // highest precedence, reject early
        if path.is_some_and(|p| p.as_os_str().is_empty()) {
            return Err(LayoutError::EmptyPath("<directory>"));
        }

        let cwd = env::current_dir().map_err(LayoutError::CurrentDirUnavailable)?;
        let root = path.map_or(cwd.clone(), |path| cwd.join(path));

        if let Some(dir_path) = separate_lit_dir {
            if dir_path.as_os_str().is_empty() {
                return Err(LayoutError::EmptyPath("--separate-lit-dir"));
            }
            // TODO: should this be a notification to the user that LIT_DIR is actually ignored
            //  because the flag has higher precedence. This is a conflict because both try to
            //  name the metadata dir
            return Ok(Self {
                metadata: cwd.join(dir_path),
                // the parent identifies the worktree even though the metadata is elsewhere
                worktree: Some(root.clone()),
                placement: MetadataPlacement::Separate {
                    link: root.join(".lit"),
                },
            });
        }

        // explicit positional path wins over LIT_DIR
        if path.is_some() {
            return if bare {
                Ok(Self {
                    metadata: root,
                    worktree: None,
                    placement: MetadataPlacement::Direct,
                })
            } else {
                // lit init <path>
                Ok(Self {
                    metadata: root.join(".lit"), // <worktree>.lit
                    worktree: Some(root),
                    placement: MetadataPlacement::Embedded,
                })
            };
        }

        let env_dir = env::var_os("LIT_DIR");
        if let Some(env_dir) = env_dir {
            if env_dir.is_empty() {
                return Err(LayoutError::EmptyPath("LIT_DIR"));
            }

            let dir = cwd.join(env_dir);
            // LIT_WORK_TREE makes sense only in conjunction with LIT_DIR without --bare. In any
            // other case it is ignored.
            let worktree = env::var_os("LIT_WORK_TREE");
            if worktree.as_ref().is_some_and(|tree| tree.is_empty()) {
                return Err(LayoutError::EmptyPath("LIT_WORK_TREE"));
            }
            // bare repos have no worktree
            if bare && worktree.is_some() {
                return Err(LayoutError::LitWorkTreeWithBare);
            }

            return if bare {
                Ok(Self {
                    metadata: dir,
                    worktree: None,
                    placement: MetadataPlacement::Direct,
                })
            } else {
                // if no WORK_TREE found we fall back to cwd
                let worktree = worktree.map_or(cwd.clone(), |path| cwd.join(path));
                Ok(Self {
                    metadata: dir,
                    worktree: Some(worktree),
                    placement: MetadataPlacement::Direct,
                })
            };
        }

        // Note: LIT_WORK_TREE is considered only when LIT_DIR is set. This branch is reached when
        // LIT_DIR is unset, so even if LIT_WORK_TREE is set, it is ignored. bare does not error,
        // non-bare uses cwd
        if bare {
            Ok(Self {
                metadata: cwd,
                worktree: None,
                placement: MetadataPlacement::Direct,
            })
        } else {
            Ok(Self {
                metadata: cwd.join(".lit"),
                worktree: Some(cwd),
                placement: MetadataPlacement::Embedded,
            })
        }
    }

    // the worktree root for non-bare or the metadata directory for bare
    fn root(&self) -> &Path {
        self.worktree.as_deref().unwrap_or(&self.metadata)
    }

    fn is_bare(&self) -> bool {
        self.worktree.is_none()
    }
}

#[derive(Debug, Args)]
pub(crate) struct Init {
    #[arg(short = 'q', long)]
    quiet: bool,
    #[arg(long)]
    bare: bool,
    // a flag that specifies a path where the .lit files live, which contradicts with --bare because
    // drops .lit dir entirely.
    //
    // Instead of initializing the repository as a directory to either $GIT_DIR or ./.git/, create a
    // text file there containing the path to the actual repository. This file acts as a filesystem-
    // agnostic Git symbolic link to the repository.
    //
    // If this is a reinitialization, the repository will be moved to the specified path.
    #[arg(long, conflicts_with = "bare")]
    separate_lit_dir: Option<PathBuf>,
    // Directory in which to initialize the repository
    path: Option<PathBuf>,
    // TODO: add the remaining flags
}

impl Init {
    // the method that does all the setup for init https://github.com/git/git/blob/master/setup.c
    // https://github.com/git/git/blob/18e66859d87fb4b76599f73460b54f0848c76b16/builtin/init-db.c#L72
    // TODO: For init, Git does not decide based merely on whether .git exist. It resolves the
    //  Git directory, then it considers the operation a reinit when HEAD:
    //      - exists and is readable
    //      - is a symlink, including a dangling link
    //  Even an invalid config will trigger the reinit message based on the above rules
    pub(super) fn execute(&self) -> Result<(), InitError> {
        // 1. We need to resolve arguments and env vars for the location of the metadata dir.
        // resolve() sets rules for a deterministic layout.
        let layout = Layout::resolve(
            // self.path.as_ref().map(PathBuf::as_ref)
            self.path.as_deref(),
            self.bare,
            self.separate_lit_dir.as_deref(),
        )?;

        // 2. Create the positional/root directory
        fs::create_dir_all(layout.root())
            .map_err(|err| InitError::from_io_error(layout.root(), err))?;

        // 3. If separate-lit-dir flag is set, we create the pointer file and also migrate an
        // existing repo if it is a reinitialization
        if let MetadataPlacement::Separate { link } = &layout.placement {
            try_migrate_metadata(link, &layout.metadata)?;
        }

        let cfg_path = layout.metadata.join("config");
        let cfg = match ConfigFile::new(&cfg_path) {
            Ok(cfg) => Some(cfg),
            Err(err)
            if err
                .io_error_kind()
                .is_some_and(|kind| kind == io::ErrorKind::NotFound) => None,
            Err(err) => return Err(InitError::Config { path: cfg_path, source: err }),
        };
        
        
        // https://github.com/git/git/blob/master/setup.c#L751
        // TODO: 4. the next step should be about repository format validation
        //
        //  format is a compatibility contract fot the repository as a whole. git needs to know that
        //  it can safely read/write in this repository. this is different from index versions or
        //  pack-index version. The 0 which is the most common one means SHA-1 object ids, loose refs
        //  + packed refs, common Git directory layout
        //
        // repairing config(reinit):
        //
        // [core]
        // 	repositoryformatversion = 0
        // 	filemode = true
        // 	bare = false
        // 	logallrefupdates = true
        //
        // if filemode is missing git appends to [core] filemode = true
        // if filemode is wrong git rewrites it
        // same for bare, if a normal repo has bare = true, git changes back to false,
        // safe for formatversion
        // TODO: we need to test the behavior of logalrefupdates
        //
        // Migration vs Reinit
        //  The requirements are different. Reinit needs to know if there is existing repository state
        //  that initialization preserve, while migration needs to know that if it is a metadata
        //  directory that lit can work on. Reinit must be more conservative.
        //      if .lit/HEAD exists, but /objects and /refs are missing, it might be a damaged repo,
        //      a repo that something went wrong in the previous init call. We have to preserve the
        //      current state, and try to repair the missing structure. A stricter requirement would
        //      not allow us to repair anything, which is the main goal for reinit.
        //
        // The code below is a naive wrong impl for detecting an existing lit repo. create_dir() will
        // fail when another entry exists with the same name, not necessarily a directory, could be
        // a symlink, regular file etc. Printing the reinit message in such case is misleading. Only
        // if the existing entry is a directory we can return true for reinit. This is what ensure_dir()
        // handles.
        //
        // let reinit = match fs::create_dir(&lit_dir) {
        //     Ok(_) => false,
        //     Err(err) if err.kind() == io::ErrorKind::AlreadyExists => true,
        //     Err(err) => return Err(InitError::from_io_error(&lit_dir, err)),
        // };
        //
        // If .lit exists as a directory we always print the reinit message without checking if it
        // contains of the expected entries. It could just be an empty where everything was deleted,
        // or just deleted the .lit related entries. It does not matter in either case.
        // let reinit = match fs::create_dir(&layout.metadata) {
        //     Ok(_) => false,
        //     Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
        //         ensure_dir(&layout.metadata).map(|_| true)?
        //     }
        //     Err(err) => return Err(InitError::from_io_error(&layout.metadata, err)),
        // };

        // on why a naive File::create() does not work read Lockfile::acquire() exactly the same case
        // File::create(&config).map_err(|err| ....)?;
        //
        // same case for creating ensure_dir() but for files, read comment above.
        // match OpenOptions::new()
        //     .write(true)
        //     .create_new(true)
        //     .open(&config)
        // {
        //     Ok(_) => {}
        //     Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
        //     Err(err) => return Err(InitError::from_io_error(&config, err)),
        // }
        // let config = layout.metadata.join("config");
        // ensure_file(&config)?;
        // if reinit {
        //     println!("Reinitialized existing Lit repository in {}", lit.display());
        // } else {
        //     println!("Initialized empty Lit repository in {}", lit.display());
        // }
        // TODO: when we write the values to config we need to see the behavior of calling reinit
        //  on a non bare repo and vice versa to determine the core.bare value
        Ok(())
    }

    fn create_dirs(&self, layout: &Layout) -> Result<(), InitError> {
        let objects = layout.metadata.join("objects");
        let refs = layout.metadata.join("refs");
        ensure_dir(&objects)?;
        ensure_dir(&refs)
    }
}

// https://github.com/git/git/blob/1a3e64c6c4a623626ff0687008732a8e007e2a1c/setup.c#L2675-L2695
// we convert an embedded repo layout into a separate one
// TODO: explain rename and file descriptors and the unavoidable TOCTOU race conditions when working
//  with paths.
fn try_migrate_metadata(
    lit_entry_path: &Path,
    destination_metadata_dir: &Path,
) -> Result<(), InitError> {
    // this is tricky
    //
    // from is the path value of the placement field that we set in Layout::resolve()
    // we don't know if this path exists yet
    //
    // lit --separate-lit-dir /new/metadata project
    //
    // if it is a fresh repo, project/.lit will be a file with a pointer to /new/metadata
    // if it is a reinit, we have to inspect lit_link, because now we have to move the contents
    // to the new location.
    //  - a directory means it is the existing metadata directory
    //  - a file means we have to check if it contains an existing pointer such as: litdir: /old/metadata
    //  and follow the pointer to find the content we want to move
    //
    // Note: we can't make a naive call fs::metadata(from), we first have to resolve it. If from
    // is a symlink, metadata() will follow it and return info based on the target BUT later when we
    // try to rename(from, to), fs::rename() does not follow symlinks, and we will rename
    // the symlink itself not the target path, so first we must call canonicalize() and then call
    // metadata to the returned value.
    let resolved_entry_path = resolve_lit_entry(lit_entry_path)
        .map_err(|err| InitError::from_io_error(lit_entry_path, err))?;
    let Some(current_metadata_dir) = resolved_entry_path else {
        // nothing to migrate, create the .lit file
        return litfile::write(lit_entry_path, destination_metadata_dir).map_err(|err| {
            InitError::LitFile {
                path: lit_entry_path.to_path_buf(),
                err,
            }
        });
    };

    // TODO: clean up the comments and ask 5.6 Sol High if there any issues when someone
    //  tries to read Index paths on Windows, since they are just byte sequences without NUL
    match fs::metadata(&current_metadata_dir) {
        Ok(metadata) if metadata.is_dir() => {
            repo::validate_metadata_dir(&current_metadata_dir)?;
            if current_metadata_dir != destination_metadata_dir {
                fs::rename(&current_metadata_dir, destination_metadata_dir)
                    .map_err(|err| InitError::from_io_error(&current_metadata_dir, err))?;
            }
        }
        Ok(metadata) if metadata.is_file() => {
            let mut file = File::open(lit_entry_path)
                .map_err(|err| InitError::from_io_error(lit_entry_path, err))?;
            // There is TOCTOU race condition between the first fs::metadata() call and
            // File::open() which we can actually handle by calling metadata() again after
            // acquiring the file descriptor for the duration of the operation
            let metadata = file
                .metadata()
                .map_err(|err| InitError::from_io_error(lit_entry_path, err))?;
            if !metadata.is_file() {
                return Err(InitError::BadEntry {
                    path: lit_entry_path.to_path_buf(),
                    entry: EntryType::from(metadata.file_type()),
                });
            }
            // must be a litfile
            let path = litfile::read(&mut file).map_err(|err| InitError::LitFile {
                path: lit_entry_path.to_path_buf(),
                err,
            })?;
            // must point to a valid lit repo
            repo::validate_metadata_dir(&path)?;
            if path != destination_metadata_dir {
                fs::rename(&path, destination_metadata_dir)
                    .map_err(|err| InitError::from_io_error(&path, err))?;
            }
        }
        Ok(metadata) => {
            return Err(InitError::BadEntry {
                path: current_metadata_dir,
                entry: EntryType::from(metadata.file_type()),
            });
        }
        Err(err) => return Err(InitError::from_io_error(&current_metadata_dir, err)),
    }

    litfile::write(lit_entry_path, destination_metadata_dir).map_err(|err| InitError::LitFile {
        path: lit_entry_path.to_path_buf(),
        err,
    })?;

    Ok(())
}

// decide if we have to set core.worktree in config
fn needs_worktree_config(layout: &Layout) -> bool {
    let Some(worktree) = &layout.worktree else {
        return false;
    };

    // core.worktree is ambiguous when LIT_DIR is used which is in the direct case
    // LIT_DIR is /foo/metadata
    // LIT_WORK_TREE is /bar
    // then we can't use the rule that worktree is the parent of metadata, we have to check
    // if worktree.join(.lit) is our metadata dir
    //
    // there is also the case where LIT_DIR is an absolute path, LIT_WORK_TREE is not set and worktree
    // ends up being the cwd, this is needs to be resolved in the same way
    match layout.placement {
        MetadataPlacement::Direct => layout.metadata != worktree.join(".lit"),
        // <worktree>/.lit is metadata, parent is worktree
        MetadataPlacement::Embedded => false,
        // pointer file, its parent identifies the worktree even the metadata is elsewhere
        MetadataPlacement::Separate { .. } => false,
    }
}

fn ensure_dir(path: &Path) -> Result<(), InitError> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            let metadata =
                fs::symlink_metadata(path).map_err(|err| InitError::from_io_error(path, err))?;
            // safe to call create_dir() in existing dirs, it will return without touching them
            // TODO: this is TOCTOU case
            if metadata.file_type().is_dir() {
                Ok(())
            } else {
                Err(InitError::BadEntry {
                    path: path.to_path_buf(),
                    entry: EntryType::from(metadata.file_type()),
                })
            }
        }
        Err(err) => Err(InitError::from_io_error(path, err)),
    }
}

fn resolve_lit_entry(path: &Path) -> io::Result<Option<PathBuf>> {
    let path = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Some(fs::canonicalize(path)?),
        Ok(_) => Some(path.to_path_buf()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => return Err(err),
    };

    Ok(path)
}

fn ensure_file(path: &Path) -> Result<(), InitError> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(_) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            let metadata =
                fs::symlink_metadata(path).map_err(|err| InitError::from_io_error(path, err))?;
            if metadata.file_type().is_file() {
                Ok(())
            } else {
                Err(InitError::BadEntry {
                    path: path.to_path_buf(),
                    entry: EntryType::from(metadata.file_type()),
                })
            }
        }
        Err(err) => Err(InitError::from_io_error(path, err)),
    }
}

#[derive(Debug)]
pub(super) enum InitError {
    CurrentDirUnavailable(io::Error),
    Io { path: PathBuf, source: io::Error },
    // TODO: should this be UnsupportedFileType
    BadEntry { path: PathBuf, entry: EntryType },
    LitFile { path: PathBuf, err: LitFileError },
    Layout(LayoutError),
    MetadataDir(MetadataDirError),
    Config { path: PathBuf, source: ConfigFileError },
}

impl InitError {
    fn from_io_error(path: &Path, err: io::Error) -> Self {
        InitError::Io {
            path: path.to_path_buf(),
            source: err,
        }
    }
}

impl Error for InitError {}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InitError::CurrentDirUnavailable(err) => {
                write!(f, "could not determine current directory: {err}")
            }
            InitError::Io { path, source } => {
                write!(f, "{}: {}", path.display(), source)
            }
            InitError::BadEntry { path, entry } => {
                write!(
                    f,
                    "{} already exists and is not a {}",
                    path.display(),
                    entry
                )
            }
            InitError::LitFile { path, err } => {
                write!(f, "{}: {}", path.display(), err)
            }
            InitError::MetadataDir(err) => write!(f, "{err}"),
            InitError::Layout(err) => write!(f, "{err}"),
            InitError::Config { path, source } => write!(f, "{}: {}", path.display(), source),
        }
    }
}

impl From<LayoutError> for InitError {
    fn from(err: LayoutError) -> Self {
        Self::Layout(err)
    }
}

impl From<MetadataDirError> for InitError {
    fn from(err: MetadataDirError) -> Self {
        Self::MetadataDir(err)
    }
}

#[derive(Debug)]
pub(super) enum LayoutError {
    CurrentDirUnavailable(io::Error),
    // initially was an enum with 4 variants: SeparateLitDir, PositionalArg, LitDir, LitWorkTree
    // but we only constructed it, never had to match or any other action
    EmptyPath(&'static str),
    LitWorkTreeWithBare,
}

impl Error for LayoutError {}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LayoutError::CurrentDirUnavailable(err) => {
                write!(f, "could not determine current directory: {err}")
            }
            LayoutError::EmptyPath(source) => {
                write!(f, "the empty string is not valid path: {}", source)
            }
            LayoutError::LitWorkTreeWithBare => {
                write!(f, "LIT_WORK_TREE not allowed with --bare")
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EntryType {
    File,
    Directory,
    Symlink,
    Other,
}

impl fmt::Display for EntryType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            EntryType::File => "file",
            EntryType::Directory => "directory",
            EntryType::Symlink => "symlink",
            EntryType::Other => "other",
        };
        f.write_str(label)
    }
}

impl From<FileType> for EntryType {
    fn from(file_type: FileType) -> Self {
        if file_type.is_file() {
            Self::File
        } else if file_type.is_dir() {
            Self::Directory
        } else if file_type.is_symlink() {
            Self::Symlink
        } else {
            Self::Other
        }
    }
}

#[cfg(test)]
mod tests {
    // TODO: fix
    // use crate::cmd::init::{EntryType, Init, InitError};
    // use std::fs;
    // use std::path::{Path, PathBuf};
    // use tempfile;
    //
    // struct TempDir {
    //     // in both with_missing_root() and with_existing_root() when I wasn't keeping temp_dir alive
    //     // drop() was called and the temp_dir path was deleted. The test init_on_a_missing_dir()
    //     // will pass but for the wrong reason, when we pass the root to Init, it calls create_dir_all()
    //     // and it would create the path again and all assertions would pass. We only want Init to create
    //     // root in {temp_dir}/root and then .lit.
    //     // The test init_on_an_existing_dir() that called with_existing_root() would fail.
    //     // with_existing_root() creates {temp_dir}/root. when drop() was called, it would delete
    //     // temp_dir and all its entries including the root. In with_existing_root() we would call
    //     // let dir_entry = temp_dir.root.join("foo"); // a parent path that no longer exists
    //     // so fs::write(&dir_entry, b":)").unwrap(); would err
    //     //
    //     // This is why we need to keep the temp_dir alive.
    //     _temp_dir: tempfile::TempDir,
    //     root: PathBuf,
    //     lit: PathBuf,
    // }
    //
    // impl TempDir {
    //     fn with_missing_root(root: &Path) -> Self {
    //         let temp_dir = tempfile::tempdir().unwrap();
    //         let root = temp_dir.path().join(root);
    //         let lit = root.join(".lit");
    //
    //         Self {
    //             _temp_dir: temp_dir,
    //             root,
    //             lit,
    //         }
    //     }
    //
    //     fn with_existing_root(root: &Path) -> Self {
    //         let temp_dir = tempfile::tempdir().unwrap();
    //         let root = temp_dir.path().join(root);
    //         fs::create_dir(&root).unwrap();
    //
    //         let lit = root.join(".lit");
    //
    //         Self {
    //             _temp_dir: temp_dir,
    //             root,
    //             lit,
    //         }
    //     }
    //
    //     fn objects(&self) -> PathBuf {
    //         self.lit.join("objects")
    //     }
    //
    //     fn refs(&self) -> PathBuf {
    //         self.lit.join("refs")
    //     }
    //
    //     fn config(&self) -> PathBuf {
    //         self.lit.join("config")
    //     }
    // }
    //
    // #[test]
    // fn init_on_a_missing_dir() {
    //     let temp_dir = TempDir::with_missing_root("test".as_ref());
    //     // root does not exist and init should create {temp_dir}/root/.lit
    //     let init = Init {
    //         path: Some(temp_dir.root.clone()),
    //         git_dir: None,
    //     };
    //
    //     init.execute().unwrap();
    //
    //     assert!(temp_dir.root.is_dir());
    //     assert!(temp_dir.lit.is_dir());
    //     assert!(temp_dir.objects().is_dir());
    //     assert!(temp_dir.refs().is_dir());
    //     assert!(temp_dir.config().is_file());
    // }
    //
    // // we assert that calling init on an existing directory does not alter its structure or any of the
    // // contents of its entries
    // #[test]
    // fn init_on_an_existing_dir() {
    //     let temp_dir = TempDir::with_existing_root("test".as_ref());
    //     let init = Init {
    //         path: Some(temp_dir.root.clone()),
    //         git_dir: None,
    //     };
    //     // entry of the existing dir
    //     let dir_entry = temp_dir.root.join("foo");
    //     fs::write(&dir_entry, b":)").unwrap();
    //
    //     init.execute().unwrap();
    //
    //     let contents = fs::read(&dir_entry).unwrap();
    //
    //     assert!(temp_dir.objects().is_dir());
    //     assert!(temp_dir.refs().is_dir());
    //     assert!(temp_dir.config().is_file());
    //     assert!(dir_entry.is_file());
    //     assert_eq!(contents, ":)".as_bytes().to_vec())
    // }
    // // TODO: write an IT tests where we call execute twice and assert on the print logic
    //
    // // calling init in an existing repo should not touch any of the files of the directory no matter
    // // if they are owned by lit or not
    // #[test]
    // fn reinit_preserves_existing_repo_files() {
    //     let temp_dir = TempDir::with_existing_root("test".as_ref());
    //     let init = Init {
    //         path: Some(temp_dir.root.clone()),
    //         git_dir: None,
    //     };
    //
    //     init.execute().unwrap();
    //
    //     let config = temp_dir.config();
    //     let object_dir = temp_dir.objects().join("ef");
    //     let blob = object_dir.join("b1e0e54a68d5928831b3e3749ec764b346c987");
    //     let head = temp_dir.refs().join("HEAD");
    //
    //     // init creates objects/, but not the two-character object subdirectory.
    //     fs::create_dir_all(&object_dir).unwrap();
    //     fs::write(
    //         &config,
    //         b"[user]\n    name = Alex Morgan\n    email = alex.morgan@example.com\n",
    //     )
    //     .unwrap();
    //     // obviously this is not the actual content of the blob, it is zlibed compress, and we could
    //     // easily use random data
    //     fs::write(&blob, b"blob 6\0hello\n").unwrap();
    //     fs::write(&head, b"821bf054e7f1fbc9a920609db2b5b6e256382b4e").unwrap();
    //
    //     let config_before = fs::read(&config).unwrap();
    //     let blob_before = fs::read(&blob).unwrap();
    //     let head_before = fs::read(&head).unwrap();
    //
    //     init.execute().unwrap();
    //
    //     // reinit should not touch any of the content of the existing files
    //     assert_eq!(fs::read(&config).unwrap(), config_before);
    //     assert_eq!(fs::read(&blob).unwrap(), blob_before);
    //     assert_eq!(fs::read(&head).unwrap(), head_before);
    // }
    //
    // #[test]
    // fn reinit_recreates_deleted_repo_files() {
    //     let temp_dir = TempDir::with_existing_root("test".as_ref());
    //     let init = Init {
    //         path: Some(temp_dir.root.clone()),
    //         git_dir: None,
    //     };
    //
    //     init.execute().unwrap();
    //
    //     let config = temp_dir.config();
    //     let objects = temp_dir.objects();
    //     let refs = temp_dir.refs();
    //     fs::remove_dir_all(&objects).unwrap();
    //     fs::remove_dir_all(&refs).unwrap();
    //     fs::remove_file(&config).unwrap();
    //
    //     init.execute().unwrap();
    //
    //     assert!(objects.is_dir());
    //     assert!(refs.is_dir());
    //     assert!(config.is_file());
    // }
    //
    // // this is true for other dirs like objects and refs, they are all created by the same method
    // // ensure_dir()
    // #[test]
    // fn init_fails_when_lit_exists_but_is_not_a_directory() {
    //     let temp_dir = TempDir::with_existing_root("test".as_ref());
    //     fs::write(temp_dir.root.join(".lit"), ":/").unwrap();
    //
    //     let init = Init {
    //         path: Some(temp_dir.root.clone()),
    //         git_dir: None,
    //     };
    //
    //     match init.execute().unwrap_err() {
    //         InitError::BadEntry { path, entry } => {
    //             assert_eq!(path, temp_dir.root.join(".lit"));
    //             assert_eq!(entry, EntryType::File);
    //         }
    //         err => panic!("expected InitError::BadEntry, got {err:?}"),
    //     }
    // }
    //
    // // this is true for all files created by init
    // #[test]
    // fn init_fails_when_config_exists_but_is_not_a_file() {
    //     let temp_dir = TempDir::with_existing_root("test".as_ref());
    //     fs::create_dir_all(temp_dir.lit.join("config")).unwrap();
    //
    //     let init = Init {
    //         path: Some(temp_dir.root.clone()),
    //         git_dir: None,
    //     };
    //
    //     match init.execute().unwrap_err() {
    //         InitError::BadEntry { path, entry } => {
    //             assert_eq!(path, temp_dir.config());
    //             assert_eq!(entry, EntryType::Directory);
    //         }
    //         err => panic!("expected InitError::BadEntry, got {err:?}"),
    //     }
    // }
}
