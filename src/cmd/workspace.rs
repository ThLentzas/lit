use std::ffi::OsStr;
use std::fs;
use std::fs::Metadata;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

// list_files should be called with the cwd as path.
// returns the paths of all the files in path
pub(super) fn list_files<P: AsRef<Path>>(path: P) -> Vec<PathBuf> {
    let mut paths = Vec::new();

    for entry in fs::read_dir(path.as_ref()).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() == ".git" {
            continue;
        }
        // Git does not store empty directories as tree entries. Git stores files/blobs and trees
        // that lead to files. If a directory contains no tracked files, there is no blob inside it,
        // so there is no tree object that needs to exist. Git trees are Merkle trees, if there is
        // no blob to hash, we can't create such tree.
        if entry.file_type().unwrap().is_dir() {
            paths.extend(list_files(entry.path()));
        } else {
            paths.push(entry.path())
        }
    }
    paths
}

pub(crate) fn stat<P: AsRef<Path>>(path: P) -> u32 {
    // Git tracks symlinks as symlinks, not as the files they point to. fs::metadata(path) follows symlinks.
    // If path is a symlink to target, we get metadata about target. fs::symlink_metadata(path) does
    // not follow. We get metadata about the symlink itself.
    let metadata = fs::symlink_metadata(path.as_ref()).unwrap();
    mode(&metadata)
}

// if we tried to write a runtime if cfg!(windows) check, the Unix-only code would still need to
// compile on Windows and would fail and vice versa.
#[cfg(unix)]
fn mode(metadata: &Metadata) -> u32 {
    // fixed values for now
    // toDo: maybe use an Enum later
    if metadata.mode() & 0o111 != 0 { 0o100755 } else { 0o100644 }
}

#[cfg(windows)]
fn mode(_meta: &Metadata) -> u32 {
    0o100644
}

