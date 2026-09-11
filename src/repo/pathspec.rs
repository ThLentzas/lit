use crate::repo::os::{OsPath, OsPathError};
use crate::repo::repo_path::RepoPath;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Component, Path};

// TODO: add magic support, what is known as glob
// the set of paths certain commands should operate on
#[derive(Debug)]
pub(crate) struct Pathspec {
    pub(crate) original: OsString,
    pub(crate) pattern: RepoPath,
}

impl Pathspec {
    // Case 1: the user provided a relative to their cwd path. ws.prefix() will return the prefix,
    // the relative path from root to cwd, and after we are done normalizing the path we have
    // the relative to root path.
    //
    // Case 2: the user provided an absolute path. In this case, the prefix is ignored because the
    // cwd is irrelevant. To get a relative to root we strip of the root from the absolute path after
    // normalization. We can strip of before normalization because strip_prefix() does a lexical
    // check and cases containing `.` or `..` will fail to match.
    //
    // root = "/home/thanos/repo", absolute = "/home/thanos/./../thanos/repo/docs/intro.md" strip_prefix()
    // fails to match in this case, after normalization though the result is docs/intro.md
    //
    // In either case, `pattern` is a normalized repo relative path. Note that even when new() returns
    // we don't know if the path actually exists or not we never touched the fs, we just express it
    // relative to root
    pub(crate) fn new(arg_path: &OsStr, prefix: &Path, root: &Path) -> Result<Self, PathspecError> {
        let cli_path = OsPath::new(arg_path)?;
        let resolved = if cli_path.is_absolute() {
            // have to clone here because we need to keep cli_path intact for reporting errors
            cli_path.clone()
        } else {
            // the join() creates the root relative path
            // prefix is the relative path from root to cwd and path is the relative path from cwd
            // to the resource the user wants to add
            OsPath::new_unchecked(prefix.join(&cli_path))
        };

        let normalized = resolved.normalize_lexically();
        let path = if normalized.is_absolute() {
            normalized.strip_prefix(root)?
        } else {
            normalized.as_path()
        };

        let mut pattern = RepoPath::new();
        for component in path.components() {
            match component {
                // path must be relative
                Component::Prefix(_) | Component::RootDir => {
                    return Err(PathspecError::OutsideRepository { path: cli_path });
                }
                Component::CurDir => continue,
                Component::ParentDir => {
                    // after normalization if path still contains '..' it means we never encountered
                    // left neighbors to pop them so the path actually lies outside the repo
                    // ../ means parent of root -> path lies outside repo
                    return Err(PathspecError::OutsideRepository { path: cli_path });
                }
                Component::Normal(name) => {
                    if name == ".lit" {
                        return Err(PathspecError::ReservedComponent {
                            path: cli_path,
                            component: name.to_os_string(),
                        });
                    }
                    pattern = pattern.join_unchecked(name);
                }
            }
        }

        Ok(Self {
            original: arg_path.to_os_string(),
            pattern,
        })
    }
}

// we could do a struct with path, kind but too few variants
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PathspecError {
    OutsideRepository { path: OsPath },
    ReservedComponent { path: OsPath, component: OsString },
    OsPath(OsPathError),
}

impl Error for PathspecError {}

impl fmt::Display for PathspecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PathspecError::OutsideRepository { path } => {
                write!(f, "path '{}' is outside the repository", path.display())
            }
            PathspecError::ReservedComponent { path, component } => {
                write!(
                    f,
                    "path '{}' contains reserved component '{}'",
                    path.display(),
                    component.to_string_lossy()
                )
            }
            PathspecError::OsPath(err) => write!(f, "{err}"),
        }
    }
}

impl From<OsPathError> for PathspecError {
    fn from(err: OsPathError) -> Self {
        Self::OsPath(err)
    }
}

#[cfg(test)]
mod tests {
    // TODO: uncomment this
    // use std::ffi::OsString;
    // use super::*;
    //
    // // instead of carrying (OsString, PathBuf, Pathspec)
    // #[derive(Debug)]
    // struct GoodPath {
    //     arg: OsString,
    //     prefix: PathBuf,
    //     pattern: PathBuf,
    // }
    //
    // impl GoodPath {
    //     fn new(arg: &OsStr, prefix: &OsStr, pattern: &OsStr) -> Self {
    //         GoodPath {
    //             arg: arg.to_os_string(),
    //             prefix: PathBuf::from(prefix),
    //             pattern: PathBuf::from(pattern),
    //         }
    //     }
    // }
    //
    // #[derive(Debug)]
    // struct BadPath {
    //     arg: OsString,
    //     prefix: PathBuf,
    //     err: PathspecError,
    // }
    //
    // impl BadPath {
    //     fn new(arg: &OsStr, prefix: &OsStr, err: PathspecError) -> Self {
    //         BadPath {
    //             arg: arg.to_os_string(),
    //             prefix: PathBuf::from(prefix),
    //             err,
    //         }
    //     }
    // }
    //
    // #[cfg(unix)]
    // fn root() -> PathBuf {
    //     PathBuf::from("/repo")
    // }
    //
    // #[cfg(windows)]
    // fn root() -> PathBuf {
    //     PathBuf::from(r"C:\repo")
    // }
    //
    // fn good_paths() -> Vec<GoodPath> {
    //     vec![
    //         GoodPath::new("main.rs".as_ref(), "src".as_ref(), "src/main.rs".as_ref()),
    //         GoodPath::new("./main.rs".as_ref(), "src".as_ref(), "src/main.rs".as_ref()),
    //         GoodPath::new(
    //             "src/./main.rs".as_ref(),
    //             "".as_ref(),
    //             "src/main.rs".as_ref(),
    //         ),
    //         GoodPath::new(".".as_ref(), "src".as_ref(), "src".as_ref()),
    //         GoodPath::new(".".as_ref(), "".as_ref(), "".as_ref()),
    //         GoodPath::new(
    //             "../README.md".as_ref(),
    //             "src".as_ref(),
    //             "README.md".as_ref(),
    //         ),
    //         GoodPath::new("..".as_ref(), "src".as_ref(), "".as_ref()),
    //         GoodPath::new("src/../lib.rs".as_ref(), "".as_ref(), "lib.rs".as_ref()),
    //         GoodPath::new("../d".as_ref(), "a/b/c".as_ref(), "a/b/d".as_ref()),
    //         // trailing slash dropped by Path::components
    //         GoodPath::new("src/".as_ref(), "".as_ref(), "src".as_ref()),
    //         // absolute paths: prefix is ignored, path is stripped against repo root
    //         GoodPath::new(
    //             root().join("README.md").as_ref(),
    //             "src".as_ref(),
    //             "README.md".as_ref(),
    //         ),
    //         GoodPath::new(
    //             root().join("src").join("main.rs").as_ref(),
    //             "".as_ref(),
    //             "src/main.rs".as_ref(),
    //         ),
    //         GoodPath::new(
    //             root().join("src").join("..").join("README.md").as_ref(),
    //             "ignore".as_ref(),
    //             "README.md".as_ref(),
    //         ),
    //         GoodPath::new(root().as_ref(), "src".as_ref(), "".as_ref()),
    //     ]
    // }
    //
    // fn bad_paths() -> Vec<BadPath> {
    //     // relative path tries to escape above repo root
    //     vec![
    //         BadPath::new(
    //             "..".as_ref(),
    //             "".as_ref(),
    //             PathspecError::OutsideRepository {
    //                 path: PathBuf::from(".."),
    //             },
    //         ),
    //         // .lit access after normalization
    //         BadPath::new(
    //             ".lit".as_ref(),
    //             "".as_ref(),
    //             PathspecError::ReservedComponent {
    //                 path: PathBuf::from(".lit"),
    //                 component: OsString::from(".lit"),
    //             },
    //         ),
    //         // absolute path outside repo
    //         BadPath::new(
    //             "/outside/file.txt".as_ref(),
    //             "".as_ref(),
    //             PathspecError::OutsideRepository {
    //                 path: PathBuf::from("/outside/file.txt"),
    //             },
    //         ),
    //         // absolute path that normalizes to filesystem root, then fails strip_prefix
    //         BadPath::new(
    //             "/../..".as_ref(),
    //             "src".as_ref(),
    //             PathspecError::OutsideRepository {
    //                 path: PathBuf::from("/../.."),
    //             },
    //         ),
    //         BadPath::new(
    //             root().join(".lit").join("HEAD").as_ref(),
    //             "src".as_ref(),
    //             PathspecError::ReservedComponent {
    //                 path: root().join(".lit").join("HEAD"),
    //                 component: OsString::from(".lit"),
    //             },
    //         ),
    //     ]
    // }
    //
    // #[test]
    // fn valid_paths() {
    //     for gc in good_paths() {
    //         let pathspec = Pathspec::new(gc.arg.as_os_str(), &gc.prefix, &root())
    //             .unwrap_or_else(|err| panic!("case failed: {gc:?}, error: {err:?}"));
    //         // same syntax as  "{:?},gc" use Debug formatting for gc
    //         assert_eq!(pathspec.pattern, gc.pattern, "{gc:?}");
    //     }
    // }
    //
    // #[test]
    // fn invalid_paths() {
    //     for gc in bad_paths() {
    //         let err = Pathspec::new(gc.arg.as_os_str(), &gc.prefix, &root()).unwrap_err();
    //         assert_eq!(err, gc.err);
    //     }
    // }
}
