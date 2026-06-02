use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::{fs, io};

// https://git-scm.com/docs/api-lockfile
// Lockfile guarantees mutual exclusion, atomic updates and cleanup of the tmp files in case of an
// unexpected exit.
//
// While a Lockfile value exists, the holder has exclusive write access to file_path. The lock is
// acquired by atomically creating a .lock` file (lock is context specific extension). Other processes
// attempting to acquire the same lock will fail until this lockfile is committed or dropped.
// toDo:  explain why not some OS level lock
pub(super) struct Lockfile {
    // the path to the file we want to write to
    pub(super) file_path: PathBuf,
    // the path to <filename>.lock
    pub(super) lock_path: PathBuf,
    // the open handle to the .lock file.
    // Option<File> rather than File solely as an implementation detail: Drop::drop requires all
    // fields to remain valid, and we need to move the file out during commit to close it before the
    // rename. The `None` state is only ever observed internally by Drop after a successful commit
    pub(super) file: Option<File>,
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
    pub(super) fn acquire(path: &Path) -> io::Result<Option<Lockfile>> {
        let file_path = path.to_path_buf();
        let lock_path = PathBuf::from(format!("{}.lock", file_path.display()));

        // a naive implementation would be do if !path.exists() then create but this causes Time of
        // check Time of Use issues. Two processes can perform the check before either proceeds and
        // would try to create the same file twice. https://doc.rust-lang.org/std/fs/
        match OpenOptions::new()
            .write(true)
            // mutual exclusion: https://doc.rust-lang.org/std/fs/
            .create_new(true)
            .open(&lock_path)
        {
            Ok(file) => Ok(Some(Lockfile {
                file_path,
                lock_path,
                file: Some(file),
            })),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub(super) fn write(&mut self, content: &[u8]) {
        let file = self.file.as_mut().unwrap();
        file.write_all(content).unwrap();
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
    pub(super) fn commit(mut self) {
        let file = self.file.take().unwrap();
        // sync_all() does not closes the file, we can still call file.write_all()
        //
        // From the docs:
        //      Files are automatically closed when they go out of scope. Errors detected on closing
        //      are ignored by the implementation of Drop. Use the method sync_all if these errors
        //      must be manually handled.
        // if let Err(e) = file.sync_all() {
        //         drop(file); // explicit close
        //         let _ = fs::remove_file(&self.lock_path); // clean up the lock
        //         return Err(e);
        //     }
        // https://doc.rust-lang.org/std/fs/struct.File.html#method.sync_all
        file.sync_all().unwrap();
        // Rust will close the file when it goes out of scope in this case, after the call to rename
        // Depending on the OS, some platforms might not allow renaming in open files, so we make
        // sure the file is closed before calling rename(). Note we don't call drop on self(Lockfile)
        // but in the <filename>.lock field of self. We can still access lock_path and file_path
        drop(file);
        //     if let Err(e) = fs::rename(&self.lock_path, &self.file_path) {
        //         let _ = fs::remove_file(&self.lock_path);
        //         return Err(e);
        //     }
        fs::rename(&self.lock_path, &self.file_path).unwrap();
    }
}

// Lockfile dropped without calling commit() (early return, panic, etc.).file is still Some, rename
// was never called, the tmp file was never deleted, and we have to do the cleanup in Drop
impl Drop for Lockfile {
    fn drop(&mut self) {
        if self.file.is_some() {
            let _ = fs::remove_file(&self.lock_path);
        }
    }
}