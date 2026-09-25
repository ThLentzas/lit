use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File, Metadata, Permissions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{self as unix_fs, MetadataExt, PermissionsExt};
use std::path::{Component, Components, Display, Path, PathBuf, StripPrefixError};
use tempfile::NamedTempFile;

#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) struct OsPath {
    inner: PathBuf,
}

impl OsPath {
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

    // this can be used in cases where paths are returned from syscalls like env::cwd()
    pub(crate) fn new_unchecked<P>(path: P) -> Self
    where
        P: Into<PathBuf>,
    {
        Self { inner: path.into() }
    }

    pub(crate) fn inner(&self) -> &Path {
        &self.inner
    }

    pub(crate) fn as_os_str(&self) -> &OsStr {
        self.inner.as_os_str()
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        self.as_os_str().as_bytes()
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

    // As for now we can't return Option<&OsPath> because PathBuf::parent() returns &Path and calling
    // .map(|parent| OsPath::new_unchecked(parent)).as_ref() returns a ref to a local variable that
    // gets dropped
    pub(crate) fn parent(&self) -> Option<OsPath> {
        self.inner.parent().map(OsPath::new_unchecked)
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
    pub(crate) fn strip_prefix<P>(&self, base: P) -> Result<&Path, OsPathError>
    where
        P: AsRef<Path>,
    {
        self.inner
            .strip_prefix(base)
            .map_err(OsPathError::StripPrefix)
    }

    pub(crate) fn with_suffix_unchecked<P>(&self, suffix: P) -> Self
    where
        P: AsRef<OsStr>,
    {
        let mut path = self.as_os_str().to_os_string();
        path.push(suffix);
        Self::new_unchecked(path)
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

    // we can't just use `==` for path comparisons because `PartialEq` for `PathBuf` calls
    // `self.components() == other.components()`. It compares path values but paths can still refer
    // to the same fs path even if they have different components.
    //
    // Read: repo/mod.rs::MetadataPlacement::from_discovery()
    pub(crate) fn has_same_canonical_path_with(&self, other: &OsPath) -> Result<bool, IoError> {
        let lhs = fs::canonicalize(self).with_context("realpath", Some(self))?;
        let rhs = fs::canonicalize(other).with_context("realpath", Some(other))?;

        Ok(lhs == rhs)
    }
}

impl AsRef<Path> for OsPath {
    fn as_ref(&self) -> &Path {
        &self.inner
    }
}

pub(crate) fn atomic_write(
    parent_dir: &OsPath,
    content: &[u8],
    destination: &OsPath,
) -> Result<(), IoError> {
    let parent = File::open(parent_dir).with_context("open", Some(parent_dir))?;
    // NamedTempFile as of 3.27.0 uses as default name .tmp + 6 alphanumeric, '.tmpA7k2Qz'
    // if the name already exists, it retries so we don't have to consider collisions
    // https://docs.rs/tempfile/3.27.0/tempfile/struct.Builder.html
    let mut tempfile =
        NamedTempFile::new_in(parent_dir).with_context("create temp file in", Some(parent_dir))?;
    tempfile
        .write_all(content)
        .with_context("write", Some(tempfile.path()))?;
    // same as Lockfile::commit()
    // durable: a guarantee that the new version of the file will be available if there is a crash
    tempfile
        .as_file()
        .sync_all()
        .with_context("fsync", Some(tempfile.path()))?;
    // if let Err(err) = fs::rename(&tempfile, destination) {
    //     // Try to clean up the temp file before returning the error
    //     let _ = fs::remove_file(tempfile.path());
    //     return Err(err);
    // }
    // https://docs.rs/tempfile/latest/tempfile/struct.NamedTempFile.html#method.persist
    tempfile
        .persist(&destination)
        .map_err(|err| err.error)
        .with_context("rename", Some(destination))?;
    // https://www.reddit.com/r/kernel/comments/1mkykhz/fsync_on_file_and_parent_directory/
    //
    // From the fsync docs: https://man7.org/linux/man-pages/man2/fsync.2.html
    //  Calling fsync() does not necessarily ensure that the entry in the directory containing the
    //  file has also reached disk. For that an explicit fsync() on a file descriptor for the
    //  directory is also needed.
    //
    // The file's content and its name in the directory are separate fs info.
    parent.sync_all().with_context("fsync", Some(parent_dir))?;

    Ok(())
}

pub(crate) fn os_str_as_bytes(os_str: &OsStr) -> &[u8] {
    os_str.as_bytes()
}

pub(crate) fn os_str_from_bytes(bytes: &[u8]) -> &OsStr {
    OsStr::from_bytes(bytes)
}

pub(crate) fn os_string_from_bytes(bytes: &[u8]) -> OsString {
    OsStr::from_bytes(bytes).to_os_string()
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

pub(super) fn stat(path: &OsPath) -> Result<StatNode, IoError> {
    // Git tracks symlinks as symlinks, not as the file they point to.
    //  fs::symlink_metadata() does not follow symlinks. We get metadata about the symlink itself
    //  fs::metadata() follows symlinks and reports metadata about the target
    let meta = fs::symlink_metadata(path).with_context("lstat", Some(path))?;
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
pub(crate) fn probe_filemode(path: &OsPath) -> Result<bool, IoError> {
    const OWNER_EXECUTE: u32 = 0o100;
    let before = fs::symlink_metadata(path).with_context("lstat", Some(path))?;
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
    fs::set_permissions(path, original_permissions).with_context("chmod", Some(path))?;
    let after = match after {
        Ok(metadata) => metadata,
        Err(_) => return Ok(false),
    };

    // XORing the original mode with the one after our attempt to set the exec-bit will always result
    // to 0 if the change was not accepted since both numbers will be identical, otherwise 1. The next
    // check is to isolate the exec-bit(XOR operates on the entire integer)
    Ok((original_mode ^ after.permissions().mode()) & OWNER_EXECUTE != 0)
}

// to determine if the fs supports symlinks we create a `test` path(does not need to exist, dangling
// symlinks are fine) and we check what the filesystem reports
pub(crate) fn probe_symlink(link: &OsPath) -> Result<bool, IoError> {
    let test = OsPath::new_unchecked("test_symlink");
    match unix_fs::symlink(&test, link) {
        Ok(_) => fs::symlink_metadata(link)
            .map(|metadata| metadata.file_type().is_symlink())
            .with_context("lstat", Some(link)),
        Err(_) => Ok(false),
    }
}

// checks the owner's exec-bit, not whether the current process can actually execute the file.
pub(crate) fn is_executable(path: &OsPath) -> Result<bool, IoError> {
    let metadata = fs::symlink_metadata(path).with_context("lstat", Some(path))?;
    Ok(matches!(file_kind(&metadata), FileKind::Regular(true)))
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
            Self::Empty => write!(f, "path is empty"),
            Self::ContainsNul(path) => {
                write!(f, "path {} contains NUL byte", path.display())
            }
            Self::StripPrefix(err) => write!(f, "{err}"),
        }
    }
}

// this avoids the call to map_err() at every operation that can fail with an io::Error
pub(crate) trait IoErrorContext<T> {
    fn with_context<P>(self, op: &'static str, path: Option<P>) -> Result<T, IoError>
    where
        P: AsRef<Path>;
}

impl<T> IoErrorContext<T> for io::Result<T> {
    fn with_context<P>(self, op: &'static str, path: Option<P>) -> Result<T, IoError>
    where
        P: AsRef<Path>,
    {
        self.map_err(|source| IoError {
            op,
            path: path.map(|path| OsPath::new_unchecked(path.as_ref())),
            source,
        })
    }
}

// wanted to provide more context on io errors
// op is operation we tried to do
// path is Option<OsPath> because we cannot report a path when failing to get the cwd
#[derive(Debug)]
pub(crate) struct IoError {
    // TODO: should ops be an Enum so we can be consistent and void typing errors?
    // fs::metadata() -> stat
    // file.metadata() -> fstat
    // fs::symlink_metadata() -> lstat
    op: &'static str,
    path: Option<OsPath>,
    source: io::Error,
}

// TODO: should we have new and new_unchecked?
impl IoError {
    pub(crate) fn new<P>(op: &'static str, path: Option<P>, source: io::Error) -> Self
    where
        P: AsRef<Path>,
    {
        Self {
            op,
            path: path.map(|path| OsPath::new_unchecked(path.as_ref())),
            source,
        }
    }

    pub(crate) fn is_not_found(&self) -> bool {
        matches!(self.source.kind(), io::ErrorKind::NotFound)
    }
}

impl Error for IoError {}

impl fmt::Display for IoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: ", self.op)
    }
}
