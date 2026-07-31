mod workspace;
mod index;
mod report;
mod db;
mod object;
mod tree;
mod refs;
mod config;
mod lockfile;
mod pathspec;
mod timestamp;
mod os;
mod path;

use std::{env, fs};
use std::path::PathBuf;

// Repository knows the paths to files like .lit/objects, .lit/config etc, path layout
// unlike the user provided paths that have to go through Workspace, these paths are known
//
// The idea is for each component to know exactly the path it anchors
struct Repository {
    // the directory that owns .lit
    root: PathBuf,
    // lit is guaranteed to be a directory
    lit: PathBuf,
}

impl Repository {
    // cwd is either the root or a subdirectory of the root
    fn discover() -> Result<Self, RepoError> {
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

    fn db_path(&self) -> PathBuf {
        self.lit.join("objects")
    }

    fn index_path(&self) -> PathBuf {
        self.lit.join("index")
    }

    fn config_path(&self) -> PathBuf {
        self.lit.join("config")
    }

    fn refs_path(&self) -> PathBuf {
        self.lit.join("refs")
    }
}