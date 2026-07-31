use std::path::{Path, PathBuf};

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

// it was created to replace the initial approach where we had a single collect() method that did
// all the work, but we had to pass 5 arguments:
//  collect_entries(ws: &Workspace, rel: &Path, stat: StatNode, db: &Database, out: &mut Vec<IndexEntry>)
struct EntryCollector<'a> {
    workspace: &'a crate::command::workspace::Workspace,
    db: &'a crate::command::db::Database,
    path: &'a Path,
    entries: Vec<crate::command::index::IndexEntry>,
}

impl<'a> EntryCollector<'a> {
    fn new(workspace: &'a crate::command::workspace::Workspace, db: &'a crate::command::db::Database, path: &'a Path) -> Self {
        Self {
            workspace,
            db,
            path,
            entries: Vec::new(),
        }
    }

    // standard recursive approach, collect is the function that triggers the recursion with some initial
    // state
    fn collect(&mut self) -> Result<(), crate::command::error::AddError> {
        let stat = self.workspace.stat(self.path)?;
        self.collect_entries(self.path, stat)?;

        Ok(())
    }

    // we don't have to call index::validate_path() because the path is result of Pathspec::new()
    // and we have already done lexical normalization
    fn collect_entries(&mut self, relative: &Path, stat: crate::command::index::StatNode) -> Result<(), crate::command::error::AddError> {
        match stat.mode {
            0o100644 | 0o100755 => {
                let content = self.workspace.read_file(relative)?;
                let oid = self.db.store(crate::command::object::Object::Blob(content))?;
                let path_bytes = index::path_to_bytes(relative);
                self.entries.push(crate::command::index::IndexEntry::new(path_bytes, oid, stat));
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
                let oid = self.db.store(crate::command::object::Object::Blob(content))?;
                // it is more of sanity check
                // the call from os::stat() gives the link-target length so it will match anyway
                // but not sure for windows. Setting it from the actual blob is clearer.
                let stat = crate::command::index::StatNode {
                    file_size: size,
                    ..stat
                };
                let path_bytes = index::path_to_bytes(relative);
                self.entries.push(crate::command::index::IndexEntry::new(path_bytes, oid, stat));
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

#[derive(Debug)]
pub(super) enum AddError {
    Repo(crate::command::error::RepoError),
    Index(crate::command::error::IndexError),
    DbError(crate::command::error::DbError),
    WsError(crate::command::error::WorkspaceError),
    Pathspec(crate::command::error::PathspecError),
    Lockfile(crate::command::error::LockfileError),
}

impl From<crate::command::error::RepoError> for AddError {
    fn from(err: crate::command::error::RepoError) -> Self {
        AddError::Repo(err)
    }
}

impl From<crate::command::error::PathspecError> for AddError {
    fn from(err: crate::command::error::PathspecError) -> Self {
        AddError::Pathspec(err)
    }
}

impl From<crate::command::error::IndexError> for AddError {
    fn from(err: crate::command::error::IndexError) -> Self {
        AddError::Index(err)
    }
}

impl From<crate::command::error::DbError> for AddError {
    fn from(err: crate::command::error::DbError) -> Self {
        AddError::DbError(err)
    }
}

impl From<crate::command::error::WorkspaceError> for AddError {
    fn from(err: crate::command::error::WorkspaceError) -> Self {
        AddError::WsError(err)
    }
}

impl From<crate::command::error::LockfileError> for AddError {
    fn from(err: crate::command::error::LockfileError) -> Self {
        AddError::Lockfile(err)
    }
}