use chrono::Local;

pub(crate) enum Object {
    Blob(Vec<u8>),
    Tree(Vec<Entry>),
    Commit(Commit)
}

impl Object {
    pub(crate) fn obj_type(&self) -> &'static str {
        match self {
            Self::Blob(_) => "blob",
            Self::Tree(_) => "tree",
            Self::Commit(_) => "commit",
        }
    }

    // For blobs, we return the content, the bytes representing the file
    // For trees, we concatenate the bytes of each entry. Each entry is represented as:
    // mode, a space, the filename, a null byte, and then twenty bytes for the object id
    pub(crate) fn serialize(&self) -> Vec<u8> {
        match self {
            Self::Blob(content) => content.to_vec(),
            Self::Tree(entries) => {
                let mut bytes = Vec::new();
                // <mode> <file name>\0<20 bytes hash>
                // the hash has a fixed length of 20 bytes. There's no delimiter between that and
                // the next entry's mode because we don't need one, we just count 20 bytes and stop.
                // Whatever comes next is the start of the next entry.
                entries.into_iter().for_each(|entry| {
                    bytes.extend_from_slice(b"100644 ");
                    bytes.extend_from_slice(entry.name.as_bytes());
                    bytes.push(0);
                    bytes.extend_from_slice(&entry.id);
                });
                bytes
            }
            // tree <tree-oid-in-hex>
            // author <name> <email> <timestamp> <timezone>
            // committer <name> <email> <timestamp> <timezone>
            //
            // <message>
            Self::Commit(commit) => {
                let mut bytes = Vec::new();
                bytes.extend_from_slice(b"tree ");
                bytes.extend_from_slice(commit.tree_id.as_bytes());
                bytes.push(b'\n');
                bytes.extend_from_slice(b"author ");
                bytes.extend_from_slice(commit.author.name.as_bytes());
                bytes.push(b' ');
                bytes.extend_from_slice(commit.author.email.as_bytes());
                bytes.push(b' ');
                bytes.extend_from_slice(commit.author.timestamp.as_bytes());
                bytes.push(b'\n');
                bytes.extend_from_slice(b"committer ");
                bytes.extend_from_slice(commit.committer.name.as_bytes());
                bytes.push(b' ');
                bytes.extend_from_slice(commit.committer.email.as_bytes());
                bytes.push(b' ');
                bytes.extend_from_slice(commit.committer.timestamp.as_bytes());
                bytes.extend_from_slice(b"\n\n");
                bytes.extend_from_slice(commit.message.as_bytes());
                bytes
            }
        }
    }
}

pub(crate) struct Commit {
    pub(crate) author: Signature,
    pub(crate) committer: Signature,
    pub(crate) parent: Option<String>,
    pub(crate) message: String,
    // Everything in a commit is already plain text, the author/committer names and emails, the
    // timestamp, the blank line separator, the message. Using a hex id for the tree reference keeps
    // the entire format consistent. Using 20 raw binary bytes in the middle of an otherwise text
    // format would be weird.
    // Trees contain many entries. Using 20 raw bytes instead of 40 hex chars saves 50% per id,
    // which adds up significantly. Saving 20 bytes per commit is negligible. Saving 20 bytes per
    // entry in a tree with thousands of entries matters.
    pub(crate) tree_id: String,
}

pub(crate) struct Entry {
    pub(crate) id: [u8; 20],
    pub(crate) name: String,
}

pub(crate) struct Signature {
    pub(crate) email: String,
    pub(crate) name: String,
    // let now = Local::now();
    // let timestamp = now.timestamp();
    // let offset = now.offset();
    pub(crate) timestamp: String
}
