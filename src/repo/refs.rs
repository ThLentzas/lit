use crate::repo::db::DbError;
use crate::repo::lockfile::{Lockfile, LockfileError};
use crate::repo::object::OidError;
use crate::repo::object::oid::Oid;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::{fmt, fs, io};

pub(crate) struct Refs {
    refs: PathBuf,
    head: PathBuf,
}

impl Refs {
    pub(crate) fn new(root: &Path) -> Self {
        Self {
            refs: root.join("refs"),
            head: root.join("HEAD"),
        }
    }
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
    pub(crate) fn update_head<F>(&self, f: F) -> Result<(), RefError>
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
        // commit-building logic, they belong in command.rs where we know about authors, messages, the database, etc.
        // But the steps are interleaved. Step 3 needs the result of step 2 (the parent). Step 5 needs
        // the result of step 4 (the new OID). We can't separate them into two phases like "first do
        // all the lock stuff, then build the commit" because the data dependencies cross the boundary.
        //
        // TODO: revisit this design choice
        // A solution would be to pass all the logic to construct a commit to update_head() which leads
        // to bad coupling, refs are responsible for HEAD and refs. With the closure approach we
        // just provide the behavior once we get access to parent. Could also be an approach where we
        // return the lock and continue our logic but this will need to change are refs api and lockfile
        // quite a lot.
        F: FnOnce(Vec<Oid>) -> Result<Oid, DbError>,
    {
        let mut lockfile = Lockfile::acquire(&self.head)?;
        let parent = self.read_head()?;
        let parent = parent
            .into_iter()
            .map(|hex| Oid::from_hex(&hex).map_err(RefError::from))
            .collect::<Result<Vec<_>, _>>()?;
        let new_id = f(parent)?;

        lockfile.write(new_id.to_hex().as_bytes())?;
        // lockfile.write("\n".as_bytes());
        lockfile.commit()?;

        Ok(())
    }

    pub(super) fn read_head(&self) -> Result<Option<String>, RefError> {
        // HEAD contains the id as a 40-character hex string already (plain text 903a71ad300d5aa1ba0c0495ce9341f42e3fcd7c)
        // we know it is valid utf8 so we can call read_to_string()
        match fs::read_to_string(&self.head) {
            Ok(s) => Ok(Some(s)),
            // no HEAD yet (first commit)
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            // something actually went wrong (permissions, corrupt data, etc.)
            Err(err) => Err(RefError::Io {
                path: self.head.clone(),
                source: err,
            }),
        }
    }

    pub(super) fn new_unborn_branch(&self) -> Result<(), RefError> {
        let mut head_lock = Lockfile::acquire(&self.head)?;
        // TODO: this should change to main in the future
        head_lock.write(b"ref: refs/heads/master\n")?;
        head_lock.commit()?;

        Ok(())
    }

    // symrefs: a reference that points to another reference.
    // refs/heads/ contains one copy per local branch, refs/heads/<name> is a branch reference.
    // Each branch ref contains the object ID of that branch's current latest commit.
    // .git/HEAD is a symref, it points to the current branch
    // when we create a new commit, the current content of the branch ref becomes the commit's parent
    // OID and the new ID replaces it. HEAD is never moved.
    //
    // symrefs solve the following problem. If both HEAD and the branch ref hold the OID how does
    // Git know which branch should move when a new commit is created. If we make a new commit Git
    // knows that HEAD should be updated but which branch? Multiple branches are allowed to point to
    // the same commit. By making HEAD a symref, only the target branch changes. This also solves
    // the orphan/unborn branch problem. For the first commit, there is no commit ID that HEAD could
    // contain. However HEAD, can already express the intended branch. The first commit then creates
    // the /refs/heads/master entry.
    //
    // DETACHED HEAD
    //
    // when we checkout a specific commit, HEAD points directly to a commit instead of symbolically
    // referring to a branch.
    // It is no longer attached to any branch(it is DETACHED)
    //
    // We can make commits while detached. HEAD will point to the new commit
    //       X <- HEAD
    //      /
    // A <- B <- C <- main
    //
    // The new commit is valid and stored in the object database. The important difference is that no
    // branch moves to point to it, because HEAD does not name a branch.
    //
    // If we switch back to main the state becomes:
    //
    //        X
    //      /
    // A <- B <- C <- main <- HEAD
    //
    // Now no branch points to X. It has not necessarily been deleted, it can usually still be found
    // through the reflog, but it may eventually become unreachable and be garbage-collected.
    //
    // To keep the detached work we need to create the branch at the current commit.
    //
    //       X <- experiment <- HEAD
    //      /
    // A <- B <- C <- main
    //
    // Attached: HEAD -> branch -> commit
    // Detached: HEAD -----------> commit
    //
    // workspace and index must reflect that move
    //
    // https://stackoverflow.com/questions/10228760/how-do-i-fix-a-git-detached-head
    //
    // If we simply want to inspect a specific commit, no changes need to be preserved, no new history
    // was created, we can move back to our branch by git checkout main. Internally, Git writes HEAD
    // as a symref from an OID.
    //
    // if we call commit without creating a new branch while we have a DETACHED HEAD Git allows it.
    // Git creates a new commit whose parent is the checked-out commit then updates DETACHED HEAD
    // directly. No branch is updated.
    //
    //        X <- Y <- HEAD
    //      /
    // A <- B <- C <- main
    //
    // Now we are in the same situation as before in order to have a way to reference to the new
    // line of development we need to create a branch, otherwise we have no way of referncing Y.
}

#[derive(Debug)]
pub(crate) enum RefError {
    Io { path: PathBuf, source: io::Error },
    Lockfile(LockfileError),
    Database(DbError),
    Oid(OidError),
}

impl Error for RefError {}

impl fmt::Display for RefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RefError::Io { path, source } => {
                write!(f, "{}: {source}", path.display())
            }
            RefError::Lockfile(err) => write!(f, "{err}"),
            RefError::Database(err) => write!(f, "{err}"),
            RefError::Oid(err) => write!(f, "{err}"),
        }
    }
}

impl From<LockfileError> for RefError {
    fn from(err: LockfileError) -> Self {
        RefError::Lockfile(err)
    }
}

impl From<DbError> for RefError {
    fn from(err: DbError) -> Self {
        RefError::Database(err)
    }
}

impl From<OidError> for RefError {
    fn from(err: OidError) -> Self {
        RefError::Oid(err)
    }
}
