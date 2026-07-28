use crate::command::error::PathspecError;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

// toDo: add magic support
// the set of paths certain commands should operate on
#[derive(Debug)]
pub(super) struct Pathspec {
    // normalized repo relative path
    pub(super) pattern: PathBuf,
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
    pub(super) fn new(arg: &OsStr, prefix: &Path, root: &Path) -> Result<Self, PathspecError> { 
        let pattern = if Path::new(arg).is_absolute() {
            let absolute = normalize_absolute(arg.as_ref())?;
            absolute.strip_prefix(root)
                .map(Path::to_path_buf)
                .map_err(|_| PathspecError::OutsideRepository {
                    path: PathBuf::from(arg),
                })?
        } else {
            normalize_relative(arg.as_ref(), prefix)?
        };

        Ok(Self { pattern })
    }
}

// we do lexical normalization we never interact with fs, we never call canonicalize()
//
// For absolute paths, failing to pop is not an error. It just means we are already at the filesystem
// root.
// /../.. normalizes to /
// C:\..\.. normalizes to C:\
fn normalize_absolute(absolute: &Path) -> Result<PathBuf, PathspecError> {
    let mut path = PathBuf::new();

    for component in absolute.components() {
        match component {
            // A Windows path prefix, e.g., C: C:\, or \\server\share.
            // large variety of prefix types, check docs
            // does not occur on Unix.
            //
            // for absolute keep it as is
            Component::Prefix(prefix) => path.push(prefix.as_os_str()),
            // Unix: "/"
            // Windows: the "\" after a prefix like "C:\"
            Component::RootDir => path.push(component.as_os_str()),
            // we don't push or pop any component we stay where we are which is the point of `.`
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = path.pop();
            }
            Component::Normal(name) => {
                if name == ".lit" {
                    return Err(PathspecError::ReservedComponent {
                        path: absolute.to_path_buf(),
                        component: name.to_os_string(),
                    });
                }
                path.push(name);
            }
        }
    }
    Ok(path)
}

// we do lexical normalization we never interact with fs, we never call canonicalize()
fn normalize_relative(relative: &Path, prefix: &Path) -> Result<PathBuf, PathspecError> {
    let mut path = prefix.to_path_buf();

    for component in Path::new(relative).components() {
        match component {
            // can't happen since there is check if the path is absolute that triggers normalize_absolute()
            // this method gets called only if the above check failed
            // RootDir and Prefix can only appear in an absolute path
            Component::RootDir | Component::Prefix(_) => {
                unreachable!("normalize_relative() was called with absolute path")
            }
            Component::CurDir => {}
            // we don't push or pop any component we stay where we are which is the point of `.`
            Component::ParentDir => {
                // Case: cwd = repo root, prefix = "" and the path is ../
                // this falls outside the repository
                if !path.pop() {
                    return Err(PathspecError::OutsideRepository {
                        path: PathBuf::from(relative),
                    });
                }
            }
            Component::Normal(name) => {
                if name == ".lit" {
                    return Err(PathspecError::ReservedComponent {
                        path: relative.to_path_buf(),
                        component: name.to_os_string(),
                    });
                }
                path.push(name);
            }
        }
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use super::*;

    // instead of carrying (OsString, PathBuf, Pathspec)
    #[derive(Debug)]
    struct GoodPath {
        arg: OsString,
        prefix: PathBuf,
        pattern: PathBuf,
    }

    impl GoodPath {
        fn new(arg: &OsStr, prefix: &OsStr, pattern: &OsStr) -> Self {
            GoodPath {
                arg: arg.to_os_string(),
                prefix: PathBuf::from(prefix),
                pattern: PathBuf::from(pattern),
            }
        }
    }

    #[derive(Debug)]
    struct BadPath {
        arg: OsString,
        prefix: PathBuf,
        err: PathspecError,
    }

    impl BadPath {
        fn new(arg: &OsStr, prefix: &OsStr, err: PathspecError) -> Self {
            BadPath {
                arg: arg.to_os_string(),
                prefix: PathBuf::from(prefix),
                err,
            }
        }
    }

    #[cfg(unix)]
    fn root() -> PathBuf {
        PathBuf::from("/repo")
    }

    #[cfg(windows)]
    fn root() -> PathBuf {
        PathBuf::from(r"C:\repo")
    }

    fn good_paths() -> Vec<GoodPath> {
        vec![
            GoodPath::new("main.rs".as_ref(), "src".as_ref(), "src/main.rs".as_ref()),
            GoodPath::new("./main.rs".as_ref(), "src".as_ref(), "src/main.rs".as_ref()),
            GoodPath::new(
                "src/./main.rs".as_ref(),
                "".as_ref(),
                "src/main.rs".as_ref(),
            ),
            GoodPath::new(".".as_ref(), "src".as_ref(), "src".as_ref()),
            GoodPath::new(".".as_ref(), "".as_ref(), "".as_ref()),
            GoodPath::new(
                "../README.md".as_ref(),
                "src".as_ref(),
                "README.md".as_ref(),
            ),
            GoodPath::new("..".as_ref(), "src".as_ref(), "".as_ref()),
            GoodPath::new("src/../lib.rs".as_ref(), "".as_ref(), "lib.rs".as_ref()),
            GoodPath::new("../d".as_ref(), "a/b/c".as_ref(), "a/b/d".as_ref()),
            // trailing slash dropped by Path::components
            GoodPath::new("src/".as_ref(), "".as_ref(), "src".as_ref()),
            // absolute paths: prefix is ignored, path is stripped against repo root
            GoodPath::new(
                root().join("README.md").as_ref(),
                "src".as_ref(),
                "README.md".as_ref(),
            ),
            GoodPath::new(
                root().join("src").join("main.rs").as_ref(),
                "".as_ref(),
                "src/main.rs".as_ref(),
            ),
            GoodPath::new(
                root().join("src").join("..").join("README.md").as_ref(),
                "ignore".as_ref(),
                "README.md".as_ref(),
            ),
            GoodPath::new(root().as_ref(), "src".as_ref(), "".as_ref()),
        ]
    }

    fn bad_paths() -> Vec<BadPath> {
        // relative path tries to escape above repo root
        vec![
            BadPath::new(
                "..".as_ref(),
                "".as_ref(),
                PathspecError::OutsideRepository {
                    path: PathBuf::from(".."),
                },
            ),
            // .lit access after normalization
            BadPath::new(
                ".lit".as_ref(),
                "".as_ref(),
                PathspecError::ReservedComponent {
                    path: PathBuf::from(".lit"),
                    component: OsString::from(".lit"),
                },
            ),
            // absolute path outside repo
            BadPath::new(
                "/outside/file.txt".as_ref(),
                "".as_ref(),
                PathspecError::OutsideRepository {
                    path: PathBuf::from("/outside/file.txt"),
                },
            ),
            // absolute path that normalizes to filesystem root, then fails strip_prefix
            BadPath::new(
                "/../..".as_ref(),
                "src".as_ref(),
                PathspecError::OutsideRepository {
                    path: PathBuf::from("/../.."),
                },
            ),
            BadPath::new(
                root().join(".lit").join("HEAD").as_ref(),
                "src".as_ref(),
                PathspecError::ReservedComponent {
                    path: root().join(".lit").join("HEAD"),
                    component: OsString::from(".lit"),
                },
            ),
        ]
    }

    #[test]
    fn valid_paths() {
        for gc in good_paths() {
            let pathspec = Pathspec::new(gc.arg.as_os_str(), &gc.prefix, &root())
                .unwrap_or_else(|err| panic!("case failed: {gc:?}, error: {err:?}"));
            // same syntax as  "{:?},gc" use Debug formatting for gc
            assert_eq!(pathspec.pattern, gc.pattern, "{gc:?}");
        }
    }

    #[test]
    fn invalid_paths() {
        for gc in bad_paths() {
            let err = Pathspec::new(gc.arg.as_os_str(), &gc.prefix, &root()).unwrap_err();
            assert_eq!(err, gc.err);
        }
    }
}
