use crate::repo::os::{self, IoError, IoErrorContext, OsPath, OsPathError};
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::Read;

const PREFIX: &'static str = "litdir: ";
const FILE_MAX_SIZE: u64 = 64 * 1024;

// verifies that the file is a pointer file, not that it points to a valid lit rep
// this is not a good api design because we have to pass the file_path just for error messaging
// File does not have a single permanent path, it can be renamed while open
pub(crate) fn read(file: &mut File, file_path: &OsPath) -> Result<OsPath, LitFileError> {
    let mut buf = Vec::new();
    file.take(FILE_MAX_SIZE + 1)
        .read_to_end(&mut buf)
        .with_context("read", Some(file_path))
        .map_err(|err| LitFileError {
            path: file_path.clone(),
            kind: LitFileErrorKind::Io(err),
        })?;

    if buf.len() > FILE_MAX_SIZE as usize {
        return Err(LitFileError {
            path: file_path.clone(),
            kind: LitFileErrorKind::TooLarge,
        });
    }

    let bytes = buf
        .strip_prefix(PREFIX.as_bytes())
        .ok_or_else(|| LitFileError {
            path: file_path.clone(),
            kind: LitFileErrorKind::MissingPrefix,
        })?;
    // we need to check for CRLF first because LF could leave behind a dangling \r
    // CR is not supported
    let bytes = bytes
        .strip_suffix(b"\r\n")
        .or_else(|| bytes.strip_suffix(b"\n"))
        .ok_or_else(|| LitFileError {
            path: file_path.clone(),
            kind: LitFileErrorKind::MissingLineEnding,
        })?;
    if bytes.is_empty() {
        return Err(LitFileError {
            path: file_path.clone(),
            kind: LitFileErrorKind::Empty,
        });
    }
    let path = os::os_str_from_bytes(bytes);

    OsPath::new(path).map_err(|err| LitFileError {
        path: file_path.clone(),
        kind: LitFileErrorKind::OsPath(err),
    })
}

pub(crate) fn write(pointer_file: &OsPath, path_bytes: &[u8]) -> Result<(), IoError> {
    // unwrap is always sound because path is constructed in Layout::resolve() in the Separate branch
    // which always does root.join(.lit) so even for C:\ or / we get C:\.lit, /.lit which guarantees
    // no None. The None needs the path to be root and nothing after it
    let parent = pointer_file.parent().unwrap();
    let mut content = Vec::with_capacity(PREFIX.len() + path_bytes.len() + 1);
    content.extend_from_slice(PREFIX.as_bytes());
    content.extend_from_slice(&path_bytes);
    content.push(b'\n');

    // The naive approach of File::crate() has one more problem. create() follows symlinks so if
    // path is a symlink and its target does not exist it will create it and write the content there
    // not replace .lit dir entry. It gets even worse if target exists because create() truncates the
    // file which means that we will an existing file.
    os::atomic_write(&parent, &content, pointer_file)
}

#[derive(Debug)]
pub(crate) struct LitFileError {
    path: OsPath,
    kind: LitFileErrorKind,
}

#[derive(Debug)]
pub(crate) enum LitFileErrorKind {
    Io(IoError),
    TooLarge,
    MissingPrefix,
    Empty,
    MissingLineEnding,
    OsPath(OsPathError),
}

impl Error for LitFileError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.kind {
            LitFileErrorKind::Io(source) => Some(source),
            LitFileErrorKind::OsPath(source) => Some(source),
            _ => None,
        }
    }
}

impl fmt::Display for LitFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.kind)
    }
}

impl fmt::Display for LitFileErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(_) => write!(f, "litfile file I/O failed",),
            Self::TooLarge => write!(f, "litfile exceeds maximum size: {}", FILE_MAX_SIZE),
            Self::MissingPrefix => {
                write!(f, "invalid lit file format: missing {} prefix", PREFIX)
            }
            Self::Empty => write!(f, "invalid litfile format: empty path"),
            Self::MissingLineEnding => {
                write!(f, "invalid litfile format: missing line ending")
            }
            Self::OsPath(_) => write!(f, "bad fs path"),
        }
    }
}
