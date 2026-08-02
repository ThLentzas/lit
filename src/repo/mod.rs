pub(super) mod workspace;
pub(super) mod index;
pub(super) mod report;
pub(super) mod db;
pub(super) mod object;
pub(super) mod tree;
pub(super) mod refs;
pub(super) mod config;
pub(super) mod lockfile;
pub(super) mod pathspec;
pub(super) mod timestamp;
pub(super) mod os;
pub(super) mod path;

use std::{env, fs, io};
use std::path::PathBuf;

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
    // cwd is either the root or a subdirectory of the root
    pub(super) fn discover() -> Result<Self, RepoError> {
        let cwd = env::current_dir().map_err(RepoError::CurrentDir)?;
        let mut dir = fs::canonicalize(&cwd).map_err(|err| RepoError::Io {
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
                return Err(RepoError::NotRepository);
            }
        }
    }

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
pub(super) enum RepoError {
    CurrentDir(io::Error),
    // cwd is not a lit repo or any of the parent directories
    NotRepository,
    Io { path: PathBuf, source: io::Error }
}