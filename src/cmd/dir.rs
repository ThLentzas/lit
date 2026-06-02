use crate::cmd::object::TreeEntry;
use indexmap::IndexMap;
use indexmap::map::Entry as MapEntry;
use std::ffi::OsString;
use std::path::Path;

const MODE: u32 = 0o40000;

enum DirEntry {
    File { blob_id: [u8; 20], mode: u32 },
    Dir(IndexMap<OsString, DirEntry>),
}

#[derive(Default)]
// in memory representation of the Tree
pub(super) struct Dir {
    entries: IndexMap<OsString, DirEntry>,
}

impl Dir {
    pub(super) fn new() -> Self {
        Self::default()
    }
    
    // This is method is called for every path returned by workspace::list_files()
    //
    // We have 2 paths: src/main.rs and src/database/tree.rs
    // The first time we call insert, we create the directory 'src' and then because main.rs is the
    // actual file we push it as a file entry for 'src'
    // The second time, src already exists, we don't create the directory again, but we retrieve for
    // it its files/subdirectories. Now because 'database' does not exist as a subdirectory for 'src'
    // we create it first and then set it as the new root/parent. 'tree.rs' is a file so we add as
    // a file in the database directory.
    //
    // By the time we are done, we have the in memory representation of the tree. Next we have to
    // take this in memory representation and store it in the disk
    //
    // Builds the in-memory representation of a tree
    //
    // After sorting paths we want to create a nested structure that represents a directory so then
    // map the directory's entries to Vec<Entry>.
    //
    // Example:
    //  After sorting the paths from workspace::list_files(), we have:
    //
    //  Entries of root dir:
    //      foo-bar.txt
    //      foo/a.txt
    //      main.rs
    //      src/bar/test.rs
    //      src/foo/hello.rs
    //      src/foo/test.rs
    //
    // What we observe after sorting is that this is the order we want to have.
    //
    // root entries -> Vec<Entry> [foo-bar.txt, foo, main.rs, src]
    // All we have to do is maintain the insertion order as we build Dir. Then all we have to do is
    // walk the directory and map DirEntry to TreeEntry.
    //
    // Instead of sorting + IndexMap we could use a BTreeMap. BTreeMap does not allow us to provide
    // a custom comparator. It sorts by the key’s Ord implementation. So we would need a custom key
    // type and impl Ord for it.
    pub(super) fn add(&mut self, path: &Path, blob_id: [u8; 20], mode: u32) {
        let mut entries = &mut self.entries;
        let mut components = path.iter().peekable();

        while let Some(component) = components.next() {
            let name = component.to_os_string();

            // If peek().is_none() is true, the current component is the final component of the path.
            // Since paths always point to files, the final component is guaranteed to be a file.
            if components.peek().is_none() {
                entries.insert(name, DirEntry::File { blob_id, mode });
            } else {
                // the current component is a directory -> retrieve its entries
                entries = match entries.entry(name) {
                    // directory already exists return a mutable reference to it
                    MapEntry::Occupied(entry) => match entry.into_mut() {
                        // we can't have a subdirectory and a file with the same name in the same
                        // directory, it is guaranteed by the OS
                        DirEntry::File { .. } => {
                            unreachable!("path conflict: expected directory, found file");
                        }
                        DirEntry::Dir(dir) => dir,
                    },
                    // directory does not exist, create it and return a mutable reference
                    MapEntry::Vacant(entry) => match entry.insert(DirEntry::Dir(IndexMap::new())) {
                        DirEntry::File { .. } => unreachable!(),
                        DirEntry::Dir(dir) => dir,
                    },
                }
            }
        }
    }

    // Builds the Merkle Tree.
    //
    // First we hash the leaves. For each file in the workspace, read its contents, compute the
    // blob's hash, store the blob keyed by that hash. Every file has an oid now.
    // These are the leaf hashes of the Merkle tree. This step is done in Commit::execute()
    //
    // Next we take the flat list of paths and arrange them into a nested in-memory tree mirroring
    // the directory layout. No hashing happens here yet, this is purely about shape. This is the
    // logic of Dir::insert()
    //
    // Last we hash interior nodes bottom-up. Walk the in-memory structure in post-order
    // At each tree node:
    //  Each blob child already has its hash (from Step 1).
    //  Each subtree child gets recursively visited first, which produces its hash.
    //  Once all children have hashes, serialize the current tree, hash that serialization, store the
    //  tree object keyed by the hash, return the hash to the parent(This is done via the closure that
    //  calls db.store()).
    pub(super) fn into_tree<F>(self, f: &F) -> [u8; 20]
    where
        // Fn and not FnMut
        // the only captured value of the closure is a Database instance, we only call db.store()
        // and store() takes &self not &mut self
        F: Fn(Vec<TreeEntry>) -> [u8; 20],
    {
        let mut entries = Vec::new();
        for (name, entry) in self.entries {
            match entry {
                DirEntry::File { blob_id, mode } => entries.push(TreeEntry {
                    id: blob_id,
                    name,
                    mode,
                }),
                DirEntry::Dir(dir) => {
                    let child = Dir { entries: dir };
                    // get the child hash to build the parent
                    let child_id = child.into_tree(f);
                    entries.push(TreeEntry {
                        id: child_id,
                        name,
                        // safe to hardcode MODE, we don't need to call stat on the directory
                        // it is a component of a filepath
                        mode: MODE,
                    });
                }
            }
        }
        f(entries)
    }
}

//toDo: from_index()