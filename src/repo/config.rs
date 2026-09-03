use crate::repo::config::doc::{ConfigDoc, ConfigDocError, ConfigKey, SectionKey};
use crate::repo::os;
use std::borrow::Cow;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::{fmt, io};

mod doc;
pub(super) mod parse;

// could also be named: SectionEntry
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

    fn into_value(self) -> Value<'file> {
        self.value
    }
}

pub(crate) struct ConfigEntry<'file> {
    // in the output we need the whole key, not just the name of the variable.
    key: ConfigKey,
    value: Value<'file>,
}

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
pub(crate) struct ConfigFile {
    doc: ConfigDoc,
}

impl ConfigFile {
    pub(crate) fn new(path: &Path) -> Result<Self, ConfigFileError> {
        // TODO: This should take the root and call join for local
        let doc = ConfigDoc::load(path)?;
        Ok(Self { doc })
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
    pub(crate) fn get(&self, name: &OsStr) -> Result<ConfigEntry<'_>, ConfigFileError> {
        let key = ConfigKey::from_name(name).ok_or(ConfigFileError::BadKey(name.to_os_string()))?;
        let pos = match self.doc.key_last_pos(&key) {
            Some(p) => p,
            // TODO: ExitCode error 1
            None => return Err(ConfigFileError::NotFound(name.to_os_string())),
        };

        // when Value is Cow::Borrowed the lifetime is tied to self, in this case doc, and doc lives
        // in Config which lives enough so we can print for example the output.
        let value = self.doc.value_at(pos);
        let entry = ConfigEntry { key, value };

        Ok(entry)
    }

    // multivalue key, not all variables of a section
    pub(crate) fn get_all(&self, name: &OsStr) -> Result<Vec<Value<'_>>, ConfigFileError> {
        let key = ConfigKey::from_name(name).ok_or(ConfigFileError::BadKey(name.to_os_string()))?;
        let positions = self
            .doc
            .key_positions(&key)
            .ok_or_else(|| ConfigFileError::NotFound(name.to_os_string()))?;
        let mut values = Vec::with_capacity(positions.len());

        for &position in positions {
            values.push(self.doc.value_at(position))
        }

        Ok(values)
    }

    pub(crate) fn get_str(&self, name: &OsStr) -> Result<Option<Cow<'_, str>>, ConfigFileError> {
        // TODO: verify against Git if not found is an err,
        // valid key not found, delegate to caller to determine what to do
        let entry = match self.get(name) {
            Ok(value) => value,
            Err(_) => return Ok(None),
        };

        match entry.value {
            // valueless boolean is type mismatch
            Value::ImplicitlyTrue => Err(ConfigFileError::MissingValue(name.to_os_string())),
            Value::Bytes(Cow::Borrowed(bytes)) => str::from_utf8(bytes)
                .map(|s| Some(Cow::Borrowed(s)))
                .map_err(|_| ConfigFileError::IncompatibleType {
                    key: name.to_os_string(),
                    value: bytes.to_vec(),
                    actual_type: "string",
                }),
            // we can't call str::from_utf8 in the owned case because we will have a reference to
            // a local variable(bytes) that gets dropped when get_str() returns and also this is not
            // what we want for the Owned case of Cow.
            Value::Bytes(Cow::Owned(bytes)) => String::from_utf8(bytes)
                .map(|s| Some(Cow::Owned(s)))
                .map_err(|err| ConfigFileError::IncompatibleType {
                    key: name.to_os_string(),
                    // returns back the bytes that attempted to parse and failed so we can avoid
                    // the clone call
                    value: err.into_bytes(),
                    actual_type: "string",
                }),
        }
    }

    // TODO: should we consider negative values?
    pub fn get_int(&self, name: &OsStr) -> Result<Option<u64>, ConfigFileError> {
        let entry = match self.get(name) {
            Ok(value) => value,
            Err(_) => return Ok(None),
        };

        match entry.value {
            Value::ImplicitlyTrue => Err(ConfigFileError::MissingValue(name.to_os_string())),
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
                        return Err(ConfigFileError::IncompatibleType {
                            key: name.to_os_string(),
                            value: bytes.into_owned(),
                            actual_type: "numeric",
                        });
                    }

                    // git supports certain suffixes like k = 1024, m = 1048576(1024 * 1024),
                    // g = 1073741824(1024 * 1024 * 1024) we don't
                    let Some(next) = val
                        .checked_mul(10)
                        .and_then(|n| n.checked_add(digit as u64))
                    else {
                        return Err(ConfigFileError::IncompatibleType {
                            key: name.to_os_string(),
                            value: bytes.into_owned(),
                            actual_type: "numeric",
                        });
                    };
                    val = next
                }
                Ok(Some(val))
            }
        }
    }

    // TODO: fix
    //  as of now section_exists() is only invoked internally when trying to migrate a lit repo
    //  and the value is always [extensions] so for now str is fine but we need to change it to OsStr
    // pub(crate) fn section_exists(&self, section: &str) -> bool {
    //     let key = unsafe { SectionKey::new_unchecked(section.as_bytes(), None) };
    //     self.doc.section_exists(&key)
    // }

    pub(crate) fn set(
        mut self,
        name: &OsStr,
        value: &OsStr,
    ) -> Result<ModifiedConfigFile, ConfigFileError> {
        let key = ConfigKey::from_name(name).ok_or(ConfigFileError::BadKey(name.to_os_string()))?;
        let value = os::os_str_as_bytes(value)
            .map_err(|_| ConfigFileError::BadValue(value.to_os_string()))?;

        match self.doc.key_positions(&key) {
            Some(positions) if positions.single() => {
                self.doc.replace_value(positions.first(), &value);
            }
            // By default, Git will not replace any key with multiple occurrences
            // it does not matter if they are on the same block or separate
            // two [core] blocks each with editor or 1 [core] block with multiple editor keys, both
            // will be rejected with a message:  cannot overwrite multiple values with a single value
            Some(_) => return Err(ConfigFileError::MultipleValues(name.to_os_string())),
            None => {
                self.doc.insert_variable(&key, &value);
            }
        }
        Ok(ModifiedConfigFile { doc: self.doc })
    }

    // logic is identical to set()
    //  - no occurrences: insert one new variable
    //  - one occurrence: replace it,
    //  - multiple: replace them all with the new value
    pub(crate) fn set_all(
        mut self,
        name: &OsStr,
        value: &OsStr,
    ) -> Result<ModifiedConfigFile, ConfigFileError> {
        let value = os::os_str_as_bytes(value)
            .map_err(|_| ConfigFileError::BadValue(value.to_os_string()))?;
        let key = ConfigKey::from_name(name).ok_or(ConfigFileError::BadKey(name.to_os_string()))?;

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
        Ok(ModifiedConfigFile { doc: self.doc })
    }

    pub(crate) fn section_entries(
        &self,
        section: &str,
    ) -> Result<Option<Vec<VariableEntry<'_>>>, ConfigFileError> {
        let section = SectionKey::new(section.as_bytes())
            .ok_or(ConfigFileError::BadSectionName(section.to_owned()))?;
        Ok(self.doc.section_entries(&section))
    }
}

// Returned after any mutation because inserting, replacing or removing lines invalidates the indexes
// built from the original line positions and spans. For example inserting a new line make the lines
// after stale because the indexes now are off. ModifiedConfig holds the new state that needs to be
// serialized and written back to disk, this is why all mutations take mut self, because we can't use
// the original config after. We could also have a reindex() method where it writes back to the file
// and reads the new version? But in theory no method should invoke two consecutive mutation on the
// same config
pub(crate) struct ModifiedConfigFile {
    doc: ConfigDoc,
}

impl ModifiedConfigFile {
    pub(crate) fn serialize(&self) -> Vec<u8> {
        self.doc.serialize()
    }
}

// TODO: should config file error include the path itself where things went wrong because the caller
//  might not know from where exactly we tried to read?
#[derive(Debug)]
pub(crate) enum ConfigFileError {
    Doc(ConfigDocError),
    BadKey(OsString),
    BadValue(OsString),
    BadSectionName(String),
    MultipleValues(OsString),
    NotFound(OsString),
    MissingValue(OsString),
    IncompatibleType {
        key: OsString,
        value: Vec<u8>,
        actual_type: &'static str,
    },
}

impl ConfigFileError {
    // TODO: for now the only variant of io::ErrorKind we care is NotFound so maybe we need this
    //  to be a boolean since the caller only cares if it is NotFound or not?
    pub(crate) fn io_error_kind(&self) -> Option<io::ErrorKind> {
        match self {
            Self::Doc(ConfigDocError::Io { source, .. }) => Some(source.kind()),
            _ => None,
        }
    }
}

impl From<ConfigDocError> for ConfigFileError {
    fn from(err: ConfigDocError) -> Self {
        Self::Doc(err)
    }
}

impl Error for ConfigFileError {}

impl fmt::Display for ConfigFileError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ConfigFileError::Doc(doc) => write!(f, "{}", doc),
            ConfigFileError::BadKey(key) => {
                write!(
                    f,
                    "key does not contain a section: {}",
                    key.to_string_lossy()
                )
            }
            ConfigFileError::BadValue(value) => {
                write!(f, "bad value: {}", value.to_string_lossy())
            }
            ConfigFileError::BadSectionName(value) => {
                write!(
                    f,
                    "bad section name: {}. Must be alphanumeric and/or '-'",
                    value
                )
            }
            ConfigFileError::MultipleValues(key) => {
                write!(f, "key: {} has multiple values", key.to_string_lossy())
            }
            ConfigFileError::NotFound(key) => {
                write!(f, "key: {} not found", key.to_string_lossy())
            }
            ConfigFileError::MissingValue(key) => {
                write!(f, "missing value for: {}", key.to_string_lossy())
            }
            ConfigFileError::IncompatibleType {
                key,
                value,
                actual_type,
            } => {
                write!(
                    f,
                    "bad {} config value '{}' for '{}'",
                    actual_type,
                    // TODO: do we need to iterate and call ReadableByte?
                    String::from_utf8_lossy(&value),
                    key.to_string_lossy()
                )
            }
        }
    }
}
