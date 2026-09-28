use crate::repo::config::{ConfigFile, ConfigFileError};
use crate::repo::format::{
    FormatVersion, ObjectFormat, ObjectFormatError, RefFormat, RefFormatError, RefStorage,
    RepositoryFormat, RepositoryFormatError,
};
use crate::repo::litfile::{self, LitFileError};
use crate::repo::lockfile::{Lockfile, LockfileError};
use crate::repo::os::{self, IoError, IoErrorContext, OsPath, OsPathError};
use crate::repo::refs::{RefError, Refs};
use crate::repo::{self, EntryType, RepositoryError, environment};
use clap::Args;
use std::borrow::Cow;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::{fmt, fs, io};
use tempfile::{Builder, NamedTempFile, TempDir};

#[derive(Debug, Args)]
// conditionally adds an attribute during compilation, here it means if compiling tests apply the
// attribute. We need `Init::default()` for testing but outside that, `Init` must only be created by
// Clap
#[cfg_attr(test, derive(Default))]
pub(crate) struct Init {
    #[arg(short = 'q', long)]
    quiet: bool,
    #[arg(long)]
    bare: bool,
    // it conflicts with bare because it separates the metadata directory from a working tree, while
    // bare creates a repo without a working tree. The pointer file left behind by this flag must
    // live somewhere in this case the root of the working tree
    //
    // git init --separate-git-dir /path/to/repo.git path/to/worktree
    #[arg(long, conflicts_with = "bare")]
    separate_lit_dir: Option<PathBuf>,
    #[arg(long, value_enum)]
    object_format: Option<ObjectFormat>,
    // files and reftable are the only possible values for the flag
    // the `backend://payload` syntax is config only
    #[arg(long, value_enum)]
    ref_format: Option<RefFormat>,
    // Directory in which to initialize the repository
    path: Option<PathBuf>,
    #[arg(short = 'b', long)]
    initial_branch: Option<OsString>,
}

impl Init {
    // https://github.com/git/git/blob/fa7f9290efe2bd22dd736689597b474b93798e11/setup.c#L2841-L2945
    // TODO: create hooks, info, description
    pub(super) fn execute(&self) -> Result<(), InitError> {
        // resolve arguments and env vars for the location of the metadata dir.
        let destination = Destination::resolve(
            self.path.as_deref(),
            self.bare,
            self.separate_lit_dir.as_deref(),
        )?;
        // create the positional/root directory first
        // non-bare repos need the working tree directory while bare repos need the metadata
        fs::create_dir_all(destination.root()).with_context("mkdir", Some(destination.root()))?;
        let cwd = environment::cwd()?;
        // we relocate before loading configuration so the rest of init uses the destination repo
        if let Some(link) = destination.pointer_file() {
            // TODO: we need to write a test and see what happens if someone provides --separate-lit-dir
            //  flag for an existing bare repo
            // Note: what we don't currently do is to check if the user provided formats(if any) conflict
            // against the repo we want to migrate before the migration happens. It happens after,
            // which means that if it fails the repository has already been migrated. This mimics
            // git's behavior.
            //
            // `init_db()` link above does the following:
            //  First it invokes `separate_git_dir()` that performs the move and writes the pointer,
            //  and then calls `repository_format_configure()` to check for a mismatch.
            //  The flow is: move metadata -> write pointer -> check repo format -> reject on mismatch
            //  This means that on a mismatch we never move back the repo
            try_migrate_metadata(link, destination.metadata_dir(), &cwd)?;
        }

        ensure_dir(destination.metadata_dir())?;
        let cfg_path = destination.metadata_dir().join_unchecked("config");
        let mut lockfile = Lockfile::acquire(&cfg_path)?;
        // https://github.com/git/git/blob/3cb9185f65410273787f74333cc027d2ea5daada/setup.c#L751
        // a fresh repo, or a reinit with a missing file we have to repair
        // only an Io::NotFound error produces an empty in memory ConfigFile later
        let mut cfg = ConfigFile::new_or_empty(cfg_path)?;
        self.finalize_repo_format(&mut cfg)?;

        // TODO: next is apply_repository_format()
        //  https://github.com/git/git/blob/fa7f9290efe2bd22dd736689597b474b93798e11/setup.c#L2884
        //  apply_repo_format() checks at this point for GIT_SHALLOW_FILE, need to revisit when we
        //  support shallow repositories(contains truncated history)
        //  next is to copy any templates
        //  https://github.com/git/git/blob/fa7f9290efe2bd22dd736689597b474b93798e11/setup.c#L2587
        let reinit = is_reinit(destination.metadata_dir())?;
        // When a tracked entry's mode differs from what is recorded, Git must distinguish if the
        // change was actually made by the user, or it is a false positive because the environment
        // does not support Unix permissions(Windows, a fs mounted without permissions)
        if probe_fs_for_filemode(destination.metadata_dir())? {
            cfg.set_all("core.filemode", "true")?;
        } else {
            cfg.set_all("core.filemode", "false")?;
        }
        // -`lit init project` and `lit init --bare project`, causes no confusion on what happens.
        // Different metadata directories. For non-bare the metadata entries are created in
        // project/.lit, then directly inside `project/`, so `project/HEAD`, `project/config` etc.
        // -`LIT_DIR = repo/meta` and `lit init` with or without --bare results in metadata entries
        // ending up in the same directory with different worktree state.
        //
        // we honor the invariant that we set in resolve() that explicit --bare flag wins. Calling
        // --bare on an existing repo will set the `core.bare = true` and delete all `core.worktree`
        // instances.
        if destination.is_bare() {
            cfg.unset_all("core.worktree")?;
            cfg.set_all("core.bare", "true")?;
        } else {
            cfg.set_all("core.bare", "false")?;
            // https://git-scm.com/docs/git-config#Documentation/git-config.txt-corelogAllRefUpdates
            // From the docs: `This value is true by default in a repository that has a working
            //  directory associated with it, and false by default in a bare repository.`
            // The term `working directory` translates to Destinations's worktree, not the cwd.
            //
            // https://github.com/git/git/blob/3699d22b59a6ea467ce13edb81b6bdea0398c803/setup.c#L2628-L2641
            // Did some manual testing against Git and if the repo is bare it ignores the value if
            // present, if absent also performs no action, absence = implicit false`. As seen from
            // the code too, it completes ignores it for bare repos.
            // https://github.com/git/git/blob/3699d22b59a6ea467ce13edb81b6bdea0398c803/setup.c#L2636-L2637
            // as of 2.55 Git rejects a bad boolean value, respects one if present and defaults to
            // true if absent
            match cfg.get_bool("core.logallrefupdates") {
                Ok(_) => {}
                Err(err) if err.is_key_not_found() => {
                    cfg.set("core.logallrefupdates", "true")?;
                }
                Err(err) => return Err(InitError::Config(err)),
            }
            if destination.needs_worktree_config()? {
                // non-bare so worktree_dir() is safe to unwarp
                let value = os::os_str_from_bytes(destination.worktree_dir().unwrap().as_bytes());
                cfg.set_all("core.worktree", value)?;
            } else {
                // The intended `.lit` relationship provides the worktree, remove an old override
                // that could select another location
                cfg.unset_all("core.worktree")?;
            }
        }

        // I couldn't understand why those 2 cfg settings are checked only for new repos and left
        // unchecked for existing ones.
        // https://github.com/git/git/blob/3699d22b59a6ea467ce13edb81b6bdea0398c803/setup.c#L2643-L2660
        if !reinit {
            // absence -> implicitly true
            // https://github.com/git/git/blob/3699d22b59a6ea467ce13edb81b6bdea0398c803/setup.c#L2646-L2653
            // only writes false in the else block
            if !probe_fs_for_symlink_support(destination.metadata_dir())? {
                cfg.set_all("core.symlinks", "false")?;
            }
            // absence -> implicitly false
            if !probe_fs_for_case_sensitivity(destination.metadata_dir())? {
                cfg.set_all("core.ignorecase", "true")?;
            }
        }
        setup_object_db(destination.metadata_dir(), &cwd)?;
        setup_ref_backend(
            destination.metadata_dir(),
            // as_ref() would give us Option<&OsString>
            self.initial_branch.as_deref(),
            &cfg,
            reinit,
        )?;
        // TODO: Git adjust the message based on share repo settings
        //  https://github.com/git/git/blob/d38352cd43ab9745686d697872408bc3249a153f/setup.c#L2929-L2938
        if !reinit {
            println!(
                "Initialized existing Lit repository in {}",
                destination.metadata_dir().display()
            );
        } else {
            println!(
                "Reinitialized existing Lit repository in {}",
                destination.metadata_dir().display()
            );
        }
        lockfile.write(&cfg.serialize())?;
        lockfile.commit()?;

        Ok(())
    }

    // https://github.com/git/git/blob/b8242b093d9e941a34460d715e3ce616a34ac3fe/setup.c#L2787-L2801
    // precedence: flag > env var > config value
    fn resolve_object_format(&self, cfg: &ConfigFile) -> Result<ObjectFormat, InitError> {
        if let Some(format) = self.object_format {
            return Ok(format);
        }

        if let Some(hash) = environment::var(environment::LIT_DEFAULT_HASH) {
            let hash = os::os_str_as_bytes(&hash);
            return ObjectFormat::try_from(hash).map_err(InitError::UnknownObjectFormat);
        }

        // TODO: cfg needs to look for this in the global config not local?
        match cfg.get_str("init.defaultObjectFormat") {
            Ok(hash) => {
                ObjectFormat::try_from(hash.as_bytes()).map_err(InitError::UnknownObjectFormat)
            }
            Err(err) if err.is_key_not_found() => Ok(ObjectFormat::default()),
            Err(err) => Err(InitError::Config(err)),
        }
    }

    fn resolve_ref_storage(&self, cfg: &ConfigFile) -> Result<RefStorage, InitError> {
        let mut ref_storage = RefStorage::default();

        // https://github.com/git/git/blob/b8242b093d9e941a34460d715e3ce616a34ac3fe/setup.c#L2824-L2838
        // https://github.com/git/git/blob/b8242b093d9e941a34460d715e3ce616a34ac3fe/environment.h#L46
        // when I wrote this, I couldn't find any reference for that env var in the docs
        // In the src code, linked above, the branch that checks this env var is a separate one, disconnected
        // from the rest of the logic. It has the highest precedence, it overwrites any previously set
        // value.
        if let Some(ref_backend) = environment::var(environment::LIT_REFERENCE_BACKEND) {
            let ref_backend = os::os_str_as_bytes(ref_backend.as_os_str());
            ref_storage = RefStorage::try_from(ref_backend)?;

            return Ok(ref_storage);
        }

        // https://github.com/git/git/blob/b8242b093d9e941a34460d715e3ce616a34ac3fe/setup.c#L2803-L2821
        if let Some(ref_format) = self.ref_format {
            *ref_storage.format_mut() = ref_format;
        } else if let Some(ref_format) = environment::var(environment::LIT_DEFAULT_REF_FORMAT) {
            let ref_format = os::os_str_as_bytes(ref_format.as_os_str());
            *ref_storage.format_mut() = RefFormat::try_from(ref_format)?;
        } else {
            match cfg.get_str("init.defaultRefFormat") {
                Ok(ref_format) => {
                    *ref_storage.format_mut() = RefFormat::try_from(ref_format.as_bytes())?;
                }
                // if the config value is not set, storage is already default, we don't have to
                // perform any action
                Err(err) if err.is_key_not_found() => {}
                Err(err) => return Err(InitError::Config(err)),
            }
        }
        Ok(ref_storage)
    }

    fn finalize_repo_format(&self, cfg: &mut ConfigFile) -> Result<(), InitError> {
        // defer falling back to default for now, read comment below
        let repo_format = RepositoryFormat::from_config(&cfg)?;
        // https://github.com/git/git/blob/b8242b093d9e941a34460d715e3ce616a34ac3fe/setup.c#L2765
        // if repo format is absent, init options like --object-format or --ref-format can select the
        // format. For an existing format its options must not conflict with the user provided ones
        // If we initialize repo_format to default, we wouldn't be able to make the distinction
        let mut repo_format = match repo_format {
            // for an existing repo don't allow the user to specify a different hash/ref format, it
            // can lead to unexpected behavior/corruption of the repo.
            Some(repo_format) => {
                if self
                    .object_format
                    .is_some_and(|obj_format| obj_format != *repo_format.object_format())
                {
                    return Err(InitError::HashMismatch);
                }
                if self
                    .ref_format
                    .is_some_and(|format| format != *repo_format.ref_storage().format())
                {
                    return Err(InitError::RefStorageMisMatch);
                }
                repo_format
            }
            None => {
                // safe to default it and let user provided option overwrite the formats
                let mut repo_format = RepositoryFormat::default();
                *repo_format.object_format_mut() = self.resolve_object_format(cfg)?;
                *repo_format.ref_storage_mut() = self.resolve_ref_storage(cfg)?;
                repo_format
            }
        };
        finalize_format_version(cfg, &mut repo_format)?;
        Ok(())
    }
}

// we keep the state after applying our path rules
// the paths are all absolute but not canonical, they are resolved based on the rules
// read comment in Self::resolve()
// it is an intermediate representation of the layout, since those paths are not normalized yet(it
// is likely that they won't exist for a fresh repo), symlinks are not resolved etc.
#[derive(Debug)]
struct Destination {
    metadata_dir: OsPath,
    worktree_dir: Option<OsPath>,
    pointer_file: Option<OsPath>,
}

impl Destination {
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
    fn resolve(
        path: Option<&Path>,
        bare: bool,
        separate_lit_dir: Option<&Path>,
    ) -> Result<Self, DestinationError> {
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
            let pointer_file = root.join_unchecked(".lit");
            return Ok(Self {
                metadata_dir: cwd.join(metadata_dir)?,
                worktree_dir: Some(root),
                pointer_file: Some(pointer_file),
            });
        }

        // explicit positional path wins over `LIT_DIR`
        if path.is_some() {
            return if bare {
                Ok(Self {
                    metadata_dir: root,
                    worktree_dir: None,
                    pointer_file: None,
                })
            } else {
                Ok(Self {
                    // <worktree>/.lit
                    metadata_dir: root.join_unchecked(".lit"),
                    worktree_dir: Some(root),
                    pointer_file: None,
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
        if let Some(metadata_dir) = environment::var(environment::LIT_DIR) {
            let metadata_dir = cwd.join(metadata_dir)?;
            // LIT_WORK_TREE makes sense only in conjunction with LIT_DIR without --bare. In any
            // other case it is ignored.
            let worktree = environment::var(environment::LIT_WORK_TREE);
            let worktree = worktree.map(OsPath::new).transpose()?;
            // bare repos have no worktree
            if bare && worktree.is_some() {
                return Err(DestinationError::LitWorkTreeWithBare);
            }

            let worktree_dir = if bare {
                None
            } else {
                // if no WORK_TREE found we fall back to cwd
                // https://kernel.googlesource.com/pub/scm/git/git.git/%2B/d8a267404cb2a9376bed0ede45c759a0b30590d7/t/t1510-repo-setup.sh
                // https://git-scm.com/docs/git#Documentation/git.txt---work-treeltpathgt
                // LIT_WORK_TREE is resolved against cwd if relative
                Some(worktree.map_or(cwd.clone(), |worktree| cwd.join_unchecked(worktree)))
            };
            return Ok(Self {
                metadata_dir,
                worktree_dir,
                pointer_file: None,
            });
        }

        // Note: LIT_WORK_TREE is considered only when LIT_DIR is set. This branch is reached when
        // LIT_DIR is unset, so even if LIT_WORK_TREE is set, it is ignored. bare does not error,
        // non-bare uses cwd
        if bare {
            Ok(Self {
                metadata_dir: root,
                worktree_dir: None,
                pointer_file: None,
            })
        } else {
            Ok(Self {
                metadata_dir: root.join_unchecked(".lit"),
                worktree_dir: Some(root),
                pointer_file: None,
            })
        }
    }

    // the worktree root for non-bare or the metadata directory for bare
    fn root(&self) -> &OsPath {
        self.worktree_dir.as_ref().unwrap_or(&self.metadata_dir)
    }

    fn metadata_dir(&self) -> &OsPath {
        &self.metadata_dir
    }

    fn worktree_dir(&self) -> Option<&OsPath> {
        self.worktree_dir.as_ref()
    }

    fn pointer_file(&self) -> Option<&OsPath> {
        self.pointer_file.as_ref()
    }

    fn is_bare(&self) -> bool {
        self.worktree_dir.is_none()
    }

    // TODO: provide 1 example for each case
    // resolve() provides absolute paths but those are not canonical, and we can not determine if
    // we should write `core.worktree` without making an `fs` call. Symlinks can make the comparison
    // incorrectly conclude that the setting is necessary. Paths with special components('.', '..')
    // also can lead to the same behavior. We might end up writing redundant config.
    //
    // 1. no worktree -> no setting(bare)
    //  `metadata_dir` = /repos/app.git
    //  `worktree_dir` = None,
    //  `pointer_file` = None
    // 2. pointer_file -> no setting
    //  `metadata_dir` = /storage/app-metadata
    //  `worktree_dir` = /projects/app,
    //  `pointer_file` = /projects/app/.lit -> `litdir: /storage/app-metadata`
    //      During discovery in `search_ancestors()` we find the `pointer_file` and we set as `worktree_dir`
    //      /project/app and `metadata_dir` its target, so worktree is not needed
    // 3. same canonical path -> no setting
    //  `metadata_dir` = /projects/app/.lit
    //  `worktree_dir` = /projects/app,
    //  `pointer_file` = None
    //      Same as case 2, we discover the `metadata_dir` which now is a directory and through that
    //      we can determine the worktree
    // 4. `.lit` missing -> write setting
    //  `metadata_dir` = /storage/app-metadata
    //  `worktree_dir` = /projects/app,
    //  `pointer_file` = None
    //      Both directories exist, but the `entry` does not. Calling canonicalize() returns `NotFound`
    //      we need to write the setting because we can not discover the worktree otherwise.
    // 5. different canonical paths -> write
    //  `metadata_dir` = /storage/app-metadata
    //  `worktree_dir` = /projects/app,
    //  `pointer_file` = None
    //      `entry` = /projects/app/.lit exists but paths disagree `metadata_dir` != `entry` so we
    //      must write the setting and set `core.worktree` to `/projects/app`
    // 6. any other error propagates
    //  if we have a symlink whose target is a directory we can't traverse, canonicalize will return
    //  something like `PermissionDenied` which we propagate.
    fn needs_worktree_config(&self) -> Result<bool, IoError> {
        let Some(worktree_dir) = &self.worktree_dir else {
            return Ok(false);
        };

        if self.pointer_file().is_some() {
            return Ok(false);
        }

        let metadata_dir = fs::canonicalize(self.metadata_dir())
            .with_context("realpath", Some(self.metadata_dir()))?;
        let entry = worktree_dir.join_unchecked(".lit");

        match fs::metadata(&entry) {
            Ok(_) => Ok(entry.same_canonical_path_with(&metadata_dir)?),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(true),
            Err(err) => Err(IoError::new("realpath", Some(&entry), err)),
        }
    }
}

// https://github.com/git/git/blob/fa7f9290efe2bd22dd736689597b474b93798e11/setup.c#L2515-L2525
// git checks if HEAD is accessible or a symlink(dangling is fine)
// we never check if the head_path points to an actual HEAD file, all we care about at this point is
// if something exists at that path, because overwriting can be destructive and lead to unexpected
// behavior.
fn is_reinit(path: &OsPath) -> Result<bool, IoError> {
    let head = path.join_unchecked("HEAD");
    match fs::symlink_metadata(&head) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(IoError::new("lstat", Some(&head), err)),
    }
}

// Git probes the filesystem because it needs to know whether a reported executable-bit difference
// represents a real change or merely a limitation of the filesystem interface.
// From the docs: `Some filesystems lose the executable bit when a file that is marked as executable
//   is checked out, or checks out a non-executable file with executable bit on. git probe the
//   filesystem to see if it handles the executable bit correctly and this variable is
//   automatically set as necessary.`
//
// Example:
//  We commit a script with mode 100755. We check out that repo in an environment that does not
//  preserve the Unix executable bit and its metadata now reports the script as non-executable,
//  even though we changed nothing. If Git trusted that result, it would record the change into
//  a commit. With core.fileMode = false, Git ignores the working-tree executable-bit difference
//  while continuing to keep track of the file.
//
// Git's docs about filemode mention filesystem and cross-environment situations that can cause
// this. https://git-scm.com/docs/git-config#Documentation/git-config.txt-corefileMode
fn probe_fs_for_filemode(probe_dir: &OsPath) -> Result<bool, IoError> {
    // we create a temporary file inside the metadata directory for the filemode probe. We don't try
    // to test it against an existing file.
    // https://docs.rs/tempfile/latest/tempfile/struct.Builder.html#method.permissions
    // the permissions of the new file are 600
    let tempfile =
        NamedTempFile::new_in(probe_dir).with_context("create temp file in", Some(probe_dir))?;
    let path = OsPath::new_unchecked(tempfile.path());
    // TODO: review
    //  Git considers more to detect trust: https://github.com/git/git/blob/3699d22b59a6ea467ce13edb81b6bdea0398c803/setup.c#L2623
    //  it maps to os::probe_filemode(&path)? && (reinit || !os::is_executable(&path)?);
    //  I don't know what it does exactly or why it needs it, maybe because it uses the cfg file for
    //  the test? I am not sure so I drop it for now
    let trust = os::probe_filemode(&path)?;
    // the explicit call to close is to report any potential errors when closing the file
    tempfile.close().with_context("unlink", Some(&path))?;

    Ok(trust)
}

// https://git-scm.com/docs/git-config#Documentation/git-config.txt-coresymlinks
//
// similar to how we probe for filemode we do the same for symlinks
// core.symlinks control whether Git checks out tracked symbolic links as actual filesystem symlinks
//
// if we have a tracked symlink file like `current` that contains `releases/v1`
// when checking out that entry, Git uses `core.symlinks` to choose how to represent that file.
//  - true: follows the symlink and reads the content of target
//  - false: reads the text `releases/v1`
fn probe_fs_for_symlink_support(probe_dir: &OsPath) -> Result<bool, IoError> {
    let temp_dir =
        TempDir::new_in(probe_dir).with_context("create temp file in", Some(probe_dir))?;
    let parent = OsPath::new_unchecked(temp_dir.path());
    let link = parent.join_unchecked("link");
    let support = os::probe_symlink(&link)?;

    temp_dir.close().with_context("unlink", Some(&parent))?;

    Ok(support)
}

// https://git-scm.com/docs/git-config#Documentation/git-config.txt-coreignoreCase
//
// This setting is important when matching a working-tree path to a tracked entry and comparing paths
// already stored in the repo.
//
// TODO: review this for add and status
// If index contains src/Parser.rs but the working-tree traversal returns src/parser.rs with
// core.ignorecase = true our lookup should recognize that entry and not try to create a new one.
// The src/parser.rs should also not appear as untracked. HEAD to index comparisons remain exact
// because an intentionally staged rename from Parser.rs to parser.rs is a real repo change
//  Delete this after: core.ignorecase = true -> tracked.eq_ignore_ascii_case(observed)
//  else tracked == observed
fn probe_fs_for_case_sensitivity(path: &OsPath) -> Result<bool, IoError> {
    let tempfile = Builder::new()
        .prefix(".lit-case-probe")
        .suffix(".case")
        .tempfile_in(path)
        .with_context("create tempfile in", Some(path))?;
    let probe_path = tempfile.path().with_extension("CASE");

    let case_sensitive = match fs::symlink_metadata(&probe_path) {
        Ok(_) => false,
        Err(err) if err.kind() == io::ErrorKind::NotFound => true,
        Err(err) => return Err(IoError::new("lstat", Some(&probe_path), err)),
    };

    Ok(case_sensitive)
}

// TODO: revisit when we support packfiles and worktree
// https://github.com/git/git/blob/f0ef1b96a076d08dc972a8d2cb0d1cfd60931eb6/setup.c#L2666-L2689
fn setup_object_db(metadata_dir: &OsPath, cwd: &OsPath) -> Result<(), InitError> {
    // the env var has the higher precedence than the <common_directory>/objects where <common_directory>
    // in our case is the layout.metadata(). This is until we support linked worktree
    // Read: https://git-scm.com/book/en/v2/Git-Internals-Environment-Variables
    //       https://git-scm.com/docs/git-init
    let objects_dir = repo::resolve_objects_dir(metadata_dir, cwd)?;
    let info = objects_dir.join_unchecked("info");
    let pack = objects_dir.join_unchecked("pack");
    ensure_dir(&objects_dir)?;
    ensure_dir(&info)?;
    ensure_dir(&pack)
}

// name: --initial-branch option value
// TODO: this is implementation is incomplete
//  currently we create the backend as files. We never consider the ref storage itself. When we support
//  reftables we need to rewrite it. Even this impl with files does not support files with payload
//  refstorage.format = RefFormat::Files, payload = None,
//  Probably need to move the logic to Refs something like create_on_disk() or something
fn setup_ref_backend(
    metadata: &OsPath,
    name: Option<&OsStr>,
    cfg: &ConfigFile,
    reinit: bool,
) -> Result<(), InitError> {
    let refs = metadata.join_unchecked("refs");
    let heads = refs.join_unchecked("heads");
    let tags = refs.join_unchecked("tags");

    ensure_dir(&refs)?;
    ensure_dir(&heads)?;
    ensure_dir(&tags)?;

    if !reinit {
        let refs = Refs::new(metadata);
        // we can't do let name = match name and make a single refs.new_unborn_branch(name) because
        // when we get back the entry for cfg.get_bytes() we create an &OsStr but the entry gets
        // dropped which makes OsStr invalid
        match name {
            // user provided branch name has the highest precedence
            Some(name) => refs.new_unborn_branch(Some(name))?,
            None => match cfg.get_bytes("init.defaultBranch") {
                Ok(entry) => {
                    let name = os::os_str_from_bytes(entry.as_ref());
                    refs.new_unborn_branch(Some(name))?
                }
                Err(err) if err.is_key_not_found() => refs.new_unborn_branch(None)?,
                Err(err) => return Err(InitError::Config(err)),
            },
        }
    }
    Ok(())
}

// a limitation of the rust compiler on disjoint borrows
//  fn update_config(cfg: &mut ConfigFile, format: &mut RepositoryFormat) {
//      let object_format = format.object_format(); // &ObjectFormat 1st immutable borrow
//
//       if *object_format == ObjectFormat::Sha256
//          || *ref_format == RefFormat::RefTable
//          || ref_storage.has_payload() {
//          *format.version_mut() = FormatVersion::V1 // <- cannot borrow *format as mutable because it is also borrowed as immutable
//      }
//
//      if *object_format == ObjectFormat::Sha256 {...} // 2nd immutable borrow
//  }
//
// This is a case of disjoint borrows. We have an immutable borrow to object_format and a mutable
// borrow to version, but we still get the `cannot borrow..` error. This behavior is caused by the
// functions' signature. The distinction that those borrows are disjoint is not exposed to the caller.
// The compiler simply can't see it. It knows that object_format() returns a reference borrowing
// from self, and version_mut() requires exclusive access to self. It does not inspect the bodies
// to determine which fields they access. With direct field access, the compiler can see that the
// borrows are disjoint and allows it. Check ConfigDoc::remove_section().
fn finalize_format_version(
    cfg: &mut ConfigFile,
    format: &mut RepositoryFormat,
) -> Result<(), InitError> {
    // https://github.com/git/git/blob/fa7f9290efe2bd22dd736689597b474b93798e11/setup.c#L2460
    //
    // at this point we update the format version to v1. When we parsed config if no version was found
    // we call RepositoryFormat::default() which sets it to v0. It is the None branch in init where
    // we also resolve object format and ref storage
    if *format.object_format() == ObjectFormat::Sha256
        || *format.ref_storage().format() == RefFormat::RefTable
        || format.ref_storage().has_payload()
    {
        *format.version_mut() = FormatVersion::V1
    }

    let object_format = format.object_format();
    let ref_storage = format.ref_storage();
    let ref_format = ref_storage.format();

    if *object_format == ObjectFormat::Sha256 {
        cfg.set_all("extensions.objectformat", object_format.name())?;
    }
    if let Some(payload) = ref_storage.payload() {
        let payload = os::os_str_from_bytes(payload);
        cfg.set_all("extensions.refstorage", payload)?;
    } else if *ref_format == RefFormat::RefTable {
        cfg.set_all("extensions.refstorage", ref_format.name())?;
    }

    // sha1 is implicit, we remove any explicit object-format declaration
    if *format.object_format() == ObjectFormat::Sha1 {
        cfg.unset_all("extensions.objectformat")?;
    }
    // same as sha1 above, payload with files format as in `files://<payload>` must be persisted
    if *format.ref_storage().format() == RefFormat::Files && !ref_storage.has_payload() {
        cfg.unset_all("extensions.refstorage")?;
    }

    // TODO: https://github.com/git/git/blob/47ce80527c56f462cb97db4ca8125342204d3783/setup.c#L2500
    //  At this point git checks for `init.defaultSubmodulePathConfig` if set, it enables the
    //  `extensions.submodulePathConfig` extension
    //  https://git-scm.com/docs/git-config#Documentation/git-config.txt-submodulePathConfig
    //  It requires v1 so when we support it we need to check set the version
    cfg.set("core.repositoryformatversion", format.version().as_str())?;

    // there are 2 cases that we still need to check:
    //  - v0 with v1 extensions
    //  - v1 with unknown extensions
    //
    // if the version was absent execute() set it to v0, the default, and finalize updated to v1 if
    // object format was sha256, ref format was reftable or had a payload. In the initial from_confg()
    // call we never look for extensions if the version is absent, so after finalizing the version
    // we can now safely check for version-extensions compatibility.
    let _ = RepositoryFormat::from_config(cfg)?.unwrap();

    Ok(())
}

// https://github.com/git/git/blob/1a3e64c6c4a623626ff0687008732a8e007e2a1c/setup.c#L2675-L2696
// If separate-lit-dir flag is set, we create the pointer file and also migrate an existing
// repo if it is a reinitialization
// Migration vs Reinit
//  The requirements are different. Reinit needs to know if there is existing repository state
//  that initialization preserve, while migration needs to know that if it is a metadata
//  directory that is safe to relocate and lit can work on. Reinit must be more conservative.
//      if .lit/HEAD exists, but /objects and /refs are missing, it might be a damaged repo,
//      a repo that something went wrong in the previous init call. We have to preserve the
//      current state, and try to repair the missing structure. A stricter requirement would
//      not allow us to repair anything, which is the main goal for reinit.
//
// link: absolute path to project/.lit pointer file
fn try_migrate_metadata(
    link: &OsPath,
    metadata_destination: &OsPath,
    cwd: &OsPath,
) -> Result<(), InitError> {
    let Some(entry) = resolve_entry_if_symlink(link)? else {
        // if the pointer points nowhere, nothing to migrate, create the .lit file
        return Ok(litfile::write(link, metadata_destination.as_bytes())?);
    };
    // Note: don't call Repository::discover_at()
    // at this point we have already resolved the destination of the repo from the init arguments.
    // it means we know the destination, but we have not yet created or validated anything
    // what is left is to determine for migration is metadata resolution, structural validation and
    // format validation.
    let paths = repo::validate_metadata_for_migration(entry.into_owned(), cwd)?;
    // rename does not automatically canonicalize either path, `from` is guaranteed to be an absolute
    // canonical path, but `to` is not. `metadata_destination` is absolute by construction in
    // Destination::resolve() but not canonical(still can be a symlink)
    // a NotFound error for the `to` path is accepted by fs::rename()
    // fs::rename() does not replace the symlink's target but the symlink itself
    match fs::canonicalize(metadata_destination) {
        Ok(to) => {
            if paths.metadata_dir().inner() != to {
                fs::rename(paths.metadata_dir(), &to).with_context("rename", Some(to))?
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            fs::rename(paths.metadata_dir(), metadata_destination)
                .with_context("rename", Some(metadata_destination))?
        }
        Err(err) => {
            return Err(InitError::Io(IoError::new(
                "realpath",
                Some(metadata_destination),
                err,
            )));
        }
    }
    litfile::write(link, metadata_destination.as_bytes())?;

    Ok(())
}

fn ensure_dir(path: &OsPath) -> Result<(), InitError> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        // if it already exists, we need to make sure that is actually a directory and not some other
        // entry type
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(path).with_context("lstat", Some(path))?;
            // safe to call create_dir() in existing dirs, it will return without touching them
            if metadata.is_dir() {
                Ok(())
            } else {
                Err(InitError::BadEntry {
                    path: path.clone(),
                    entry: EntryType::from(metadata.file_type()),
                })
            }
        }
        Err(err) => Err(InitError::Io(IoError::new("mkdir", Some(path), err))),
    }
}

// lit --separate-lit-dir /new/metadata project
//
// from Destination's construction link points to `project/.lit`
//
// if `project/.lit` exists we need to inspect it and move the contents to the new location
//  - a directory means we treat it as metadata directory
//  - a file means it must be a litfile such as: `litdir: /old/metadata`, follow the pointer to
//  find the content we want to move
//  - a symlink must be resolved first
//
// Note: we can't make a naive call fs::metadata(link), we first have to resolve it. If link
// is a symlink, metadata() will follow it and return info based on the target BUT later when we
// try to rename, fs::rename() does not follow symlinks, and we will rename the symlink itself not
// the target path, so first we must call canonicalize() and then call metadata on the returned value.
fn resolve_entry_if_symlink(pointer_file: &OsPath) -> Result<Option<Cow<'_, OsPath>>, IoError> {
    match fs::symlink_metadata(pointer_file) {
        Ok(metadata) if metadata.is_symlink() => {
            // returns an Error if dangling and always follows the symlink chain to the final target
            let resolved =
                fs::canonicalize(pointer_file).with_context("realpath", Some(pointer_file))?;
            Ok(Some(Cow::Owned(OsPath::new_unchecked(resolved))))
        }
        Ok(_) => Ok(Some(Cow::Borrowed(pointer_file))),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(IoError::new("lstat", Some(pointer_file), err)),
    }
}

#[derive(Debug)]
pub(super) enum DestinationError {
    Io(IoError),
    OsPath(OsPathError),
    LitWorkTreeWithBare,
}

impl Error for DestinationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            Self::OsPath(source) => Some(source),
            Self::LitWorkTreeWithBare => None,
        }
    }
}

impl fmt::Display for DestinationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(_) => write!(f, "destination resolution I/O failed"),
            Self::OsPath(_) => write!(f, "bad path"),
            Self::LitWorkTreeWithBare => {
                write!(f, "LIT_WORK_TREE not allowed with --bare option")
            }
        }
    }
}

impl From<IoError> for DestinationError {
    fn from(err: IoError) -> Self {
        Self::Io(err)
    }
}

impl From<OsPathError> for DestinationError {
    fn from(err: OsPathError) -> Self {
        Self::OsPath(err)
    }
}

#[derive(Debug)]
pub(super) enum InitError {
    Foo(DestinationError),
    Io(IoError),
    Lockfile(LockfileError),
    // TODO: should this be UnsupportedFileType
    BadEntry { path: OsPath, entry: EntryType },
    Refs(RefError),
    LitFile(LitFileError),
    Migration(RepositoryError),
    Config(ConfigFileError),
    OsPath(OsPathError),
    RepositoryFormat(RepositoryFormatError),
    UnknownRefStorage(RefFormatError),
    UnknownObjectFormat(ObjectFormatError),
    HashMismatch,
    RefStorageMisMatch,
}

impl Error for InitError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Foo(source) => Some(source),
            Self::Io(source) => Some(source),
            Self::Lockfile(source) => Some(source),
            Self::Refs(source) => Some(source),
            Self::LitFile(source) => Some(source),
            Self::Migration(source) => Some(source),
            Self::Config(source) => Some(source),
            Self::OsPath(source) => Some(source),
            Self::RepositoryFormat(source) => Some(source),
            Self::UnknownRefStorage(source) => Some(source),
            Self::UnknownObjectFormat(source) => Some(source),
            // don't try to use _ because if we add a new entry variant we would swallow it without
            // knowing it
            Self::BadEntry { .. } => None,
            Self::HashMismatch => None,
            Self::RefStorageMisMatch => None,
        }
    }
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Foo(_) => write!(f, "could not resolve repository destination"),
            Self::Io(_) => write!(f, "fs operation failed during initialization"),
            Self::Lockfile(_) => write!(f, "could not update repository metadata"),
            Self::BadEntry { path, entry } => {
                write!(
                    f,
                    "{} already exists and is not a {}",
                    path.display(),
                    entry
                )
            }
            Self::Refs(_) => write!(f, "could not initialize repository refs"),
            Self::LitFile(_) => write!(f, "could not process repository pointer"),
            Self::Migration(_) => write!(f, "could not prepare metadata directory"),
            Self::Config(_) => write!(f, "could not configure repository"),
            Self::OsPath(_) => write!(f, "bad path"),
            Self::RepositoryFormat(_) => write!(f, "could not determine repository format"),
            Self::UnknownRefStorage(_) => write!(f, "bad reference storage selection"),
            Self::UnknownObjectFormat(_) => write!(f, "bad object format selection"),
            Self::HashMismatch => write!(
                f,
                "attempted to reinitialize repository with different hash"
            ),
            Self::RefStorageMisMatch => write!(
                f,
                "attempted to reinitialize repository with different storage format"
            ),
        }
    }
}

impl From<DestinationError> for InitError {
    fn from(err: DestinationError) -> Self {
        Self::Foo(err)
    }
}

impl From<IoError> for InitError {
    fn from(err: IoError) -> Self {
        Self::Io(err)
    }
}

impl From<LockfileError> for InitError {
    fn from(err: LockfileError) -> Self {
        Self::Lockfile(err)
    }
}

impl From<RefError> for InitError {
    fn from(err: RefError) -> Self {
        Self::Refs(err)
    }
}

impl From<LitFileError> for InitError {
    fn from(err: LitFileError) -> Self {
        Self::LitFile(err)
    }
}

impl From<RepositoryError> for InitError {
    fn from(err: RepositoryError) -> Self {
        Self::Migration(err)
    }
}

impl From<ConfigFileError> for InitError {
    fn from(err: ConfigFileError) -> Self {
        Self::Config(err)
    }
}

impl From<OsPathError> for InitError {
    fn from(err: OsPathError) -> Self {
        Self::OsPath(err)
    }
}

impl From<RepositoryFormatError> for InitError {
    fn from(err: RepositoryFormatError) -> Self {
        Self::RepositoryFormat(err)
    }
}

impl From<ObjectFormatError> for InitError {
    fn from(err: ObjectFormatError) -> Self {
        Self::UnknownObjectFormat(err)
    }
}

impl From<RefFormatError> for InitError {
    fn from(err: RefFormatError) -> Self {
        Self::UnknownRefStorage(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sealed_test::prelude::*;
    use std::env;
    use std::io::Write;

    // `separate_lit_dir` is set, worktree is cwd, pointer_file points to `cwd/.lit`
    #[test]
    fn l01() {
        let path = OsPath::new_unchecked("/foo/bar");
        let cwd = environment::cwd().unwrap();
        let destination = Destination::resolve(None, false, Some(path.inner())).unwrap();

        assert_eq!(destination.metadata_dir(), &path);
        assert_eq!(destination.worktree_dir(), Some(&cwd));
        assert_eq!(
            destination.pointer_file(),
            Some(&cwd.join_unchecked(".lit"))
        );
    }

    // `separate_lit_dir` is set, positional path is set, pointer_file points to `<positional_path>/.lit`
    #[test]
    fn l02() {
        let path = OsPath::new_unchecked("/projects/lit");
        let separate_lit_dir = OsPath::new_unchecked("/foo/bar");
        let destination =
            Destination::resolve(Some(path.inner()), false, Some(separate_lit_dir.inner()))
                .unwrap();

        assert_eq!(destination.metadata_dir(), &separate_lit_dir);
        assert_eq!(destination.worktree_dir(), Some(&path));
        assert_eq!(
            destination.pointer_file(),
            Some(&path.join_unchecked(".lit"))
        );
    }

    // `LIT_DIR` is set, but ignored since `separate_lit_dir` has higher precedence
    #[sealed_test]
    fn l03() {
        unsafe {
            env::set_var("LIT_DIR", "/home/user/storage");
        }
        let path = OsPath::new_unchecked("/foo/bar");
        let destination = Destination::resolve(None, false, Some(path.inner())).unwrap();
        let cwd = environment::cwd().unwrap();

        assert_eq!(destination.metadata_dir(), &path);
        assert_eq!(destination.worktree_dir(), Some(&cwd));
        assert_eq!(
            destination.pointer_file(),
            Some(&cwd.join_unchecked(".lit"))
        );
    }

    // only positional path is set
    #[test]
    fn l04() {
        let path = OsPath::new_unchecked("/foo");
        let destination = Destination::resolve(Some(path.inner()), false, None).unwrap();

        assert_eq!(destination.metadata_dir(), &(path.join_unchecked(".lit")));
        assert_eq!(
            destination.worktree_dir(),
            Some(&OsPath::new_unchecked("/foo"))
        );
        assert_eq!(destination.pointer_file(), None);
    }

    // positional with bare
    #[test]
    fn l05() {
        let path = OsPath::new_unchecked("/foo");
        let destination = Destination::resolve(Some(path.inner()), true, None).unwrap();

        assert_eq!(destination.metadata_dir(), &path);
        assert_eq!(destination.worktree_dir(), None);
        assert_eq!(destination.pointer_file(), None);
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
        let destination = Destination::resolve(Some(path.inner()), true, None).unwrap();

        assert_eq!(destination.metadata_dir(), &path);
        assert_eq!(destination.worktree_dir(), None);
        assert_eq!(destination.pointer_file(), None);
    }

    // `LIT_DIR` with bare
    #[sealed_test]
    fn l07() {
        unsafe {
            env::set_var("LIT_DIR", "/foo/bar/");
        }
        let lit_dir = environment::var("LIT_DIR").unwrap();
        let destination = Destination::resolve(None, true, None).unwrap();

        assert_eq!(destination.metadata_dir(), &OsPath::new_unchecked(lit_dir));
        assert_eq!(destination.worktree_dir(), None);
        assert_eq!(destination.pointer_file(), None);
    }

    // worktree is cwd
    #[sealed_test]
    fn l08() {
        unsafe {
            env::set_var("LIT_DIR", "/foo/bar/..");
        }
        let cwd = environment::cwd().unwrap();
        let lit_dir = environment::var("LIT_DIR").unwrap();
        let destination = Destination::resolve(None, false, None).unwrap();

        assert_eq!(destination.metadata_dir(), &OsPath::new_unchecked(lit_dir));
        assert_eq!(destination.worktree_dir(), Some(&cwd));
        assert_eq!(destination.pointer_file(), None);
    }

    // `LIT_WORK_TREE` is relative, resolve it against cwd
    #[sealed_test]
    fn l09() {
        unsafe {
            env::set_var("LIT_DIR", "/home/user/metadata");
            env::set_var("LIT_WORK_TREE", "projects/lit");
        }
        let cwd = environment::cwd().unwrap();
        let lit_dir = environment::var("LIT_DIR").unwrap();
        let lit_work_tree = environment::var("LIT_WORK_TREE").unwrap();
        let destination = Destination::resolve(None, false, None).unwrap();

        assert_eq!(destination.metadata_dir(), &OsPath::new_unchecked(lit_dir));
        assert_eq!(
            destination.worktree_dir(),
            Some(&cwd.join_unchecked(lit_work_tree))
        );
        assert_eq!(destination.pointer_file(), None);
    }

    // env exclusive
    #[sealed_test]
    fn l10() {
        unsafe {
            env::set_var("LIT_DIR", "/home/user/storage");
            env::set_var("LIT_WORK_TREE", "/projects/jolt");
        }
        let lit_dir = environment::var("LIT_DIR").unwrap();
        let lit_work_tree = environment::var("LIT_WORK_TREE").unwrap();
        let destination = Destination::resolve(None, false, None).unwrap();

        assert_eq!(destination.metadata_dir(), &OsPath::new_unchecked(lit_dir));
        assert_eq!(
            destination.worktree_dir(),
            Some(&OsPath::new_unchecked(lit_work_tree))
        );
        assert_eq!(destination.pointer_file(), None);
    }

    // bare
    #[test]
    fn l11() {
        let cwd = environment::cwd().unwrap();
        let destination = Destination::resolve(None, true, None).unwrap();

        assert_eq!(destination.metadata_dir(), &cwd);
        assert_eq!(destination.worktree_dir(), None);
        assert_eq!(destination.pointer_file(), None);
    }

    // nothing is set
    #[test]
    fn l12() {
        let cwd = environment::cwd().unwrap();
        let destination = Destination::resolve(None, false, None).unwrap();

        assert_eq!(destination.metadata_dir(), &cwd.join_unchecked(".lit"));
        assert_eq!(destination.worktree_dir(), Some(&cwd));
        assert_eq!(destination.pointer_file(), None);
    }

    // if `LIT_DIR` is not set, `LIT_WORK_TREE` is set and bare is true, `LIT_WORK_TREE` is ignored
    #[sealed_test]
    fn l13() {
        unsafe {
            env::set_var("LIT_WORK_TREE", "/projects/jolt");
        }
        let cwd = environment::cwd().unwrap();
        let destination = Destination::resolve(None, true, None).unwrap();

        assert_eq!(destination.metadata_dir(), &cwd);
        assert_eq!(destination.worktree_dir(), None);
        assert_eq!(destination.pointer_file(), None);
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
        let error = Destination::resolve(None, true, None).unwrap_err();

        assert!(matches!(error, DestinationError::LitWorkTreeWithBare));
    }

    // TODO: when we have a test Environment we should expose methods like `tempfile_with_content()`
    #[test]
    fn hash_algo_mismatch() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(b"[core]\n\trepositoryformatversion = 0")
            .unwrap();
        let mut cfg = ConfigFile::new(OsPath::new_unchecked(file.path())).unwrap();
        let mut init = Init::default();
        init.object_format = Some(ObjectFormat::Sha256);

        let error = init.finalize_repo_format(&mut cfg).unwrap_err();

        assert!(matches!(error, InitError::HashMismatch));
    }

    #[test]
    fn ref_format_mismatch() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(b"[core]\n\trepositoryformatversion = 0")
            .unwrap();
        let mut cfg = ConfigFile::new(OsPath::new_unchecked(file.path())).unwrap();
        let mut init = Init::default();
        init.ref_format = Some(RefFormat::RefTable);

        let error = init.finalize_repo_format(&mut cfg).unwrap_err();

        assert!(matches!(error, InitError::RefStorageMisMatch));
    }

    #[sealed_test]
    fn explicit_object_format_flag_wins() {
        unsafe {
            env::set_var("LIT_DEFAULT_HASH", "sha1");
        }

        let file = NamedTempFile::new().unwrap();
        let mut cfg = ConfigFile::new(OsPath::new_unchecked(file.path())).unwrap();
        let mut init = Init::default();
        init.object_format = Some(ObjectFormat::Sha256);

        let object_format = init.resolve_object_format(&mut cfg).unwrap();

        assert_eq!(object_format, ObjectFormat::Sha256)
    }

    #[sealed_test]
    fn env_var_wins_over_cfg_setting() {
        unsafe {
            env::set_var("LIT_DEFAULT_HASH", "sha256");
        }

        let mut file = NamedTempFile::new().unwrap();
        file.write_all(b"[init]\n\tdefaultObjectFormat = sha1").unwrap();
        let mut cfg = ConfigFile::new(OsPath::new_unchecked(file.path())).unwrap();
        let init = Init::default();

        let object_format = init.resolve_object_format(&mut cfg).unwrap();

        assert_eq!(object_format, ObjectFormat::Sha256)
    }

    #[test]
    fn object_format_from_cfg_setting() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(b"[init]\n\tdefaultObjectFormat = sha256").unwrap();
        let mut cfg = ConfigFile::new(OsPath::new_unchecked(file.path())).unwrap();
        let init = Init::default();

        let object_format = init.resolve_object_format(&mut cfg).unwrap();

        assert_eq!(object_format, ObjectFormat::Sha256)
    }

    #[test]
    fn everything_absent_fallback_to_default() {
        let mut cfg = ConfigFile::new_or_empty(OsPath::new_unchecked("/test")).unwrap();
        let init = Init::default();

        let object_format = init.resolve_object_format(&mut cfg).unwrap();

        assert_eq!(object_format, ObjectFormat::Sha1)
    }
}
