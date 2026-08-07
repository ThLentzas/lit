use std::env::Args;
use std::error::Error;
use std::fs::{FileType, OpenOptions};
use std::iter::{Peekable, Skip};
use std::path::{Path, PathBuf};
use std::{env, fmt, fs, io};

pub(crate) struct Init {
    pub(crate) path: PathBuf,
}

impl Init {
    // we want to set the path to always be absolute,
    // TODO: Read the Chapter for init on why
    // join() if the second path is absolute, it replaces the first entirely, else it gets appended.
    // lit init: creates .lit in the cwd
    // lit init /home/thanos/projects/1: works fine it is already an absolute path
    pub fn new(args: &mut Peekable<Skip<Args>>) -> Self {
        let cwd = env::current_dir().unwrap();
        let path = match args.next() {
            Some(p) => cwd.join(p),
            None => cwd,
        };

        Self { path }
    }

    // when calling init in an existing repo Git does not overwrite any of the existing files, it
    // will try to create any that are missing.
    // TODO: create a Struct for the tests , and have 1 method for each path
    pub(super) fn execute(&self) -> Result<(), InitError> {
        fs::create_dir_all(&self.path).map_err(|err| InitError::from_io_error(&self.path, err))?;
        let lit = self.path.join(".lit");
        // The code below is a naive wrong impl for detecting an existing lit repo. create_dir() will
        // fail when another entry exists with the same name, not necessarily a directory, could be
        // a symlink, regular file etc. Printing the reinit message in such case is misleading. Only
        // if the existing entry is a directory we can return true for reinit. This is what ensure_dir()
        // handles.
        //
        // let reinit = match fs::create_dir(&lit_dir) {
        //     Ok(_) => false,
        //     Err(err) if err.kind() == io::ErrorKind::AlreadyExists => true,
        //     Err(err) => return Err(InitError::from_io_error(&lit_dir, err)),
        // };
        //
        // If .lit exists as a directory we always print the reinit message without checking if it
        // contains of the expected entries. It could just be an empty where everything was deleted,
        // or just deleted the .lit related entries. It does not matter in either case.
        let reinit = match fs::create_dir(&lit) {
            Ok(_) => false,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                ensure_dir(&lit).map(|_| true)?
            }
            Err(err) => return Err(InitError::from_io_error(&lit, err)),
        };

        let objects = lit.join("objects");
        let refs = lit.join("refs");
        let config = lit.join("config");
        ensure_dir(&objects)?;
        ensure_dir(&refs)?;
        // on why a naive File::create() does not work read Lockfile::acquire() exactly the same case
        // File::create(&config).map_err(|err| ....)?;
        //
        // same case for creating ensure_dir() but for files, read comment above.
        // match OpenOptions::new()
        //     .write(true)
        //     .create_new(true)
        //     .open(&config)
        // {
        //     Ok(_) => {}
        //     Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
        //     Err(err) => return Err(InitError::from_io_error(&config, err)),
        // }
        ensure_file(&config)?;
        // TODO: At this point we need to some setup for template files
        // TODO: one such case is to create the config file with the core section [core]
        if reinit {
            println!("Reinitialized existing Lit repository in {}", lit.display());
        } else {
            println!("Initialized empty Lit repository in {}", lit.display());
        }
        Ok(())
    }
}

fn ensure_dir(path: &Path) -> Result<(), InitError> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            let metadata =
                fs::symlink_metadata(path).map_err(|err| InitError::from_io_error(path, err))?;
            // safe to call create_dir() in existing dirs, it will return without touching them
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

fn ensure_file(path: &Path) -> Result<(), InitError> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(_) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            let metadata =
                fs::symlink_metadata(path).map_err(|err| InitError::from_io_error(path, err))?;
            if metadata.file_type().is_file() {
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

#[derive(Debug)]
pub(super) enum InitError {
    Io { path: PathBuf, source: io::Error },
    BadEntry { path: PathBuf, entry: EntryType },
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
            InitError::Io { path, source } => {
                write!(f, "{}: {}", path.display(), source)
            }
            InitError::BadEntry { path, entry } => {
                write!(
                    f,
                    "{} already exists and is not a {}",
                    path.display(),
                    entry.to_string()
                )
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EntryType {
    File,
    Directory,
    Symlink,
    Other,
}

impl EntryType {
    fn to_string(&self) -> String {
        match self {
            EntryType::File => "file".to_owned(),
            EntryType::Directory => "directory".to_owned(),
            EntryType::Symlink => "symlink".to_owned(),
            EntryType::Other => "other".to_owned(),
        }
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
    use crate::cmd::init::{EntryType, Init, InitError};
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile;

    struct TempDir {
        // in both with_missing_root() and with_existing_root() when I wasn't keeping temp_dir alive
        // drop() was called and the temp_dir path was deleted. The test init_on_a_missing_dir()
        // will pass but for the wrong reason, when we pass the root to Init, it calls create_dir_all()
        // and it would create the path again and all assertions would pass. We only want Init to create
        // root in {temp_dir}/root and then .lit.
        // The test init_on_an_existing_dir() that called with_existing_root() would fail.
        // with_existing_root() creates {temp_dir}/root. when drop() was called, it would delete
        // temp_dir and all its entries including the root. In with_existing_root() we would call
        // let dir_entry = temp_dir.root.join("foo"); // a parent path that no longer exists
        // so fs::write(&dir_entry, b":)").unwrap(); would err
        //
        // This is why we need to keep the temp_dir alive.
        _temp_dir: tempfile::TempDir,
        root: PathBuf,
        lit: PathBuf,
    }

    impl TempDir {
        fn with_missing_root(root: &Path) -> Self {
            let temp_dir = tempfile::tempdir().unwrap();
            let root = temp_dir.path().join(root);
            let lit = root.join(".lit");

            Self {
                _temp_dir: temp_dir,
                root,
                lit,
            }
        }

        fn with_existing_root(root: &Path) -> Self {
            let temp_dir = tempfile::tempdir().unwrap();
            let root = temp_dir.path().join(root);
            fs::create_dir(&root).unwrap();

            let lit = root.join(".lit");

            Self {
                _temp_dir: temp_dir,
                root,
                lit,
            }
        }

        fn objects(&self) -> PathBuf {
            self.lit.join("objects")
        }

        fn refs(&self) -> PathBuf {
            self.lit.join("refs")
        }

        fn config(&self) -> PathBuf {
            self.lit.join("config")
        }
    }

    #[test]
    fn init_on_a_missing_dir() {
        let temp_dir = TempDir::with_missing_root("test".as_ref());
        // root does not exist and init should create {temp_dir}/root/.lit
        let init = Init {
            path: temp_dir.root.clone(),
        };

        init.execute().unwrap();

        assert!(temp_dir.root.is_dir());
        assert!(temp_dir.lit.is_dir());
        assert!(temp_dir.objects().is_dir());
        assert!(temp_dir.refs().is_dir());
        assert!(temp_dir.config().is_file());
    }

    // we assert that calling init on an existing directory does not alter its structure or any of the
    // contents of its entries
    #[test]
    fn init_on_an_existing_dir() {
        let temp_dir = TempDir::with_existing_root("test".as_ref());
        let init = Init {
            path: temp_dir.root.clone(),
        };
        // entry of the existing dir
        let dir_entry = temp_dir.root.join("foo");
        fs::write(&dir_entry, b":)").unwrap();

        init.execute().unwrap();

        let contents = fs::read(&dir_entry).unwrap();

        assert!(temp_dir.objects().is_dir());
        assert!(temp_dir.refs().is_dir());
        assert!(temp_dir.config().is_file());
        assert!(dir_entry.is_file());
        assert_eq!(contents, ":)".as_bytes().to_vec())
    }
    // TODO: write an IT tests where we call execute twice and assert on the print logic

    // calling init in an existing repo should not touch any of the files of the directory no matter
    // if they are owned by lit or not
    #[test]
    fn reinit_preserves_existing_repo_files() {
        let temp_dir = TempDir::with_existing_root("test".as_ref());
        let init = Init {
            path: temp_dir.root.clone(),
        };

        init.execute().unwrap();

        let config = temp_dir.config();
        let object_dir = temp_dir.objects().join("ef");
        let blob = object_dir.join("b1e0e54a68d5928831b3e3749ec764b346c987");
        let head = temp_dir.refs().join("HEAD");

        // init creates objects/, but not the two-character object subdirectory.
        fs::create_dir_all(&object_dir).unwrap();
        fs::write(
            &config,
            b"[user]\n    name = Alex Morgan\n    email = alex.morgan@example.com\n",
        )
        .unwrap();
        // obviously this is not the actual content of the blob, it is zlibed compress, and we could
        // easily use random data
        fs::write(&blob, b"blob 6\0hello\n").unwrap();
        fs::write(&head, b"821bf054e7f1fbc9a920609db2b5b6e256382b4e").unwrap();

        let config_before = fs::read(&config).unwrap();
        let blob_before = fs::read(&blob).unwrap();
        let head_before = fs::read(&head).unwrap();

        init.execute().unwrap();

        // reinit should not touch any of the content of the existing files
        assert_eq!(fs::read(&config).unwrap(), config_before);
        assert_eq!(fs::read(&blob).unwrap(), blob_before);
        assert_eq!(fs::read(&head).unwrap(), head_before);
    }

    #[test]
    fn reinit_recreates_deleted_repo_files() {
        let temp_dir = TempDir::with_existing_root("test".as_ref());
        let init = Init {
            path: temp_dir.root.clone(),
        };

        init.execute().unwrap();

        let config = temp_dir.config();
        let objects = temp_dir.objects();
        let refs = temp_dir.refs();
        fs::remove_dir_all(&objects).unwrap();
        fs::remove_dir_all(&refs).unwrap();
        fs::remove_file(&config).unwrap();

        init.execute().unwrap();

        assert!(objects.is_dir());
        assert!(refs.is_dir());
        assert!(config.is_file());
    }

    // this is true for other dirs like objects and refs, they are all created by the same method
    // ensure_dir()
    #[test]
    fn init_fails_when_lit_exists_but_is_not_a_directory() {
        let temp_dir = TempDir::with_existing_root("test".as_ref());
        fs::write(temp_dir.root.join(".lit"), ":/").unwrap();

        let init = Init {
            path: temp_dir.root.clone(),
        };

        match init.execute().unwrap_err() {
            InitError::BadEntry { path, entry } => {
                assert_eq!(path, temp_dir.root.join(".lit"));
                assert_eq!(entry, EntryType::File);
            }
            err => panic!("expected InitError::BadEntry, got {err:?}"),
        }
    }

    // this is true for all files created by init
    #[test]
    fn init_fails_when_config_exists_but_is_not_a_file() {
        let temp_dir = TempDir::with_existing_root("test".as_ref());
        fs::create_dir_all(temp_dir.lit.join("config")).unwrap();

        let init = Init {
            path: temp_dir.root.clone(),
        };

        match init.execute().unwrap_err() {
            InitError::BadEntry { path, entry } => {
                assert_eq!(path, temp_dir.config());
                assert_eq!(entry, EntryType::Directory);
            }
            err => panic!("expected InitError::BadEntry, got {err:?}"),
        }
    }
}
