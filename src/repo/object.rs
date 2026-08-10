use crate::repo::config::{Config, ConfigError};
use crate::repo::object::mode::Mode;
use crate::repo::object::parse::ParseError;
use crate::repo::timestamp::Timestamp;
use std::env::{self, VarError};
use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use crate::repo::object::oid::Oid;

pub(crate) mod mode;
mod parse;
pub(crate) mod oid;

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
    pub(crate) fn author(cfg: &Config) -> Result<Self, SignatureError> {
        let name = match env::var("GIT_AUTHOR_NAME") {
            Ok(name) => name,
            Err(err) => match err {
                VarError::NotPresent => match cfg.get_str("author.name".as_ref())? {
                    Some(name) => name,
                    None => cfg
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
                VarError::NotPresent => match cfg.get_str("author.email".as_ref())? {
                    Some(name) => name,
                    None => cfg
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

    pub(crate) fn committer(cfg: &Config) -> Result<Self, SignatureError> {
        let name = match env::var("GIT_COMMITTER_NAME") {
            Ok(name) => name,
            Err(err) => match err {
                VarError::NotPresent => match cfg.get_str("committer.name".as_ref())? {
                    Some(name) => name,
                    None => cfg
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
                VarError::NotPresent => match cfg.get_str("committer.email".as_ref())? {
                    Some(name) => name,
                    None => cfg
                        .get_str("user.email".as_ref())?
                        .ok_or(SignatureError::NotFound("committer email"))?,
                },
                // write!(f, "environment variable was not valid Unicode: {:?}", s)
                VarError::NotUnicode(s) => {
                    return Err(SignatureError::EnvNotUnicode {
                        var: "GIT_COMMITTER_EMAIL",
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
            ObjectType::Blob => write!(f, "blob"),
            ObjectType::Tree => write!(f, "tree"),
            ObjectType::Commit => write!(f, "commit")
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
    ConfigError(ConfigError),
}

impl From<ConfigError> for SignatureError {
    fn from(err: ConfigError) -> Self {
        SignatureError::ConfigError(err)
    }
}

unsafe fn pair_to_u8_unchecked(buf: &[u8; 2]) -> u8 {
    let first = to_base10_digit(buf[0]);
    let second = to_base10_digit(buf[1]);
    (first << 4) | second
}

fn pair_to_u8(buf: &[u8; 2]) -> Result<u8, HexError> {
    let first = buf[0];
    let second = buf[1];

    if !is_hex_digit(first) {
        return Err(HexError {
            digit: first,
            pos: 0,
        });
    }
    if !is_hex_digit(second) {
        return Err(HexError {
            digit: second,
            pos: 1,
        });
    }

    let first = to_base10_digit(first);
    let second = to_base10_digit(second);
    // there are a lot of ways to write the conversion
    // This is what we want: second * 16u8.pow(0) + first * 16u8.pow(1) but because 16^0 is always 0
    // and 16^1 is always 16 we can write as follows first * 16 + second
    //
    // 1 byte = [4 high] [4 bits]
    // because each hex digit is in the 0 - 15 range we can use exactly 4 bits
    // 'af' -> 'a' = 10 = 1010, 'f' = 15 = 1111, 10101111
    //
    // 1011 are the high bits 1111 are the low bits
    // first << 4 moves first into the high bits and the low bits of the number are all 0s
    // 'a' as u8 is written as 00001011 with extra padding, shifting 10110000
    // next we want to set 'f' to the low bits, we use OR
    // a OR 0 = a
    // 'f' in u8 is 00001111 so the high bits of 'a' are ORed with 0 so they stay as is and the low
    // bits of 'a' are 0s which are ORed with the low bits of 'f' and become 'f'
    Ok((first << 4) | second)
}

fn to_base10_digit(byte: u8) -> u8 {
    if byte.is_ascii_digit() {
        byte - b'0'
    } else {
        byte - b'a' + 10
    }
}

// we can't use the is_ascii_hex() from std because it includes the capital case letters and Git
// writes the hash always using lower case letters. Even if they are same in some sense, we have to
// stay case-sensitive because they produce different hashes when it comes to storing commits.
pub(crate) fn is_hex_digit(byte: u8) -> bool {
    matches!(byte, b'0'..=b'9' | b'a'..=b'f')
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
