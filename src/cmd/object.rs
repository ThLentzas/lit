use std::env;
use std::env::VarError;
use std::ffi::OsString;
use std::io::Write;
use crate::cmd::config::Config;
use crate::cmd::timestamp;

pub(super) enum Object {
    Blob(Vec<u8>),
    Tree(Vec<Entry>),
    Commit(Commit)
}

impl Object {
    pub(super) fn obj_type(&self) -> &[u8] {
        match self {
            Self::Blob(_) => b"blob",
            Self::Tree(_) => b"tree",
            Self::Commit(_) => b"commit",
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
                bytes.extend_from_slice(commit.root_id.as_bytes());
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
    pub(super) root_id: String,
}

#[derive(Debug)]
pub(super) enum SignatureError {
    NotFound(& 'static str),
    NotUnicode(OsString),
}

// https://git-scm.com/book/en/v2/Git-Internals-Environment-Variables
pub(super) struct Signature {
    pub(super) email: String,
    pub(super) name: String,
    pub(super) timestamp: String
}

impl Signature {
    pub(super) fn author(config: &Config) -> Result<Self, SignatureError> {
        let name = match env::var("GIT_AUTHOR_NAME") {
            Ok(s) => s,
            Err(err) => match err {
                VarError::NotPresent => {
                    // toDo: enforce in config that variables are always utf8 valid seq
                    let name = config.get("author.name".as_ref())
                        .or_else(|| config.get("user.name".as_ref()))
                        .ok_or(SignatureError::NotFound("author name"))?;
                    unsafe { String::from_utf8_unchecked(name.to_vec()) }
                },
                // write!(f, "environment variable was not valid unicode: {:?}", s)
                VarError::NotUnicode(s) => return Err(SignatureError::NotUnicode(s))
            }
        };

        let email = match env::var("GIT_AUTHOR_EMAIL") {
            Ok(s) => s,
            Err(err) => match err {
                VarError::NotPresent => {
                    // toDo: enforce in config that variables are always utf8 valid seq
                    let email = config.get("author.email".as_ref())
                        .or_else(|| config.get("user.email".as_ref()))
                        .ok_or(SignatureError::NotFound("author email"))?;
                    unsafe { String::from_utf8_unchecked(email.to_vec()) }
                },
                // write!(f, "environment variable was not valid unicode: {:?}", s)
                VarError::NotUnicode(s) => return Err(SignatureError::NotUnicode(s))
            }
        };

        Ok(Self { name, email, timestamp: timestamp::now() })
    }

    pub(super) fn committer(config: &Config) -> Result<Self, SignatureError> {
        let name = match env::var("GIT_COMMITTER_NAME") {
            Ok(s) => s,
            Err(err) => match err {
                VarError::NotPresent => {
                    // toDo: enforce in config that variables are always utf8 valid seq
                    let name = config.get("committer.name".as_ref())
                        .or_else(|| config.get("user.name".as_ref()))
                        .ok_or(SignatureError::NotFound("committer name"))?;
                    unsafe { String::from_utf8_unchecked(name.to_vec()) }
                },
                // write!(f, "environment variable was not valid unicode: {:?}", s)
                VarError::NotUnicode(s) => return Err(SignatureError::NotUnicode(s))
            }
        };

        let email = match env::var("GIT_COMMITTER_EMAIL") {
            Ok(s) => s,
            Err(err) => match err {
                VarError::NotPresent => {
                    // toDo: enforce in config that variables are always utf8 valid seq
                    let email = config.get("committer.email".as_ref())
                        .or_else(|| config.get("user.email".as_ref()))
                        .ok_or(SignatureError::NotFound("committer email"))?;
                    unsafe { String::from_utf8_unchecked(email.to_vec()) }
                },
                // write!(f, "environment variable was not valid unicode: {:?}", s)
                VarError::NotUnicode(s) => return Err(SignatureError::NotUnicode(s))
            }
        };

        Ok(Self { name, email, timestamp: timestamp::now() })
    }
}

pub(super) struct Entry {
    pub(super) oid: [u8; 20],
    // read the notes "Filenames"
    pub(super) name: Vec<u8>,
    pub(super) mode: u32,
}
