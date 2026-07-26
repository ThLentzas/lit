use crate::cmd::config::Config;
use crate::cmd::db::Database;
use crate::cmd::error::{AddError, CommitError, InitError, RepoError};
use crate::cmd::index::tree::Tree;
use crate::cmd::index::{Index, IndexEntry, StatNode};
use crate::cmd::lockfile::Lockfile;
use crate::cmd::object::{Object, Signature};
use crate::cmd::pathspec::Pathspec;
use crate::cmd::workspace::Workspace;
use std::env::Args;
use std::io::ErrorKind;
use std::iter::{Peekable, Skip};
use std::path::{Path, PathBuf};
use std::{env, fs, io};
use crate::cmd::refs::Refs;
use crate::cmd::status::Status;
use crate::hex;

pub mod db;
mod lockfile;
pub mod object;
pub mod refs;

mod config;
mod error;
pub mod index;
mod os;
mod pathspec;
pub mod timestamp;
pub mod workspace;
pub mod status;

// init creates repository structure
// add creates/updates the index
// commit consumes the index
pub(super) enum Command {
    Init(Init),
    Add(Add),
    Commit(Commit),
    Status(Status),
}

impl Command {
    // toDo: error handling
    pub(super) fn execute(&mut self) {
        match self {
            Command::Init(cmd) => cmd.execute().unwrap(),
            Command::Add(cmd) => cmd.execute().unwrap(),
            Command::Commit(cmd) => cmd.execute().unwrap(),
            Command::Status(cmd) => cmd.execute().unwrap(),
        }
    }
}

// Repository knows the paths to files like .lit/objects, .lit/config etc, path layout
// unlike the user provided paths that have to go through Workspace, these paths are known
//
// The idea is for each component to know exactly the path it anchors
struct Repository {
    // the directory that owns .lit
    root: PathBuf,
    // lit is guaranteed to be a directory
    lit: PathBuf,
}

impl Repository {
    // cwd is either the root or a subdirectory of the root
    fn discover() -> Result<Self, RepoError> {
        let cwd = env::current_dir().map_err(RepoError::CurrentDir)?;
        let mut dir = fs::canonicalize(&cwd).map_err(|err| RepoError::Io {
            path: cwd,
            source: err,
        })?;

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

    fn db_path(&self) -> PathBuf {
        self.lit.join("objects")
    }

    fn index_path(&self) -> PathBuf {
        self.lit.join("index")
    }

    fn config_path(&self) -> PathBuf {
        self.lit.join("config")
    }

    fn refs_path(&self) -> PathBuf {
        self.lit.join("refs")
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

    // when calling init in an existing repo Git does not overwrite any of the existing files, it
    // will try to create any that are missing.
    fn execute(&self) -> Result<(), InitError> {
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

pub(super) struct Add {
    // user provided path
    pub(super) paths: Vec<PathBuf>,
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
        let db = Database { path: repo.db_path(), };
        let workspace = Workspace { root: repo.root.clone(), };
        let mut index = Index::new(repo.index_path());
        let mut lockfile = Lockfile::acquire(&index.path)?;
        index.load()?;

        for path in self.paths.iter() {
            // if the user called add . from root then the prefix is "" and the pathspec.pattern is
            // also  "". This is fine because in collect_entries() for the dir case we call ws.list_dir()
            // which does self.root.join(relative) so absolute root + "" give us the absolute to root
            // path which is what we want.
            let pathspec = if path.is_absolute() {
                Pathspec::new(path.as_os_str(), Path::new(""), &repo.root)?
            } else {
                let prefix = workspace.prefix()?;
                Pathspec::new(path.as_os_str(), &prefix, &repo.root)?
            };
            let mut collector = EntryCollector::new(&workspace, &db, &pathspec.pattern);
            collector.collect()?;
            index.add_entries(collector.finish())?;
        }
        if index.modified {
            lockfile.write(&index.serialize())?;
        }
        lockfile.commit()?;
        Ok(())
    }
}

pub(super) struct Commit;

impl Commit {
    fn execute(&self) -> Result<(), CommitError> {
        let repo = Repository::discover()?;
        let db = Database {
            path: repo.db_path(),
        };
        let refs = Refs { path: repo.refs_path() };
        // TODO: modify index by refreshing the metadata and then it becomes a read-modify-write, and remove this comment it no longer stands
        // for index itself
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
        let mut index = Index::new(repo.index_path());
        let _index_lock = Lockfile::acquire(&index.path)?;
        index.load()?;
        // need to rethink how this load() is called because now load() is called without
        // knowing if Signature will actually read the info from config or env
        let mut config = Config::new(repo.config_path());
        config.load()?;
        let author = Signature::author(&config)?;
        let committer = Signature::committer(&config)?;
        let tree_id = Tree::from_index(index).write(&db)?;

        refs.update_head(|parent| {
            let commit = object::Commit {
                author,
                parent,
                committer,
                message: "".to_string(),
                // convert the [u8, 20] to its hex value
                root_id: hex::bytes_as_hex(&tree_id),
            };
            db.store(Object::Commit(commit))
        })?;
        Ok(())
    }
}

// it was created to replace the initial approach where we had a single collect() method that did
// all the work, but we had to pass 5 arguments:
//  collect_entries(ws: &Workspace, rel: &Path, stat: StatNode, db: &Database, out: &mut Vec<IndexEntry>)
struct EntryCollector<'a> {
    workspace: &'a Workspace,
    db: &'a Database,
    path: &'a Path,
    entries: Vec<IndexEntry>,
}

impl<'a> EntryCollector<'a> {
    fn new(workspace: &'a Workspace, db: &'a Database, path: &'a Path) -> Self {
        Self {
            workspace,
            db,
            path,
            entries: Vec::new(),
        }
    }

    // standard recursive approach, collect is the function that triggers the recursion with some initial
    // state
    fn collect(&mut self) -> Result<(), AddError> {
        let stat = self.workspace.stat(self.path)?;
        self.collect_entries(self.path, stat)?;

        Ok(())
    }

    // we don't have to call index::validate_path() because the path is result of Pathspec::new()
    // and we have already done lexical normalization
    fn collect_entries(&mut self, relative: &Path, stat: StatNode) -> Result<(), AddError> {
        match stat.mode {
            0o100644 | 0o100755 => {
                let content = self.workspace.read_file(relative)?;
                let oid = self.db.store(Object::Blob(content))?;
                let path_bytes = index::path_to_bytes(relative);
                self.entries.push(IndexEntry::new(path_bytes, oid, stat));
            }
            // https://stackoverflow.com/questions/954560/how-does-git-handle-symbolic-links
            // the content of the blob is the target path as bytes
            // the file size is the length of the above sequence
            //
            // if the user deletes target, then we have a dangling reference which is allowed. It's up
            // to the user to remove the symlink
            0o120000 => {
                let target = self.workspace.read_link(relative)?;
                let content = index::path_to_bytes(&target);
                let size = content.len().min(u32::MAX as usize) as u32;
                let oid = self.db.store(Object::Blob(content))?;
                // it is more of sanity check
                // the call from os::stat() gives the link-target length so it will match anyway
                // but not sure for windows. Setting it from the actual blob is clearer.
                let stat = StatNode {
                    file_size: size,
                    ..stat
                };
                let path_bytes = index::path_to_bytes(relative);
                self.entries.push(IndexEntry::new(path_bytes, oid, stat));
            }

            // toDo: check if the mode returned by stat() contains permission bits
            0o040000 => {
                // toDo: look at --ignore-errors flag
                //
                for (path, stat) in self.workspace.dir_entries(relative)? {
                    self.collect_entries(&path, stat)?;
                }
            }
            // toDo: for now we silently ignore unsupported type
            _ => {}
        }
        Ok(())
    }

    fn finish(self) -> Vec<IndexEntry> {
        self.entries
    }
}
