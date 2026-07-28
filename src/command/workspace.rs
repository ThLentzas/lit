use crate::command::error::WorkspaceError;
use crate::command::index::StatNode;
use crate::command::os;
use std::path::{Path, PathBuf};
use std::{env, fs};

// root relative path -> fs calls
//
// our internal language is root relative paths, that's what the index stores, what pathspec match
// etc. The syscalls though need absolute paths, Workspace is responsible for doing this translation
// It is the working tree viewed from the repo root.
pub(super) struct Workspace {
    // absolute root + relative to root path = absolute path
    pub(super) root: PathBuf,
}

impl Workspace {
    // resolves cwd path to root relative path
    //
    // prefix refers to the path from the repository to the cwd
    //
    // user provided path = main.rs
    // root = /repo
    // cwd = /repo/src
    // prefix = src
    // 
    // we need the prefix to later compute repo relative paths
    // if cwd = root then prefix is ""
    pub(super) fn prefix(&self) -> Result<PathBuf, WorkspaceError> {
        let cwd = env::current_dir().map_err(|err| WorkspaceError::CurrentDir { source: err })?;

        cwd.strip_prefix(&self.root)
            .map(Path::to_path_buf)
            .map_err(|_| WorkspaceError::OutsideRepository { path: cwd })
    }

    // returns the target's path
    pub(super) fn read_link(&self, path: &Path) -> Result<PathBuf, WorkspaceError> {
        let absolute = self.root.join(path);

        fs::read_link(&absolute).map_err(|err| WorkspaceError::Io {
            path: absolute,
            source: err,
        })
    }

    pub(super) fn read_file(&self, path: &Path) -> Result<Vec<u8>, WorkspaceError> {
        let absolute = self.root.join(path);

        fs::read(&absolute).map_err(|err| WorkspaceError::Io {
            path: absolute,
            source: err,
        })
    }

    pub(super) fn stat(&self, path: &Path) -> Result<StatNode, WorkspaceError> {
        let absolute = self.root.join(path);

        os::stat(&absolute).map_err(|err| WorkspaceError::Io {
            path: absolute,
            source: err,
        })
    }

    // returns all entries of a directory pointed by the path
    // the returned path is repo relative
    // This approach returns a flat list, and it is up to the caller if they want to recurse for
    // directories
    pub fn dir_entries(&self, relative: &Path) -> Result<Vec<(PathBuf, StatNode)>, WorkspaceError> {
        let absolute = self.root.join(relative);
        let read_dir = fs::read_dir(&absolute).map_err(|err| WorkspaceError::Io {
            path: absolute,
            source: err,
        })?;

        let mut entries = Vec::new();
        // opening the directory can succeed, but reading one of its entries can fail later.
        // permission/access changes while iterating
        // directory is modified/deleted while iterating
        // filesystem/network drive error
        // entries are deleted
        for entry in read_dir {
            // If the iterator itself errs, there is no DirEntry, hence no name, the parent is
            // genuinely the most precise path available.
            let entry = entry.map_err(|err| WorkspaceError::Io {
                path: relative.to_path_buf(),
                source: err,
            })?;

            let name = entry.file_name();
            // toDo: should this be an error?
            // remove .git and add .litignore
            if name == ".lit" || name == ".git" || name == "target" {
                continue;
            }

            let child = relative.join(&name);
            let stat = os::stat(&entry.path()).map_err(|err| WorkspaceError::Io {
                path: child.clone(),
                source: err,
            })?;
            entries.push((child, stat));
        }

        // Git does not store empty directories as tree entries. Git stores files/blobs and trees
        // that lead to files. If a directory contains no tracked files, there is no blob inside it,
        // so there is no tree object that needs to exist. Git trees are Merkle trees, if there is
        // no blob to hash, we can't create such tree.
        Ok(entries)
    }

    pub(super) fn contains_trackable_file(&self, path: &Path, mode: u32) -> Result<bool, WorkspaceError> {
        if matches!(mode, os::REGULAR | os::EXECUTABLE | os::SYMLINK) {
            return Ok(true);
        }
        if mode != os::DIR {
            return Ok(false);
        }

        for (path, stat) in self.dir_entries(path)? {
            if self.contains_trackable_file(&path, stat.mode)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}
