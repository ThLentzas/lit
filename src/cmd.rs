use crate::cmd::error::{AddError, CommitError, RepoError};
use crate::cmd::index::{Index, IndexEntry, StatNode};
use crate::cmd::lockfile::Lockfile;
use crate::cmd::object::Object;
use crate::cmd::object::Signature;
use crate::cmd::tree_builder::InMemTree;
use std::env::Args;
use std::iter::{Peekable, Skip};
use std::path::{Path, PathBuf};
use std::{env, fs};

pub mod db;
mod lockfile;
mod object;
pub mod refs;
pub mod tree_builder;

mod error;
pub mod index;
mod os;
pub mod workspace;

// init creates repository structure
// add creates/updates the index
// commit consumes the index
pub(super) enum Command {
    Init(Init),
    Commit(Commit),
    Add(Add),
}

impl Command {
    // toDo: make sure that a lit repo actually exists before executing any command apart from init
    // toDo: check git docs on what happens when calling init on a directory that already has .lit
    // toDo: error handling
    pub(super) fn execute(&mut self) {
        match self {
            Command::Init(cmd) => cmd.execute(),
            Command::Commit(cmd) => cmd.execute().unwrap(),
            Command::Add(cmd) => cmd.execute().unwrap(),
        }
    }
}

struct Repository {
    root: PathBuf,
    // lit is guaranteed to be a directory
    lit: PathBuf,
}

impl Repository {
    // cwd is either the root(the directory that owns .lit) or a subdirectory of the root
    fn discover() -> Result<Self, RepoError> {
        let mut dir = env::current_dir().map_err(|err| RepoError::CurrentDir(err))?;

        loop {
            let lit = dir.join(".lit");
            if lit.is_dir() {
                return Ok(Self { root: dir, lit });
            }
            // sets dir to parent, returns false if parent is None
            if !dir.pop() {
                return Err(RepoError::NotRepository);
            }
        }
    }

    fn objects_path(&self) -> PathBuf {
        self.lit.join("objects")
    }

    fn index_path(&self) -> PathBuf {
        self.lit.join("index")
    }

    fn head_path(&self) -> PathBuf {
        self.lit.join("HEAD")
    }
}

pub(super) struct Init {
    pub(super) path: PathBuf,
}

impl Init {
    // toDo: maybe those two methods could be one like cmd::init() since we don't really need state?
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

    fn execute(&self) {
        if self.path.exists() && !self.path.is_dir() {
            // Error
        }
        // on Linux the files will be hidden by default since any file that starts with
        // . is hidden
        let git_dir = self.path.join(".lit");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        fs::create_dir_all(git_dir.join("refs")).unwrap();
    }
}

pub(super) struct Commit;

impl Commit {
    fn execute(&self) -> Result<(), CommitError> {
        let repo = Repository::discover()?;
        let db_path = repo.objects_path();

        // We must acquire the lock on .lit/index for the entire commit, not just read the file.
        // Lockfile's atomic rename guarantees we never read half-written/corrupted data, but that
        // is not enough here. We read the index, build tree objects from it, write a commit, and
        // update HEAD.
        //
        // It is a time-of-check to time-of-use case. Without holding the lock across the whole
        // operation, a concurrent add could happen in between our read and our write:
        //   commit reads the index
        //   add acquires the lock, modifies the index
        //   commit builds its tree from the old index without the new changes
        //
        // The result is an old-but-valid index: no corruption, but we committed a staged state the
        // user had already moved past, and the committed tree now disagrees with .lit/index.
        // Concurrent commits are not a concern , they are read only
        //
        // A question to ask here is how many different people work in a repository locally? Most
        // of the time 1 so we could just call index.load() without acquiring the lock but now with
        // agents it is a different story
        let index_path = repo.index_path();
        let _index_lock = Lockfile::acquire(&index_path)?;
        let mut index = Index::new(repo.index_path());
        index.load()?;

        let tree_id = InMemTree::from_index(index).write(&db_path)?;
        refs::update_head(&repo.head_path(), |parent| {
            // toDo: move this logic on a new() method for Commit where we read the author/committer from the .gitconfig file
            let commit = object::Commit {
                author: Signature {
                    name: "".to_string(),
                    email: "".to_string(),
                    timestamp: "".to_string(),
                },
                parent,
                committer: Signature {
                    name: "".to_string(),
                    email: "".to_string(),
                    timestamp: "".to_string(),
                },
                message: "".to_string(),
                // convert the [u8, 20] to its hex value
                tree_id: tree_id.iter().map(|b| format!("{:02x}", b)).collect(),
            };
            db::store(&db_path, Object::Commit(commit))
        })?;
        Ok(())
    }
}

pub(super) struct Add {
    // user provided path
    pub(super) path: PathBuf,
}

impl Add {
    // cwd -> where the user ran the command from
    // root -> directory that owns .lit
    //
    // Git commands can be run from inside the repo, not only the root
    // cd jolt/src/parser
    // git add lexer.rs
    // git commit
    //
    // Git still knows the repository root is jolt and the index path should be src/parser/lexer.rs
    // not just lexer.rs(what the user provided, self.path in our case)
    fn execute(&self) -> Result<(), AddError> {
        let repo = Repository::discover()?;
        let db_path = repo.objects_path();
        let (absolute, relative) = self.resolve_path(&repo.root)?;
        let mut index = Index::new(repo.index_path());
        let mut entries = Vec::new();
        collect_entries(&absolute, &relative, &db_path, &mut entries)?;
        index.update(entries)?;

        Ok(())
    }

    // this function resolves the user provided path
    // it returns the absolute path to be used later for calling stat() and the repository relative
    // path for Index
    // 2 different views for the same entry path
    fn resolve_path(&self, root: &Path) -> Result<(PathBuf, PathBuf), AddError> {
        let abs_root = root.canonicalize().map_err(|err| AddError::Io {
            path: root.to_path_buf(),
            source: err,
        })?;

        // self.parent() returns Some("") for relative paths with one component
        // lit add foo.txt
        // the parent directory as the current working directory
        // canonicalize(".") -> treats '.' as current working directory
        // toDo: when we implement the path spec logic we want to support lit add . and '.' means
        // add the current directory, not add a file named dot.
        let parent = match self.path.parent() {
            Some(p) if p.as_os_str().is_empty() => Path::new("."),
            Some(p) => p,
            None => unreachable!(),
        };

        let name = self.path.file_name().unwrap();
        let abs_parent = parent.canonicalize().map_err(|err| AddError::Io {
            path: parent.to_path_buf(),
            source: err,
        })?;

        // we can't call self.path.canonicalize() because if it is a symlink, canonicalize() will
        // resolve it and instead of returning the absolute path of the symlink, it returns the
        // absolute path of the target
        //
        // https://stackoverflow.com/questions/33157267/get-actual-path-symlink-is-pointing-to
        //
        // to address that we canonicalize the parent path and then join the name of the last component
        // it does not change anything for file/dir but allows us to compute the absolute path for
        // the symlink itself
        let abs_path = abs_parent.join(name);
        // provided path is not part of the repository
        if !abs_path.starts_with(&abs_root) {
            return Err(AddError::OutsideRepository {
                path: self.path.clone(),
                root: abs_root,
            });
        }
        // strip the prefix to get the relative to the lit repo path that we need for Index
        let repo_relative = abs_path.strip_prefix(&abs_root).unwrap().to_path_buf();
        Ok((abs_path, repo_relative))
    }
}

// absolute and relative do not need to be the exact same type.
fn collect_entries(
    absolute: &Path,
    relative: &Path,
    db_path: &Path,
    out: &mut Vec<IndexEntry>,
) -> Result<(), AddError> {
    let stat = os::stat(absolute).map_err(|err| AddError::StatFile {
        path: relative.to_path_buf(),
        source: err,
    })?;

    match stat.mode {
        0o100644 | 0o100755 => {
            let content = fs::read(absolute).map_err(|err| AddError::Io {
                path: absolute.to_path_buf(),
                source: err,
            })?;
            let oid = db::store(db_path, Object::Blob(content))?;
            let path_bytes = index::to_path_bytes(&relative);
            index::validate_index_path(&path_bytes)?;
            out.push(IndexEntry::new(path_bytes, oid, stat));
        }

        0o120000 => {
            let target = fs::read_link(absolute).map_err(|err| AddError::Io {
                path: absolute.to_path_buf(),
                source: err,
            })?;
            let content = index::to_path_bytes(&target);
            let size = content.len().min(u32::MAX as usize) as u32;
            let oid = db::store(db_path, Object::Blob(content))?;
            // the content of the blob is the target path as bytes
            // the file size is the length of the above sequence
            // it is more of sanity check
            // the call from os::stat() gives the link-target length so it will match anyway
            // but not sure for windows. Setting it from the actual blob is clearer.
            let stat = StatNode {
                file_size: size,
                ..stat
            };
            let path_bytes = index::to_path_bytes(&relative);
            index::validate_index_path(&path_bytes)?;
            out.push(IndexEntry::new(index::to_path_bytes(relative), oid, stat));
        }

        // toDo: check if the mode returned by stat() contains permission bits
        0o040000 => {
            // can err when path does not exist, permission denied, or path points at a non-directory file
            let read_dir = fs::read_dir(absolute).map_err(|err| AddError::Io {
                path: absolute.to_path_buf(),
                source: err,
            })?;

            // toDo: look at --ignore-errors flag
            for entry in read_dir {
                // opening the directory can succeed, but reading one of its entries can fail later.
                // permission/access changes while iterating
                // directory is modified/deleted while iterating
                // filesystem/network drive error
                // entries are deleted
                let entry = entry.map_err(|err| AddError::Io {
                    // Using the parent directory as path is okay here because we may not have a
                    // child name yet.
                    // toDo: is there a way to actually point to the entry that failed?
                    path: absolute.to_path_buf(),
                    source: err,
                })?;
                let name = entry.file_name();

                if name == ".lit" {
                    continue;
                }

                let child_absolute = entry.path();
                let child_relative = relative.join(&name);

                collect_entries(&child_absolute, &child_relative, db_path, out)?;
            }
        }
        // toDo: for now we silently ignore unsupported types
        other => {}
    }
    Ok(())
}
