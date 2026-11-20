use crate::command::print::ReadableBytes;
use crate::repo::os::{OsPath, OsPathError};
use crate::repo::repopath::RepoPath;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Component, Path};

// TODO: glob
// the set of paths certain commands should operate on
#[derive(Debug)]
pub(crate) struct Pathspec {
    pattern: RepoPath,
    original: OsString,
}

impl Pathspec {
    // `prefix`: the root relative prefix for cwd, empty if root = cwd
    //
    // Case 1: the user provided a relative to their cwd path. `prefix` is always `Some()`
    // Case 2: the user provided an absolute path. `prefix` is always `None`. The notion of `root
    // relative prefix` does not make sense for absolute paths.
    //
    // To get a relative to root we strip of the root from the absolute path after
    // normalization. We can strip of before normalization because strip_prefix() does a lexical
    // check and cases containing `.` or `..` will fail to match.
    //
    // root = "/home/thanos/repo", absolute = "/home/thanos/./../thanos/repo/docs/intro.md" strip_prefix()
    // fails to match in this case, after normalization though the result is docs/intro.md
    //
    // In either case, `pattern` is a normalized root relative path. Note that resolution is purely
    // lexical, we don't access the fs or follow symlinks.
    pub(crate) fn new(
        arg_path: &OsStr,
        prefix: Option<&Path>,
        root: &OsPath,
    ) -> Result<Self, PathspecError> {
        let input_path = OsPath::new(arg_path)?;
        let resolved = match prefix {
            Some(prefix) => OsPath::new_unchecked(prefix.join(&input_path)),
            None => input_path.clone(),
        };

        let normalized = resolved.normalize_lexically();
        // after normalization we need to produce the root relative path first and then walk its
        // components to make sure it's not outside of the repository or contains reserved names
        //  root = /repo
        //  cwd = /repo/src
        //  arg = ../../README.md
        //  root relative = src/../../README.md
        //  normalized = ../README.md -> falls out of root
        //
        // we can't reject paths with reserved components before normalization because the final path
        // might not contain any.
        //  cwd = /repo
        //  arg = .lit/../README.md
        //  normalized = README.md
        // if the path is relative it stay as is, else we attempt to strip the root which can result
        // in outside repo errors.
        let path = match prefix {
            Some(_) => normalized.as_ref(),
            None => match normalized.strip_prefix(root) {
                Ok(path) => path,
                Err(_) => {
                    return Err(PathspecError::OutsideRepository {
                        candidate: input_path,
                        root: root.clone(),
                    });
                }
            },
        };
        let mut pattern = RepoPath::empty();

        for component in path.components() {
            match component {
                // path can't be absolute at this point
                // the previous match guarantees that if it is relative, it stays as is otherwise
                // we attemp to strip root from it will always leaves either an empty or relative
                // path. In either case, the path can't be absolute. `Prefix` can't occur on Unix
                Component::Prefix(_) | Component::RootDir => {
                    unreachable!("resolved path still contains a root or platform prefix: {path:?}")
                }
                Component::CurDir => continue,
                Component::ParentDir => {
                    // after normalization if the path's first component is '..' it always escapes
                    // the repository
                    return Err(PathspecError::OutsideRepository {
                        candidate: input_path,
                        root: root.clone(),
                    });
                }
                Component::Normal(name) => {
                    // TODO: case insensitive check?
                    if name == ".lit" {
                        return Err(PathspecError::ReservedComponent(input_path));
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

    pub(crate) fn pattern(&self) -> &RepoPath {
        &self.pattern
    }

    pub(crate) fn original(&self) -> &OsStr {
        &self.original
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PathspecError {
    // `candidate` is the user provided verbatim
    OutsideRepository { candidate: OsPath, root: OsPath },
    ReservedComponent(OsPath),
    OsPath(OsPathError),
}

impl Error for PathspecError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::OsPath(source) => Some(source),
            Self::OutsideRepository { .. } => None,
            Self::ReservedComponent(_) => None,
        }
    }
}

impl fmt::Display for PathspecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OsPath(_) => write!(f, "bad path"),
            Self::OutsideRepository { candidate, root } => {
                write!(
                    f,
                    "'{}' is outside repository at '{}'",
                    ReadableBytes(candidate.as_bytes()),
                    ReadableBytes(root.as_bytes())
                )
            }
            Self::ReservedComponent(path) => {
                write!(f, "'{}' contains reserved component '.lit'", path.display(),)
            }
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
    use super::*;

    fn root() -> OsPath {
        OsPath::new_unchecked("/repo")
    }

    #[test]
    fn valid_relative_paths() {
        let root = root();
        let cases = [
            ("main.rs", "src", "src/main.rs"),
            ("./main.rs", "src", "src/main.rs"),
            ("..", "src", ""),
            // contains `.lit` but is skipped
            ("src/.lit/../main.rs", "", "src/main.rs"),
        ];

        for (arg, prefix, pattern) in cases {
            let pathspec = Pathspec::new(OsStr::new(arg), Some(Path::new(prefix)), &root)
                .unwrap_or_else(|error| {
                    panic!("arg = {arg:?}, prefix = {prefix:?}: error = {error:?}")
                });
            assert_eq!(pattern.as_bytes(), pathspec.pattern().as_bytes());
        }
    }

    #[test]
    fn valid_absolute_path() {
        let root = root();
        let pattern = "README.md";
        let arg = "/repo/README.md";
        let pathspec = Pathspec::new(OsStr::new(arg), None, &root)
            .unwrap_or_else(|error| panic!("arg = {arg:?}: error = {error:?}"));
        assert_eq!(pattern.as_bytes(), pathspec.pattern().as_bytes());
    }

    #[test]
    fn reject_relative_paths_that_escape_repo() {
        let root = root();
        let cases = [
            ("..", ""),
            ("../../outside.txt", "src"),
            ("src/../../outside.txt", ""),
        ];

        for (arg, prefix) in cases {
            let error = Pathspec::new(OsStr::new(arg), Some(Path::new(prefix)), &root).unwrap_err();
            assert_eq!(
                error,
                PathspecError::OutsideRepository {
                    candidate: OsPath::new_unchecked(arg),
                    root: root.clone()
                }
            );
        }
    }

    #[test]
    fn reject_absolute_paths_that_escape_repo() {
        let root = root();
        let cases = ["/home", "/", "/root/../outside.txt"];

        for arg in cases {
            let error = Pathspec::new(OsStr::new(arg), None, &root).unwrap_err();
            assert_eq!(
                error,
                PathspecError::OutsideRepository {
                    candidate: OsPath::new_unchecked(arg),
                    root: root.clone()
                }
            );
        }
    }

    #[test]
    fn reject_reserved_components_in_paths() {
        let root = root();
        let arg = "/repo/.lit/src";

        let error = Pathspec::new(OsStr::new(arg), None, &root).unwrap_err();
        assert_eq!(
            error,
            PathspecError::ReservedComponent(OsPath::new_unchecked(arg))
        );
    }
}
