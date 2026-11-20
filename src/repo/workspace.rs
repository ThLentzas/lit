use crate::repo::os::{self, FileKind, IoError, IoErrorContext, OsPath, StatNode};
use crate::repo::repopath::RepoPath;
use std::fs;
use std::fs::File;
use std::path::Path;

// root relative path -> fs calls
//
// our internal language is root relative paths, that's what the index stores, what pathspec match
// etc. The syscalls though need absolute paths, Workspace is responsible for doing this translation
// It is the working tree viewed from the repo root.
//
// Note: Workspace does not need to do any conversion for the `core.ignorecase` flag. The os itself
// will resolve the spelling.
pub(crate) struct Workspace<'repo> {
    // root of the working tree
    root: &'repo OsPath,
}

impl<'repo> Workspace<'repo> {
    pub(super) fn new(root: &'repo OsPath) -> Self {
        Self { root }
    }
    pub(crate) fn root(&self) -> &OsPath {
        self.root
    }
    // resolves cwd path to root relative path
    //
    // prefix refers to the path from root to the cwd
    //
    // user provided path = main.rs
    // root = /repo
    // cwd = /repo/src
    // prefix = src
    //
    // we need the prefix to later compute repo relative paths
    // if cwd is root then prefix is an empty path
    // 
    // for now the only call is with `cwd` as path which comes from `env::current_dir()` and it is
    // already a canonical path, root is also in canonical form from repository discovery, so we don't
    // need any lexical normalization
    pub(crate) fn root_relative_prefix<'a>(&self, path: &'a OsPath) -> Option<&'a Path> {
        path.strip_prefix(&self.root).ok()
    }

    // returns the target's path
    pub(crate) fn read_link(&self, path: &RepoPath) -> Result<Vec<u8>, IoError> {
        let absolute = self.to_absolute(path);
        let path = fs::read_link(&absolute).with_context("openlink", Some(absolute))?;
        Ok(os::os_str_as_bytes(path.as_os_str()).to_vec())
    }

    //
    pub(crate) fn read_file(&self, path: &RepoPath) -> Result<Vec<u8>, IoError> {
        let absolute = self.to_absolute(path);

        fs::read(&absolute).with_context("open", Some(&absolute))
    }

    // for a line based file like `.litignore` determine its path and return the handler to the
    // parser so we can use a `BufReader`
    pub(crate) fn open_file(&self, path: &RepoPath) -> Result<File, IoError> {
        let absolute = self.to_absolute(path);
        File::open(&absolute).with_context("open", Some(&absolute))
    }

    pub(crate) fn stat(&self, path: &RepoPath) -> Result<StatNode, IoError> {
        let absolute = self.to_absolute(path);

        os::stat(&absolute)
    }

    // returns all entries of a directory pointed by the path, a list of all entries that live under
    // path, it is up to the caller if they want to recurse for subdirectories
    pub(crate) fn dir_entries(
        &self,
        path: &RepoPath,
    ) -> Result<Vec<(RepoPath, StatNode)>, IoError> {
        let absolute = self.to_absolute(path);
        let read_dir = match fs::read_dir(&absolute) {
            Ok(read_dir) => read_dir,
            Err(err) => return Err(IoError::new("opendir", Some(&absolute), err)),
        };

        let mut entries = Vec::new();
        // opening the directory can succeed, but reading one of its entries can fail later.
        // permission/access changes while iterating
        // directory is modified/deleted while iterating
        // filesystem/network drive error
        // entries are deleted
        for entry in read_dir {
            // if the iterator itself errs, there is no DirEntry, hence no name, the parent is
            // genuinely the most precise path available.
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => return Err(IoError::new("opendir", Some(&absolute), err)),
            };
            let name = entry.file_name();
            // TODO: remove .git and add .litignore, check if we can use matches!()
            if name == ".lit" || name == ".git" || name == "target" {
                continue;
            }
            // the root relative path of each entry is the root relative path of the parent + the
            // entry's name
            let child = path.join_unchecked(&name);
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
    ) -> Result<bool, IoError> {
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

    fn to_absolute(&self, path: &RepoPath) -> OsPath {
        self.root
            .join_unchecked(os::os_str_from_bytes(path.as_bytes()))
    }
}
