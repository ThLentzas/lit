pub(super) mod config;
pub(super) mod db;
mod diagnostic;
pub(super) mod environment;
pub(super) mod format;
pub(super) mod index;
pub(super) mod litfile;
pub(super) mod lockfile;
pub(super) mod object;
pub(super) mod os;
pub(super) mod pathspec;
pub(super) mod refs;
pub(super) mod repo_path;
pub(super) mod report;
pub(super) mod timestamp;
pub(super) mod tree;
pub(super) mod workspace;

use crate::repo::config::{ConfigFile, ConfigFileError};
use crate::repo::db::Database;
use crate::repo::format::{ObjectFormat, RepositoryFormat, RepositoryFormatError};
use crate::repo::litfile::LitFileError;
use crate::repo::object::OidError;
use crate::repo::object::oid::Oid;
use crate::repo::os::{IoError, IoErrorContext, OsPath, OsPathError};
use crate::repo::workspace::Workspace;
use std::error::Error;
use std::fs::{self, File, FileType};
use std::path::Path;
use std::{fmt, io};
use crate::repo::index::Index;
use crate::repo::refs::Refs;

// describes how the resolved worktree is connected to the metadata directory
enum MetadataPlacement {
    // metadata dir is used directly, typically bare repos where there is no worktree
    Direct,
    // Ordinary `<worktree>/.lit` dir
    Embedded,
    // `<worktree>/.lit` is a pointer file to metadata
    Separate { pointer_file: OsPath },
}

impl MetadataPlacement {
    // TODO: we need to list every possible combination and then write a test for each. In our tests
    //  we need to make sure that any bare repo ends up with worktree_dir: None and any non-bare repo
    //  always ends up with Some(worktree_dir)
    //
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
    // 2. `pointer_file` is `projects/app/.lit` pointing to `/storage/app-metadata`
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
            if metadata_dir.has_same_canonical_path_with(&entry)? {
                return Ok(Self::Embedded);
            }
        } else if metadata.is_file() {
            // this is case 2 mentioned in the comment above
            if let Some(pointer_file) = pointer_file
                && pointer_file.has_same_canonical_path_with(&entry)?
            {
                return Ok(Self::Separate { pointer_file });
            }
        }
        Ok(Self::Direct)
    }
}

pub(super) struct Layout {
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
    /// Determines the repository layout
    pub(super) fn resolve(
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
            return Ok(Self {
                metadata_dir: cwd.join(metadata_dir)?,
                // the parent identifies the worktree even though the metadata is elsewhere
                worktree_dir: Some(root.clone()),
                placement: MetadataPlacement::Separate {
                    pointer_file: root.join_unchecked(".lit"),
                },
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
    pub(super) fn root(&self) -> &OsPath {
        self.worktree_dir.as_ref().unwrap_or(&self.metadata_dir)
    }

    pub(super) fn metadata_dir(&self) -> &OsPath {
        &self.metadata_dir
    }

    pub(super) fn worktree_dir(&self) -> Option<&OsPath> {
        self.worktree_dir.as_ref()
    }

    // link is always absolute by construction
    pub(super) fn pointer_file(&self) -> Option<&OsPath> {
        if let MetadataPlacement::Separate { pointer_file } = &self.placement {
            return Some(pointer_file);
        }
        None
    }

    pub(super) fn is_bare(&self) -> bool {
        self.worktree_dir.is_none()
    }

    // decide if we have to set core.worktree in config
    pub(super) fn needs_worktree_config(&self) -> bool {
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

    fn from_discovery(
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

// validate that the directory pointed by path is a valid Lit repository before migration for the
// separate-lit-dir flag
pub(super) fn validate_metadata_for_migration(
    path: OsPath,
    cwd: &OsPath,
) -> Result<RepositoryPaths, RepositoryError> {
    let location = resolve_metadata_location(path, cwd)?;
    let cfg_path = location.metadata_dir.join_unchecked("config");
    let cfg = ConfigFile::new_or_empty(cfg_path)?;
    // if version is absent, we don't discard the repo
    // the absent version does not make a structurally valid repo ineligible to move
    let _ = RepositoryFormat::from_config(&cfg)?;

    Ok(location)
}

fn require_accessible_dir(path: &OsPath) -> Result<(), RepositoryError> {
    match fs::read_dir(path) {
        Ok(_) => Ok(()),
        Err(err) => Err(RepositoryError::Io(IoError::new(
            "opendir",
            Some(path),
            err,
        ))),
    }
}

// TODO: when we add ref support, this need to check for ref: also
fn validate_head(path: &OsPath) -> Result<(), RepositoryError> {
    let bytes = fs::read(path).with_context("open", Some(path))?;

    let bytes = bytes
        .strip_suffix(b"\r\n")
        .or_else(|| bytes.strip_suffix(b"\n"))
        .unwrap_or(&bytes);

    // DETACHED HEAD
    match Oid::from_hex_bytes(bytes) {
        Ok(_) => Ok(()),
        Err(err) => Err(RepositoryError::BadHeadOid {
            path: path.clone(),
            source: err,
        }),
    }
}

// used by `discover_at()` to resolve the `LIT_DIR` env var. Must be a valid a metadata directory
// or a pointer file pointing to one. This one and done check. We don't append `.lit`, we don't
// ascend(search parents) or follow chains of pointer files. Missing or invalid entries are errors.
// We preserve the pointer path so placement can be determined relative to the resolved worktree.
// check `MetadataPlacement::from_discovery()`
//
// take ownership of the path and return it if the path itself is a valid metadata dir, otherwise
// return the path pointed by the litfile. This avoids a clone() call in the first case. There are
// other ways of doing it, we could use Cow too and the caller knows that if Cow::Borrowed then the
// path that was passed must be returned.
fn resolve_metadata_location(
    path: OsPath,
    cwd: &OsPath,
) -> Result<RepositoryPaths, RepositoryError> {
    let metadata = fs::metadata(&path).with_context("stat", Some(&path))?;

    let (metadata_dir, pointer_file) = if metadata.is_dir() {
        (path, None)
    } else if metadata.is_file() {
        let file = File::open(&path).with_context("open", Some(&path))?;
        // There is TOCTOU race condition.
        // The path may have changed since fs::metadata(). We check that the object we
        // actually opened is still file. Subsequent operations will use the handle, so
        // replacing the path will not redirect them to another file.
        let metadata = file.metadata().with_context("fstat", Some(&path))?;
        if !metadata.is_file() {
            return Err(RepositoryError::NotARegularFile(path));
        }
        // we read just once, Git does not follow chains of gitfiles
        // https://github.com/git/git/blob/e9019fcafe0040228b8631c30f97ae1adb61bcdc/setup.c#L956-L1035
        let target = litfile::read(&file, &path)?;
        (target, Some(path))
    } else {
        return Err(RepositoryError::UnexpectedEntry {
            path,
            entry: EntryType::from(metadata.file_type()),
        });
    };

    // wild case caught by AI
    //
    // We have the following:
    //
    //  `/project/.lit` -> `/config/app.lit`
    //          /project/.lit = symlink
    //          /config/app.lit = pointer file containing `litdir: /storage/current`
    //
    //  `/storage/current` -> `/storage/actual-metadata`
    //          /storage/current = symlink
    //          /storage/actual-metadata = metadata directory
    //
    // `resolve_entry_if_symlink()` returns `/config/app.lit` by following the `/project/.lit` symlink
    // `resolve_metadata_location()` reads the file and obtains `/storage/current`
    // `validate_metadata_structure()` gets invoked with `/storage/current` and makes 2 fs::read_dir()
    // calls for `/storage/current`, `/storage/current/refs` and 1 fs::read() for `/storage/current/HEAD`
    // fs::read_* follows symlinks so the actual paths that gets checked are `/storage/actual-metadata`
    // `/storage/actual-metadata/refs` and `/storage/actual-metadata/HEAD`
    //
    // `resolve_metadata_location()` returns RepositoryPaths.metadata_dir = path which is still `/config/app.lit`,
    // but what we actually want to return is the absolute, canonical metadata dir path, this is why
    // we need to make this canonicalize() call.
    //
    // explicit selection and pointer target must resolve successfully.
    let metadata_dir =
        fs::canonicalize(&metadata_dir).with_context("realpath", Some(&metadata_dir))?;
    let metadata_dir = OsPath::new_unchecked(metadata_dir);
    let objects_dir = resolve_objects_dir(&metadata_dir, &cwd)?;

    // Note: don't try to call `is_metadata_dir()` on `target`
    // `is_metadata_dir()` checks if the directory looks like a metadata dir and any structural
    // errors return Ok(false). A pointer file explicitly identifies a repository, so an
    // invalid target must produce an error rather than let the search continue.
    validate_metadata_structure(&metadata_dir, &objects_dir)?;

    Ok(RepositoryPaths {
        metadata_dir,
        objects_dir,
        pointer_file,
    })
}

fn is_metadata_dir(metadata_dir: &OsPath, objects_dir: &OsPath) -> Result<bool, RepositoryError> {
    match validate_metadata_structure(metadata_dir, objects_dir) {
        Ok(_) => Ok(true),
        // these errors mean the directory was not recognized
        Err(RepositoryError::Io(_) | RepositoryError::BadHeadOid { .. }) => Ok(false),
        // anything else is an error, for example a bad path from LIT_OBJECT_DIRECTORY
        Err(err) => Err(err),
    }
}

// https://github.com/git/git/blob/d38352cd43ab9745686d697872408bc3249a153f/setup.c#L413-L451
//
// The conditions that must hold true are:
//  - accessible dir pointed by path(r/w)
//  - valid HEAD, a proper "ref:", or a regular file HEAD that has a properly formatted sha1 object
//  name
//  - accessible objects dir or LIT_OBJECT_DIRECTORY env var
//  - accessible refs dir
//  - has a valid repository format(this is handled by the caller)
//
// This is the structure a valid Lit repo guarantees
fn validate_metadata_structure(path: &OsPath, objects_dir: &OsPath) -> Result<(), RepositoryError> {
    require_accessible_dir(path)?;
    validate_head(&path.join_unchecked("HEAD"))?;
    require_accessible_dir(&objects_dir)?;
    require_accessible_dir(&path.join_unchecked("refs"))
}

// `LIT_OBJECT_DIRECTORY` has the highest precedence when set, otherwise objects are stored under:
// `<metadata>/objects`
pub(super) fn resolve_objects_dir(
    metadata_dir: &OsPath,
    cwd: &OsPath,
) -> Result<OsPath, RepositoryError> {
    let objects_dir = match environment::var(environment::LIT_OBJECT_DIRECTORY) {
        Some(dir) => cwd.join(dir)?,
        None => metadata_dir.join_unchecked("objects"),
    };

    Ok(objects_dir)
}

// core.worktree describes the repository's working tree relative to its metadata directory. We need
// to resolve it based on the metadata dir, regardless of where we run the command, and not against
// the cwd
//
// after discovery, we need to check against the existing config file and overwrite any assumptions
// that we made for worktree during traversal
// core.bare = true, drop it.
// core.bare = false or absent, check core.worktree if present we resolve it against metadata_dir
// else we fall back to cwd
//
//  From the docs: `The value can be an absolute path or relative to the path to the .git directory,
//      which is either specified by --git-dir or GIT_DIR, or automatically discovered. If --git-dir
//      or GIT_DIR is specified but none of --work-tree, GIT_WORK_TREE and core.worktree is specified,
//      the current working directory is regarded as the top level of your working tree.
fn resolve_worktree_dir(
    cfg: &ConfigFile,
    metadata_dir: &OsPath,
    default: Option<OsPath>,
) -> Result<Option<OsPath>, RepositoryError> {
    let bare = match cfg.get_bool("core.bare") {
        Ok(bare) => bare,
        // absent defaults to false
        Err(err) if err.is_key_not_found() => false,
        Err(err) => return Err(RepositoryError::Config(err)),
    };
    let worktree_dir = if bare {
        None
    } else {
        // TODO: https://git-scm.com/docs/git-config#Documentation/git-config.txt-coreworktree
        //  we need to consider GIT_COMMON_DIR when we support linked worktrees.
        //  The value of core.worktree must be ignored if GIT_COMMON_DIR is set.
        match cfg.get_bytes("core.worktree") {
            Ok(bytes) => {
                let path = os::os_str_from_bytes(bytes.as_ref());
                Some(metadata_dir.join(path)?)
            }
            // absent, we fall back to default
            Err(err) if err.is_key_not_found() => default,
            Err(err) => return Err(RepositoryError::Config(err)),
        }
    };
    Ok(worktree_dir)
}

// probes an entry during ancestor discovery for a metadata dir or a pointer file
//
// returns `None` when path lookup reports a missing entry, a non-directory component or when an
// existing directory is not recognized as metadata. The caller then can try another candidate.
// regular files are treated as pointer files. `target` must be a valid metadata directory
fn probe_metadata_location(
    path: &OsPath,
    cwd: &OsPath,
) -> Result<Option<RepositoryPaths>, RepositoryError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err)
            if matches!(
                err.kind(),
                // for NotADirectory we check a failed path look-up
                // for a path like `project/file/.lit` where `project/file` is a regular file cannot
                // be traverse by the OS and reach `.lit`
                // metadata_location() is invoked during the ancestor walk because `dir` already
                // names a directory but just in case another process replace a directory component
                // between operations
                // `.lit` pointer files are returned correctly if the path is valid and resolved later
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None);
        }
        Err(err) => return Err(RepositoryError::Io(IoError::new("stat", Some(path), err))),
    };

    if metadata.is_dir() {
        // read the `canonicalize()` in `resolve_metadata_location()` on why we need this call
        let metadata_dir = match fs::canonicalize(path) {
            // a directory candidate
            Ok(metadata_dir) => OsPath::new_unchecked(metadata_dir),
            // failure to resolve it means this candidate is no good
            Err(_) => return Ok(None),
        };

        let objects_dir = resolve_objects_dir(path, cwd)?;
        return if is_metadata_dir(&metadata_dir, &objects_dir)? {
            Ok(Some(RepositoryPaths {
                metadata_dir,
                objects_dir,
                pointer_file: None,
            }))
        } else {
            // no good, try to ascend
            Ok(None)
        };
    }

    if !metadata.is_file() {
        return Err(RepositoryError::UnexpectedEntry {
            path: path.clone(),
            entry: EntryType::from(metadata.file_type()),
        });
    }

    let file = File::open(&path).with_context("open", Some(path))?;
    if !file.metadata().with_context("fstat", Some(path))?.is_file() {
        return Err(RepositoryError::NotARegularFile(path.clone()));
    }

    let target = litfile::read(&file, &path)?;
    // a pointer explicitly selects the target, any failure here unlike the dir case above is an
    // error no fall back
    let metadata_dir = fs::canonicalize(&target).with_context("realpath", Some(&target))?;
    let metadata_dir = OsPath::new_unchecked(metadata_dir);
    let objects_dir = resolve_objects_dir(&metadata_dir, &cwd)?;
    // Note: read the equivalent branch in `resolve_metadata_location()` on why we don't call
    // `is_metadata_dir()`
    validate_metadata_structure(&metadata_dir, &objects_dir)?;

    Ok(Some(RepositoryPaths {
        metadata_dir,
        objects_dir,
        pointer_file: Some(path.clone()),
    }))
}

fn search_ancestors(cwd: &OsPath) -> Result<(RepositoryPaths, Option<OsPath>), RepositoryError> {
    let mut dir = cwd.clone();

    loop {
        let entry = dir.join_unchecked(".lit");
        if let Some(metadata_location) = probe_metadata_location(&entry, &cwd)? {
            return Ok((metadata_location, Some(dir)));
        }

        if let Ok(metadata_dir) = fs::canonicalize(&dir) {
            let metadata_dir = OsPath::new_unchecked(metadata_dir);
            let objects_dir = resolve_objects_dir(&dir, &cwd)?;
            // check whether the directory itself contains metadata later config can determine the
            // worktree
            if is_metadata_dir(&metadata_dir, &objects_dir)? {
                return Ok((
                    RepositoryPaths {
                        metadata_dir: dir,
                        objects_dir,
                        pointer_file: None,
                    },
                    None,
                ));
            }
        }

        let Some(parent) = dir.parent() else {
            return Err(RepositoryError::NotRepository(cwd.clone()));
        };
        // TODO: At this point we need to check for Celling values.
        dir = parent;
    }
}

// extract state as we do traversal to resolve later
pub(super) struct RepositoryPaths {
    // absolute, canonical path to the metadata dir
    metadata_dir: OsPath,
    objects_dir: OsPath,
    pointer_file: Option<OsPath>,
}

impl RepositoryPaths {
    pub(super) fn metadata_dir(&self) -> &OsPath {
        &self.metadata_dir
    }
}

// TODO: https://stackoverflow.com/questions/65499497/how-does-git-know-its-in-a-git-repo
pub(super) struct Repository {
    layout: Layout,
    format: RepositoryFormat,
    // unlike refs, for objects Git provides the 'GIT_OBJECTS_DIR' env var which is a separate dir
    // that we must keep track of when we do repo discovery. If the env var is set, the <metadata>/objects
    // directory is not created and the objects are stored in the dir pointed by the env var.
    objects_dir: OsPath,
}

impl Repository {
    // Discovery never writes config
    pub(super) fn discover() -> Result<Self, RepositoryError> {
        // https://github.com/git/git/blob/d38352cd43ab9745686d697872408bc3249a153f/setup.c#L1581-L1590
        // https://git-scm.com/docs/git#Documentation/git.txt---git-dirltpathgt
        // From the docs: `Specifying the location of the ".git" directory using this option
        //  (or GIT_DIR environment variable) turns off the repository discovery that tries to find
        //  a directory with ".git" subdirectory (which is how the repository and the top-level of
        //  the working tree are discovered), and tells Git that you are at the top level of the
        //  working tree`
        if let Some(lit_dir) = environment::var(environment::LIT_DIR) {
            // the two directory paths differ in what their metadata checks return, because they
            // have different information about the worktree.
            // - `LIT_DIR` returns RepositoryPaths, has no info about worktree
            // - ancestors traversal returns (RepositoryPaths, Option<OsPath>), where Option<OsPath>
            // is the identified path
            //
            // ancestor discovery provides a default worktree location, while inspecting a `LIT_DIR`
            // path does not. During ancestor discovery finding: `/project/.lit` means `/project` is
            // the inferred worktree. If instead `/project` is the metadata directory(bare), worktree
            // is None. The default worktree may later be overridden by `config`.
            // `LIT_DIR` specifies the metadata directory itself. We don't have information about the
            // worktree. It can literally be anywhere. `LIT_WORK_TREE`, and cwd are used to as fall
            // backs.
            Self::discover_at(&lit_dir)
        } else {
            Self::discover_upward()
        }
    }

    // inspects the repository pointed by `LIT_DIR` and reports failure without searching elsewhere
    pub(super) fn discover_at<P>(path: P) -> Result<Self, RepositoryError>
    where
        P: AsRef<Path>,
    {
        let cwd = environment::cwd()?;
        let path = cwd.join(path)?;
        let paths = resolve_metadata_location(path, &cwd)?;
        let RepositoryPaths {
            metadata_dir,
            objects_dir,
            pointer_file,
        } = paths;
        let cfg = ConfigFile::new_or_empty(metadata_dir.join_unchecked("config"))?;
        // exclusive LIT_WORK_TREE var wins over any config value
        let worktree_dir = if let Some(lit_work_tree) = environment::var(environment::LIT_WORK_TREE)
        {
            Some(cwd.join(lit_work_tree)?)
        } else {
            resolve_worktree_dir(&cfg, &metadata_dir, Some(cwd))?
        };
        let layout = Layout::from_discovery(metadata_dir, worktree_dir, pointer_file)?;
        let format = RepositoryFormat::from_config(&cfg)?.unwrap_or_default();

        Ok(Self {
            layout,
            format,
            objects_dir,
        })
    }

    // Finds a repository starting from cwd and moves up to its ancestors
    fn discover_upward() -> Result<Self, RepositoryError> {
        let cwd = environment::cwd()?;
        let (paths, worktree_dir) = search_ancestors(&cwd)?;
        // https://doc.rust-lang.org/rust-by-example/flow_control/match/destructuring/destructure_structures.html
        let RepositoryPaths {
            metadata_dir,
            objects_dir,
            pointer_file,
        } = paths;
        let cfg = ConfigFile::new_or_empty(metadata_dir.join_unchecked("config"))?;
        let worktree_dir = resolve_worktree_dir(&cfg, &metadata_dir, worktree_dir)?;
        let layout = Layout::from_discovery(metadata_dir, worktree_dir, pointer_file)?;
        let format = RepositoryFormat::from_config(&cfg)?.unwrap_or_default();

        Ok(Self {
            layout,
            format,
            objects_dir,
        })
    }

    pub(super) fn database(&self) -> Database<'_> {
        Database::new(&self.objects_dir, *self.format.object_format())
    }

    // bare repos have no worktree
    pub(super) fn workspace(&self) -> Option<Workspace<'_>> {
        self.worktree_dir().map(Workspace::new)
    }

    pub(super) fn index(&self) -> Index {
        Index::new(self.metadata_dir().join_unchecked("index"))
    }

    // TODO: need testing if this should be just `new()`, for now the approach is to treat a missing
    //  config as "no local settings" instead of repository error
    pub(super) fn config(&self) -> Result<ConfigFile, ConfigFileError> {
        ConfigFile::new_or_empty(self.config_path())
    }

    pub(super) fn refs(&self) -> Refs {
        Refs::new(self.metadata_dir())
    }

    pub(super) fn metadata_dir(&self) -> &OsPath {
        self.layout.metadata_dir()
    }

    pub(super) fn worktree_dir(&self) -> Option<&OsPath> {
        self.layout.worktree_dir()
    }

    pub(super) fn objects_dir(&self) -> &OsPath {
        &self.objects_dir
    }

    pub(super) fn config_path(&self) -> OsPath {
        self.layout.metadata_dir.join_unchecked("config")
    }

    pub(super) fn refs_path(&self) -> OsPath {
        self.layout.metadata_dir.join_unchecked("refs")
    }

    pub(super) fn is_bare(&self) -> bool {
        self.layout.is_bare()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EntryType {
    File,
    Directory,
    Symlink,
    Other,
}

impl fmt::Display for EntryType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::File => "file",
            Self::Directory => "directory",
            Self::Symlink => "symlink",
            Self::Other => "other",
        };
        f.write_str(label)
    }
}

impl From<FileType> for EntryType {
    fn from(file_type: FileType) -> Self {
        if file_type.is_file() {
            Self::File
        } else if file_type.is_dir() {
            Self::Directory
        } else if file_type.is_symlink() {
            Self::Symlink
        } else {
            Self::Other
        }
    }
}

#[derive(Debug)]
pub(super) enum LayoutError {
    Io(IoError),
    OsPath(OsPathError),
    LitWorkTreeWithBare,
}

impl Error for LayoutError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            Self::OsPath(source) => Some(source),
            Self::LitWorkTreeWithBare => None,
        }
    }
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(_) => write!(f, "layout resolution I/O failed"),
            Self::OsPath(_) => write!(f, "bad path"),
            Self::LitWorkTreeWithBare => {
                write!(f, "LIT_WORK_TREE not allowed with --bare option")
            }
        }
    }
}

impl From<IoError> for LayoutError {
    fn from(err: IoError) -> Self {
        Self::Io(err)
    }
}

impl From<OsPathError> for LayoutError {
    fn from(err: OsPathError) -> Self {
        Self::OsPath(err)
    }
}

#[derive(Debug)]
pub(super) enum RepositoryError {
    Io(IoError),
    OsPath(OsPathError),
    Litfile(LitFileError),
    Config(ConfigFileError),
    RepositoryFormat(RepositoryFormatError),
    NotRepository(OsPath),
    // initial inspection of the supplied path failed, dir or regular file
    UnexpectedEntry { path: OsPath, entry: EntryType },
    // inspection of an opened pointer-file handle, regular file only
    NotARegularFile(OsPath),
    BadHeadOid { path: OsPath, source: OidError },
}

impl Error for RepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            Self::OsPath(source) => Some(source),
            Self::Litfile(source) => Some(source),
            Self::Config(source) => Some(source),
            Self::RepositoryFormat(source) => Some(source),
            Self::NotRepository { .. } => None,
            Self::UnexpectedEntry { .. } => None,
            Self::NotARegularFile(_) => None,
            Self::BadHeadOid { source, .. } => Some(source),
        }
    }
}

impl fmt::Display for RepositoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(_) => write!(f, "repository I/O failed"),
            Self::OsPath(_) => write!(f, "bad path"),
            Self::Litfile(_) => write!(f, "could not read metadata pointer"),
            Self::Config(_) => write!(f, "could not load repository configuration"),
            Self::RepositoryFormat(_) => write!(f, "bad repository format"),
            Self::NotRepository(path) => write!(f, "not a lit repository: {}", path.display()),
            Self::UnexpectedEntry { path, entry } => {
                write!(
                    f,
                    "expected a metadata directory or a regular pointer file at {}, found {}",
                    path.display(),
                    entry
                )
            }
            Self::NotARegularFile(path) => {
                write!(f, "not a regular file: {}", path.display())
            }
            Self::BadHeadOid { path, .. } => {
                write!(f, "bad object id in HEAD at {}", path.display())
            }
        }
    }
}

impl From<IoError> for RepositoryError {
    fn from(err: IoError) -> Self {
        Self::Io(err)
    }
}

impl From<OsPathError> for RepositoryError {
    fn from(err: OsPathError) -> Self {
        Self::OsPath(err)
    }
}

impl From<LitFileError> for RepositoryError {
    fn from(err: LitFileError) -> Self {
        Self::Litfile(err)
    }
}

impl From<ConfigFileError> for RepositoryError {
    fn from(err: ConfigFileError) -> Self {
        Self::Config(err)
    }
}

impl From<RepositoryFormatError> for RepositoryError {
    fn from(err: RepositoryFormatError) -> Self {
        Self::RepositoryFormat(err)
    }
}
