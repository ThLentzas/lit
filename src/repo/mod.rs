pub(super) mod config;
pub(super) mod db;
pub(super) mod index;
pub(super) mod litfile;
pub(super) mod lockfile;
pub(super) mod object;
pub(super) mod os;
pub(super) mod path;
pub(super) mod pathspec;
pub(super) mod refs;
pub(super) mod report;
pub(super) mod timestamp;
pub(super) mod tree;
pub(super) mod workspace;

use crate::repo::config::{ConfigFile, ConfigFileError};
use crate::repo::object::OidError;
use crate::repo::object::oid::Oid;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::{env, fmt, fs, io};

// validate that the directory pointed by path is a valid Lit repository before migration for the
// separate-lit-dir flag
// If `path` already points to a dir it moves it, if it points to a regular file then it must be a
// litfile, so it reads it first before moving. https://github.com/git/git/blob/master/setup.c#L2674
// https://github.com/git/git/blob/master/setup.c#L413
//
// The conditions that must hold true are:
//  - accessible dir pointed by path(r/w)
//  - valid HEAD, a proper "ref:", or a regular file HEAD that has a properly formatted sha1 object
//  name.
//  - accessible objects dir or LIT_OBJECT_DIRECTORY env var
//  - accessible refs dir
//  - has a valid core.formatversion value
//
// This is the structure a valid Lit repo guarantees
pub(super) fn validate_metadata_dir(path: &Path) -> Result<(), MetadataDirError> {
    require_accessible_dir(path)?;
    validate_head(&path.join("HEAD"))?;
    let objects_dir = match env::var_os("LIT_OBJECT_DIRECTORY") {
        None => path.join("objects"),
        Some(dir) => PathBuf::from(dir),
    };
    require_accessible_dir(&objects_dir)?;
    require_accessible_dir(&path.join("refs"))?;
    validate_format_version(path)?;

    Ok(())
}

fn require_accessible_dir(path: &Path) -> Result<(), MetadataDirError> {
    match fs::read_dir(path) {
        Ok(_) => Ok(()),
        Err(err) => Err(MetadataDirError::Io {
            path: path.to_path_buf(),
            op: "opendir",
            source: err,
        }),
    }
}

// TODO: when we add ref support, this need to check for ref: also
fn validate_head(path: &Path) -> Result<(), MetadataDirError> {
    let bytes = fs::read(path).map_err(|err| MetadataDirError::Io {
        path: path.to_path_buf(),
        op: "read",
        source: err,
    })?;

    let bytes = bytes
        .strip_suffix(b"\r\n")
        .or_else(|| bytes.strip_suffix(b"\n"))
        .unwrap_or(&bytes);

    match Oid::from_hex_bytes(bytes) {
        Ok(_) => Ok(()),
        Err(err) => Err(MetadataDirError::HeadBadOid {
            path: path.to_path_buf(),
            source: err,
        }),
    }
}

// https://git-scm.com/docs/gitrepository-layout
fn validate_format_version(path: &Path) -> Result<(), MetadataDirError> {
    let cfg_path = path.join("config");
    let cfg = ConfigFile::new(&cfg_path).map_err(|err| MetadataDirError::Config {
        path: cfg_path,
        source: err,
    })?;
    let version = match cfg.get_int("core.repositoryformatversion".as_ref()) {
        Ok(Some(val)) => val,
        // not found default's to 0
        Ok(None) => 0,
        Err(err) => {
            return Err(MetadataDirError::Config {
                path: path.to_path_buf(),
                source: err,
            });
        }
    };

    match version {
        0 => Ok(()),
        _ => Err(MetadataDirError::UnrecognizedVersion {
            path: path.to_path_buf(),
        }),
    }
}

// TODO: if repo never calls anything from cmd we good in terms of design it is clear separation

// Repository knows the paths to files like .lit/objects, .lit/config etc, path layout
// unlike the user provided paths that have to go through Workspace, these paths are known
//
// The idea is for each component to know exactly the path it anchors
pub(super) struct Repository {
    // the directory that owns .lit
    pub(super) root: PathBuf,
    // lit is guaranteed to be a directory
    lit: PathBuf,
}

impl Repository {
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
        path: PathBuf,
        source: io::Error,
    },
    HeadBadOid {
        path: PathBuf,
        source: OidError,
    },
    UnrecognizedVersion {
        path: PathBuf,
    },
    Config {
        path: PathBuf,
        source: ConfigFileError,
    },
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
            MetadataDirError::UnrecognizedVersion { path } => {
                write!(f, "unsupported format version in: {}", path.display())
            }
            MetadataDirError::Config { path, source } => {
                write!(f, "{}: {}", path.display(), source)
            }
        }
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
