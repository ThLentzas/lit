use std::error::Error;
use std::ffi::OsStr;
use std::fs::{Metadata, Permissions};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs as unix_fs;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use std::os::windows::fs as windows_fs;
#[cfg(windows)]
use std::os::windows::fs::MetadataExt;
use std::path::{Component, Components, Display, Path, PathBuf, StripPrefixError};
use std::{fmt, fs, io};

#[cfg(windows)]
const EPOCH_DIFF: u64 = 11_644_473_000;

#[cfg(windows)]
const TICKS_PER_SECOND: u64 = 10_000_000;

#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) struct OsPath {
    inner: PathBuf,
}

impl OsPath {
    #[cfg(unix)]
    pub(crate) fn new<P>(path: P) -> Result<Self, OsPathError>
    where
        P: Into<PathBuf>,
    {
        let path = path.into();
        let bytes = path.as_os_str().as_bytes();

        if bytes.is_empty() {
            return Err(OsPathError::Empty);
        }
        if memchr::memchr(0, bytes).is_some() {
            return Err(OsPathError::ContainsNul(path));
        }

        Ok(Self { inner: path })
    }

    // this validation is very weak.
    // We need stronger validation based on: https://learn.microsoft.com/en-us/windows/win32/fileio/naming-a-file
    #[cfg(windows)]
    pub(crate) fn new<P>(path: P) -> Result<Self, OsPathError>
    where
        P: Into<PathBuf>,
    {
        let path = path.into();
        // with encode_wide() we can inspect native code units
        // the u16 we get back is going to be the same for all ASCII characters as utf8
        // A: 0x0041 for utf16, 0x41 for utf8,
        // in utf16 all ASCII chars are represented with a high byte being 0x00, A: 00, 41. this
        // tripped me off initially that doing NUL checks would return true even if the path name
        // was just 'A' but we never look at each byte individually. In this case, we just get 41
        // back, its binary is 00000000(0x00) 01000001(0x41)
        let units: Vec<u16> = path.as_os_str().encode_wide().collect();

        if units.is_empty() {
            return Err(OsPathError::Empty);
        }
        if units.contains(&0) {
            return Err(OsPathError::ContainsNul(path));
        }
        Ok(Self { inner: path })
    }

    // this can be used in cases where paths are returned from syscalls like env::cwd()
    pub(crate) fn new_unchecked<P>(path: P) -> Self
    where
        P: Into<PathBuf>,
    {
        Self { inner: path.into() }
    }

    pub(crate) fn as_path(&self) -> &Path {
        &self.inner
    }

    pub(crate) fn as_os_str(&self) -> &OsStr {
        self.inner.as_os_str()
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        self.as_os_str().as_encoded_bytes()
    }

    pub(crate) fn is_absolute(&self) -> bool {
        self.inner.is_absolute()
    }

    pub(crate) fn is_relative(&self) -> bool {
        self.inner.is_relative()
    }

    pub(crate) fn components(&self) -> Components<'_> {
        self.inner.components()
    }

    pub(crate) fn join<P>(&self, path: P) -> Result<Self, OsPathError>
    where
        P: AsRef<Path>,
    {
        let path = Self::new(path.as_ref().to_path_buf())?;
        Ok(self.join_unchecked(path))
    }

    pub(crate) fn join_unchecked<P>(&self, path: P) -> Self
    where
        P: AsRef<Path>,
    {
        Self::new_unchecked(self.inner.join(path))
    }

    // returns a &Path because what is left can be an empty path and that would break the invariant
    // of OsPath
    pub(crate) fn strip_prefix(&self, base: &Path) -> Result<&Path, OsPathError> {
        self.inner
            .strip_prefix(base)
            .map_err(OsPathError::StripPrefix)
    }

    pub(crate) fn display(&self) -> Display<'_> {
        self.inner.display()
    }

    // remove redundant components: ./src/./main.rs simplifies to src/main.rs
    // src/.. becomes .
    //
    // 1. if the accumulated path ends with a normal filename component, simply put if it has a left
    // neighbor, pop it
    // 2. if we reached root, ignore '..', otherwise we preserve it
    //
    // Note: when I first wrote this in pathspec.rs::normalize() I thought that the goal was to eliminate
    // all the '.', '..' components, but I was wrong because we rewrite the path in a simplified form
    // it is a different representation of the same path
    //
    // Read notes it is explained fully there with examples.
    pub(crate) fn normalize_lexically(&self) -> OsPath {
        let mut path = PathBuf::new();

        // components() does some normalization
        // it can not have empty components: a//b is normalized to a/b
        for component in self.components() {
            match component {
                // A Windows path prefix, e.g., C: C:\, or \\server\share.
                // large variety of prefix types, check docs
                // does not occur on Unix.
                //
                // for absolute, we keep it as is, for relative we never encounter it
                Component::Prefix(prefix) => path.push(prefix.as_os_str()),
                // Unix: "/"
                // Windows: the "\" after a prefix like "C:\"
                Component::RootDir => path.push(component.as_os_str()),
                // we don't push or pop any component we stay where we are
                Component::CurDir => {}
                Component::ParentDir => {
                    // we pop only if it has a left neighbor
                    if matches!(path.components().next_back(), Some(Component::Normal(_))) {
                        path.pop();
                    // ../../ cant be simplified
                    // if path.has_root() is true we have a /.. case where it simplifies to /
                    } else if !path.has_root() {
                        path.push("..")
                    }
                }
                Component::Normal(name) => {
                    path.push(name);
                }
            }
        }
        // if the path ends being empty it means current dir, but we can't return an empty path because
        // it breaks the invariant of OsPath
        if path.as_os_str().is_empty() {
            path.push(".");
        }
        Self::new_unchecked(path)
    }
}

impl AsRef<Path> for OsPath {
    fn as_ref(&self) -> &Path {
        self.as_path()
    }
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub(crate) struct StatNode {
    pub(crate) kind: FileKind,
    pub(crate) stat: FileStat,
}

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
    //  fs::symlink_metadata() does not follow symlinks. We get metadata about the symlink itself
    //  fs::metadata() follows symlinks and reports metadata about the target
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
//  https://github.com/git-for-windows/git/blob/39c2bbe4d25b979d3e0f28c4d914d430b32e7e6a/compat/stat.c
//  stat() for Windows: https://github.com/git/git/blob/fa7f9290efe2bd22dd736689597b474b93798e11/compat/mingw.c#L1240-L1280
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

// https://github.com/git/git/blob/47ce80527c56f462cb97db4ca8125342204d3783/setup.c#L2616-L2626
// probe: a small test a program performs to discover how its environment actually behaves.
// TODO: we need to update status and add to honor this config var
//  when false we ignore executable-bit differences for tracked regular files and preserve their
//  existing index mode when staging content changes
#[cfg(unix)]
pub(crate) fn probe_filemode(path: &OsPath) -> io::Result<bool> {
    const OWNER_EXECUTE: u32 = 0o100;
    let before = fs::symlink_metadata(path)?;
    // only for files
    if !before.is_file() {
        return Ok(false);
    }

    let original_permissions = before.permissions();
    let original_mode = original_permissions.mode();
    // this is the change we want to make
    let toggle = original_mode ^ OWNER_EXECUTE;
    if fs::set_permissions(path, Permissions::from_mode(toggle)).is_err() {
        return Ok(false);
    }
    // even if reading the modified permissions fails, we don't return, we still need to restore
    // file's mode back to the original.
    let after = fs::symlink_metadata(path);
    // restore mode
    fs::set_permissions(path, original_permissions)?;
    let after = match after {
        Ok(metadata) => metadata,
        Err(_) => return Ok(false),
    };

    // XORing the original mode with the one after our attempt to set the exec-bit will always result
    // to 0 if the change was not accepted since both numbers will be identical, otherwise 1. The next
    // check is to isolate the exec-bit(XOR operates on the entire integer)
    Ok((original_mode ^ after.permissions().mode()) & OWNER_EXECUTE != 0)
}

#[cfg(windows)]
pub(crate) fn probe_filemode(_path: &OsPath) -> io::Result<bool> {
    // For now the windows stat impl does not report executable bits
    Ok(false)
}

// to determine if the fs supports symlinks we create a `test` path(does not need to exist, dangling
// symlinks are fine) and we check what the filesystem reports
pub(crate) fn probe_symlink(link: &OsPath) -> io::Result<bool> {
    let test = OsPath::new_unchecked("test_symlink");
    match symlink(&test, link) {
        Ok(_) => fs::symlink_metadata(link).map(|metadata| metadata.file_type().is_symlink()),
        Err(_) => Ok(false),
    }
}

#[cfg(unix)]
fn symlink(original: &OsPath, link: &OsPath) -> io::Result<()> {
    unix_fs::symlink(original, link)
}

#[cfg(windows)]
fn symlink(original: &OsPath, link: &OsPath) -> io::Result<()> {
    windows_fs::symlink_file(original, link)
}

// checks the owner's exec-bit, not whether the current process can actually execute the file.
pub(crate) fn is_executable(path: &OsPath) -> io::Result<bool> {
    let metadata = fs::symlink_metadata(path)?;
    Ok(matches!(file_kind(&metadata), FileKind::Regular(true)))
}

// TODO: this needs to change to check for a non-zero byte, OsStr does not have this guarantee
//  we need to have different methods for paths and OsStr not every OsStr is a path
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

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OsPathError {
    Empty,
    ContainsNul(PathBuf),
    StripPrefix(StripPrefixError),
}

impl Error for OsPathError {}

impl fmt::Display for OsPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OsPathError::Empty => write!(f, "path is empty"),
            OsPathError::ContainsNul(path) => {
                write!(f, "path {} contains NUL byte", path.display())
            }
            OsPathError::StripPrefix(err) => write!(f, "{err}"),
        }
    }
}

// TODO: when we support IoError remove this make os calls return IoError
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
