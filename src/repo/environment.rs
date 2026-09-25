use crate::repo::os::{IoError, IoErrorContext, OsPath, OsPathError};
use std::ffi::{OsStr, OsString};
use std::env;

pub(crate) const LIT_DIR: &str = "LIT_DIR";
pub(crate) const LIT_WORK_TREE: &str = "LIT_WORK_TREE";
pub(crate) const LIT_OBJECT_DIRECTORY: &str = "LIT_OBJECT_DIRECTORY";
pub(crate) const LIT_CEILING_DIRECTORIES: &str = "LIT_CEILING_DIRECTORIES";
pub(crate) const LIT_DEFAULT_HASH: &str = "LIT_DEFAULT_HASH";
pub(crate) const LIT_REFERENCE_BACKEND: &str = "LIT_REFERENCE_BACKEND";
pub(crate) const LIT_DEFAULT_REF_FORMAT: &str = "LIT_DEFAULT_REF_FORMAT";

// pub(super) fn metadata_dir() -> Result<Option<OsPath>> {
//     read_path(LIT_DIR)
// }

pub(crate) fn var<K: AsRef<OsStr>>(key: K) -> Option<OsString> {
    env::var_os(key)
}

pub(crate) fn cwd() -> Result<OsPath, IoError> {
    let path = env::current_dir().with_context::<&OsPath>("getcwd", None)?;
    Ok(OsPath::new_unchecked(path))
}

// fn read_path(path: &str) -> Result<Option<OsPath>> {
//     env::var_os(path)
//         .map(OsPath::new)
//         .transpose()
//         .map_err(|err| EnvironmentError {
//             name: OsString::from(path),
//             kind: EnvironmentErrorKind::BadPath(err),
//         })
// }
