use crate::repo::config::file::{ConfigFile, ConfigFileError, ConfigKey, Value};
use crate::repo::os;
use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

mod file;
pub(super) mod parse;

// the .gitconfig which the global file Git looks for any configuration is created lazily on first
// write, unlike .git/config which is created when we call init
// TODO: global, system, Read Chapter 25.2.3 and 25.3
pub(crate) struct Config {
    file: ConfigFile,
}

impl Config {
    pub(crate) fn new(path: PathBuf) -> Result<Self, ConfigError> {
        let file = ConfigFile::load(&path)?;
        Ok(Self { file })
    }

    // The api for retrieving values is designed as follows:
    //  - when config is invoked as a command with get what we return is always a byte slice. The
    //  returned value is then displayed with the same logic as status.
    //  - when other commands need values from config it is up to the caller to invoke one of the
    //  typed functions based on their requirements. For example, commit needs the user's information
    //  which it can get from .config. In this case, the caller invokes get_str() because name/email
    //  are human-readable.
    //
    // name is an &OsStr because subsection can contain pretty much anything
    pub(crate) fn get(&self, name: &OsStr) -> Result<Option<Value<'_>>, ConfigError> {
        let key = ConfigKey::from_name(name).ok_or(ConfigError::BadKey(name.to_os_string()))?;
        let pos = match self.file.key_last_pos(&key) {
            Some(p) => p,
            None => return Ok(None),
        };

        Ok(Some(self.file.value_at(pos)))
    }

    // TODO: when config is invoked as a command, impl Printer
    pub(crate) fn get_str(&self, name: &OsStr) -> Result<Option<Cow<'_, str>>, ConfigError> {
        match self.get(name)? {
            // valid key not found, delegate to caller to determine what to do
            None => Ok(None),
            // valueless boolean is type mismatch
            Some(Value::ImplicitlyTrue) => Err(ConfigError::MissingValue(name.to_os_string())),
            Some(Value::Bytes(Cow::Borrowed(bytes))) => str::from_utf8(&bytes)
                .map(|s| Some(Cow::Borrowed(s)))
                .map_err(|_| ConfigError::NotUnicode {
                    key: name.to_os_string(),
                    value: bytes.to_vec(),
                }),
            Some(Value::Bytes(Cow::Owned(bytes))) => String::from_utf8(bytes)
                .map(|s| Some(Cow::Owned(s)))
                .map_err(|err| ConfigError::NotUnicode {
                    key: name.to_os_string(),
                    // returns back the bytes that attempted to parse and failed so we can avoid
                    // the clone call
                    value: err.into_bytes(),
                }),
        }
    }

    pub(crate) fn set(&mut self, name: &OsStr, value: &OsStr) -> Result<(), ConfigError> {
        let value =
            os::os_str_as_bytes(value).map_err(|_| ConfigError::BadKey(name.to_os_string()))?;
        let key = ConfigKey::from_name(name).ok_or(ConfigError::BadKey(name.to_os_string()))?;

        match self.file.key_positions(&key) {
            Some(&[index]) => Ok(self.file.replace_value(index, &value)),
            // By default, Git will not replace any key with multiple occurrences
            // it does not matter if they are on the same block or separate
            // two [core] blocks each with editor or 1 block with multiple editor keys, both will be
            // rejected with a message:  cannot overwrite multiple values with a single value
            Some([_, ..]) => Err(ConfigError::MultipleValues(name.to_os_string())),
            // TODO: can only be None at this case, because Some(&[]) is an invalid internal state
            // the key either exists 1 value or does not exist it will never exist with an empty Vec
            Some(&[]) => unreachable!(),
            None => Ok(self.file.insert_variable(&key, &value)),
        }
    }
}

#[derive(Debug)]
pub(crate) enum ConfigError {
    File(ConfigFileError),
    BadKey(OsString),
    MultipleValues(OsString),
    NotFound(OsString),
    MissingValue(OsString),
    NotUnicode { key: OsString, value: Vec<u8> },
}

impl From<ConfigFileError> for ConfigError {
    fn from(err: ConfigFileError) -> Self {
        Self::File(err)
    }
}
