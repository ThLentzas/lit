use std::io::Write;
use chrono::Local;

pub(super) enum Object {
    Blob(Vec<u8>),
    Tree(Vec<Entry>),
    Commit(Commit)
}

impl Object {
    pub(super) fn obj_type(&self) -> &'static str {
        match self {
            Self::Blob(_) => "blob",
            Self::Tree(_) => "tree",
            Self::Commit(_) => "commit",
        }
    }

    // For blobs, we return the content, the bytes representing the file
    // For trees, we concatenate the bytes of each entry. Each entry is represented as:
    // mode, a space, the filename, a null byte, and then twenty bytes for the object id
    pub(super) fn serialize(&self) -> Vec<u8> {
        match self {
            Self::Blob(content) => content.to_vec(),
            Self::Tree(entries) => {
                let mut bytes = Vec::new();
                // <mode> <name>\0<20 bytes hash>
                // the hash has a fixed length of 20 bytes. There's no delimiter between that and
                // the next entry's mode because we don't need one, we just count 20 bytes and stop.
                // Whatever comes next is the start of the next entry.
                for entry in entries {
                    // we have to store the ASCII bytes of the mode so for 100644: [49, 48, 48, 54, 52, 52]
                    // convert the numeric value into textual octal digits and append those ASCII
                    // bytes to the Vec<u8>
                    // To get the textual representation in octal we do repeated division by 8
                    // for 33188(base 10) This will give us 100644 in base 10, and then we can just
                    // do b'0' + digit as in Leetcode problems. '1', '0', '0', '6', '4', '4'
                    // The output now is: [49, 48, 48, 54, 52, 52]
                    //
                    // It avoids allocation, write! formats directly into the buffer.
                    // let s = format!("{:o} ", entry.mode);
                    // bytes.extend_from_slice(s.as_bytes());
                    write!(&mut bytes, "{:o} ", entry.mode).unwrap();
                    bytes.extend(&entry.name);
                    bytes.push(0);
                    bytes.extend_from_slice(&entry.oid);
                }
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

pub(super) struct Commit {
    pub(super) author: Signature,
    pub(super) committer: Signature,
    pub(super) parent: Option<String>,
    pub(super) message: String,
    // Everything in a commit is already plain text, the author/committer names and emails, the
    // timestamp, the blank line separator, the message. Using a hex id for the tree reference keeps
    // the entire format consistent. Using 20 raw binary bytes in the middle of an otherwise text
    // format would be weird.
    // Trees contain many entries. Using 20 raw bytes instead of 40 hex chars saves 50% per id,
    // which adds up significantly. Saving 20 bytes per commit is negligible. Saving 20 bytes per
    // entry in a tree with thousands of entries matters.
    pub(super) tree_id: String,
}

pub(super) struct Signature {
    pub(super) email: String,
    pub(super) name: String,
    // let now = Local::now();
    // let timestamp = now.timestamp();
    // let offset = now.offset();
    pub(super) timestamp: String
}

pub(super) struct Entry {
    pub(super) oid: [u8; 20],
    // read the notes "Filenames"
    pub(super) name: Vec<u8>,
    pub(super) mode: u32,
}
