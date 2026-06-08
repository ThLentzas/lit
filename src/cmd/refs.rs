use crate::cmd::error::{DbError, LockfileError, RefError};
use crate::cmd::lockfile::Lockfile;
use std::path::Path;
use std::{fs, io};

// We can't use the same approach to update the head as we did to write objects. In the writing
// object case, we don't care about competing writes. The object path is derived from its content. If
// both processes are writing the same object, they should be writing the same bytes. So the main
// concern there was just: atomicity of the write, that no one sees a half-written file. The tmp
// file + rename pattern guarantees that the write will appear atomic. So if process A creates
// tmp_obj_ABC and process B creates tmp_obj_123, both will write the same content, it doesn't matter
// who wins, the result is the same.
//
// This is not enough for HEAD.
//
// Unlike objects, HEAD's content changes with every commit. Two processes might genuinely be trying
// to write different values. Two commit processes running simultaneously, each trying to set HEAD
// to their new commit. We can no longer assume that those 2 processes are writing the same content.
// The tmp file + rename pattern can't protect against this. If Process A writes to HEAD.tmpABC and
// Process B writes to HEAD.tmpXYZ, they don't know about each other. Both will happily rename to
// HEAD, and whichever happens last silently overwrites the other. One commit gets lost. The solution
// is file locking.
//
// We set the rule that anytime that we want to write to HEAD, we don't do it directly, we do it via
// HEAD.lock. In most cases, we need to read HEAD before updating. The HEAD.lock file functions as a
// mutex for HEAD operations. By convention, whoever holds it has exclusive write access to HEAD. We
// need to acquire the lock before we attempt to read.
//
// Read-modify-write like commit, needs to read HEAD (to find the parent), then write a new value
// (the new commit pointing to that parent)
//
// Process A reads HEAD -> sees commit X
// Process B reads HEAD -> also sees commit X
// Process A creates a new commit with parent X, writes to HEAD -> now HEAD = A's commit
// Process B creates a new commit with parent X, writes to HEAD -> now HEAD = B's commit
// A's commit is now orphaned. The history "lost" A even though the commit object still exists in
// .git/objects/.
//
// This is the lost update problem. To fix this we hold HEAD.lock for the entire duration, read-modify-write
// That way the entire process is executed without any interuptions.
//
// Lockfile will guarantee that only 1 process can hold the lock to <filename>.lock because the file
// is created as exclusive(its existence blocks other writers). We combine mutual exclusion, atomic
// write, and lock release into a single filesystem operation.
//
// We pass the path to .lit. The caller does not know the path to HEAD. The functions know the path
// to HEAD. Similar logic to the db_path. The database knows where to store the objects
pub(super) fn update_head<F>(path: &Path, f: F) -> Result<(), RefError>
where
    // For commit there are 6 steps:
    //
    // Acquire the lock
    // Read HEAD (parent OID)
    // Build the commit object using that parent
    // Store the commit -> get new OID
    // Write the new OID to HEAD
    // Release the lock
    //
    // Steps 1, 2, 5, 6 are locking and HEAD operations, they belong in refs.rs. Steps 3 and 4 are
    // commit-building logic, they belong in cmd.rs where we know about authors, messages, the database, etc.
    // But the steps are interleaved. Step 3 needs the result of step 2 (the parent). Step 5 needs
    // the result of step 4 (the new OID). We can't separate them into two phases like "first do
    // all the lock stuff, then build the commit" because the data dependencies cross the boundary.
    //
    // toDo: revisit this design choice
    // A solution would be to pass all the logic to construct a commit to update_head() which leads
    // to bad coupling, refs are responsible for HEAD and refs. With the closure approach we
    // just provide the behavior once we get access to parent. Could also be an approach where we
    // return the lock and continue our logic but this will need to change are refs api and lockfile
    // quite a lot.
    F: FnOnce(Option<String>) -> Result<[u8; 20], DbError>
{
    let mut lockfile = Lockfile::acquire(&path)?;
    let parent = read_head(path)?;
    let new_id = f(parent)?;
    // convert the [u8; 20] hash into its hex representation
    let new_id: String = new_id.iter().map(|c| format!("{:02x}", c)).collect();

    lockfile.write(new_id.as_bytes())?;
    // lockfile.write("\n".as_bytes());
    lockfile.commit()?;

    Ok(())
}

pub(super) fn read_head(path: &Path) -> Result<Option<String>, LockfileError>{
    let head_path = path.join("HEAD");
    // HEAD contains the id as a 40-character hex string already (plain text 903a71ad300d5aa1ba0c0495ce9341f42e3fcd7c)
    // we know it is valid utf8 so we can call read_to_string()
    match fs::read_to_string(&head_path) {
        Ok(s) => Ok(Some(s)),
        // no HEAD yet (first commit)
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        // something actually went wrong (permissions, corrupt data, etc.)
        Err(err) => Err(LockfileError::Io { path: head_path.to_path_buf(), source: err }),
    }
}