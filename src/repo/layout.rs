use crate::repo::os::{IoError, OsPath};
use crate::repo::{LayoutError, environment};
use std::path::Path;
use std::{fs, io};

// describes how the resolved worktree is connected to the metadata directory
#[derive(Debug, PartialEq, Eq)]
enum MetadataPlacement {
    // metadata dir is used directly, typically bare repos where there is no worktree
    Direct,
    // Ordinary `<worktree>/.lit` dir
    Embedded,
    // `<worktree>/.lit` is a pointer file to metadata
    Separate { pointer_file: OsPath },
}

impl MetadataPlacement {
    // we can't naively compare paths without resolution first
    //
    // 1. `worktree_dir` has the `core.worktree` value resolved based on metadata dir
    //      metadata_dir = /projects/app/.lit
    //      core.worktree = ..
    //      worktree = metadata_dir.join(worktree_dir) -> /projects/app/.lit/..
    //
    //      let entry = dir.join_unchecked(".lit"); which creates /projects/app/.lit/../.lit
    //      the comparison then fails, and we end up with `Direct` which
    //      is incorrect because /projects/app/.lit/../.lit normalizes to /projects/app/.lit so
    //      the result should be `Embedded`. core.worktree is /projects/app
    // 2. `pointer_file` is symlink `projects/app/.lit` pointing to `/storage/app-metadata`
    //      worktree is `core.worktree` = `projects/app/src/..`
    //      entry = projects/app/src/../.lit
    //      the comparison fails, and we get `Direct` instead of `Separate`. The actual worktree
    //      path is `projects/app` and `/projects/app/.lit is a pointer file
    // 3. worktree is reach via symlink
    //      `/home/user/app` is a symlink to `/projects/app
    //      `LIT_DIR` = /projects/app/.lit
    //      `LIT_WORK_TREE` = /home/user/app
    //
    //      we compare /projects/app/.lit to /home/user/app/.lit again getting `Direct` instead of
    //      `Embedded`
    //
    // there 2 guarantees for the args:
    //  1. metadata_dir is an absolute canonical path
    //  2. worktree_dir is an absolute path, it can still contain `..`, traverse symlinks or name
    //  non-existing location
    fn from_discovery(
        metadata_dir: &OsPath,
        worktree_dir: Option<&OsPath>,
        pointer_file: Option<OsPath>,
    ) -> Result<Self, IoError> {
        // bare repos, no relationship to classify
        let Some(worktree_dir) = worktree_dir else {
            return Ok(Self::Direct);
        };

        let entry = worktree_dir.join_unchecked(".lit");
        let metadata = match fs::metadata(&entry) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok(Self::Direct);
            }
            Err(err) => return Err(IoError::new("stat", Some(entry), err)),
        };

        if metadata.is_dir() {
            // if entry exists and is a dir compare with the metadata dir
            if metadata_dir.same_canonical_path_with(&entry)? {
                return Ok(Self::Embedded);
            }
        } else if metadata.is_file() {
            // this is case 2 mentioned in the comment above
            if let Some(pointer_file) = pointer_file
                && pointer_file.same_canonical_path_with(&entry)?
            {
                return Ok(Self::Separate { pointer_file });
            }
        }
        Ok(Self::Direct)
    }
}

#[derive(Debug)]
pub(crate) struct Layout {
    // directory containing the repository metadata (HEAD, config, objects, ...)
    metadata_dir: OsPath,
    // root of the working tree for non-bare
    // None for bare
    worktree_dir: Option<OsPath>,
    // describes how the metadata is connected to the worktree
    placement: MetadataPlacement,
}

impl Layout {
    // There are 4 factors that determine the location of metadata dir when initializing a repo.
    // --bare, --separate_lit_dir, path and the LIT_DIR/LIT_WORK_TREE env vars.
    //
    //  Resolves the metadata and worktree locations without touching the fs
    //
    // Git's init impl will try to "guess" whether a repo should be bare from the value of GIT_DIR
    // https://github.com/git/git/blob/18e66859d87fb4b76599f73460b54f0848c76b16/builtin/init-db.c#L17-L48
    //  We avoid this behavior and set the following rules:
    //  - A repository is bare only when --bare is provided.
    //  - LIT_DIR has the same meaning as GIT_DIR. It names the metadata directory itself, not the
    //  directory in which an embedded `.lit` directory should be created, this is what the path
    //  positional arg refers to.
    //  - A positional path names the repository location and has precedence over LIT_DIR:
    //      - non-bare: <path> is the worktree and metadata is stored in <path>/.lit
    //      - bare: <path> is the metadata directory and there is no worktree
    //  - If there is no positional path or --separate-lit-dir, LIT_DIR selects the metadata directory:
    //      - non-bare: LIT_WORK_TREE selects the worktree, falling back to cwd when unset
    //      - bare: no worktree, so LIT_WORK_TREE is ignored
    //  - LIT_WORK_TREE without LIT_DIR is ignored
    //  - With no explicit location, non-bare init uses cwd/.lit and bare init uses cwd directly
    // These are the rules on how we resolve paths: https://kernel.googlesource.com/pub/scm/git/git.git/%2B/d8a267404cb2a9376bed0ede45c759a0b30590d7/t/t1510-repo-setup.sh
    /// Determines the repository layout
    pub(crate) fn resolve(
        path: Option<&Path>,
        bare: bool,
        separate_lit_dir: Option<&Path>,
    ) -> Result<Self, LayoutError> {
        // highest precedence, reject early
        // if we call map(OsPath::new) we get Option<Result<T, E>> but what we want is Result<Option<T>, E>
        // that is what transpose does
        let path = path.map(OsPath::new).transpose()?;
        let cwd = environment::cwd()?;
        // root of the worktree
        let root = path
            .as_ref()
            .map_or(cwd.clone(), |path| cwd.join_unchecked(path));

        if let Some(metadata_dir) = separate_lit_dir {
            // TODO: should this be a notification to the user that LIT_DIR is actually ignored
            //  because the flag has higher precedence. This is a conflict because both try to
            //  name the metadata dir
            let pointer_file = root.join_unchecked(".lit");
            return Ok(Self {
                metadata_dir: cwd.join(metadata_dir)?,
                // the parent identifies the worktree even though the metadata is elsewhere
                worktree_dir: Some(root),
                placement: MetadataPlacement::Separate { pointer_file },
            });
        }

        // explicit positional path wins over LIT_DIR
        if path.is_some() {
            return if bare {
                Ok(Self {
                    metadata_dir: root,
                    worktree_dir: None,
                    placement: MetadataPlacement::Direct,
                })
            } else {
                // lit init <path>
                Ok(Self {
                    metadata_dir: root.join_unchecked(".lit"), // <worktree>.lit
                    worktree_dir: Some(root),
                    placement: MetadataPlacement::Embedded,
                })
            };
        }

        // LIT_DIR names the metadata directory itself containing HEAD, config, objects/. We don't
        // append `.lit` to it. This is why placement is MetadataPlacement::Direct
        // LIT_WORK_TREE names the working tree root, the directory containing the files we want to
        // work on
        //
        // They can be completely separate:
        //  - LIT_DIR: /srv/lit-metadata/project
        //  - LIT_WORK_TREE: /home/thanos/projects/project
        if let Some(env_dir) = environment::var(environment::LIT_DIR) {
            let env_dir = cwd.join(env_dir)?;
            // LIT_WORK_TREE makes sense only in conjunction with LIT_DIR without --bare. In any
            // other case it is ignored.
            let worktree = environment::var(environment::LIT_WORK_TREE);
            let worktree = worktree.map(OsPath::new).transpose()?;
            // bare repos have no worktree
            if bare && worktree.is_some() {
                return Err(LayoutError::LitWorkTreeWithBare);
            }

            return if bare {
                Ok(Self {
                    metadata_dir: env_dir,
                    worktree_dir: None,
                    placement: MetadataPlacement::Direct,
                })
            } else {
                // if no WORK_TREE found we fall back to cwd
                // https://kernel.googlesource.com/pub/scm/git/git.git/%2B/d8a267404cb2a9376bed0ede45c759a0b30590d7/t/t1510-repo-setup.sh
                // https://git-scm.com/docs/git#Documentation/git.txt---work-treeltpathgt
                // LIT_WORK_TREE is resolved against cwd if relative
                let worktree =
                    worktree.map_or(cwd.clone(), |worktree| cwd.join_unchecked(worktree));
                Ok(Self {
                    metadata_dir: env_dir,
                    worktree_dir: Some(worktree),
                    placement: MetadataPlacement::Direct,
                })
            };
        }

        // Note: LIT_WORK_TREE is considered only when LIT_DIR is set. This branch is reached when
        // LIT_DIR is unset, so even if LIT_WORK_TREE is set, it is ignored. bare does not error,
        // non-bare uses cwd
        if bare {
            Ok(Self {
                metadata_dir: cwd,
                worktree_dir: None,
                placement: MetadataPlacement::Direct,
            })
        } else {
            Ok(Self {
                metadata_dir: cwd.join_unchecked(".lit"),
                worktree_dir: Some(cwd),
                placement: MetadataPlacement::Embedded,
            })
        }
    }

    // the worktree root for non-bare or the metadata directory for bare
    pub(crate) fn root(&self) -> &OsPath {
        self.worktree_dir.as_ref().unwrap_or(&self.metadata_dir)
    }

    pub(crate) fn metadata_dir(&self) -> &OsPath {
        &self.metadata_dir
    }

    pub(crate) fn worktree_dir(&self) -> Option<&OsPath> {
        self.worktree_dir.as_ref()
    }

    // link is always absolute by construction
    pub(crate) fn pointer_file(&self) -> Option<&OsPath> {
        if let MetadataPlacement::Separate { pointer_file } = &self.placement {
            return Some(pointer_file);
        }
        None
    }

    pub(crate) fn is_bare(&self) -> bool {
        self.worktree_dir.is_none()
    }

    // decide if we have to set core.worktree in config
    pub(crate) fn needs_worktree_config(&self) -> bool {
        let Some(worktree) = &self.worktree_dir else {
            return false;
        };

        // core.worktree is only set when LIT_DIR is used which is in the direct case
        //  LIT_DIR: /foo/metadata
        //  LIT_WORK_TREE: /bar
        // we can't use the rule that worktree is the parent of metadata, we have to check
        // if worktree.join(.lit) is our metadata dir
        //
        // It is important to set the core.worktree in this case because when we try to discover if
        // a repo is a lit repo via LIT_DIR we need a way to identify the worktree. First we check
        // for LIT_WORK_TREE, then for core.worktree and then we fall back to cwd.
        //
        // there is also the case where LIT_DIR is an absolute path, LIT_WORK_TREE is not set and
        // worktree ends up being the cwd, this needs to be resolved in the same way
        match self.placement {
            MetadataPlacement::Direct => self.metadata_dir != worktree.join_unchecked(".lit"),
            // <worktree>/.lit is metadata, parent is worktree
            MetadataPlacement::Embedded => false,
            // pointer file, its parent identifies the worktree even the metadata is elsewhere
            MetadataPlacement::Separate { .. } => false,
        }
    }

    pub(super) fn from_discovery(
        metadata_dir: OsPath,
        worktree_dir: Option<OsPath>,
        pointer_file: Option<OsPath>,
    ) -> Result<Self, IoError> {
        let placement =
            MetadataPlacement::from_discovery(&metadata_dir, worktree_dir.as_ref(), pointer_file)?;

        Ok(Self {
            metadata_dir,
            worktree_dir,
            placement,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sealed_test::prelude::*;
    use std::env;

    // `separate_lit_dir` is set, worktree is cwd, pointer_file points to `cwd/.lit`
    #[test]
    fn l01() {
        let path = OsPath::new_unchecked("/foo/bar");
        let layout = Layout::resolve(None, true, Some(path.inner())).unwrap();
        let cwd = environment::cwd().unwrap();

        assert_eq!(layout.metadata_dir(), &path);
        assert_eq!(layout.worktree_dir(), Some(&cwd));
        assert_eq!(
            layout.placement,
            MetadataPlacement::Separate {
                pointer_file: cwd.join_unchecked(".lit")
            }
        );
    }

    // `separate_lit_dir` is set, positional path is set, pointer_file points to `<positional_path>/.lit`
    #[test]
    fn l02() {
        let path = OsPath::new_unchecked("/projects/lit");
        let separate_lit_dir = OsPath::new_unchecked("/foo/bar");
        let layout =
            Layout::resolve(Some(path.inner()), true, Some(separate_lit_dir.inner())).unwrap();

        assert_eq!(layout.metadata_dir(), &separate_lit_dir);
        assert_eq!(layout.worktree_dir(), Some(&path));
        assert_eq!(
            layout.placement,
            MetadataPlacement::Separate {
                pointer_file: path.join_unchecked(".lit")
            }
        );
    }

    // `LIT_DIR` is set, but ignored since `separate_lit_dir` has higher precedence
    #[sealed_test]
    fn l03() {
        unsafe {
            env::set_var("LIT_DIR", "/home/user/storage");
        }
        let path = OsPath::new_unchecked("/foo/bar");
        let layout = Layout::resolve(None, true, Some(path.inner())).unwrap();
        let cwd = environment::cwd().unwrap();

        assert_eq!(layout.metadata_dir(), &path);
        assert_eq!(layout.worktree_dir(), Some(&cwd));
        assert_eq!(
            layout.placement,
            MetadataPlacement::Separate {
                pointer_file: cwd.join_unchecked(".lit")
            }
        );
    }

    // only positional path is set
    #[test]
    fn l04() {
        let path = OsPath::new_unchecked("/foo");
        let layout = Layout::resolve(Some(path.inner()), false, None).unwrap();

        assert_eq!(layout.metadata_dir(), &(path.join_unchecked(".lit")));
        assert_eq!(layout.worktree_dir(), Some(&OsPath::new_unchecked("/foo")));
        assert_eq!(layout.placement, MetadataPlacement::Embedded);
    }

    // positional with bare
    #[test]
    fn l05() {
        let path = OsPath::new_unchecked("/foo");
        let layout = Layout::resolve(Some(path.inner()), true, None).unwrap();

        assert_eq!(layout.metadata_dir(), &path);
        assert_eq!(layout.worktree_dir(), None);
        assert_eq!(layout.placement, MetadataPlacement::Direct);
    }

    // `LIT_DIR` and `LIT_WORK_TREE` are both set, but ignored since positional path has higher
    // precedence
    #[sealed_test]
    fn l06() {
        unsafe {
            env::set_var("LIT_DIR", "/home/user/storage");
            env::set_var("LIT_WORK_TREE", "/projects/jolt");
        }
        let path = OsPath::new_unchecked("/foo/bar");
        let layout = Layout::resolve(Some(path.inner()), true, None).unwrap();

        assert_eq!(layout.metadata_dir(), &path);
        assert_eq!(layout.worktree_dir(), None);
        assert_eq!(layout.placement, MetadataPlacement::Direct);
    }

    // `LIT_DIR` with bare
    #[sealed_test]
    fn l07() {
        unsafe {
            env::set_var("LIT_DIR", "/foo/bar/");
        }
        let layout = Layout::resolve(None, true, None).unwrap();
        let lit_dir = environment::var("LIT_DIR").unwrap();

        assert_eq!(layout.metadata_dir(), &OsPath::new_unchecked(lit_dir));
        assert_eq!(layout.worktree_dir(), None);
        assert_eq!(layout.placement, MetadataPlacement::Direct);
    }

    // worktree is cwd
    #[sealed_test]
    fn l08() {
        unsafe {
            env::set_var("LIT_DIR", "/foo/bar/..");
        }
        let layout = Layout::resolve(None, false, None).unwrap();
        let cwd = environment::cwd().unwrap();
        let lit_dir = environment::var("LIT_DIR").unwrap();

        assert_eq!(layout.metadata_dir(), &OsPath::new_unchecked(lit_dir));
        assert_eq!(layout.worktree_dir(), Some(&cwd));
        assert_eq!(layout.placement, MetadataPlacement::Direct);
    }

    // `LIT_WORK_TREE` is relative, resolve it against cwd
    #[sealed_test]
    fn l09() {
        unsafe {
            env::set_var("LIT_DIR", "/home/user/metadata");
            env::set_var("LIT_WORK_TREE", "projects/lit");
        }
        let layout = Layout::resolve(None, false, None).unwrap();
        let cwd = environment::cwd().unwrap();
        let lit_dir = environment::var("LIT_DIR").unwrap();
        let lit_work_tree = environment::var("LIT_WORK_TREE").unwrap();

        assert_eq!(layout.metadata_dir(), &OsPath::new_unchecked(lit_dir));
        assert_eq!(
            layout.worktree_dir(),
            Some(&cwd.join_unchecked(lit_work_tree))
        );
        assert_eq!(layout.placement, MetadataPlacement::Direct);
    }

    // env exclusive
    #[sealed_test]
    fn l10() {
        unsafe {
            env::set_var("LIT_DIR", "/home/user/storage");
            env::set_var("LIT_WORK_TREE", "/projects/jolt");
        }
        let layout = Layout::resolve(None, false, None).unwrap();
        let lit_dir = environment::var("LIT_DIR").unwrap();
        let lit_work_tree = environment::var("LIT_WORK_TREE").unwrap();

        assert_eq!(layout.metadata_dir(), &OsPath::new_unchecked(lit_dir));
        assert_eq!(
            layout.worktree_dir(),
            Some(&OsPath::new_unchecked(lit_work_tree))
        );
        assert_eq!(layout.placement, MetadataPlacement::Direct);
    }

    // bare
    #[test]
    fn l11() {
        let layout = Layout::resolve(None, true, None).unwrap();
        let cwd = environment::cwd().unwrap();

        assert_eq!(layout.metadata_dir(), &cwd);
        assert_eq!(layout.worktree_dir(), None);
        assert_eq!(layout.placement, MetadataPlacement::Direct);
    }

    // nothing is set
    #[test]
    fn l12() {
        let layout = Layout::resolve(None, false, None).unwrap();
        let cwd = environment::cwd().unwrap();

        assert_eq!(layout.metadata_dir(), &cwd.join_unchecked(".lit"));
        assert_eq!(layout.worktree_dir(), Some(&cwd));
        assert_eq!(layout.placement, MetadataPlacement::Embedded);
    }

    // if `LIT_DIR` is not set, `LIT_WORK_TREE` is set and bare is true, `LIT_WORK_TREE` is ignored
    #[sealed_test]
    fn l13() {
        unsafe {
            env::set_var("LIT_WORK_TREE", "/projects/jolt");
        }
        let layout = Layout::resolve(None, true, None).unwrap();
        let cwd = environment::cwd().unwrap();

        assert_eq!(layout.metadata_dir(), &cwd);
        assert_eq!(layout.worktree_dir(), None);
        assert_eq!(layout.placement, MetadataPlacement::Direct);

        // assert!(matches!(error, LayoutError::LitWorkTreeWithBare));
    }

    // if `LIT_DIR` and `LIT_WORK_TREE` are set and bare is true, conflict
    // unlike the test above where `LIT_WORK_TREE` is ignored since `LIT_DIR` is absent, now that is
    // present we consider it and it conflicts with bare.
    #[sealed_test]
    fn l14() {
        unsafe {
            env::set_var("LIT_DIR", "/home/user/storage");
            env::set_var("LIT_WORK_TREE", "/projects/jolt");
        }
        let error = Layout::resolve(None, true, None).unwrap_err();
        assert!(matches!(error, LayoutError::LitWorkTreeWithBare));
    }
}
