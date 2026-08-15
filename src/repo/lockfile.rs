use std::error::Error;
use std::{fmt, fs, io};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

// https://git-scm.com/docs/api-lockfile
// Lockfile guarantees mutual exclusion, atomic updates and cleanup of the tmp files in case of an
// unexpected exit.
//
// While a Lockfile value exists, the holder has exclusive write access to file_path. The lock is
// acquired by atomically creating a .lock` file (lock is context specific extension). Other processes
// attempting to acquire the same lock will fail until this lockfile is committed or dropped.
// TODO:  explain why not some OS level lock
pub(crate) struct Lockfile {
    // the path to the file we want to write to
    pub(super) file_path: PathBuf,
    // the path to <filename>.lock
    pub(super) lock_path: PathBuf,
    // the open handle to the .lock file.
    // Option<File> rather than File solely as an implementation detail: Drop::drop requires all
    // fields to remain valid, and we need to move the file out during commit to close it before the
    // rename. The `None` state is only ever observed internally by Drop after a successful commit
    pub(super) file: Option<File>,
    // read drop() impl below
    committed: bool,
}

impl Lockfile {
    // we can't just call fs::write() and pass the path to <filename>.lock
    //
    // fs::write() calls File::create() and File::create() calls OpenOptions::new().write(true)
    // .create(true).truncate(true).open(path.as_ref()) which is not what we want. When create is set to
    // true, it will create the file if it doesn't exist or open it if it does.(truncate it too if the flag
    // is set). Both cases succeed. No way to distinguish them, no way to prevent the overwrite.
    // If two processes trying to update HEAD both would succeed, one would create the HEAD.lock tmo file,
    // the other would open and truncate. They would race on the content. Read also refs::update_head()
    // which is a use case of Lockfile.
    //
    // We need to acquire the file handle by calling create_new() instead. create_new() will return the
    // a file handler atomically. If someone tries to get a handler for the same file it will fail. Now
    // we can write to that tmp file without getting interupted. It creates the file exclusively meaning
    // once a process holds the lock for the specific file any other process would fail to acquire it
    //
    // returns Ok(Some(()) when it successfully acquires the lock
    // returns Ok(None) if it fails to acquire the lock
    // returns Err for any Io
    pub(crate) fn acquire(path: &Path) -> Result<Lockfile, LockfileError> {
        let file_path = path.to_path_buf();
        let lock_path = PathBuf::from(format!("{}.lock", file_path.display()));

        // a naive implementation would be if !path.exists() then create but this causes Time of
        // check Time of Use issues. Two processes can perform the check before either proceeds and
        // would try to create the same file twice. https://doc.rust-lang.org/std/fs/
        match OpenOptions::new()
            .write(true)
            // mutual exclusion: https://doc.rust-lang.org/std/fs/
            .create_new(true)
            .open(&lock_path)
        {
            Ok(file) => Ok(Lockfile {
                file_path,
                lock_path,
                file: Some(file),
                committed: false,
            }),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                Err(LockfileError::LockDenied { path: lock_path })
            }
            Err(err) => Err(LockfileError::Io {
                path: lock_path,
                source: err,
            }),
        }
    }

    // TODO: the current impl of write creates an allocation where the caller invokes serialize()
    //  which returns a Vec<u8> and that is passed to write(). Can we do better? Can we make Lockfile
    //  impl Write?
    pub(crate) fn write(&mut self, content: &[u8]) -> Result<(), LockfileError> {
        // safe to call unwrap because file is None only when commit() returns
        let file = self.file.as_mut().unwrap();
        match file.write_all(content) {
            Ok(_) => Ok(()),
            Err(err) => Err(LockfileError::Io {
                path: self.lock_path.clone(),
                source: err,
            }),
        }
    }

    // write_all() returns when the data have been handed to the OS, but the OS typically buffers
    // writes in memory and flushes to disk later. If the system crashes between write and the OS
    // flushing, the data is gone, even though write_all succeeded.
    //
    // sync_all (the fsync system call) tells the OS flush this to disk now, don't return until
    // it's persistent.
    //
    // we consume self because once we commit the changes Lockfile is no longer needed. It's a one
    // time use. We try to enforce it via the type system
    pub(crate) fn commit(mut self) -> Result<(), LockfileError> {
        let file = self.file.take().unwrap();
        // sync_all() does not close the file, we can still call file.write_all()
        //
        // From the docs:
        //      Files are automatically closed when they go out of scope. Errors detected on closing
        //      are ignored by the implementation of Drop. Use the method sync_all if these errors
        //      must be manually handled.
        // https://doc.rust-lang.org/std/fs/struct.File.html#method.sync_all
        // Rust will close the file when it goes out of scope in this case, after the call to rename
        // Depending on the OS, some platforms might not allow renaming in open files, so we make
        // sure the file is closed before calling rename(). Note we don't call drop on self(Lockfile)
        // but in the <filename>.lock field of self. We can still access lock_path and file_path
        file.sync_all().map_err(|err| LockfileError::Io {
            path: self.lock_path.clone(),
            source: err,
        })?;
        drop(file);
        fs::rename(&self.lock_path, &self.file_path).map_err(|err| LockfileError::Io {
            path: self.file_path.clone(),
            source: err,
        })?;
        self.committed = true;

        Ok(())
    }
}

// Lockfile dropped without calling commit() (early return, panic, etc.).file is still Some, rename
// was never called, the tmp file was never deleted, and we have to do the cleanup in Drop. An approach
// with if self.file.is_some() won't work if commit was called but failed because file is no longer
// Some is None but because it failed dropped will not clear it, this is the reason why we need this
// flag. If committed is true, the tmp file was successfully deleted, in any other case we do the
// manual cleanup
impl Drop for Lockfile {
    fn drop(&mut self) {
        // tempfile implements the same logic in the drop for TempDir
        if !self.committed {
            let _ = fs::remove_file(&self.lock_path);
        }
    }
}

#[derive(Debug)]
pub(crate) enum LockfileError {
    Io { path: PathBuf, source: io::Error },
    LockDenied { path: PathBuf },
}

impl Error for LockfileError {}

impl fmt::Display for LockfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LockfileError::Io { path, source } => {
                write!(f, "{}: {}", path.display(), source)
            }
            LockfileError::LockDenied { path } => {
                write!(f, "could not acquire lock on {}", path.display())
            }
        }
    }
}
