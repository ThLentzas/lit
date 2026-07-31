use std::{fs, io};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::fs::Metadata;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(windows)]
use std::os::windows::fs::MetadataExt;
use crate::repo::index::StatNode;

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
pub(super) fn stat(path: &Path) -> Result<StatNode, OsError> {
    // Git tracks symlinks as symlinks, not as the file they point to.
    //  fs::metadata(path) follows symlinks. If path is a symlink to target, we get metadata about
    //  target.
    //  fs::symlink_metadata(path) does not follow. We get metadata about the symlink itself.
    let meta = fs::symlink_metadata(path).map_err(|err| {
        OsError::Io { path: path.to_path_buf(), source: err }
    })?;

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

// TODO: we need to check if it correctly retrieves information when a dir path does not have a trailing slash
// TODO: a/b is a dir not a file named b inside a
#[cfg(windows)]
pub(super) fn stat(path: &Path) -> Result<StatNode, OsError> {
    let meta = fs::symlink_metadata(path).map_err(|err| {
        OsError::Io { path: path.to_path_buf(), source: err }
    })?;
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
pub(super) fn name_as_bytes(name: &OsStr) -> Result<Vec<u8>, OsError>  {
    Ok(name.as_bytes().to_vec())
}

#[cfg(unix)]
pub(super) fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(bytes))
}

// TODO: check if true: Windows accepts / in paths. The Win32 file APIs (and therefore Rust's Path on
// TODO: Windows) treat / and \ as equivalent separators, so you never need to convert separators
// TODO: OsString is WTF-16 internally, so bytes must go through UTF-8, which is safe because Git for
// TODO: Windows stores index paths as UTF-8 by convention
//
// For Unix getting the underlying bytes for an OsStr is straightforward. A component is any byte
// sequence excluding NUL and /. Call as_bytes() and store them verbatim. The problem is with Windows
// and the WTF-16 encoding. If we tried to store the bytes verbatim we will not be able to store them
// in the index. In a WTF-16 encoding ASCII sequences always carry a 0x00 byte(LE or BE does not matter)
// which then will be rejected by Index because paths can't contain NUL. The workaround is to try to
// convert it to UTF-8(strict). Rust stores OsStr as WTF8, UTF8 with unpaired surrogates. Internally,
// it takes the u16 bit values returned by the OS, gets the Codepoint and converts that to UTF8.
// This is why for [00,41] we don't get two bytes in UTF8, because first it maps the byte sequence
// to the codepoint(41) and then it turns that to UTF8 bytes.
#[cfg(windows)]
pub(super) fn name_as_bytes(name: &OsStr) -> Result<Vec<u8>, OsError> {
    match name.to_str() {
        Some(utf8) => Ok(utf8.as_bytes().to_vec()),
        None => Err(OsError::NotUnicode { bytes: name.as_encoded_bytes().to_vec() }),
    }
}

#[cfg(windows)]
pub(super) fn bytes_to_path(bytes: &[u8]) -> Result<PathBuf, OsError> {
    match str::from_utf8(bytes) {
        // FromIterator pushes each component with native separators
        Ok(utf8) => Ok(utf8.split('/').collect()),
        Err(_) => Err(OsError::NotUnicode { bytes: bytes.to_vec() }),
    }
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

#[derive(Debug)]
pub(super) enum OsError {
    NotUnicode { bytes: Vec<u8> },
    Io { path: PathBuf, source: io::Error}
}


