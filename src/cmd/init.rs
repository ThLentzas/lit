use crate::cli;
use crate::repo::config::{ConfigFile, ConfigFileError};
use crate::repo::format::{
    FormatVersion, ObjectFormat, ObjectFormatError, RefFormat, RefStorage, RefStorageError,
    RepositoryFormat, RepositoryFormatError,
};
use crate::repo::litfile::{self, LitFileError};
use crate::repo::os::OsPath;
use crate::repo::{self, Layout, LayoutError, MetadataDirError, os};
use clap::Args;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, FileType};
use std::path::{Path, PathBuf};
use std::{env, fmt};
use std::{io, result};
use tempfile::{NamedTempFile, TempDir};

#[derive(Debug, Args)]
pub(crate) struct Init {
    #[arg(short = 'q', long)]
    quiet: bool,
    #[arg(long)]
    bare: bool,
    // a flag that specifies a path where the .lit files live, which contradicts with --bare because
    // drops .lit dir entirely.
    //
    // Instead of initializing the repository as a directory to either $GIT_DIR or ./.git/, create a
    // text file there containing the path to the actual repository. This file acts as a filesystem-
    // agnostic Git symbolic link to the repository.
    //
    // If this is a reinitialization, the repository will be moved to the specified path.
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
    // TODO: add the remaining flags
}

impl Init {
    // https://github.com/git/git/blob/fa7f9290efe2bd22dd736689597b474b93798e11/setup.c#L2841-L2945
    // TODO: we need to see if init sets GIT_DIR env var
    pub(super) fn execute(&self) -> Result<()> {
        // 1. We need to resolve arguments and env vars for the location of the metadata dir.
        // resolve() sets rules for a deterministic layout.
        let layout = Layout::resolve(
            // self.path.as_ref().map(PathBuf::as_ref)
            self.path.as_deref(),
            self.bare,
            self.separate_lit_dir.as_deref(),
        )?;

        // 2. Create the positional/root directory
        fs::create_dir_all(layout.root())
            .map_err(|err| InitError::from_io_error(layout.root(), err))?;

        // 3. If separate-lit-dir flag is set, we create the pointer file and also migrate an
        // existing repo if it is a reinitialization
        if let Some(link) = layout.separate_link() {
            try_migrate_metadata(link, &layout.metadata())?;
        }

        let cfg_path = layout.metadata().join("config");
        // https://github.com/git/git/blob/3cb9185f65410273787f74333cc027d2ea5daada/setup.c#L751
        let mut cfg = match ConfigFile::new(&cfg_path) {
            Ok(cfg) => Some(cfg),
            Err(err) if err.is_io_not_found() => None,
            Err(err) => {
                return Err(InitError::Config {
                    path: cfg_path,
                    source: err,
                });
            }
        };

        let repo_format = match cfg.as_ref() {
            Some(cfg) => RepositoryFormat::from_config(cfg)?,
            None => None,
        };
        // https://github.com/git/git/blob/b8242b093d9e941a34460d715e3ce616a34ac3fe/setup.c#L2765
        let repo_format = match repo_format {
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
                let mut repo_format = RepositoryFormat::default();
                *repo_format.object_format_mut() =
                    resolve_object_format(self.object_format, cfg.as_ref())?;
                *repo_format.ref_storage_mut() =
                    resolve_ref_storage(self.ref_format, cfg.as_ref())?;
                repo_format
            }
        };
        let mut cfg = cfg.unwrap_or_else(ConfigFile::empty);

        // TODO: next is apply_repository_format()
        //  https://github.com/git/git/blob/fa7f9290efe2bd22dd736689597b474b93798e11/setup.c#L2884
        //  apply_repo_format() checks at this point for GIT_SHALLOW_FILE, need to revisit when we
        //  support shallow repositories(contains truncated history)
        ensure_dir(layout.metadata())?;
        //  TODO: next is to copy any templates
        //   https://github.com/git/git/blob/fa7f9290efe2bd22dd736689597b474b93798e11/setup.c#L2587
        let reinit = is_reinit(layout.metadata())?;
        // When a tracked entry's mode differs from what is recorded, Git must distinguish if the
        // change was actually made by the user, or it is a false positive because the environment
        // does not support Unix permissions(Windows, a fs mounted without permissions)
        let trust_filemode = trust_filemode(layout.metadata(), reinit)?;
        if trust_filemode {
            cfg.set_all("core.filemode".as_ref(), "true".as_ref())?;
        } else {
            cfg.set_all("core.filemode".as_ref(), "false".as_ref())?;
        }
        // -`lit init project` and then `lit init --bare project` there is no confusion on what
        // happens. Different metadata directories. For non-bare the metadata entries are created in
        // project/.lit, then directly inside project, so project/HEAD, project/config etc.
        // -`LIT_DIR = repo/meta` and then `lit init` with or without --bare results metadata entries
        // end up in the same directory with different worktree state.
        //
        // we honor the invariant that we set in resolve() that explicit --bare flag wins. Calling
        // --bare on an existing repo will set the `core.bare = true` and delete all `core.worktree`
        // instances.
        if layout.is_bare() {
            cfg.unset_all("core.worktree".as_ref())?;
            cfg.set_all("core.bare".as_ref(), "true".as_ref())?;
        } else {
            cfg.set_all("core.bare".as_ref(), "false".as_ref())?;
            // https://git-scm.com/docs/git-config#Documentation/git-config.txt-corelogAllRefUpdates
            // From the docs: `This value is true by default in a repository that has a working
            //  directory associated with it, and false by default in a bare repository.`
            // The term `working directory` translates to Layout's worktree, not the cwd.
            //
            // https://github.com/git/git/blob/3699d22b59a6ea467ce13edb81b6bdea0398c803/setup.c#L2628-L2641
            // Did some manual testing against Git and if the repo is bare it ignores the value if
            // present, if absent also performs no action, absence = implicit false`. As seen from
            // the code too, it completes ignores it for bare repos.
            // https://github.com/git/git/blob/3699d22b59a6ea467ce13edb81b6bdea0398c803/setup.c#L2636-L2637
            // as of 2.55 Git rejects a bad boolean value, respects one if present and defaults to
            // true if absent
            match cfg.get_bool("core.logallrefupdates".as_ref()) {
                Ok(_) => {}
                Err(err) if err.is_not_found() => {
                    cfg.set("core.logallrefupdates".as_ref(), "true".as_ref())?;
                }
                Err(err) => {
                    return Err(InitError::Config {
                        path: cfg_path,
                        source: err,
                    });
                }
            }
            if layout.needs_worktree_config() {
                cfg.set_all("core.worktree".as_ref(), "false".as_ref())?;
            }
        }

        // I couldn't understand why those 2 cfg settings are checked only for new repos and left
        // unchecked for existing ones.
        // https://github.com/git/git/blob/3699d22b59a6ea467ce13edb81b6bdea0398c803/setup.c#L2643-L2660
        if !reinit {
            // absence -> implicitly true
            // https://github.com/git/git/blob/3699d22b59a6ea467ce13edb81b6bdea0398c803/setup.c#L2646-L2653
            // only writes false in the else block
            if !support_symlinks(layout.metadata()) {
                cfg.set_all("core.symlinks".as_ref(), "false".as_ref())?;
            }
            // absence -> implicitly false
            if !is_case_sensitive_fs(layout.metadata()) {
                cfg.set_all("core.ignorecase".as_ref(), "true".as_ref())?;

            }
        }
        setup_object_db(layout.metadata())?;


        // TODO: top priorities is for ConfigFileError to report the error path because it knows where
        //  it read config from and the IoError wrapper
        // Migration vs Reinit
        //  The requirements are different. Reinit needs to know if there is existing repository state
        //  that initialization preserve, while migration needs to know that if it is a metadata
        //  directory that is safe to relocate and lit can work on. Reinit must be more conservative.
        //      if .lit/HEAD exists, but /objects and /refs are missing, it might be a damaged repo,
        //      a repo that something went wrong in the previous init call. We have to preserve the
        //      current state, and try to repair the missing structure. A stricter requirement would
        //      not allow us to repair anything, which is the main goal for reinit.
        Ok(())
    }
}

// https://github.com/git/git/blob/fa7f9290efe2bd22dd736689597b474b93798e11/setup.c#L2515-L2525
// git checks if HEAD is accessible or a symlink(dangling is fine)
// we never check if the head_path points to an actual HEAD file, all we care about at this point is
// if something exists at that path, because overwriting can be destructive and lead to unexpected
// behavior.
fn is_reinit(path: &Path) -> Result<bool> {
    let head = path.join("HEAD");
    match fs::symlink_metadata(&head) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(InitError::Io {
            path: path.to_path_buf(),
            source: err,
        }),
    }
}

fn update_config(cfg: &mut ConfigFile, format: &mut RepositoryFormat) -> Result<()> {
    finalize_format_version(cfg, format)?;
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

// Git probes the filesystem because it needs to know whether a reported executable-bit difference
// represents a real change or merely a limitation of the filesystem interface.
// From the docs:
//  `Some filesystems lose the executable bit when a file that is marked as executable is
//   checked out, or checks out a non-executable file with executable bit on. git probe the
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
fn trust_filemode(probe_path: &OsPath) -> io::Result<bool> {
    // we create a temporary file inside the metadata directory for the filemode probe. We don't try
    // to test it against an existing file.
    let tempfile = NamedTempFile::new_in(probe_path)?;
    // TODO: look at the builder and we need to set permissions upon creation?
    let path = OsPath::new_unchecked(tempfile.path());
    // TODO: review
    //  Git considers more to detect trust: https://github.com/git/git/blob/3699d22b59a6ea467ce13edb81b6bdea0398c803/setup.c#L2623
    //  it maps to os::probe_filemode(&path)? && (reinit || !os::is_executable(&path)?);
    //  I don't know what it does exactly or why it needs it, maybe because it uses the cfg file for
    //  the test? I am not sure so I drop it for now
    let trust = os::probe_filemode(&path)?;
    // the explicit call to close is to report any potential errors when closing the file
    tempfile.close()?;

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
fn support_symlinks(path: &OsPath) -> io::Result<bool> {
    let temp_dir = TempDir::new_in(path)?;
    let parent = OsPath::new_unchecked(temp_dir.path());
    let link = parent.join_unchecked("link");
    let support = os::probe_symlink(&link)?;

    temp_dir.close()?;

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
fn is_case_sensitive_fs(path: &OsPath) -> io::Result<bool> {
    let prefix = cli::generate(8);
    let filename = format!("{prefix}_test");
    File::create(path.join_unchecked(&filename))?;
    let probe_filename = format!("{prefix}_TEst");

    let insensitive = match fs::symlink_metadata(path.join_unchecked(probe_filename)) {
        Ok(_) => false,
        Err(err) if err.kind() == io::ErrorKind::NotFound => true,
        Err(err) => return Err(err),
    };
    fs::remove_file(&filename)?;

    Ok(insensitive)
}

// TODO: revisit when we support packfiles and worktree
// https://github.com/git/git/blob/f0ef1b96a076d08dc972a8d2cb0d1cfd60931eb6/setup.c#L2666-L2689
fn setup_object_db(metadata: &OsPath) -> Result<()> {
    // the env var has the higher precedence than the <common_directory>/objects where <common_directory>
    // in our case is the layout.metadata(). This is until we support linked worktree
    // Read: https://git-scm.com/book/en/v2/Git-Internals-Environment-Variables
    //       https://git-scm.com/docs/git-init
    // TODO: this is the same logic on validate_metadata_dir() it resolves the object dir, should be
    //  extracted and reused?
    let objects = match env::var_os("LIT_OBJECT_DIRECTORY") {
        Some(var) => {
            let cwd = env::current_dir()?;
            let cwd = OsPath::new_unchecked(cwd);
            cwd.join(var)?
        }
        None => metadata.join_unchecked("objects"),
    };
    let info = objects.join_unchecked("info");
    let pack = objects.join_unchecked("pack");
    ensure_dir(&objects)?;
    ensure_dir(&info)?;
    ensure_dir(&pack)
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
) -> result::Result<(), ConfigFileError> {
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
        cfg.set_all(
            "extensions.objectformat".as_ref(),
            object_format.name().as_ref(),
        )?;
    }
    if let Some(payload) = ref_storage.payload() {
        let payload = unsafe { OsStr::from_encoded_bytes_unchecked(payload) };
        cfg.set_all("extensions.refstorage".as_ref(), payload)?;
    } else if *ref_format == RefFormat::RefTable {
        cfg.set_all("extensions.refstorage".as_ref(), ref_format.name().as_ref())?;
    }

    // sha1 is implicit, we remove any explicit object-format declaration
    if *format.object_format() == ObjectFormat::Sha1 {
        cfg.unset_all("extensions.objectformat".as_ref())?;
    }
    // same as sha1 above, payload with files format as in `files://<payload>` must be persisted
    if *format.ref_storage().format() == RefFormat::Files && !ref_storage.has_payload() {
        cfg.unset_all("extensions.refstorage".as_ref())?;
    }

    // TODO: https://github.com/git/git/blob/47ce80527c56f462cb97db4ca8125342204d3783/setup.c#L2500
    //  At this point git checks for `init.defaultSubmodulePathConfig` if set, it enables the
    //  `extensions.submodulePathConfig` extension
    //  https://git-scm.com/docs/git-config#Documentation/git-config.txt-submodulePathConfig
    //  It requires v1 so when we support it we need to check set the version

    cfg.set(
        "core.repositoryformatversion".as_ref(),
        format.version().as_str().as_ref(),
    )?;

    Ok(())
}

// https://github.com/git/git/blob/1a3e64c6c4a623626ff0687008732a8e007e2a1c/setup.c#L2675-L2696
// we convert an embedded repo layout into a separate one
// TODO: explain rename and file descriptors and the unavoidable TOCTOU race conditions when working
//  with paths.
fn try_migrate_metadata(lit_entry_path: &Path, destination_metadata_dir: &Path) -> Result<()> {
    // this is tricky
    //
    // from is the path value of the placement field that we set in Layout::resolve()
    // we don't know if this path exists yet
    //
    // lit --separate-lit-dir /new/metadata project
    //
    // if it is a fresh repo, project/.lit will be a file with a pointer to /new/metadata
    // if it is a reinit, we have to inspect lit_link, because now we have to move the contents
    // to the new location.
    //  - a directory means it is the existing metadata directory
    //  - a file means we have to check if it contains an existing pointer such as: litdir: /old/metadata
    //  and follow the pointer to find the content we want to move
    //
    // Note: we can't make a naive call fs::metadata(from), we first have to resolve it. If from
    // is a symlink, metadata() will follow it and return info based on the target BUT later when we
    // try to rename(from, to), fs::rename() does not follow symlinks, and we will rename
    // the symlink itself not the target path, so first we must call canonicalize() and then call
    // metadata to the returned value.
    let resolved_entry_path = resolve_lit_entry(lit_entry_path)
        .map_err(|err| InitError::from_io_error(lit_entry_path, err))?;
    let Some(current_metadata_dir) = resolved_entry_path else {
        // nothing to migrate, create the .lit file
        return litfile::write(lit_entry_path, destination_metadata_dir).map_err(|err| {
            InitError::LitFile {
                path: lit_entry_path.to_path_buf(),
                err,
            }
        });
    };

    // TODO: clean up the comments and ask Astra if there any issues when someone
    //  tries to read Index paths on Windows, since they are just byte sequences without NUL
    match fs::metadata(&current_metadata_dir) {
        Ok(metadata) if metadata.is_dir() => {
            repo::validate_metadata_dir(&current_metadata_dir)?;
            if current_metadata_dir != destination_metadata_dir {
                fs::rename(&current_metadata_dir, destination_metadata_dir)
                    .map_err(|err| InitError::from_io_error(&current_metadata_dir, err))?;
            }
        }
        Ok(metadata) if metadata.is_file() => {
            let mut file = File::open(lit_entry_path)
                .map_err(|err| InitError::from_io_error(lit_entry_path, err))?;
            // There is TOCTOU race condition between the first fs::metadata() call and
            // File::open() which we can actually handle by calling metadata() again after
            // acquiring the file descriptor for the duration of the operation
            let metadata = file
                .metadata()
                .map_err(|err| InitError::from_io_error(lit_entry_path, err))?;
            if !metadata.is_file() {
                return Err(InitError::BadEntry {
                    path: lit_entry_path.to_path_buf(),
                    entry: EntryType::from(metadata.file_type()),
                });
            }
            // must be a litfile
            let path = litfile::read(&mut file).map_err(|err| InitError::LitFile {
                path: lit_entry_path.to_path_buf(),
                err,
            })?;
            // must point to a valid lit repo
            repo::validate_metadata_dir(&path)?;
            if path != destination_metadata_dir {
                fs::rename(&path, destination_metadata_dir)
                    .map_err(|err| InitError::from_io_error(&path, err))?;
            }
        }
        Ok(metadata) => {
            return Err(InitError::BadEntry {
                path: current_metadata_dir,
                entry: EntryType::from(metadata.file_type()),
            });
        }
        Err(err) => return Err(InitError::from_io_error(&current_metadata_dir, err)),
    }

    litfile::write(lit_entry_path, destination_metadata_dir).map_err(|err| InitError::LitFile {
        path: lit_entry_path.to_path_buf(),
        err,
    })?;

    Ok(())
}

// https://github.com/git/git/blob/b8242b093d9e941a34460d715e3ce616a34ac3fe/setup.c#L2787-L2801
// precedence: flag > env var > config value(only in .litconfig or system, the local config does not
// exist yet for a new repo)
fn resolve_object_format(
    flag: Option<ObjectFormat>,
    cfg: Option<&ConfigFile>,
) -> Result<ObjectFormat> {
    if let Some(format) = flag {
        return Ok(format);
    }

    if let Some(hash) = env::var_os("LIT_DEFAULT_HASH") {
        let hash = hash.as_encoded_bytes();
        return ObjectFormat::try_from(hash).map_err(InitError::UnknownObjectFormat);
    }

    if let Some(cfg) = cfg {
        // TODO: cfg needs to look for this in the global config not local?
        return match cfg.get_str("init.defaultObjectFormat".as_ref()) {
            Ok(hash) => {
                ObjectFormat::try_from(hash.as_bytes()).map_err(InitError::UnknownObjectFormat)
            }
            Err(err) if err.is_io_not_found() => Ok(ObjectFormat::default()),
            Err(err) => Err(InitError::Config {
                path: Path::new("").to_path_buf(),
                source: err,
            }),
        };
    }
    Ok(ObjectFormat::default())
}

fn resolve_ref_storage(flag: Option<RefFormat>, cfg: Option<&ConfigFile>) -> Result<RefStorage> {
    let mut ref_storage = RefStorage::default();

    // https://github.com/git/git/blob/b8242b093d9e941a34460d715e3ce616a34ac3fe/setup.c#L2824-L2838
    // https://github.com/git/git/blob/b8242b093d9e941a34460d715e3ce616a34ac3fe/environment.h#L46
    // when I wrote this, I couldn't find any reference for that env var in the docs
    // In the src code, linked above, the branch that checks this env var is a separate one, disconnected
    // from the above logic. It has the highest precedence, it overwrites any previously set value.
    if let Some(ref_backend) = env::var_os("LIT_REFERENCE_BACKEND") {
        let ref_backend = ref_backend.as_encoded_bytes();
        ref_storage = RefStorage::try_from(ref_backend)?;

        return Ok(ref_storage);
    }

    // https://github.com/git/git/blob/b8242b093d9e941a34460d715e3ce616a34ac3fe/setup.c#L2803-L2821
    if let Some(ref_format) = flag {
        *ref_storage.format_mut() = ref_format;
    } else if let Some(ref_format) = env::var_os("LIT_DEFAULT_REF_FORMAT") {
        let ref_format = ref_format.as_encoded_bytes();
        *ref_storage.format_mut() = RefFormat::try_from(ref_format)
            .map_err(|err| InitError::UnknownRefStorage(RefStorageError(err)))?;
    } else {
        if let Some(cfg) = cfg {
            match cfg.get_str("init.defaultRefFormat".as_ref()) {
                Ok(ref_format) => {
                    *ref_storage.format_mut() = RefFormat::try_from(ref_format.as_ref().as_bytes())
                        .map_err(|err| InitError::UnknownRefStorage(RefStorageError(err)))?;
                }
                Err(err)
                    if err
                        .io_error_kind()
                        // if the config value is not set, storage is already default, we don't have to
                        // perform any action
                        .is_some_and(|kind| kind == io::ErrorKind::NotFound) => {}
                Err(err) => {
                    return Err(InitError::Config {
                        path: Path::new("").to_path_buf(),
                        source: err,
                    });
                }
            }
        }
    }
    Ok(ref_storage)
}

fn ensure_dir(path: &OsPath) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        // if it already exists, we need to make sure that is actually a directory and not some other
        // entry type
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            let metadata =
                fs::symlink_metadata(path).map_err(|err| InitError::from_io_error(path, err))?;
            // safe to call create_dir() in existing dirs, it will return without touching them
            // TODO: this is TOCTOU case
            if metadata.file_type().is_dir() {
                Ok(())
            } else {
                Err(InitError::BadEntry {
                    path: path.to_path_buf(),
                    entry: EntryType::from(metadata.file_type()),
                })
            }
        }
        Err(err) => Err(InitError::from_io_error(path, err)),
    }
}

fn resolve_lit_entry(path: &Path) -> io::Result<Option<PathBuf>> {
    let path = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Some(fs::canonicalize(path)?),
        Ok(_) => Some(path.to_path_buf()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => return Err(err),
    };

    Ok(path)
}

pub(crate) type Result<T> = result::Result<T, InitError>;

#[derive(Debug)]
pub(super) enum InitError {
    CurrentDirUnavailable(io::Error),
    Io {
        path: PathBuf,
        source: io::Error,
    },
    // TODO: should this be UnsupportedFileType
    BadEntry {
        path: PathBuf,
        entry: EntryType,
    },
    LitFile {
        path: PathBuf,
        err: LitFileError,
    },
    Layout(LayoutError),
    MetadataDir(MetadataDirError),
    Config {
        path: PathBuf,
        source: ConfigFileError,
    },
    RepositoryFormat(RepositoryFormatError),
    UnknownRefStorage(RefStorageError),
    UnknownObjectFormat(ObjectFormatError),
    HashMismatch,
    RefStorageMisMatch,
}

impl InitError {
    fn from_io_error(path: &Path, err: io::Error) -> Self {
        InitError::Io {
            path: path.to_path_buf(),
            source: err,
        }
    }
}

impl Error for InitError {}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InitError::CurrentDirUnavailable(err) => {
                write!(f, "could not determine current directory: {err}")
            }
            InitError::Io { path, source } => {
                write!(f, "{}: {}", path.display(), source)
            }
            InitError::BadEntry { path, entry } => {
                write!(
                    f,
                    "{} already exists and is not a {}",
                    path.display(),
                    entry
                )
            }
            InitError::LitFile { path, err } => {
                write!(f, "{}: {}", path.display(), err)
            }
            InitError::MetadataDir(source) => write!(f, "{source}"),
            InitError::Layout(source) => write!(f, "{source}"),
            InitError::Config { path, source } => write!(f, "{}: {}", path.display(), source),
            InitError::RepositoryFormat(source) => write!(f, "{source}"),
            InitError::UnknownRefStorage(source) => write!(f, "{source}"),
            InitError::UnknownObjectFormat(source) => write!(f, "{source}"),
            InitError::HashMismatch => write!(
                f,
                "attempted to reinitialize repository with different hash"
            ),
            InitError::RefStorageMisMatch => write!(
                f,
                "attempted to reinitialize repository with different storage format"
            ),
        }
    }
}

impl From<LayoutError> for InitError {
    fn from(err: LayoutError) -> Self {
        Self::Layout(err)
    }
}

impl From<MetadataDirError> for InitError {
    fn from(err: MetadataDirError) -> Self {
        Self::MetadataDir(err)
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

impl From<RefStorageError> for InitError {
    fn from(err: RefStorageError) -> Self {
        Self::UnknownRefStorage(err)
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
            EntryType::File => "file",
            EntryType::Directory => "directory",
            EntryType::Symlink => "symlink",
            EntryType::Other => "other",
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

#[cfg(test)]
mod tests {
    // TODO: fix
    // use crate::cmd::init::{EntryType, Init, InitError};
    // use std::fs;
    // use std::path::{Path, PathBuf};
    // use tempfile;
    //
    // struct TempDir {
    //     // in both with_missing_root() and with_existing_root() when I wasn't keeping temp_dir alive
    //     // drop() was called and the temp_dir path was deleted. The test init_on_a_missing_dir()
    //     // will pass but for the wrong reason, when we pass the root to Init, it calls create_dir_all()
    //     // and it would create the path again and all assertions would pass. We only want Init to create
    //     // root in {temp_dir}/root and then .lit.
    //     // The test init_on_an_existing_dir() that called with_existing_root() would fail.
    //     // with_existing_root() creates {temp_dir}/root. when drop() was called, it would delete
    //     // temp_dir and all its entries including the root. In with_existing_root() we would call
    //     // let dir_entry = temp_dir.root.join("foo"); // a parent path that no longer exists
    //     // so fs::write(&dir_entry, b":)").unwrap(); would err
    //     //
    //     // This is why we need to keep the temp_dir alive.
    //     _temp_dir: tempfile::TempDir,
    //     root: PathBuf,
    //     lit: PathBuf,
    // }
    //
    // impl TempDir {
    //     fn with_missing_root(root: &Path) -> Self {
    //         let temp_dir = tempfile::tempdir().unwrap();
    //         let root = temp_dir.path().join(root);
    //         let lit = root.join(".lit");
    //
    //         Self {
    //             _temp_dir: temp_dir,
    //             root,
    //             lit,
    //         }
    //     }
    //
    //     fn with_existing_root(root: &Path) -> Self {
    //         let temp_dir = tempfile::tempdir().unwrap();
    //         let root = temp_dir.path().join(root);
    //         fs::create_dir(&root).unwrap();
    //
    //         let lit = root.join(".lit");
    //
    //         Self {
    //             _temp_dir: temp_dir,
    //             root,
    //             lit,
    //         }
    //     }
    //
    //     fn objects(&self) -> PathBuf {
    //         self.lit.join("objects")
    //     }
    //
    //     fn refs(&self) -> PathBuf {
    //         self.lit.join("refs")
    //     }
    //
    //     fn config(&self) -> PathBuf {
    //         self.lit.join("config")
    //     }
    // }
    //
    // #[test]
    // fn init_on_a_missing_dir() {
    //     let temp_dir = TempDir::with_missing_root("test".as_ref());
    //     // root does not exist and init should create {temp_dir}/root/.lit
    //     let init = Init {
    //         path: Some(temp_dir.root.clone()),
    //         git_dir: None,
    //     };
    //
    //     init.execute().unwrap();
    //
    //     assert!(temp_dir.root.is_dir());
    //     assert!(temp_dir.lit.is_dir());
    //     assert!(temp_dir.objects().is_dir());
    //     assert!(temp_dir.refs().is_dir());
    //     assert!(temp_dir.config().is_file());
    // }
    //
    // // we assert that calling init on an existing directory does not alter its structure or any of the
    // // contents of its entries
    // #[test]
    // fn init_on_an_existing_dir() {
    //     let temp_dir = TempDir::with_existing_root("test".as_ref());
    //     let init = Init {
    //         path: Some(temp_dir.root.clone()),
    //         git_dir: None,
    //     };
    //     // entry of the existing dir
    //     let dir_entry = temp_dir.root.join("foo");
    //     fs::write(&dir_entry, b":)").unwrap();
    //
    //     init.execute().unwrap();
    //
    //     let contents = fs::read(&dir_entry).unwrap();
    //
    //     assert!(temp_dir.objects().is_dir());
    //     assert!(temp_dir.refs().is_dir());
    //     assert!(temp_dir.config().is_file());
    //     assert!(dir_entry.is_file());
    //     assert_eq!(contents, ":)".as_bytes().to_vec())
    // }
    // // TODO: write an IT tests where we call execute twice and assert on the print logic
    //
    // // calling init in an existing repo should not touch any of the files of the directory no matter
    // // if they are owned by lit or not
    // #[test]
    // fn reinit_preserves_existing_repo_files() {
    //     let temp_dir = TempDir::with_existing_root("test".as_ref());
    //     let init = Init {
    //         path: Some(temp_dir.root.clone()),
    //         git_dir: None,
    //     };
    //
    //     init.execute().unwrap();
    //
    //     let config = temp_dir.config();
    //     let object_dir = temp_dir.objects().join("ef");
    //     let blob = object_dir.join("b1e0e54a68d5928831b3e3749ec764b346c987");
    //     let head = temp_dir.refs().join("HEAD");
    //
    //     // init creates objects/, but not the two-character object subdirectory.
    //     fs::create_dir_all(&object_dir).unwrap();
    //     fs::write(
    //         &config,
    //         b"[user]\n    name = Alex Morgan\n    email = alex.morgan@example.com\n",
    //     )
    //     .unwrap();
    //     // obviously this is not the actual content of the blob, it is zlibed compress, and we could
    //     // easily use random data
    //     fs::write(&blob, b"blob 6\0hello\n").unwrap();
    //     fs::write(&head, b"821bf054e7f1fbc9a920609db2b5b6e256382b4e").unwrap();
    //
    //     let config_before = fs::read(&config).unwrap();
    //     let blob_before = fs::read(&blob).unwrap();
    //     let head_before = fs::read(&head).unwrap();
    //
    //     init.execute().unwrap();
    //
    //     // reinit should not touch any of the content of the existing files
    //     assert_eq!(fs::read(&config).unwrap(), config_before);
    //     assert_eq!(fs::read(&blob).unwrap(), blob_before);
    //     assert_eq!(fs::read(&head).unwrap(), head_before);
    // }
    //
    // #[test]
    // fn reinit_recreates_deleted_repo_files() {
    //     let temp_dir = TempDir::with_existing_root("test".as_ref());
    //     let init = Init {
    //         path: Some(temp_dir.root.clone()),
    //         git_dir: None,
    //     };
    //
    //     init.execute().unwrap();
    //
    //     let config = temp_dir.config();
    //     let objects = temp_dir.objects();
    //     let refs = temp_dir.refs();
    //     fs::remove_dir_all(&objects).unwrap();
    //     fs::remove_dir_all(&refs).unwrap();
    //     fs::remove_file(&config).unwrap();
    //
    //     init.execute().unwrap();
    //
    //     assert!(objects.is_dir());
    //     assert!(refs.is_dir());
    //     assert!(config.is_file());
    // }
    //
    // // this is true for other dirs like objects and refs, they are all created by the same method
    // // ensure_dir()
    // #[test]
    // fn init_fails_when_lit_exists_but_is_not_a_directory() {
    //     let temp_dir = TempDir::with_existing_root("test".as_ref());
    //     fs::write(temp_dir.root.join(".lit"), ":/").unwrap();
    //
    //     let init = Init {
    //         path: Some(temp_dir.root.clone()),
    //         git_dir: None,
    //     };
    //
    //     match init.execute().unwrap_err() {
    //         InitError::BadEntry { path, entry } => {
    //             assert_eq!(path, temp_dir.root.join(".lit"));
    //             assert_eq!(entry, EntryType::File);
    //         }
    //         err => panic!("expected InitError::BadEntry, got {err:?}"),
    //     }
    // }
    //
    // // this is true for all files created by init
    // #[test]
    // fn init_fails_when_config_exists_but_is_not_a_file() {
    //     let temp_dir = TempDir::with_existing_root("test".as_ref());
    //     fs::create_dir_all(temp_dir.lit.join("config")).unwrap();
    //
    //     let init = Init {
    //         path: Some(temp_dir.root.clone()),
    //         git_dir: None,
    //     };
    //
    //     match init.execute().unwrap_err() {
    //         InitError::BadEntry { path, entry } => {
    //             assert_eq!(path, temp_dir.config());
    //             assert_eq!(entry, EntryType::Directory);
    //         }
    //         err => panic!("expected InitError::BadEntry, got {err:?}"),
    //     }
    // }
}
