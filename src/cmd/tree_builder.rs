use std::path::Path;
use crate::cmd::object::{Object, TreeEntry};
use indexmap::IndexMap;
use indexmap::map::Entry as MapEntry;
use crate::cmd::db;
use crate::cmd::error::DbError;
use crate::cmd::index::Index;

const MODE: u32 = 0o40000;

enum TreeNode {
    Blob { oid: [u8; 20], mode: u32 },
    Tree(IndexMap<Vec<u8>, TreeNode>),
}

#[derive(Default)]
// in memory representation of the Tree
pub(super) struct InMemTree {
    entries: IndexMap<Vec<u8>, TreeNode>,
}

impl InMemTree {
    fn new() -> Self {
        Self::default()
    }

    pub(super) fn from_index(index: Index) -> Self {
        let mut builder = Self::new();

        for entry in index.entries {
            builder.insert(entry.path, entry.oid, entry.stat.mode);
        }

        builder
    }

    // We have 2 paths: src/main.rs and src/database/tree.rs
    // The first time we call insert, we create the directory 'src' and then because main.rs is the
    // actual file we push it as a Blob entry for 'src'
    // The second time, src already exists, we don't create the directory again, but we retrieve for
    // it its entries. Now because 'database' does not exist as a subdirectory for 'src'
    // we create it first and then set it as the new root/parent. 'tree.rs' is a file so we add as
    // a blob in the database directory.
    //
    // By the time we are done, we have the in memory representation of the tree.
    //
    // Builds the in-memory representation of a tree
    //
    // Instead of sorting + IndexMap we could use a BTreeMap. BTreeMap does not allow us to provide
    // a custom comparator. It sorts by the key’s Ord implementation. So we would need a custom key
    // type and impl Ord for it.
    fn insert(&mut self, path: Vec<u8>, oid: [u8; 20], mode: u32) {
        let mut entries = &mut self.entries;
        let mut components = path.split(|&b| b == b'/').peekable();

        while let Some(component) = components.next() {
            let name = component.to_vec();
            // If peek().is_none() is true, the current component is the final component of the path.
            // Since paths always point to files, the final component is guaranteed to be a file.
            if components.peek().is_none() {
                entries.insert(name, TreeNode::Blob { oid, mode });
            } else {
                // the current component is a directory -> retrieve its entries
                entries = match entries.entry(name) {
                    // directory already exists return a mutable reference to it
                    MapEntry::Occupied(entry) => match entry.into_mut() {
                        // we can't have a subdirectory and a file with the same name in the same
                        // directory, it is guaranteed by the OS
                        TreeNode::Blob { .. } => {
                            unreachable!("path conflict: expected directory, found file");
                        }
                        TreeNode::Tree(dir) => dir,
                    },
                    // directory does not exist, create it and return a mutable reference
                    MapEntry::Vacant(entry) => match entry.insert(TreeNode::Tree(IndexMap::new())) {
                        TreeNode::Blob { .. } => unreachable!(),
                        TreeNode::Tree(dir) => dir,
                    },
                }
            }
        }
    }

    // Builds the Merkle Tree.
    //
    // First we hash the leaves. For each file, read its contents, compute the blob's hash, store
    // the blob keyed by that hash. Every file has an oid now. These are the leaf hashes of the
    // Merkle tree. This step is done by the Index.
    //
    // Next we walk the list of entries and arrange them into a nested in-memory tree mirroring
    // the tree layout. No hashing happens here yet, this is purely about shape. This is the
    // logic of TreeBuilder::insert()
    //
    // Last we hash interior nodes bottom-up. Walk the in-memory structure in post-order
    // At each tree node:
    //  Each blob child already has its hash
    //  Each subtree child gets recursively visited first, which produces its hash.
    //  Once all children have hashes, serialize the current tree, hash that serialization, store the
    //  tree object keyed by the hash, return the hash to the parent(This is done via the closure that
    //  calls db.store()).
    //
    // Example:
    //
    // Index entries:
    //  foo-bar.txt          -> blob A, mode 100644
    //  foo/a.txt            -> blob B, mode 100644
    //  main.rs              -> blob C, mode 100644
    //  src/bar/test.rs      -> blob D, mode 100644
    //  src/foo/hello.rs     -> blob E, mode 100644
    //  src/foo/test.rs      -> blob F, mode 100644
    //
    // After building the InMemTree it looks like this:
    //  root
    //  ├── foo-bar.txt         blob A
    //  ├── foo
    //  │    └── a.txt          blob B
    //  ├── main.rs             blob C
    //  └── src
    //       ├── bar
    //       │   └── test.rs    blob D
    //       └── foo
    //           ├── hello.rs   blob E
    //           └── test.rs    blob F
    //
    // We start from the root, and we walk the tree bottom up
    //  foo-bar.txt -> leaf return the id to the parent
    //  foo -> tree, recurse
    //      a.txt -> leaf return the id to the parent
    //  for foo/ now we can store entries:
    //      vec![
    //          TreeEntry {
    //              name: "a.txt",
    //              oid: B,
    //              mode: 100644,
    //          },
    //      ]
    //
    // Once we are done we have:
    //  src/
    //
    //  vec![
    //     TreeEntry {
    //         name: "bar",
    //         oid: T_src_bar,
    //         mode: 40000,
    //     },
    //     TreeEntry {
    //         name: "foo",
    //         oid: T_src_foo,
    //         mode: 40000,
    //     },
    // ]
    //
    // root/
    //
    //  vec![
    //     TreeEntry {
    //         name: "foo-bar.txt",
    //         oid: A,
    //         mode: 100644,
    //     },
    //     TreeEntry {
    //         name: "foo",
    //         oid: T_foo,
    //         mode: 40000,
    //     },
    //     TreeEntry {
    //         name: "main.rs",
    //         oid: C,
    //         mode: 100644,
    //     },
    //     TreeEntry {
    //         name: "src",
    //         oid: T_src,
    //         mode: 40000,
    //     },
    //  ]
    //
    //
    // If the process fails midway we don't have to roll back tree objects that were successfully
    // written. Objects are content-addressed(.lit/objects/<hash>) and immutable. If we store a
    // subtree and then later fail before updating HEAD that subtree becomes an unreachable object.
    //
    // When we successfully write an object, nothing in the repository history points to it yet
    // unless a commit references it, and nothing points to that commit unless HEAD or a branch
    // reference is updated.
    //
    // 1. Store subtree for src                OK
    // 2. Store root tree                      FAIL
    // 3. Store commit                         not attempted
    // 4. Update HEAD                          not attempted
    //
    // Because step 2 failed, we never created the final commit and never moved HEAD, so HEAD still
    // points to the old commit. The new objects exist on disk, but are unreachable. Unreachable
    // means: starting from HEAD, following commit -> tree -> subtree -> blob, there is no path to
    // those objects.
    // HEAD -> old commit -> old tree -> old blobs
    //
    // That is why rollback is not required. The visible state of the repository is controlled by
    // refs like HEAD, not by the presence of loose objects in .lit/objects.
    pub(super) fn write(self, db_path: &Path) -> Result<[u8; 20], DbError> {
        let mut entries = Vec::new();

        for (name, node) in self.entries {
            let (oid, mode) = match node {
                TreeNode::Blob { oid, mode } => (oid, mode),
                TreeNode::Tree(dir) => {
                    let child_oid = InMemTree { entries: dir }.write(db_path)?;
                    (child_oid, MODE)
                }
            };
            entries.push(TreeEntry { oid, name, mode });
        }
        db::store(db_path, Object::Tree(entries))
    }
}