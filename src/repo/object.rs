use crate::repo::config::{ConfigFile, ConfigFileError};
use crate::repo::object::mode::Mode;
use crate::repo::object::oid::Oid;
use crate::repo::object::parse::ParseError;
use crate::repo::timestamp::Timestamp;
use std::env::{self, VarError};
use std::error::Error;
use std::ffi::OsString;
use std::fmt;

pub(crate) mod mode;
pub(crate) mod oid;
mod parse;

pub(crate) struct Entry {
    pub(crate) mode: Mode,
    // read the notes "Filenames"
    pub(crate) name: Vec<u8>,
    pub(crate) oid: Oid,
}

pub(crate) struct Commit {
    pub(crate) author: Signature,
    pub(crate) committer: Signature,
    pub(crate) parents: Vec<Oid>,
    pub(crate) message: String,
    // Everything in a commit is already plain text, the author/committer names and emails, the
    // timestamp, the blank line separator, the message. Using a hex id for the tree reference keeps
    // the entire format consistent. Using 20 raw binary bytes in the middle of an otherwise text
    // format would be weird.
    // Trees contain many entries. Using 20 raw bytes instead of 40 hex chars saves 50% per id,
    // which adds up significantly. Saving 20 bytes per commit is negligible. Saving 20 bytes per
    // entry in a tree with thousands of entries matters.
    pub(crate) root_id: Oid,
}

// https://git-scm.com/book/en/v2/Git-Internals-Environment-Variables
pub(crate) struct Signature {
    pub(crate) email: String,
    pub(crate) name: String,
    pub(crate) timestamp: Timestamp,
}

impl Signature {
    pub(crate) fn author(cfg: &ConfigFile) -> Result<Self, SignatureError> {
        let name = match env::var("GIT_AUTHOR_NAME") {
            Ok(name) => name,
            Err(err) => match err {
                VarError::NotPresent => {
                    get_with_fallback(cfg, "author.name", "user.name", "author name")?
                }
                VarError::NotUnicode(s) => {
                    return Err(SignatureError::EnvNotUnicode {
                        var: "GIT_AUTHOR_NAME",
                        value: s,
                    });
                }
            },
        };

        let email = match env::var("GIT_AUTHOR_EMAIL") {
            Ok(email) => email,
            Err(err) => match err {
                VarError::NotPresent => {
                    get_with_fallback(cfg, "author.email", "user.email", "author email")?
                }
                VarError::NotUnicode(s) => {
                    return Err(SignatureError::EnvNotUnicode {
                        var: "GIT_AUTHOR_EMAIL",
                        value: s,
                    });
                }
            },
        };

        Ok(Self {
            name,
            email,
            timestamp: Timestamp::now(),
        })
    }

    pub(crate) fn committer(cfg: &ConfigFile) -> Result<Self, SignatureError> {
        let name = match env::var("GIT_COMMITTER_NAME") {
            Ok(name) => name,
            Err(err) => match err {
                VarError::NotPresent => {
                    get_with_fallback(cfg, "committer.name", "user.name", "commiter name")?
                }
                VarError::NotUnicode(s) => {
                    return Err(SignatureError::EnvNotUnicode {
                        var: "GIT_COMMITTER_NAME",
                        value: s,
                    });
                }
            },
        };
        let email = match env::var("GIT_COMMITTER_EMAIL") {
            Ok(email) => email,
            Err(err) => match err {
                VarError::NotPresent => {
                    get_with_fallback(cfg, "committer.email", "user.email", "committer email")?
                }
                VarError::NotUnicode(s) => {
                    return Err(SignatureError::EnvNotUnicode {
                        var: "GIT_COMMITTER_EMAIL",
                        value: s,
                    });
                }
            },
        };

        Ok(Self {
            name,
            email,
            timestamp: Timestamp::now(),
        })
    }
}

fn get_with_fallback(
    cfg: &ConfigFile,
    key: &'static str,
    fallback: &'static str,
    name: &'static str,
) -> Result<String, SignatureError> {
    let value = match cfg.get_str(key) {
        Ok(name) => Some(name),
        Err(err) if err.is_key_not_found() => None,
        Err(err) => return Err(SignatureError::ConfigError(err)),
    };

    match value {
        Some(value) => Ok(value),
        None => match cfg.get_str(fallback) {
            Ok(value) => Ok(value),
            Err(err) if err.is_key_not_found() => Err(SignatureError::NotFound(name)),
            Err(err) => Err(SignatureError::ConfigError(err)),
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObjectType {
    Blob,
    Tree,
    Commit,
}

impl ObjectType {
    pub(crate) fn try_from_str(val: &str) -> Option<Self> {
        match val {
            "blob" => Some(Self::Blob),
            "tree" => Some(Self::Tree),
            "commit" => Some(Self::Commit),
            _ => None,
        }
    }
}

impl fmt::Display for ObjectType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // when it is a literal we can write it directly without write!(f, "{}", "blob")
            Self::Blob => write!(f, "blob"),
            Self::Tree => write!(f, "tree"),
            Self::Commit => write!(f, "commit"),
        }
    }
}

pub(crate) enum Object {
    Blob(Vec<u8>),
    Tree(Vec<Entry>),
    Commit(Commit),
}

impl Object {
    pub(crate) fn obj_type(&self) -> ObjectType {
        match self {
            Self::Blob(_) => ObjectType::Blob,
            Self::Tree(_) => ObjectType::Tree,
            Self::Commit(_) => ObjectType::Commit,
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
                    bytes.extend_from_slice(entry.oid.as_bytes());
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
                bytes.extend_from_slice(commit.root_id.to_hex().as_bytes());
                bytes.extend_from_slice(b"\n");
                for parent in &commit.parents {
                    bytes.extend_from_slice(b"parent ");
                    bytes.extend_from_slice(parent.to_hex().as_bytes());
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
                bytes.extend_from_slice(b"\n\n");
                bytes.extend_from_slice(commit.message.as_bytes());
                bytes
            }
        }
    }
}

#[derive(Debug)]
pub(crate) enum SignatureError {
    NotFound(&'static str),
    EnvNotUnicode { var: &'static str, value: OsString },
    ConfigError(ConfigFileError),
}

impl Error for SignatureError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ConfigError(source) => Some(source),
            Self::NotFound(_) => None,
            Self::EnvNotUnicode { .. } => None,
        }
    }
}

impl fmt::Display for SignatureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConfigError(_) => write!(f, "bad config"),
            Self::NotFound(name) => write!(f, "{name} is not set"),
            Self::EnvNotUnicode { var, value } => {
                write!(
                    f,
                    "environment variable {var} was not valid Unicode: {value:?}",
                )
            }
        }
    }
}

impl From<ConfigFileError> for SignatureError {
    fn from(err: ConfigFileError) -> Self {
        SignatureError::ConfigError(err)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OidError {
    BadDigit { pos: usize, digit: u8 },
    BadLength,
}

impl Error for OidError {}

impl fmt::Display for OidError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OidError::BadDigit { pos, digit } => {
                write!(
                    f,
                    "invalid hexadecimal digit '{}' at position {pos}",
                    char::from(*digit)
                )
            }

            OidError::BadLength => {
                write!(f, "object id must be 40 hexadecimal characters")
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct HexError {
    digit: u8,
    pos: usize,
}
