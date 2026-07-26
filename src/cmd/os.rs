use std::{fs, io};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use crate::cmd::index::StatNode;
use std::fs::Metadata;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(windows)]
use std::os::windows::fs::MetadataExt;

pub(super) const REGULAR: u32 = 0o100644;
pub(super) const EXECUTABLE: u32 = 0o100755;
pub(super) const DIR: u32 = 0o040000;
pub(super) const SYMLINK: u32 = 0o120000;
const UNSUPPORTED: u32 = 0;
#[cfg(windows)]
const EPOCH_DIFF: u64 = 11_644_473_000;
#[cfg(windows)]
const TICKS_PER_SECOND: u64 = 10_000_000;

// Read notes
#[cfg(windows)]
fn to_unix_time(filetime: u64) -> u64 {
    (filetime / TICKS_PER_SECOND).saturating_sub(EPOCH_DIFF)
}

#[cfg(windows)]
fn to_unix_time_nsec(filetime: u64) -> u64 {
    (filetime % TICKS_PER_SECOND) * 100
}

#[cfg(unix)]
pub(super) fn stat(path: &Path) -> Result<StatNode, io::Error> {
    // Git tracks symlinks as symlinks, not as the files they point to. 
    //  fs::metadata(path) follows symlinks. If path is a symlink to target, we get metadata about 
    //  target. 
    //  fs::symlink_metadata(path) does not follow. We get metadata about the symlink itself.
    let meta = fs::symlink_metadata(path)?;

    Ok(StatNode {
        ctime: meta.ctime() as u32,
        ctime_nsec: meta.ctime_nsec() as u32,
        mtime: meta.mtime() as u32,
        mtime_nsec: meta.mtime_nsec() as u32,
        dev: meta.dev() as u32,
        ino: meta.ino() as u32,
        mode: mode(&meta),
        uid: meta.uid(),
        gid: meta.gid(),
        file_size: meta.size() as u32,
    })
}

#[cfg(windows)]
pub(super) fn stat(path: &Path) -> Result<StatNode, io::Error> {
    let meta = fs::symlink_metadata(path)?;
    let ctime = meta.creation_time();
    let mtime = meta.last_write_time();

    Ok(StatNode {
        ctime: to_unix_time(ctime) as u32,
        ctime_nsec: to_unix_time_nsec(ctime) as u32,
        mtime: to_unix_time(mtime) as u32,
        mtime_nsec: to_unix_time_nsec(mtime) as u32,
        dev: 0,
        ino: 0,
        mode: mode(&meta),
        uid: 0,
        gid: 0,
        file_size: meta.file_size() as u32,
    })
}

#[cfg(unix)]
fn mode(meta: &Metadata) -> u32 {
    let file_type = meta.file_type();

    if file_type.is_symlink() {
        SYMLINK
    } else if meta.is_file() {
        if meta.mode() & 0o111 != 0 {
            EXECUTABLE
        } else {
            REGULAR
        }
    } else if meta.is_dir() {
        DIR
    } else { // unsupported
        UNSUPPORTED
    }
}

#[cfg(unix)]
pub(super) fn name_as_bytes(name: &OsStr) -> &[u8] {
    name.as_bytes()
}

#[cfg(unix)]
pub(super) fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(bytes))
}

// TODO: check if true: Windows accepts / in paths. The Win32 file APIs (and therefore Rust's Path on
// TODO: Windows) treat / and \ as equivalent separators, so you never need to convert separators
// TODO: OsString is WTF-16 internally, so bytes must go through UTF-8 — which is safe because Git for 
// TODO: Windows stores index paths as UTF-8 by convention
#[cfg(windows)]
pub(super) fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes))
}

#[cfg(windows)]
fn mode(meta: &Metadata) -> u32 {
    let file_type = meta.file_type();

    if file_type.is_symlink() {
        SYMLINK
    } else if meta.is_file() { // all files on Windows are treated as regular
        REGULAR
    } else if meta.is_dir() {
        DIR
    } else {
        UNSUPPORTED
    }
}

#[cfg(windows)]
pub(super) fn name_as_bytes(name: &OsStr) -> &[u8] {
    // toDo: check if this unwrap is safe
    let s = name.to_str().unwrap();
    s.as_bytes()
}


