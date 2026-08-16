use crate::repo::config::doc::{ConfigDoc, ConfigDocError, ConfigKey, Value};
use crate::repo::os;
use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

mod doc;
pub(super) mod parse;

// the .gitconfig which the global file Git looks for any configuration is created lazily on first
// write, unlike .git/config which is created when we call init
// TODO: global, system, Read Chapter 25.2.3 and 25.3
// TODO: --list silently ignores sections that have no variables
pub(crate) struct ConfigFile {
    doc: ConfigDoc,
}

impl ConfigFile {
    pub(crate) fn new(path: PathBuf) -> Result<Self, ConfigFileError> {
        let doc = ConfigDoc::load(&path)?;
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
    pub(crate) fn get(&self, name: &OsStr) -> Result<Value<'_>, ConfigFileError> {
        let key = ConfigKey::from_name(name).ok_or(ConfigFileError::BadKey(name.to_os_string()))?;
        let pos = match self.doc.key_last_pos(&key) {
            Some(p) => p,
            // TODO: ExitCode error 1
            None => return Err(ConfigFileError::NotFound(name.to_os_string())),
        };

        Ok(self.doc.value_at(pos))
    }

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
        // valid key not found, delegate to caller to determine what to do
        let value = match self.get(name) {
            Ok(value) => value,
            Err(_) => return Ok(None),
        };

        match value {
            // valueless boolean is type mismatch
            Value::ImplicitlyTrue => Err(ConfigFileError::MissingValue(name.to_os_string())),
            Value::Bytes(Cow::Borrowed(bytes)) => str::from_utf8(bytes)
                .map(|s| Some(Cow::Borrowed(s)))
                .map_err(|_| ConfigFileError::NotUnicode {
                    key: name.to_os_string(),
                    value: bytes.to_vec(),
                }),
            // we can't call str::from_utf8 in the owned case because we will have a reference to
            // a local variable(bytes) that gets dropped when get_str() returns and also this is not
            // what we want for the Owned case of Cow.
            Value::Bytes(Cow::Owned(bytes)) => String::from_utf8(bytes)
                .map(|s| Some(Cow::Owned(s)))
                .map_err(|err| ConfigFileError::NotUnicode {
                    key: name.to_os_string(),
                    // returns back the bytes that attempted to parse and failed so we can avoid
                    // the clone call
                    value: err.into_bytes(),
                }),
        }
    }

    pub(crate) fn set(
        mut self,
        name: &OsStr,
        value: &OsStr,
    ) -> Result<ModifiedConfigFile, ConfigFileError> {
        let value = os::os_str_as_bytes(value)
            .map_err(|_| ConfigFileError::BadValue(value.to_os_string()))?;
        let key = ConfigKey::from_name(name).ok_or(ConfigFileError::BadKey(name.to_os_string()))?;

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
}

// Returned after any mutation because inserting, replacing or removing lines invalidates the indexes
// built from the original line positions and spans. For example inserting a new line make the lines
// after stale because the indexes now are off. ModifiedConfig holds the new state that needs to be
// serialized and written back to disk, this is why all mutations take mut self, because we can't use
// the original config after. We could also have a reindex() method where it writes back to the file
// and reads the new version?
pub(crate) struct ModifiedConfigFile {
    doc: ConfigDoc,
}

impl ModifiedConfigFile {
    pub(crate) fn serialize(&self) -> Vec<u8> {
        self.doc.serialize()
    }
}

#[derive(Debug)]
pub(crate) enum ConfigFileError {
    Doc(ConfigDocError),
    BadKey(OsString),
    BadValue(OsString),
    MultipleValues(OsString),
    NotFound(OsString),
    MissingValue(OsString),
    NotUnicode { key: OsString, value: Vec<u8> },
}

impl From<ConfigDocError> for ConfigFileError {
    fn from(err: ConfigDocError) -> Self {
        Self::Doc(err)
    }
}
