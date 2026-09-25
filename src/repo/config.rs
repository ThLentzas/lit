use crate::repo::config::doc::{ConfigDoc, ConfigDocError, ConfigKey, SectionKey};
use crate::repo::os::{self, OsPath};
use std::borrow::Cow;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::{fmt, result};

mod doc;
pub(super) mod parse;

pub(crate) struct VariableEntry<'file> {
    name: &'file [u8],
    value: Value<'file>,
}

impl<'file> VariableEntry<'file> {
    pub(crate) fn name(&self) -> &[u8] {
        self.name
    }

    pub(crate) fn value(&self) -> &Value<'_> {
        &self.value
    }

    pub(crate) fn into_value(self) -> Value<'file> {
        self.value
    }
}

pub(crate) struct ConfigEntry<'file> {
    // in the output we need the whole key, not just the name of the variable.
    key: ConfigKey,
    value: Value<'file>,
}

impl<'file> ConfigEntry<'file> {
    pub(crate) fn key(&self) -> &ConfigKey {
        &self.key
    }
    pub(crate) fn value(&self) -> &Value<'_> {
        &self.value
    }
}

// read doc.rs::interpret_value() on why we use Cow
pub(crate) enum Value<'a> {
    // valueless
    // not just boolean, because boolean can also mean false
    ImplicitlyTrue,
    Bytes(Cow<'a, [u8]>),
}

impl<'a> Value<'a> {
    pub(crate) fn to_bool(&self) -> Option<bool> {
        match self {
            Value::ImplicitlyTrue => Some(true),
            Value::Bytes(bytes) => {
                if bytes.as_ref() == b"true" {
                    Some(true)
                } else if bytes.as_ref() == b"false" {
                    Some(false)
                } else {
                    None
                }
            }
        }
    }
}

// the .gitconfig which the global file Git looks for any configuration is created lazily on first
// write, unlike .git/config which is created when we call init
// TODO: global, system, Read Chapter 25.2.3 and 25.3
// TODO: --list silently ignores sections that have no variables
// TODO: should the name/values just be anything that impl AsRef<OsStr> instead of the caller having
//  to invoke as_ref() for every argument?
pub(crate) struct ConfigFile {
    path: OsPath,
    doc: ConfigDoc,
}

// TODO:
//  From docs:
//      This command(config) will fail with non-zero status upon error. Some exit codes are:
//          The section or key is invalid (ret=1),
//          no section or name was provided (ret=2),
//          the config file is invalid (ret=3),
//          the config file cannot be written (ret=4),
//          you try to unset an option which does not exist (ret=5),
//          you try to unset/set an option for which multiple lines match (ret=5), or
//          you try to use an invalid regexp (ret=6).
impl ConfigFile {
    pub(crate) fn new(path: OsPath) -> Result<Self> {
        let doc = match ConfigDoc::load(&path) {
            Ok(doc) => doc,
            Err(err) => {
                return Err(ConfigFileError {
                    path,
                    kind: ConfigFileErrorKind::Doc(err),
                });
            }
        };
        Ok(Self { path, doc })
    }

    // creates an in-memory empty ConfigFile, we associate that file with a path
    pub(crate) fn empty(path: OsPath) -> Self {
        Self {
            path,
            doc: ConfigDoc::empty(),
        }
    }

    pub(crate) fn new_or_empty(path: OsPath) -> Result<Self> {
        let doc = match ConfigDoc::load(&path) {
            Ok(doc) => doc,
            Err(err) if err.is_io_not_found() => return Ok(Self::empty(path)),
            Err(err) => {
                return Err(ConfigFileError {
                    path,
                    kind: ConfigFileErrorKind::Doc(err),
                });
            }
        };
        Ok(Self { path, doc })
    }

    // The api for retrieving values is designed as follows:
    //  - when config is invoked as a command with get what we return is an interpreted value. It is
    //  then displayed with the same logic as status.
    //  - when other commands need values from config it is up to the caller to invoke one of the
    //  typed functions based on their requirements. For example, commit needs the user's information
    //  In this case, the caller invokes get_str() because name/email are human-readable.
    //
    // name is an &OsStr because subsection can contain pretty much anything
    //
    // TODO: when we impl Printer for Config the bytes returned by interpret_value() are returned
    //  verbatim, no special handling like RepoPaths. Git prints the bytes raw then a new line. Probably
    //  should do some handling for non printable characters? In Git if the value contains NUL everything
    //  after is dropped during printing. All the bytes are printed as is, no octal, no escaping,
    //  no quoting
    pub(crate) fn get(&self, name: &OsStr) -> Result<ConfigEntry<'_>> {
        let key = ConfigKey::from_name(name).ok_or_else(|| ConfigFileError {
            path: self.path.clone(),
            kind: ConfigFileErrorKind::BadKey(name.to_os_string()),
        })?;

        match self.doc.key_positions(&key) {
            Some(positions) if positions.single() => {
                // when Value is Cow::Borrowed the lifetime is tied to self, in this case doc, and doc lives
                // in Config which lives enough so we can print for example the output.
                let value = self.doc.value_at(positions.first());
                let entry = ConfigEntry { key, value };

                Ok(entry)
            }
            // By default, Git will not replace any key with multiple occurrences
            // it does not matter if they are on the same block or separate
            // two [core] blocks each with editor or 1 [core] block with multiple editor keys, both
            // will be rejected with a message:  cannot overwrite multiple values with a single value
            Some(_) => Err(ConfigFileError {
                path: self.path.clone(),
                kind: ConfigFileErrorKind::MultipleValues(name.to_os_string()),
            }),
            None => Err(ConfigFileError {
                path: self.path.clone(),
                kind: ConfigFileErrorKind::NotFound(name.to_os_string()),
            }),
        }
    }

    // multivalue key, not all variables of a section
    pub(crate) fn get_all(&self, name: &OsStr) -> Result<Vec<Value<'_>>> {
        let key = ConfigKey::from_name(name).ok_or_else(|| ConfigFileError {
            path: self.path.clone(),
            kind: ConfigFileErrorKind::BadKey(name.to_os_string()),
        })?;
        let positions = self
            .doc
            .key_positions(&key)
            .ok_or_else(|| ConfigFileError {
                path: self.path.clone(),
                kind: ConfigFileErrorKind::NotFound(name.to_os_string()),
            })?;
        let mut values = Vec::with_capacity(positions.len());

        for &position in positions {
            values.push(self.doc.value_at(position))
        }

        Ok(values)
    }

    pub(crate) fn get_str(&self, name: &OsStr) -> Result<String> {
        // TODO: verify against Git if not found is an err,
        let entry = self.get(name)?;
        match entry.value {
            // valueless boolean is type mismatch
            Value::ImplicitlyTrue => Err(ConfigFileError {
                path: self.path.clone(),
                kind: ConfigFileErrorKind::IncompatibleType {
                    key: name.to_os_string(),
                    value: None,
                    expected: "string",
                },
            }),
            Value::Bytes(bytes) => {
                String::from_utf8(bytes.to_vec()).map_err(|err| {
                    ConfigFileError {
                        path: self.path.clone(),
                        kind: ConfigFileErrorKind::IncompatibleType {
                            key: name.to_os_string(),
                            // returns back the bytes that it attempted to parse and failed so we can avoid
                            // the clone call
                            value: Some(err.into_bytes()),
                            expected: "string",
                        },
                    }
                })
            }
        }
    }

    // TODO: should we consider negative values?
    pub fn get_int(&self, name: &OsStr) -> Result<u64> {
        let entry = self.get(name)?;

        match entry.value {
            // In C, this probably returns 1
            Value::ImplicitlyTrue => Err(ConfigFileError {
                path: self.path.clone(),
                kind: ConfigFileErrorKind::IncompatibleType {
                    key: name.to_os_string(),
                    value: None,
                    expected: "numeric",
                },
            }),
            Value::Bytes(bytes) => {
                let mut val = 0u64;
                // atoi
                for &byte in bytes.as_ref() {
                    // wrapping sub will always return a numeric value above 9
                    // '0' - '9' map to [48, 57]
                    // any value less than 48 will be negative but with wrapping_sub() the value
                    // wraps to 208-255
                    // any value above 57, lands in 10-207
                    // a single check for anything above 9 is enough
                    let digit = byte.wrapping_sub(b'0');
                    if digit > 9 {
                        // to_vec() allocates and copies in both cases
                        // into_owned() moves the Vec
                        return Err(ConfigFileError {
                            path: self.path.clone(),
                            kind: ConfigFileErrorKind::IncompatibleType {
                                key: name.to_os_string(),
                                value: None,
                                expected: "numeric",
                            },
                        });
                    }

                    // git supports certain suffixes like k = 1024, m = 1048576(1024 * 1024),
                    // g = 1073741824(1024 * 1024 * 1024) we don't
                    let Some(next) = val
                        .checked_mul(10)
                        .and_then(|n| n.checked_add(digit as u64))
                    else {
                        return Err(ConfigFileError {
                            path: self.path.clone(),
                            kind: ConfigFileErrorKind::IncompatibleType {
                                key: name.to_os_string(),
                                value: None,
                                expected: "numeric",
                            },
                        });
                    };
                    val = next
                }
                Ok(val)
            }
        }
    }

    // Git reports something like: <path> bad boolean config value 'foo' for 'core.logallrefupdates'
    pub(crate) fn get_bool(&self, name: &OsStr) -> Result<bool> {
        // TODO: verify against Git if not found is an err,
        let entry = self.get(name)?;
        match entry.value {
            Value::ImplicitlyTrue => Ok(true),
            Value::Bytes(bytes) => match bytes.as_ref() {
                b"true" => Ok(true),
                b"false" => Ok(false),
                bytes => Err(ConfigFileError {
                    path: self.path.clone(),
                    kind: ConfigFileErrorKind::IncompatibleType {
                        key: name.to_os_string(),
                        value: Some(bytes.to_vec()),
                        expected: "bool",
                    },
                }),
            },
        }
    }

    // Result<&[u8], ..> won't work because we call bytes.as_ref() and we would return a reference to
    // the owned variant of Cow which is lives in entry that gets dropped when get_bytes() return
    // Result<Cow<'_, [u8], ..> moves out Cow from entry(this confused me at the start) it does not
    // borrow anything, the caller gets ownership and can call as_ref()
    pub(crate) fn get_bytes(&self, name: &OsStr) -> Result<Cow<'_, [u8]>> {
        let entry = self.get(name)?;
        match entry.value {
            Value::ImplicitlyTrue => Err(ConfigFileError {
                path: self.path.clone(),
                kind: ConfigFileErrorKind::IncompatibleType {
                    key: name.to_os_string(),
                    value: None,
                    expected: "any byte",
                },
            }),
            Value::Bytes(bytes) => Ok(bytes),
        }
    }

    pub(crate) fn set(&mut self, name: &OsStr, value: &OsStr) -> Result<()> {
        let key = ConfigKey::from_name(name).ok_or_else(|| ConfigFileError {
            path: self.path.clone(),
            kind: ConfigFileErrorKind::BadKey(name.to_os_string()),
        })?;
        let value = os::os_str_as_bytes(value);

        match self.doc.key_positions(&key) {
            Some(positions) if positions.single() => {
                self.doc.replace_value(positions.first(), &value);
            }
            // By default, Git will not replace any key with multiple occurrences
            // it does not matter if they are on the same block or separate
            // two [core] blocks each with editor or 1 [core] block with multiple editor keys, both
            // will be rejected with a message:  cannot overwrite multiple values with a single value
            Some(_) => {
                return Err(ConfigFileError {
                    path: self.path.clone(),
                    kind: ConfigFileErrorKind::MultipleValues(name.to_os_string()),
                });
            }
            None => {
                self.doc.insert_variable(&key, &value);
            }
        }
        Ok(())
    }

    // logic is identical to set()
    //  - no occurrences: insert one new variable
    //  - one occurrence: replace it,
    //  - multiple: replace them all with the new value
    pub(crate) fn set_all(&mut self, name: &OsStr, value: &OsStr) -> Result<()> {
        let value = os::os_str_as_bytes(value);
        let key = ConfigKey::from_name(name).ok_or_else(|| ConfigFileError {
            path: self.path.clone(),
            kind: ConfigFileErrorKind::BadKey(name.to_os_string()),
        })?;

        // The code below won't work because key_positions() returns &NonEmpty<VariablePos> which ties
        // the lifetime of the return ref to self, in this case we have an active immutable borrow
        // to self.doc. Then inside the loop we call self.doc.replace() which takes a mutable borrow
        // to self, causing conflict. positions.copied() wouldn't solve it either inside the loop
        // because now VariablePos is owned but positions is still borrowing from self which is active
        // inside the loop so that still fails.
        //
        // A cleaner solution that avoids the allocation is to move the entire logic to self.doc and
        // use disjoint field borrowing. key_positions() borrow from self.doc.index.keys and
        // self.doc.replace_value() mutates only self.doc.lines, nothing overlaps but the borrow checker
        // can't know that.
        //
        // match self.doc.key_positions(&key) {
        //  Some(positions) => {
        //      for position in positions {
        //          self.doc.replace_value(*position, &value);
        //      }
        //  }
        let positions = self
            .doc
            .key_positions(&key)
            .map(|positions| positions.into_iter().copied().collect::<Vec<_>>());
        match positions {
            Some(positions) => {
                for position in positions {
                    self.doc.replace_value(position, &value);
                }
            }
            None => {
                self.doc.insert_variable(&key, &value);
            }
        }
        Ok(())
    }

    pub(crate) fn unset(&mut self, name: &OsStr) -> Result<()> {
        // TODO: make a method for this. It happens to every call that works with ConfigKey
        let key = ConfigKey::from_name(name).ok_or_else(|| ConfigFileError {
            path: self.path.clone(),
            kind: ConfigFileErrorKind::BadKey(name.to_os_string()),
        })?;
        match self.doc.key_positions(&key) {
            Some(positions) if positions.single() => {
                self.doc.remove_line(positions.first());
            }
            Some(_) => {
                return Err(ConfigFileError {
                    path: self.path.clone(),
                    kind: ConfigFileErrorKind::MultipleValues(name.to_os_string()),
                });
            }
            None => {}
        }

        Ok(())
    }

    pub(crate) fn unset_all(&mut self, name: &OsStr) -> Result<()> {
        let key = ConfigKey::from_name(name).ok_or_else(|| ConfigFileError {
            path: self.path.clone(),
            kind: ConfigFileErrorKind::BadKey(name.to_os_string()),
        })?;
        let positions = self
            .doc
            .key_positions(&key)
            .map(|positions| positions.into_iter().copied().collect::<Vec<_>>());
        if let Some(positions) = positions {
            for position in positions {
                self.doc.remove_line(position)
            }
        }

        Ok(())
    }

    // removes all occurrences of the section
    pub(crate) fn remove_section(&mut self, section: &OsStr) -> Result<()> {
        let section =
            SectionKey::new(os::os_str_as_bytes(section)).ok_or_else(|| ConfigFileError {
                path: self.path.clone(),
                kind: ConfigFileErrorKind::BadSectionName(section.to_os_string()),
            })?;
        Ok(self.doc.remove_section(&section))
    }

    pub(crate) fn section_entries(
        &self,
        section: &OsStr,
    ) -> Result<Option<Vec<VariableEntry<'_>>>> {
        let section =
            SectionKey::new(os::os_str_as_bytes(section)).ok_or_else(|| ConfigFileError {
                path: self.path.clone(),
                kind: ConfigFileErrorKind::BadSectionName(section.to_os_string()),
            })?;
        Ok(self.doc.section_entries(&section))
    }

    pub(crate) fn serialize(&self) -> Vec<u8> {
        self.doc.serialize()
    }
}

type Result<T> = result::Result<T, ConfigFileError>;

// TODO: should we drop the mandatory path for variants like BadSectionName, BadKey?
#[derive(Debug)]
pub(crate) struct ConfigFileError {
    path: OsPath,
    kind: ConfigFileErrorKind,
}

impl ConfigFileError {
    pub(crate) fn is_io_not_found(&self) -> bool {
        match &self.kind {
            ConfigFileErrorKind::Doc(source) => source.is_io_not_found(),
            _ => false,
        }
    }

    pub(crate) fn is_key_not_found(&self) -> bool {
        matches!(&self.kind, ConfigFileErrorKind::NotFound(_))
    }
}

impl Error for ConfigFileError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.kind {
            ConfigFileErrorKind::Doc(err) => Some(err),
            // No other nested errors
            _ => None,
        }
    }
}

impl fmt::Display for ConfigFileError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.kind)
    }
}

#[derive(Debug)]
pub(crate) enum ConfigFileErrorKind {
    Doc(ConfigDocError),
    BadKey(OsString),
    BadSectionName(OsString),
    MultipleValues(OsString),
    NotFound(OsString),
    IncompatibleType {
        key: OsString,
        // mismatch on a valueless boolean does not have a value to return
        value: Option<Vec<u8>>,
        // Don't try to add an actual_type field because a bad value does not necessarily have an
        // actual type
        expected: &'static str,
    },
}

impl fmt::Display for ConfigFileErrorKind {
    // TODO: address all those to_string_lossy()
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Doc(_) => write!(f, "could not load config"),
            Self::BadKey(key) => {
                write!(
                    f,
                    "key does not contain a section: {}",
                    key.to_string_lossy()
                )
            }
            Self::BadSectionName(value) => {
                write!(
                    f,
                    "bad section name: {}. Must be alphanumeric and/or '-'",
                    value.to_string_lossy()
                )
            }
            Self::MultipleValues(key) => {
                write!(f, "key: {} has multiple values", key.to_string_lossy())
            }
            Self::NotFound(key) => {
                write!(f, "key: {} not found", key.to_string_lossy())
            }
            Self::IncompatibleType {
                key,
                value,
                expected,
            } => {
                write!(
                    f,
                    "bad {} config value '{}' for '{}'",
                    expected,
                    // TODO: do we need to iterate and call ReadableByte?
                    String::from_utf8_lossy(&Vec::new()),
                    key.to_string_lossy()
                )
            }
        }
    }
}
