use std::{env, fs, io};
use std::env::Args;
use std::io::ErrorKind;
use std::iter::{Peekable, Skip};
use std::path::{Path, PathBuf};

pub(super) struct Init {
    pub(super) path: PathBuf,
}

impl Init {
    // toDo: maybe those two methods could be one like command::init() since we don't really need state?
    // we want to set the path to always be absolute, toDo: Read the Chapter for init on why
    // join() if the second path is absolute, it replaces the first entirely, else it gets appended.
    // lit init: creates .lit in the cwd
    // lit init /home/thanos/projects/1: works fine it is already an absolute path
    pub(super) fn new(args: &mut Peekable<Skip<Args>>) -> Self {
        let cwd = env::current_dir().unwrap();
        let path = match args.next() {
            Some(p) => cwd.join(p),
            None => cwd,
        };

        Self { path }
    }

    // when calling init in an existing repo Git does not overwrite any of the existing files, it
    // will try to create any that are missing.
    pub(super) fn execute(&self) -> Result<(), InitError> {
        fs::create_dir_all(&self.path).map_err(|err| Self::io_error(&self.path, err))?;
        // on Linux the files will be hidden by default since any file that starts with . is hidden
        let lit_dir = self.path.join(".lit");

        let reinit = match fs::create_dir(&lit_dir) {
            Ok(_) => false,
            Err(err) if err.kind() == ErrorKind::AlreadyExists => true,
            Err(err) => return Err(Self::io_error(&lit_dir, err)),
        };

        let objects = lit_dir.join("objects");
        let refs = lit_dir.join("refs");
        // safe to call create_dir_all() in existing dirs, it will return without touching them
        fs::create_dir_all(&objects).map_err(|err| Self::io_error(&objects, err))?;
        fs::create_dir_all(&refs).map_err(|err| Self::io_error(&refs, err))?;
        // toDo: At this point we need to some setup for template files
        // toDo: one such case is to create the config file with the core section [core]

        if reinit {
            println!("Reinitialized existing Lit repository in {}", lit_dir.display());
        } else {
            println!("Initialized empty Lit repository in {}", lit_dir.display());
        }
        Ok(())
    }

    fn io_error(path: &Path, err: io::Error) -> InitError {
        InitError::Io {
            path: path.to_path_buf(),
            source: err,
        }
    }
}

#[derive(Debug)]
pub(super) enum InitError {
    Io { path: PathBuf, source: io::Error },
}