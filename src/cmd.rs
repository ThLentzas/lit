use crate::cmd::db::Database;
use crate::cmd::dir::Dir;
use crate::cmd::error::{AddError, RepoError};
use crate::cmd::index::{Index, IndexEntry, StatNode};
use crate::cmd::object::Object;
use crate::cmd::object::Signature;
use std::env::Args;
use std::iter::{Peekable, Skip};
use std::path::{Path, PathBuf};
use std::{env, fs};

pub mod db;
pub mod dir;
mod lockfile;
mod object;
pub mod refs;

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
    pub(super) fn execute(&mut self) {
        match self {
            Command::Init(cmd) => cmd.execute(),
            Command::Commit(cmd) => cmd.execute(),
            Command::Add(cmd) => cmd.execute().unwrap(),
        }
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
        // if you are on Linux the files will be hidden by default since any file that starts with
        // . is hidden
        let git_dir = self.path.join(".lit");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        fs::create_dir_all(git_dir.join("refs")).unwrap();
    }
}

pub(super) struct Commit;

impl Commit {
    fn execute(&self) {
        let cwd = env::current_dir().unwrap();
        let lit = cwd.join(".lit");

        // the user called commit from a path that does not have a lit repo
        if !lit.is_dir() {
            //return Err("fatal: not a lit repository".into());
        }

        // toDo: check that .lit/objects exists
        let db_path = lit.join("objects");
        let db = Database { path: db_path };
        // We read from all the files from the root and its subdirectories, and we create a flast list
        // of each entry. We create a nested structure, and then we build the tree bottom up. We don't
        // try to translate the filesystem structure directly into tree objects. Coupling is the answer
        // Read Chatper: 5.2.3
        let mut paths = workspace::list_files(&cwd);
        paths.sort_by(|a, b| {
            // read below for strip_prefix()
            let a = a.strip_prefix(&cwd).unwrap();
            let b = b.strip_prefix(&cwd).unwrap();
            // Git compares entry names as raw byte sequences(memcmp), no encoding, no locale, no
            // platform variation
            os::name_to_bytes(a.as_os_str()).cmp(&os::name_to_bytes(b.as_os_str()))
        });
        let mut root = Dir::new();
        for path in paths {
            let content = fs::read(&path).unwrap();
            // hash the blobs/leaves of the Merkle tree
            let blob_id = db.store(Object::Blob(content));
            let mode = workspace::stat(&path);
            // the path returned by list_files() includes machine specific location of the repo, but
            // we only care about the path inside repo.
            //
            // C:\Users\Thanos\projects\lit\src\database\tree.rs
            //
            // we only care about src\database\tree.rs and the cwd is C:\Users\Thanos\projects\lit
            //
            // strip_prefix errors if base is not a prefix of self (i.e., starts_with returns false)
            // cwd is always a parent component of every path, safe to unwrap
            let path = path.strip_prefix(&cwd).unwrap();
            root.add(path, blob_id, mode);
        }
        let tree_id = root.into_tree(&|entries| db.store(Object::Tree(entries)));
        refs::update_head(&lit, |parent| {
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
                tree_id: tree_id.into_iter().map(|b| format!("{:02x}", b)).collect(),
            };
            db.store(Object::Commit(commit))
        });
    }
}

    pub(super) struct Add {
    // user provided path
    path: PathBuf,
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
        let root = find_root()?;
        let lit = root.join(".lit");
        let db_path = lit.join("objects");
        // don't use exists(), because it can still exist but not be a directory
        if !db_path.is_dir() {
            return Err(RepoError::MissingRepoFile { path: db_path })?;
        }

        let db = Database { path: db_path };
        let (absolute, relative) = self.resolve_path(&root)?;
        let mut index = Index::new();
        let index_path = lit.join("index");
        let mut entries = Vec::new();
        collect_entries(&absolute, &relative, &db, &mut entries)?;
        index.update(&index_path, entries)?;
        
        Ok(())
    }

    // this function resolves the user provided path
    // it returns the absolute path to be used later for calling stat() and the repository relative
    // path for Index
    // 2 different views for the same entry path
    fn resolve_path(&self, root: &Path) -> Result<(PathBuf, PathBuf), AddError> {
        // toDo: make a trait or some sort of a conversion method
        let abs_root = root.canonicalize().map_err(|err| AddError::Io {
            path: root.to_path_buf(),
            source: err,
        })?;

        // the closure covers the case where the user input is just a bare filename/path component, like
        // lit add foo.txt
        // the parent directory as the current working directory
        // canonicalize(".") -> treats '.' as current working directory
        // toDo: when we implement the path spec logic we want to support lit add . and '.' means
        // add the current directory, not add a file named dot.
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        let name = self.path.file_name().unwrap();
        let abs_parent = parent.canonicalize().map_err(|err| AddError::Io {
            path: parent.to_path_buf(),
            source: err,
        })?;
        // we can't use the code below to find the absolute path of the user's provided path because
        // if it is a symlink canonicalize() resolves symlinks, and instead of returning the absolute
        // path of the symlink, it returns the absolute path of the target
        //
        // https://stackoverflow.com/questions/33157267/get-actual-path-symlink-is-pointing-to
        //
        //
        // let abs_path = match self.path.canonicalize() {
        //     Ok(p) => p,
        //     // 2 error variants can occur, ErrorKind::NotFound, ErrorKind::NotDirectory
        //     Err(err) => {
        //         return Err(AddError::Io {
        //             path: self.path.clone(),
        //             source: err,
        //         });
        //     }
        // };

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

// cwd is either the root(the directory that owns .lit) or a subdirectory of the root
fn find_root() -> Result<PathBuf, RepoError> {
    let mut dir = env::current_dir().map_err(|err| RepoError::CurrentDir(err))?;

    loop {
        if dir.join(".lit").is_dir() {
            return Ok(dir);
        }
        // sets dir to parent, returns false if parent is None
        if !dir.pop() {
            return Err(RepoError::NotRepository);
        }
    }
}

// absolute and relative do not need to be the exact same type.
fn collect_entries(
    absolute: &Path,
    relative: &Path,
    db: &Database,
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
            let oid = db.store(Object::Blob(content));
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
            let oid = db.store(Object::Blob(content));
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

            // toDo: --ignore-errors
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

                collect_entries(&child_absolute, &child_relative, db, out)?;
            }
        }
        // toDo: for now we silently ignore unsupported types
        other => {},
    }
    Ok(())
}
