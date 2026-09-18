pub(super) mod config;
pub(super) mod db;
pub(super) mod format;
pub(super) mod index;
pub(super) mod litfile;
pub(super) mod lockfile;
pub(super) mod object;
pub(super) mod os;
pub(super) mod pathspec;
pub(super) mod refs;
pub(super) mod repo_path;
pub(super) mod report;
pub(super) mod timestamp;
pub(super) mod tree;
pub(super) mod workspace;
mod diagnostic;

use crate::repo::config::{ConfigFile, ConfigFileErrorKind};
use crate::repo::format::{RepositoryFormat, RepositoryFormatError};
use crate::repo::object::OidError;
use crate::repo::object::oid::Oid;
use crate::repo::os::{OsPath, OsPathError};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::{env, fmt, fs, io};

enum MetadataPlacement {
    // metadata dir is used directly
    Direct,
    // Ordinary `<worktree>/.lit` dir
    Embedded,
    // `<worktree>/.lit` is a pointer file to metadata
    Separate { link: OsPath },
}

pub(super) struct Layout {
    // directory containing the repository metadata (HEAD, config, objects, ...)
    metadata: OsPath,
    // root of the working tree for non-bare
    // None for bare
    worktree: Option<OsPath>,
    // describes how the metadata is connected to the worktree
    placement: MetadataPlacement,
}

impl Layout {
    // There are 4 factors that determine the location of .lit dir when initializing a repo.
    // --bare, --separate_lit_dir, path and the LIT_DIR/LIT_WORK_TREE env var.
    //
    //  Resolves the metadata and worktree locations without touching the fs
    //
    // Git's init impl will try to "guess" whether a repo should be bare from the value of GIT_DIR
    // https://github.com/git/git/blob/18e66859d87fb4b76599f73460b54f0848c76b16/builtin/init-db.c#L17-L48
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
    pub(super) fn resolve(
        path: Option<&Path>,
        bare: bool,
        separate_lit_dir: Option<&Path>,
    ) -> Result<Self, LayoutError> {
        // highest precedence, reject early
        // if we call map(OsPath::new) we get Option<Result<T, E>> but what we want is Result<Option<T>, E>
        // that is what transpose does
        let path = path.map(OsPath::new).transpose()?;

        let cwd = env::current_dir().map_err(LayoutError::CurrentDirUnavailable)?;
        let cwd = OsPath::new_unchecked(cwd);
        let root = path.as_ref().map_or(cwd.clone(), |path| cwd.join_unchecked(path));

        if let Some(dir_path) = separate_lit_dir {
            // TODO: should this be a notification to the user that LIT_DIR is actually ignored
            //  because the flag has higher precedence. This is a conflict because both try to
            //  name the metadata dir
            return Ok(Self {
                metadata: cwd.join(dir_path)?,
                // the parent identifies the worktree even though the metadata is elsewhere
                worktree: Some(root.clone()),
                placement: MetadataPlacement::Separate {
                    link: root.join_unchecked(".lit"),
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
                    metadata: root.join_unchecked(".lit"), // <worktree>.lit
                    worktree: Some(root),
                    placement: MetadataPlacement::Embedded,
                })
            };
        }

        if let Some(env_dir) = env::var_os("LIT_DIR") {
            let env_dir = cwd.join(env_dir)?;
            // LIT_WORK_TREE makes sense only in conjunction with LIT_DIR without --bare. In any
            // other case it is ignored.
            let worktree = env::var_os("LIT_WORK_TREE");
            let worktree = worktree.map(OsPath::new).transpose()?;
            // bare repos have no worktree
            if bare && worktree.is_some() {
                return Err(LayoutError::LitWorkTreeWithBare);
            }

            return if bare {
                Ok(Self {
                    metadata: env_dir,
                    worktree: None,
                    placement: MetadataPlacement::Direct,
                })
            } else {
                // if no WORK_TREE found we fall back to cwd
                let worktree =
                    worktree.map_or(cwd.clone(), |worktree| cwd.join_unchecked(worktree));
                Ok(Self {
                    metadata: env_dir,
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
                metadata: cwd.join_unchecked(".lit"),
                worktree: Some(cwd),
                placement: MetadataPlacement::Embedded,
            })
        }
    }

    // the worktree root for non-bare or the metadata directory for bare
    pub(super) fn root(&self) -> &OsPath {
        self.worktree.as_ref().unwrap_or(&self.metadata)
    }

    pub(super) fn metadata(&self) -> &OsPath {
        &self.metadata
    }

    pub(super) fn separate_link(&self) -> Option<&OsPath> {
        if let MetadataPlacement::Separate { link } = &self.placement {
            return Some(link);
        }
        None
    }

    pub(super) fn is_bare(&self) -> bool {
        self.worktree.is_none()
    }

    // decide if we have to set core.worktree in config
    pub(super) fn needs_worktree_config(&self) -> bool {
        let Some(worktree) = &self.worktree else {
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
        match self.placement {
            MetadataPlacement::Direct => self.metadata != worktree.join_unchecked(".lit"),
            // <worktree>/.lit is metadata, parent is worktree
            MetadataPlacement::Embedded => false,
            // pointer file, its parent identifies the worktree even the metadata is elsewhere
            MetadataPlacement::Separate { .. } => false,
        }
    }
}

// validate that the directory pointed by path is a valid Lit repository before migration for the
// separate-lit-dir flag
// If `path` already points to a dir it moves it, if it points to a regular file then it must be a
// litfile, so it reads it first before moving. https://github.com/git/git/blob/master/setup.c#L2674
// https://github.com/git/git/blob/master/setup.c#L413
//
// The conditions that must hold true are:
//  - accessible dir pointed by path(r/w)
//  - valid HEAD, a proper "ref:", or a regular file HEAD that has a properly formatted sha1 object
//  name
//  - accessible objects dir or LIT_OBJECT_DIRECTORY env var
//  - accessible refs dir
//  - has a valid repository format
//
// This is the structure a valid Lit repo guarantees
pub(super) fn validate_metadata_dir(path: &OsPath) -> Result<(), MetadataDirError> {
    require_accessible_dir(path)?;
    validate_head(&path.join_unchecked("HEAD"))?;
    // TODO: we need to look at the precedence here
    let objects_dir = match env::var_os("LIT_OBJECT_DIRECTORY") {
        None => path.join_unchecked("objects"),
        Some(dir) => {
            let cwd = env::current_dir().map_err(MetadataDirError::CurrentDirUnavailable)?;
            let cwd = OsPath::new_unchecked(cwd);
            cwd.join(dir)?
        },
    };

    require_accessible_dir(&objects_dir)?;
    require_accessible_dir(&path.join_unchecked("refs"))?;
    validate_format_version(path)
}

fn validate_format_version(path: &OsPath) -> Result<(), MetadataDirError> {
    let cfg_path = path.join_unchecked("config");

    match ConfigFile::new(&cfg_path) {
        Ok(cfg) => RepositoryFormat::from_config(&cfg)?
            .ok_or(MetadataDirError::MissingFormatVersion(cfg_path))
            .map(|_format| ()),
        Err(err)
            if err
                .io_error_kind()
                .is_some_and(|kind| kind == io::ErrorKind::NotFound) =>
        {
            Err(MetadataDirError::MissingConfigFile(path.clone()))
        }
        Err(err) => Err(MetadataDirError::Config {
            path: cfg_path,
            source: err,
        }),
    }
}

fn require_accessible_dir(path: &OsPath) -> Result<(), MetadataDirError> {
    match fs::read_dir(path) {
        Ok(_) => Ok(()),
        Err(err) => Err(MetadataDirError::Io {
            path: path.clone(),
            op: "opendir",
            source: err,
        }),
    }
}

// TODO: when we add ref support, this need to check for ref: also
fn validate_head(path: &OsPath) -> Result<(), MetadataDirError> {
    let bytes = fs::read(path).map_err(|err| MetadataDirError::Io {
        path: path.clone(),
        op: "read",
        source: err,
    })?;

    let bytes = bytes
        .strip_suffix(b"\r\n")
        .or_else(|| bytes.strip_suffix(b"\n"))
        .unwrap_or(&bytes);

    // DETACHED HEAD
    match Oid::from_hex_bytes(bytes) {
        Ok(_) => Ok(()),
        Err(err) => Err(MetadataDirError::HeadBadOid {
            path: path.clone(),
            source: err,
        }),
    }
}

// TODO: if repo never calls anything from cmd we good in terms of design it is clear separation

// TODO: https://stackoverflow.com/questions/65499497/how-does-git-know-its-in-a-git-repo
//
// The idea is for each component to know exactly the path it anchors
pub(super) struct Repository {
    layout: Layout,
    format: RepositoryFormat,
}

impl Repository {
    pub(super) fn new(layout: Layout, format: RepositoryFormat) -> Self {
        Self { layout, format }
    }
    // TODO: before any decision review: https://git-scm.com/docs/gitrepository-layout
    // TODO: discovery needs to first check LIT_DIR, https://git-scm.com/book/en/v2/Git-Internals-Environment-Variables
    // TODO: we also need to check if .lit is a file it might hold a pointer to metadata same as
    //  lit-link in separate-lit-dir init option
    //  we need also something like requires_worktree() for any command that can be invoked in a non-bare repo
    // cwd is either the root or a subdirectory of the root
    pub(super) fn discover() -> Result<Self, DiscoverError> {
        let cwd = env::current_dir().map_err(DiscoverError::CurrentDirUnavailable)?;
        let mut dir = fs::canonicalize(&cwd).map_err(|err| DiscoverError::Io {
            path: cwd,
            source: err,
        })?;

        loop {
            let lit = dir.join(".lit");
            if lit.is_dir() {
                return Ok(Self { root: dir, lit });
            }
            // sets dir to parent, returns false if parent is None
            if !dir.pop() {
                return Err(DiscoverError::NotRepository);
            }
        }
    }

    // TODO: this needs to consider LIT_OBJECT_DIRECTORY env var?
    pub(super) fn db_path(&self) -> PathBuf {
        self.lit.join("objects")
    }

    pub(super) fn index_path(&self) -> PathBuf {
        self.lit.join("index")
    }

    pub(super) fn config_path(&self) -> PathBuf {
        self.lit.join("config")
    }

    pub(super) fn refs_path(&self) -> PathBuf {
        self.lit.join("refs")
    }
}

#[derive(Debug)]
pub(super) enum MetadataDirError {
    Io {
        op: &'static str,
        path: OsPath,
        source: io::Error,
    },
    HeadBadOid {
        path: OsPath,
        source: OidError,
    },
    Format(RepositoryFormatError),
    Config {
        path: OsPath,
        source: ConfigFileErrorKind,
    },
    // path to config
    MissingFormatVersion(OsPath),
    MissingConfigFile(OsPath),
    CurrentDirUnavailable(io::Error),
    OsPath(OsPathError),
}

impl Error for MetadataDirError {}

impl fmt::Display for MetadataDirError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MetadataDirError::Io { op, path, source } => {
                write!(f, "{}: {} {}", op, source, path.display())
            }
            MetadataDirError::HeadBadOid { path, source } => {
                write!(f, "invalid HEAD in {}: {}", path.display(), source)
            }
            MetadataDirError::Format(err) => write!(f, "{err}"),
            MetadataDirError::Config { path, source } => {
                write!(f, "{}: {}", path.display(), source)
            }
            MetadataDirError::MissingFormatVersion(path) => {
                write!(
                    f,
                    "missing 'core.repositoryformatversion' in {}",
                    path.display()
                )
            }
            MetadataDirError::MissingConfigFile(path) => {
                write!(f, "missing config file in {}", path.display())
            }
            MetadataDirError::CurrentDirUnavailable(err) => {
                write!(f, "could not determine current directory: {err}")
            }
            MetadataDirError::OsPath(source) => {
                write!(f, "{source}")
            }
        }
    }
}

impl From<RepositoryFormatError> for MetadataDirError {
    fn from(err: RepositoryFormatError) -> Self {
        Self::Format(err)
    }
}

impl From<OsPathError> for MetadataDirError {
    fn from(err: OsPathError) -> Self {
        Self::OsPath(err)
    }
}

#[derive(Debug)]
pub(super) enum DiscoverError {
    CurrentDirUnavailable(io::Error),
    // cwd is not a lit repo or any of the parent directories
    NotRepository,
    Io { path: PathBuf, source: io::Error },
}

impl Error for DiscoverError {}

impl fmt::Display for DiscoverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiscoverError::CurrentDirUnavailable(err) => {
                write!(f, "could not determine current directory: {err}")
            }
            DiscoverError::NotRepository => {
                write!(
                    f,
                    "not a lit repository (or any of the parent directories): .lit"
                )
            }
            DiscoverError::Io { path, source } => {
                write!(f, "{}: {source}", path.display())
            }
        }
    }
}

#[derive(Debug)]
pub(super) enum LayoutError {
    CurrentDirUnavailable(io::Error),
    OsPath(OsPathError),
    LitWorkTreeWithBare,
}

impl Error for LayoutError {}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LayoutError::CurrentDirUnavailable(err) => {
                write!(f, "could not determine current directory: {err}")
            }
            LayoutError::OsPath(source) => {
                write!(f, "{source}")
            }
            LayoutError::LitWorkTreeWithBare => {
                write!(f, "LIT_WORK_TREE not allowed with --bare")
            }
        }
    }
}

impl From<OsPathError> for LayoutError {
    fn from(err: OsPathError) -> Self {
        Self::OsPath(err)
    }
}