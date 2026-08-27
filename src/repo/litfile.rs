use crate::repo::os;
use crate::repo::os::OsError;
use rand::RngExt;
use rand::distr::Alphanumeric;
use std::error::Error;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::{fmt, fs, io};

const PREFIX: &'static str = "litdir: ";
const FILE_MAX_SIZE: u64 = 64 * 1024;

// verifies that the file is a pointer file, not that it points to a valid lit repo
pub(crate) fn read(file: &mut File) -> Result<PathBuf, LitFileError> {
    let mut buf = Vec::new();
    file.take(FILE_MAX_SIZE + 1)
        .read_to_end(&mut buf)
        .map_err(LitFileError::Io)?;

    if buf.len() > FILE_MAX_SIZE as usize {
        return Err(LitFileError::TooLarge);
    }

    let bytes = buf
        .strip_prefix(PREFIX.as_bytes())
        .ok_or(LitFileError::MissingPrefix)?;
    // we need to check for CRLF first because LF could leave behind a dangling \r
    // CR is not supported
    let bytes = bytes
        .strip_suffix(b"\r\n")
        .or_else(|| bytes.strip_suffix(b"\n"))
        .ok_or(LitFileError::MissingLineEnding)?;
    if bytes.is_empty() {
        return Err(LitFileError::Empty);
    }

    os::bytes_to_path(bytes).map_err(LitFileError::BadPath)
}

// the metadata path is always absolute by construction in Layout::resolve()
pub(crate) fn write(path: &Path, metadata: &Path) -> Result<(), LitFileError> {
    // unwrap is always sound because path is constructed in Layout::resolve() in the Separate branch
    // which always does root.join(.lit) so even for C:\ or / we get C:\.lit, /.lit which guarantees
    // no None. The None needs the path to be root and nothing after it
    let tmp_path = path.parent().unwrap().join(gen_tmp_name());
    let path_bytes = os::os_str_as_bytes(metadata.as_ref()).map_err(LitFileError::BadPath)?;
    let mut content = Vec::with_capacity(PREFIX.len() + path_bytes.len() + 1);
    content.extend_from_slice(PREFIX.as_bytes());
    content.extend_from_slice(&path_bytes);
    content.push(b'\n');

    // we have to make the write appear atomic, no torn reads no competing writes, to do so we use
    // the same logic as in db.rs tmp file + rename
    // The naive approach of File::crate() has one more problem. create() follows symlinks so if
    // path is a symlink and its target does not exist it will create it and write the content there
    // not replace .lit dir entry. It gets even worse if target exists because create() truncates the
    // file which means that we will an existing file.
    // TODO: this should be a method
    fs::write(&tmp_path, content).map_err(LitFileError::Io)?;
    // we return the rename error even if remove_file() fails because that was the main reason
    if let Err(err) = fs::rename(&tmp_path, &path) {
        // Try to clean up the temp file before returning the error
        let _ = fs::remove_file(&tmp_path);
        return Err(LitFileError::Io(err));
    }

    Ok(())
}

fn gen_tmp_name() -> String {
    let suffix: String = (0..6)
        .map(|_| rand::rng().sample(Alphanumeric) as char)
        .collect();
    format!("tmp_litfile_{suffix}")
}

#[derive(Debug)]
pub(crate) enum LitFileError {
    Io(io::Error),
    BadPath(OsError),
    TooLarge,
    MissingPrefix,
    Empty,
    MissingLineEnding,
}

impl Error for LitFileError {}

impl fmt::Display for LitFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LitFileError::Io(err) => write!(f, "{}", err),
            LitFileError::BadPath(err) => write!(f, "bad path {}", err),
            LitFileError::TooLarge => write!(f, "litfile exceeds maximum size: {}", FILE_MAX_SIZE),
            LitFileError::MissingPrefix => {
                write!(f, "invalid lit file format: missing {} prefix", PREFIX)
            }
            LitFileError::Empty => write!(f, "invalid litfile format: empty path"),
            LitFileError::MissingLineEnding => {
                write!(f, "invalid litfile format: missing line ending")
            }
        }
    }
}
