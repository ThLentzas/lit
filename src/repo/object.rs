use crate::repo::config::{Config, ConfigError};
use crate::repo::object::mode::Mode;
use crate::repo::object::parse::ParseError;
use crate::repo::timestamp::Timestamp;
use std::env::{self, VarError};
use std::ffi::OsString;

pub(crate) mod mode;
mod parse;

pub(crate) struct Entry {
    pub(super) mode: Mode,
    // read the notes "Filenames"
    pub(super) name: Vec<u8>,
    // TODO: should this be a type?
    pub(super) oid: [u8; 20],
}

pub(crate) enum Object {
    Blob(Vec<u8>),
    Tree(Vec<Entry>),
    Commit(Commit),
}

impl Object {
    pub(super) fn obj_type(&self) -> &str {
        match self {
            Self::Blob(_) => "blob",
            Self::Tree(_) => "tree",
            Self::Commit(_) => "commit",
        }
    }

    pub(super) fn deserialize(buf: &[u8]) -> Result<Self, ParseError> {
        parse::parse(buf)
    }

    // a mistake I made at the start was to think that I could just impl Display and then call as_bytes()
    // but it won't work. display() needs valid utf8, oid is [u8;20], path names are platform specific
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
                    bytes.extend_from_slice(entry.mode.as_octal_bytes());
                    bytes.push(b' ');
                    bytes.extend(&entry.name);
                    bytes.push(0);
                    bytes.extend_from_slice(&entry.oid);
                }
                bytes
            }
            // tree tree_oid_hex\n
            // parent parent_oid_hex\n // repeated once per parent, omitted for root commit
            // author name <email> timestamp timezone\n
            // committer name <email> timestamp timezone\n
            // \n
            // message
            //
            // The commit body is a line-oriented text format, timestamp is written as ASCII decimal
            // The same logic applies to tree as well where mode is written as ASCII not as be_bytes()
            // mode is written as be_bytes() in the Index where the format is binary, now it is text
            Self::Commit(commit) => {
                // TODO: can we clean this up using write!()?
                let mut bytes = Vec::new();
                bytes.extend_from_slice(b"tree ");
                bytes.extend_from_slice(commit.root_id.as_bytes());
                bytes.extend_from_slice(b"\n");
                for parent in &commit.parent {
                    bytes.extend_from_slice(b"parent ");
                    bytes.extend_from_slice(parent.as_bytes());
                    bytes.extend_from_slice(b"\n");
                }
                bytes.extend_from_slice(b"author ");
                bytes.extend_from_slice(commit.author.name.as_bytes());
                bytes.extend_from_slice(b" <");
                bytes.extend_from_slice(commit.author.email.as_bytes());
                bytes.extend_from_slice(b"> ");
                bytes.extend_from_slice(commit.author.timestamp.to_string().as_bytes());
                bytes.extend_from_slice(b"\ncommitter ");
                bytes.extend_from_slice(commit.committer.name.as_bytes());
                bytes.extend_from_slice(b" <");
                bytes.extend_from_slice(commit.committer.email.as_bytes());
                bytes.extend_from_slice(b"> ");
                bytes.extend_from_slice(commit.committer.timestamp.to_string().as_bytes());
                bytes.extend_from_slice(b"\n\n<");
                bytes.extend_from_slice(commit.message.as_bytes());
                bytes
            }
        }
    }
}

pub(crate) struct Commit {
    pub(crate) author: Signature,
    pub(crate) committer: Signature,
    pub(crate) parent: Vec<String>,
    pub(crate) message: String,
    // Everything in a commit is already plain text, the author/committer names and emails, the
    // timestamp, the blank line separator, the message. Using a hex id for the tree reference keeps
    // the entire format consistent. Using 20 raw binary bytes in the middle of an otherwise text
    // format would be weird.
    // Trees contain many entries. Using 20 raw bytes instead of 40 hex chars saves 50% per id,
    // which adds up significantly. Saving 20 bytes per commit is negligible. Saving 20 bytes per
    // entry in a tree with thousands of entries matters.
    pub(crate) root_id: String,
}

// https://git-scm.com/book/en/v2/Git-Internals-Environment-Variables
pub(crate) struct Signature {
    pub(super) email: String,
    pub(super) name: String,
    pub(super) timestamp: Timestamp,
}

impl Signature {
    pub(crate) fn author(config: &Config) -> Result<Self, SignatureError> {
        let name = match env::var("GIT_AUTHOR_NAME") {
            Ok(name) => name,
            Err(err) => match err {
                VarError::NotPresent => match config.get_str("author.name".as_ref())? {
                    Some(name) => name,
                    None => config
                        .get_str("user.name".as_ref())?
                        .ok_or(SignatureError::NotFound("author name"))?,
                },
                // write!(f, "environment variable was not valid Unicode: {:?}", s)
                VarError::NotUnicode(s) => {
                    return Err(SignatureError::EnvNotUnicode {
                        var: "GIT_AUTHOR_NAME",
                        value: s,
                    });
                }
            }
            .to_owned(),
        };

        let email = match env::var("GIT_AUTHOR_EMAIL") {
            Ok(email) => email,
            Err(err) => match err {
                VarError::NotPresent => match config.get_str("author.email".as_ref())? {
                    Some(name) => name,
                    None => config
                        .get_str("user.email".as_ref())?
                        .ok_or(SignatureError::NotFound("author email"))?,
                },
                // write!(f, "environment variable was not valid Unicode: {:?}", s)
                VarError::NotUnicode(s) => {
                    return Err(SignatureError::EnvNotUnicode {
                        var: "GIT_AUTHOR_EMAIL",
                        value: s,
                    });
                }
            }
            .to_owned(),
        };

        Ok(Self {
            name,
            email,
            timestamp: Timestamp::now(),
        })
    }

    pub(crate) fn committer(config: &Config) -> Result<Self, SignatureError> {
        let name = match env::var("GIT_COMMITTER_NAME") {
            Ok(name) => name,
            Err(err) => match err {
                VarError::NotPresent => match config.get_str("committer.name".as_ref())? {
                    Some(name) => name,
                    None => config
                        .get_str("user.name".as_ref())?
                        .ok_or(SignatureError::NotFound("committer name"))?,
                },
                // write!(f, "environment variable was not valid Unicode: {:?}", s)
                VarError::NotUnicode(s) => {
                    return Err(SignatureError::EnvNotUnicode {
                        var: "GIT_COMMITTER_NAME",
                        value: s,
                    });
                }
            }
            .to_owned(),
        };

        let email = match env::var("GIT_COMMITTER_EMAIL") {
            Ok(email) => email,
            Err(err) => match err {
                VarError::NotPresent => match config.get_str("committer.email".as_ref())? {
                    Some(name) => name,
                    None => config
                        .get_str("user.email".as_ref())?
                        .ok_or(SignatureError::NotFound("committer email"))?,
                },
                // write!(f, "environment variable was not valid Unicode: {:?}", s)
                VarError::NotUnicode(s) => {
                    return Err(SignatureError::EnvNotUnicode {
                        var: "GIT_COMMITTER_NAME",
                        value: s,
                    });
                }
            }
            .to_owned(),
        };

        Ok(Self {
            name,
            email,
            timestamp: Timestamp::now(),
        })
    }
}

#[derive(Debug)]
pub(crate) enum SignatureError {
    NotFound(&'static str),
    EnvNotUnicode { var: &'static str, value: OsString },
    ConfigError(ConfigError),
}

impl From<ConfigError> for SignatureError {
    fn from(err: ConfigError) -> Self {
        SignatureError::ConfigError(err)
    }
}
