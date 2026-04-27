pub mod db;
pub mod workspace;
pub mod refs;
mod lockfile;
mod object;

use crate::cmd::db::Database;
use crate::cmd::workspace::Workspace;
use std::env::Args;
use std::iter::{Peekable, Skip};
use std::path::PathBuf;
use std::{env, fs};
use crate::cmd::object::{Entry, Signature};
use crate::cmd::object::Object;

pub(super) enum Command {
    Init(Init),
    Commit(Commit),
}

impl Command {
    // toDo: make sure that a lit repo actually exists before executing any command apart from init
    // toDo: check git docs on what happens when calling init on a directory that already has .lit
    pub(super) fn execute(&mut self) {
        match self {
            Command::Init(cmd) => cmd.execute(),
            Command::Commit(cmd) => cmd.execute(),
        }
    }
}

pub(super) struct Init {
    pub(super) path: PathBuf,
}

impl Init {
    // toDo: maybe those two methods could be one like cmd::init() since we don't really need state?
    // we want to set the path to always be absolute
    // join() if the second path is absolute, it replaces the first entirely. else it gets appended.
    // lit init: creates .lit in the cwd
    // lit init /home/thanos/projects/1:  works fine it is already an absolute path
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

pub(super) struct Commit {
    pub(super) path: PathBuf,
}

impl Commit {
    fn execute(&self) {
        let cwd = env::current_dir().unwrap();
        let lit = cwd.join(".lit");

        // the user called commit from a path that does not have a lit repo
        if !lit.is_dir() {
            //return Err("fatal: not a lit repository".into());
        }

        let workspace = Workspace { cwd };
        let db_path = lit.join("objects");
        let db = Database { path: db_path };
        let paths = workspace.list_files();

        // for now, we assume that our project has only files
        let mut entries: Vec<Entry> = paths
            .into_iter()
            .map(|path| {
                let content = fs::read(&path).unwrap();
                let blob_id = db.store(Object::Blob(content));
                Entry {
                    id: blob_id,
                    name: String::from(""),
                }
            })
            .collect();
        // Why we sort the entries?
        //
        // Sorting is required for deterministic hashing. The tree's object id is the SHA-1 of its
        // serialized bytes. If we serialize entries in a different order, we get different bytes,
        // and therefore a different hash, even though the logical content is identical.
        //
        // Two trees with the same three files:
        // Tree A: main.rs, lib.rs, foo.rs
        // Tree B: lib.rs, main.rs, foo.rs
        //
        // If entries weren't sorted, these would produce different hashes. But they represent the
        // exact same directory. Two different hashes for the same content breaks Git's fundamental
        // property: same content -> same hash. With sorted entries, both trees serialize to identical
        // bytes and produce identical hashes.
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let tree = Object::Tree(entries);
        let tree_id = db.store(tree);
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
                tree_id: tree_id.into_iter()
                    .map(|b| format!("{:02x}", b))
                    .collect(),
            };
            db.store(Object::Commit(commit))
        });
    }
}
