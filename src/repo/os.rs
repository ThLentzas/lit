use std::error::Error;
use std::ffi::OsStr;
use std::fs::Metadata;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(windows)]
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::{fmt, fs, io};

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub(crate) struct StatNode {
    pub(crate) kind: FileKind,
    pub(crate) stat: FileStat,
}

#[cfg(windows)]
const EPOCH_DIFF: u64 = 11_644_473_000;

#[cfg(windows)]
const TICKS_PER_SECOND: u64 = 10_000_000;

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub(crate) enum FileKind {
    // the flag determines if the file is executable or not by checking the permission bits
    // true means it is an executable
    Regular(bool),
    Symlink,
    Directory,
    Other,
}

// TODO: add GitLink support.
// cheap copy only 9 bytes
// TODO: explain why mode is not part of FileStat
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub(crate) struct FileStat {
    // change time, most recent time a file's attributes changed(owner group, perm, etc)
    pub(crate) ctime: u32,
    pub(crate) ctime_nsec: u32,
    // modify time, most recent time a file's contents changed
    pub(crate) mtime: u32,
    pub(crate) mtime_nsec: u32,
    pub(crate) dev: u32,
    pub(crate) ino: u32,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    // on disk size, truncated to 32-bit
    pub(crate) file_size: u32,
}

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
    let meta = fs::symlink_metadata(path).map_err(|err| OsError::Io {
        path: path.to_path_buf(),
        source: err,
    })?;
    let file_stat = FileStat {
        ctime: meta.ctime() as u32,
        ctime_nsec: meta.ctime_nsec() as u32,
        mtime: meta.mtime() as u32,
        mtime_nsec: meta.mtime_nsec() as u32,
        dev: meta.dev() as u32,
        ino: meta.ino() as u32,
        uid: meta.uid(),
        gid: meta.gid(),
        file_size: meta.size() as u32,
    };

    Ok(StatNode {
        kind: file_kind(&meta),
        stat: file_stat,
    })
}

// TODO: we need to check if it correctly retrieves information when a dir path does not have a trailing slash
// TODO: a/b is a dir not a file named b inside a
#[cfg(windows)]
pub(super) fn stat(path: &Path) -> Result<StatNode, OsError> {
    let meta = fs::symlink_metadata(path).map_err(|err| OsError::Io {
        path: path.to_path_buf(),
        source: err,
    })?;
    let ctime = meta.creation_time();
    let mtime = meta.last_write_time();

    let file_stat = FileStat {
        ctime: to_unix_time(ctime) as u32,
        ctime_nsec: to_unix_time_nsec(ctime) as u32,
        mtime: to_unix_time(mtime) as u32,
        mtime_nsec: to_unix_time_nsec(mtime) as u32,
        dev: 0,
        ino: 0,
        uid: 0,
        gid: 0,
        file_size: meta.size() as u32,
    };

    Ok(StatNode {
        kind: file_kind(&meta),
        stat: file_stat,
    })
}

#[cfg(unix)]
fn file_kind(meta: &Metadata) -> FileKind {
    let file_type = meta.file_type();

    if file_type.is_symlink() {
        FileKind::Symlink
    } else if file_type.is_file() {
        // many Unix permissions can mean executable 100700, 100710, 100711 etc
        // Git does not preserve them all, instead if the owner has the x right it is enough to classify
        // it as executable.
        if meta.mode() & 0o100 != 0 {
            FileKind::Regular(true)
        } else {
            FileKind::Regular(false)
        }
    } else if file_type.is_dir() {
        FileKind::Directory
    } else {
        FileKind::Other
    }
}

// TODO: this needs to change to check for a non-zero byte, OsStr does not have this guarantee
#[cfg(unix)]
pub(super) fn os_str_as_bytes(name: &OsStr) -> Result<Vec<u8>, OsError> {
    Ok(name.as_bytes().to_vec())
}

// TODO: this needs to change to check for a non-zero byte, OsStr does not have this guarantee
#[cfg(unix)]
pub(super) fn bytes_to_path(bytes: &[u8]) -> Result<PathBuf, OsError> {
    Ok(PathBuf::from(OsStr::from_bytes(bytes)))
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
pub(super) fn os_str_as_bytes(name: &OsStr) -> Result<Vec<u8>, OsError> {
    match name.to_str() {
        Some(utf8) => Ok(utf8.as_bytes().to_vec()),
        None => Err(OsError::NotUnicode {
            bytes: name.as_encoded_bytes().to_vec(),
        }),
    }
}

#[cfg(windows)]
pub(super) fn bytes_to_path(bytes: &[u8]) -> Result<PathBuf, OsError> {
    match str::from_utf8(bytes) {
        // FromIterator pushes each component with native separators
        // when invoked with RepoPath bytes like in the workspace::to_absolute() make sure that the
        // absolute path has the same separator for all the components
        // TODO: improve this comment
        // split() will return the components of the repo path, and then we join them with Window's
        // native separator
        Ok(utf8) => Ok(utf8.split('/').collect()),
        Err(_) => Err(OsError::NotUnicode {
            bytes: bytes.to_vec(),
        }),
    }
}

#[cfg(windows)]
fn file_kind(meta: &Metadata) -> FileKind {
    let file_type = meta.file_type();

    if file_type.is_symlink() {
        FileKind::Symlink
    } else if file_type.is_file() {
        FileKind::Regular(false)
    } else if file_type.is_dir() {
        FileKind::Directory
    } else {
        FileKind::Other
    }
}

#[derive(Debug)]
pub(super) enum OsError {
    #[cfg(windows)]
    NotUnicode {
        bytes: Vec<u8>,
    },
    Io {
        path: PathBuf,
        source: io::Error,
    },
}

impl Error for OsError {}

impl fmt::Display for OsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: Since bytes may represent arbitrary path data, {:?} is useful for debugging but not especially user-friendly
        // TODO: We need to use path printing logic of stdout_bytes()
        match self {
            #[cfg(windows)]
            OsError::NotUnicode { bytes } => {
                write!(f, "path contains invalid Unicode: {bytes:?}")
            }
            OsError::Io { path, source } => {
                write!(f, "{}: {source}", path.display())
            }
        }
    }
}
