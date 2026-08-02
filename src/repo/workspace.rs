use crate::repo::os;
use crate::repo::path::RepoPath;
use std::path::{Path, PathBuf};
use std::{env, fs, io};
use crate::repo::os::{FileKind, OsError, StatNode};

// root relative path -> fs calls
//
// our internal language is root relative paths, that's what the index stores, what pathspec match
// etc. The syscalls though need absolute paths, Workspace is responsible for doing this translation
// It is the working tree viewed from the repo root.
pub(crate) struct Workspace {
    // absolute root + relative to root path = absolute path
    pub(crate) root: PathBuf,
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
    pub(crate) fn prefix(&self) -> Result<PathBuf, WorkspaceError> {
        let cwd = env::current_dir().map_err(|err| WorkspaceError::CurrentDir { err })?;

        cwd.strip_prefix(&self.root)
            .map(Path::to_path_buf)
            .map_err(|_| WorkspaceError::OutsideRepository { path: cwd })
    }

    // returns the target's path
    pub(crate) fn read_link(&self, path: &RepoPath) -> Result<Vec<u8>, WorkspaceError> {
        let absolute = self.to_absolute(path)?;
        let path = fs::read_link(&absolute).map_err(|err| WorkspaceError::Io {
            path: absolute,
            source: err,
        })?;
        
        Ok(os::name_as_bytes(path.as_os_str())?)
    }

    pub(crate) fn read_file(&self, path: &RepoPath) -> Result<Vec<u8>, WorkspaceError> {
        let absolute = self.to_absolute(path)?;

        fs::read(&absolute).map_err(|err| WorkspaceError::Io {
            path: absolute,
            source: err,
        })
    }

    pub(crate) fn stat(&self, path: &RepoPath) -> Result<StatNode, WorkspaceError> {
        let absolute = self.to_absolute(path)?;
        
        Ok(os::stat(&absolute)?)
    }

    // returns all entries of a directory pointed by the path
    // This approach returns a flat list, and it is up to the caller if they want to recurse for
    // subdirectories
    pub(crate) fn dir_entries(
        &self,
        path: &RepoPath,
    ) -> Result<Vec<(RepoPath, StatNode)>, WorkspaceError> {
        let absolute = self.to_absolute(path)?;
        let read_dir = match fs::read_dir(&absolute) {
            Ok(read_dir) => read_dir,
            Err(err) => {
                return Err(WorkspaceError::Io {
                    path: absolute,
                    source: err,
                });
            }
        };

        let mut entries = Vec::new();
        // opening the directory can succeed, but reading one of its entries can fail later.
        // permission/access changes while iterating
        // directory is modified/deleted while iterating
        // filesystem/network drive error
        // entries are deleted
        for entry in read_dir {
            // If the iterator itself errs, there is no DirEntry, hence no name, the parent is
            // genuinely the most precise path available.
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    return Err(WorkspaceError::Io {
                        path: absolute,
                        source: err,
                    });
                }
            };
            let name = entry.file_name();
            // TODO: remove .git and add .litignore
            if name == ".lit" || name == ".git" || name == "target" {
                continue;
            }
            let bytes = os::name_as_bytes(&name)?;
            // the root relative path of each entry is the root relative path of the parent + the
            // entry's name
            let child = path.join(&bytes);
            let stat = self.stat(&child)?;
            entries.push((child, stat));
        }
        // Git does not store empty directories as tree entries. Git stores files/blobs and trees
        // that lead to files. If a directory contains no tracked files, there is no blob inside it,
        // so there is no tree object that needs to exist. Git trees are Merkle trees, if there is
        // no blob to hash, we can't create such tree.
        Ok(entries)
    }

    pub(super) fn contains_trackable_file(
        &self,
        path: &RepoPath,
        kind: &FileKind,
    ) -> Result<bool, WorkspaceError> {
        match kind {
            FileKind::Regular { .. } | FileKind::Symlink => Ok(true),
            FileKind::Other => Ok(false),
            FileKind::Directory => {
                for (path, stat) in self.dir_entries(path)? {
                    if self.contains_trackable_file(&path, &stat.kind)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
        }
    }

    fn to_absolute(&self, path: &RepoPath) -> Result<PathBuf, WorkspaceError> {
        Ok(self.root.join(os::bytes_to_path(path.as_bytes())?))
    }
}

#[derive(Debug)]
pub(crate) enum WorkspaceError {
    CurrentDir { err: io::Error },
    Io { path: PathBuf, source: io::Error },
    OutsideRepository { path: PathBuf },
    Os(OsError)
}

impl From<OsError> for WorkspaceError {
    fn from(err: OsError) -> Self {
        WorkspaceError::Os(err)
    }
}