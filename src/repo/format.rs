use std::cmp::PartialEq;
// TODO: if repo/mod.rs ends up being not too big we could move the logic there?
//  check the visibility of what gets exposed and where
use crate::repo::config::{ConfigFile, ConfigFileError, Value, VariableEntry};
use clap::ValueEnum;
use memchr::memmem;
use std::error::Error;
use std::fmt;

// Note: all extension names, even the unknown ones, are guaranteed to be valid Strings. Specifically
// each is restricted to ASCII alphanum characters and '-' since these are config-file variables
// names and the parser accepts only that character set
#[derive(Debug)]
pub(crate) enum Extension {
    // v0
    Noop,
    PreciousObjects,
    PartialClone,
    WorktreeConfig,
    // v1
    NoopV1,
    ObjectFormat,
    CompatObjectFormat,
    RefStorage,
    RelativeWorktrees,
    SubmodulePathConfig,
    Unknown,
}

impl Extension {
    fn name(&self) -> &'static str {
        match self {
            Extension::Noop => "noop",
            Extension::PreciousObjects => "preciousObjects",
            Extension::PartialClone => "partialClone",
            Extension::WorktreeConfig => "worktreeConfig",
            Extension::NoopV1 => "noop-v1",
            Extension::ObjectFormat => "objectFormat",
            Extension::CompatObjectFormat => "compatObjectFormat",
            Extension::RefStorage => "refStorage",
            Extension::RelativeWorktrees => "relativeWorktrees",
            Extension::SubmodulePathConfig => "submodulePathConfig",
            // TODO: this should probably change to unreachable!() because we should never call
            //  .name() for unknown.
            Extension::Unknown => "unknown",
        }
    }

    // https://github.com/git/git/blob/47ce80527c56f462cb97db4ca8125342204d3783/setup.c#L612
    fn is_v0_compatible(&self) -> bool {
        matches!(
            self,
            Extension::Noop
                | Extension::PreciousObjects
                | Extension::WorktreeConfig
                | Extension::PartialClone
                | Extension::Unknown
        )
    }

    // https://doc.rust-lang.org/std/convert/trait.From.html#when-to-implement-from
    fn from_bytes(bytes: &[u8]) -> Self {
        match bytes {
            // v0
            bytes if bytes.eq_ignore_ascii_case(b"noop") => Extension::Noop,
            bytes if bytes.eq_ignore_ascii_case(b"preciousObjects") => Extension::PreciousObjects,
            bytes if bytes.eq_ignore_ascii_case(b"partialClone") => Extension::PartialClone,
            bytes if bytes.eq_ignore_ascii_case(b"worktreeConfig") => Extension::WorktreeConfig,
            // v1
            bytes if bytes.eq_ignore_ascii_case(b"noop-v1") => Extension::NoopV1,
            bytes if bytes.eq_ignore_ascii_case(b"objectFormat") => Extension::ObjectFormat,
            bytes if bytes.eq_ignore_ascii_case(b"compatObjectFormat") => {
                Extension::CompatObjectFormat
            }
            bytes if bytes.eq_ignore_ascii_case(b"refStorage") => Extension::RefStorage,
            bytes if bytes.eq_ignore_ascii_case(b"relativeWorktrees") => {
                Extension::RelativeWorktrees
            }
            bytes if bytes.eq_ignore_ascii_case(b"submodulePathConfig") => {
                Extension::SubmodulePathConfig
            }
            _ => Extension::Unknown,
        }
    }
}

// v0 uses the default sha1 and files(implicit values)
// v1 enables extensions, allowing any combination of object format and ref storage
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FormatVersion {
    V0,
    V1,
}

impl FormatVersion {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            FormatVersion::V0 => "0",
            FormatVersion::V1 => "1",
        }
    }
}

impl TryFrom<u64> for FormatVersion {
    type Error = FormatVersionError;

    fn try_from(version: u64) -> Result<Self, Self::Error> {
        match version {
            0 => Ok(FormatVersion::V0),
            1 => Ok(FormatVersion::V1),
            _ => Err(FormatVersionError(version)),
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq, Copy, Clone, ValueEnum)]
pub(crate) enum RefFormat {
    #[default]
    Files,
    RefTable,
}

impl RefFormat {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            RefFormat::Files => "files",
            RefFormat::RefTable => "reftable",
        }
    }
}

impl TryFrom<&[u8]> for RefFormat {
    type Error = RefFormatError;

    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        match value {
            b"files" => Ok(Self::Files),
            b"reftable" => Ok(Self::RefTable),
            _ => Err(RefFormatError(value.to_vec())),
        }
    }
}

#[derive(Debug)]
pub(crate) struct RefStorage {
    ref_format: RefFormat,
    // <format>://<payload> is the syntax, format is also referred to as backend
    payload: Option<Vec<u8>>,
}

impl RefStorage {
    pub(crate) fn format(&self) -> &RefFormat {
        &self.ref_format
    }

    pub(crate) fn format_mut(&mut self) -> &mut RefFormat {
        &mut self.ref_format
    }

    pub(crate) fn payload(&self) -> Option<&Vec<u8>> {
        self.payload.as_ref()
    }

    pub(crate) fn has_payload(&self) -> bool {
        self.payload.is_some()
    }
}

impl Default for RefStorage {
    fn default() -> Self {
        Self {
            ref_format: RefFormat::Files,
            payload: None,
        }
    }
}

// TODO: should this be from_bytes() and drop TryFrom?
//  https://doc.rust-lang.org/std/convert/trait.From.html#when-to-implement-from
impl TryFrom<&[u8]> for RefStorage {
    type Error = RefFormatError;

    // the URI has the form <format>://<payload>
    // https://git-scm.com/docs/git-config#Documentation/git-config.txt-refStorage
    // https://github.com/git/git/blob/47ce80527c56f462cb97db4ca8125342204d3783/setup.c#L635-L648
    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        let (backend, payload) = match memmem::find(value, b"://") {
            Some(pos) => {
                let backend = &value[..pos];
                // if the payload is empty, reftable://, pos + 3 will be equal to len
                // Rust allows us to slice at len() and returns an empty slice
                // slice[len()..] returns an empty slice, it is not index out of bounds
                let payload = &value[pos + 3..];
                (backend, Some(payload))
            }
            None => (value, None),
        };
        let ref_format = RefFormat::try_from(backend)?;
        Ok(Self {
            ref_format,
            payload: payload.map(|p| p.to_vec()),
        })
    }
}

// https://docs.rs/clap/latest/clap/trait.ValueEnum.html
#[derive(Debug, PartialEq, Eq, Default, Copy, Clone, ValueEnum)]
pub(crate) enum ObjectFormat {
    #[default]
    Sha1,
    Sha256,
}

impl ObjectFormat {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            ObjectFormat::Sha1 => "sha1",
            ObjectFormat::Sha256 => "sha256",
        }
    }
}

impl TryFrom<&[u8]> for ObjectFormat {
    type Error = ObjectFormatError;

    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        match value {
            b"sha1" => Ok(Self::Sha1),
            b"sha256" => Ok(Self::Sha256),
            _ => Err(ObjectFormatError(value.to_vec())),
        }
    }
}

// https://git-scm.com/docs/gitrepository-layout#_git_repository_format_versions
//  format is a compatibility contract fot the repository as a whole. git needs to know that
//  it can safely read/write in this repository. this is different from index versions or
//  pack-index version. The 0 which is the most common one means SHA-1 object ids, loose refs
//  + packed refs, common Git directory layout
// TODO: review each extension on git's config docs. Parsing does not modify them or uses any other
//  than ref and object.
pub(crate) struct RepositoryFormat {
    version: FormatVersion,
    ref_storage: RefStorage,
    object_format: ObjectFormat,
    compat_object_format: Option<ObjectFormat>,
    precious_objects: bool,
    partial_clone: Option<Vec<u8>>,
    // TODO: https://git-scm.com/docs/git-config#Documentation/git-config.txt-worktreeConfig
    worktree_config: bool,
    relative_worktree: bool,
    submodule_path_config: bool,
}

impl RepositoryFormat {
    // Determines the repo format based on config. If no `core.repositoryformatversion` is set we
    // let the caller handle it but the extension entries are not read.
    // from_config() guarantees:
    //  - any unknown v1 extensions are rejected
    //  - unrecognizable values for known extensions are rejected
    //  - if compatObjectFormat is set, it never clashes with object format(primary hash is always
    //  different from compatibility hash).
    pub(crate) fn from_config(cfg: &ConfigFile) -> Result<Option<Self>, RepositoryFormatError> {
        let version = match cfg.get_int("core.repositoryformatversion".as_ref()) {
            Ok(version) => FormatVersion::try_from(version)?,
            Err(err) if err.is_key_not_found() => return Ok(None),
            Err(err) => return Err(RepositoryFormatError::Config(err)),
        };
        let mut format = RepositoryFormat::with_version(version);

        if let Some(entries) = cfg.section_entries("extensions".as_ref())? {
            for entry in entries {
                format.apply_extension(entry)?;
            }
        }
        // TODO: if version is v1 with no extensions Git says that it should have been v0 instead,
        //  should we warn?
        if let Some(compat_obj_fmt) = &format.compat_object_format {
            if *compat_obj_fmt == format.object_format {
                return Err(RepositoryFormatError::SamePrimaryAndCompatObjectFormat(
                    *compat_obj_fmt,
                ));
            }
        }

        Ok(Some(format))
    }

    pub(crate) fn version(&self) -> &FormatVersion {
        &self.version
    }

    pub(crate) fn version_mut(&mut self) -> &mut FormatVersion {
        &mut self.version
    }

    pub(crate) fn object_format(&self) -> &ObjectFormat {
        &self.object_format
    }

    pub(crate) fn object_format_mut(&mut self) -> &mut ObjectFormat {
        &mut self.object_format
    }

    pub(crate) fn ref_storage(&self) -> &RefStorage {
        &self.ref_storage
    }

    pub(crate) fn ref_storage_mut(&mut self) -> &mut RefStorage {
        &mut self.ref_storage
    }

    fn with_version(version: FormatVersion) -> Self {
        Self {
            version,
            ref_storage: RefStorage::default(),
            object_format: ObjectFormat::default(),
            compat_object_format: None,
            precious_objects: false,
            partial_clone: None,
            worktree_config: false,
            relative_worktree: false,
            submodule_path_config: false,
        }
    }

    fn v0() -> Self {
        Self::with_version(FormatVersion::V0)
    }

    fn apply_extension(&mut self, entry: VariableEntry<'_>) -> Result<(), RepositoryFormatError> {
        match self.version {
            FormatVersion::V0 => self.apply_v0_extension(entry),
            FormatVersion::V1 => self.apply_v1_extension(entry),
        }
    }

    fn apply_v0_extension(
        &mut self,
        entry: VariableEntry<'_>,
    ) -> Result<(), RepositoryFormatError> {
        let extension = Extension::from_bytes(entry.name());

        if !extension.is_v0_compatible() {
            return Err(RepositoryFormatError::V1ExtensionInV0(extension));
        }
        self.apply_common_extension(entry)
    }

    // https://github.com/git/git/blob/47ce80527c56f462cb97db4ca8125342204d3783/setup.c#L653-L716
    fn apply_v1_extension(
        &mut self,
        entry: VariableEntry<'_>,
    ) -> Result<(), RepositoryFormatError> {
        let extension = Extension::from_bytes(entry.name());

        match extension {
            // all v0 extensions are treated the same by v1, apart from unknown
            Extension::Noop
            | Extension::PreciousObjects
            | Extension::WorktreeConfig
            | Extension::PartialClone => self.apply_common_extension(entry)?,
            Extension::NoopV1 => {}
            Extension::ObjectFormat => {
                let Value::Bytes(bytes) = entry.value() else {
                    return Err(RepositoryFormatError::UnknownExtensionValue(extension));
                };
                self.object_format = ObjectFormat::try_from(bytes.as_ref()).map_err(|err| {
                    RepositoryFormatError::ObjectFormat {
                        extension,
                        source: err,
                    }
                })?;
            }
            Extension::CompatObjectFormat => {
                let Value::Bytes(bytes) = entry.value() else {
                    return Err(RepositoryFormatError::UnknownExtensionValue(extension));
                };
                let compat_object_format =
                    Some(ObjectFormat::try_from(bytes.as_ref()).map_err(|err| {
                        RepositoryFormatError::ObjectFormat {
                            extension,
                            source: err,
                        }
                    })?);

                // https://github.com/git/git/blob/47ce80527c56f462cb97db4ca8125342204d3783/setup.c#L681-L686
                if self.compat_object_format.is_some() {
                    return Err(RepositoryFormatError::DuplicateCompatObjectFormatExtension);
                }
                self.compat_object_format = compat_object_format;
            }
            Extension::RefStorage => {
                let Value::Bytes(bytes) = entry.value() else {
                    return Err(RepositoryFormatError::UnknownExtensionValue(extension));
                };
                self.ref_storage = RefStorage::try_from(bytes.as_ref())?;
            }
            Extension::RelativeWorktrees => {
                self.relative_worktree = entry
                    .value()
                    .to_bool()
                    .ok_or(RepositoryFormatError::UnknownExtensionValue(extension))?;
            }
            Extension::SubmodulePathConfig => {
                self.submodule_path_config = entry
                    .value()
                    .to_bool()
                    .ok_or(RepositoryFormatError::UnknownExtensionValue(extension))?;
            }
            Extension::Unknown => {
                return Err(RepositoryFormatError::UnknownV1Extension(
                    entry.name().to_vec(),
                ));
            }
        }
        Ok(())
    }

    fn apply_common_extension(
        &mut self,
        entry: VariableEntry<'_>,
    ) -> Result<(), RepositoryFormatError> {
        let extension = Extension::from_bytes(entry.name());

        match extension {
            Extension::PreciousObjects => {
                self.precious_objects = entry
                    .value()
                    .to_bool()
                    .ok_or(RepositoryFormatError::UnknownExtensionValue(extension))?;
            }
            Extension::WorktreeConfig => {
                self.worktree_config = entry
                    .value()
                    .to_bool()
                    .ok_or(RepositoryFormatError::UnknownExtensionValue(extension))?;
            }
            // TODO: verify against a git version above 2.42
            //  for partial clone, git accepts any non valueless variable
            Extension::PartialClone => match entry.into_value() {
                Value::ImplicitlyTrue => {
                    return Err(RepositoryFormatError::UnknownExtensionValue(extension));
                }
                Value::Bytes(bytes) => self.partial_clone = Some(bytes.into_owned()),
            },
            // for noop and unknown the value is ignored
            // TODO: is this error prone because if we incorrectly call it with an extension that does
            //  have a match so far, we assume it is Extension::Noop or Extension::Unknown, or adding
            //  a new one silently falls through
            _ => {}
        }

        Ok(())
    }
}

impl Default for RepositoryFormat {
    fn default() -> Self {
        Self::v0()
    }
}

#[derive(Debug)]
pub(crate) struct FormatVersionError(u64);

impl Error for FormatVersionError {}

impl fmt::Display for FormatVersionError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "unsupported format version: {}", self.0)
    }
}

#[derive(Debug)]
pub(crate) struct ObjectFormatError(pub(crate) Vec<u8>);

impl Error for ObjectFormatError {}

impl fmt::Display for ObjectFormatError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // TODO: check if we need to use our ReadableByte, we probably should because config values
        //  are arbitrary byte sequences(double check cfg parser)
        write!(f, "unknown hash algorithm: {}", self.0.escape_ascii())
    }
}

#[derive(Debug)]
// unknown backend
pub(crate) struct RefFormatError(Vec<u8>);

impl Error for RefFormatError {}

impl fmt::Display for RefFormatError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // TODO: check if we need to use our ReadableByte, we probably should because config values
        //  are arbitrary byte sequences(double check cfg parser)
        write!(
            f,
            "unknown reference backend '{}', expected 'files' or 'reftable'",
            self.0.escape_ascii()
        )
    }
}

#[derive(Debug)]
pub(crate) enum RepositoryFormatError {
    Config(ConfigFileError),
    UnsupportedVersion(FormatVersionError),
    V1ExtensionInV0(Extension),
    // a value that Git does not recognize
    UnknownExtensionValue(Extension),
    UnknownV1Extension(Vec<u8>),
    // returned by bad object format or compat object format extension values
    ObjectFormat {
        extension: Extension,
        source: ObjectFormatError,
    },
    RefStorage(RefFormatError),
    DuplicateCompatObjectFormatExtension,
    // hash algo of object format is the same as the compatibility one
    SamePrimaryAndCompatObjectFormat(ObjectFormat),
}

impl Error for RepositoryFormatError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Config(source) => Some(source),
            Self::UnsupportedVersion(source) => Some(source),
            Self::V1ExtensionInV0(_) => None,
            Self::UnknownExtensionValue(_) => None,
            Self::UnknownV1Extension(_) => None,
            Self::ObjectFormat { source, .. } => Some(source),
            Self::RefStorage(source) => Some(source),
            Self::DuplicateCompatObjectFormatExtension => None,
            Self::SamePrimaryAndCompatObjectFormat(_) => None,
        }
    }
}

impl fmt::Display for RepositoryFormatError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Config(_) => {
                write!(f, "could not repository format configuration")
            }
            Self::UnsupportedVersion(_) => {
                write!(f, "unsupported value for 'core.repositoryformatversion'")
            }
            Self::V1ExtensionInV0(extension) => {
                write!(
                    f,
                    "extension 'extensions.{}' requires format version 1 but the repository uses v0",
                    extension.name()
                )
            }
            Self::UnknownExtensionValue(extension) => {
                write!(
                    f,
                    "unknown extension value for 'extensions.{}'",
                    extension.name()
                )
            }
            Self::UnknownV1Extension(name) => {
                write!(
                    f,
                    "unknown extension 'extensions.{}' in repository format version 1",
                    name.escape_ascii()
                )
            }
            Self::ObjectFormat { extension, .. } => {
                write!(f, "invalid value for 'extensions.{}'", extension.name())
            }
            Self::RefStorage(_) => write!(f, ""),
            Self::DuplicateCompatObjectFormatExtension => {
                write!(
                    f,
                    "'extensions.compatObjectFormat' must not be specified more than once"
                )
            }
            Self::SamePrimaryAndCompatObjectFormat(format) => {
                write!(
                    f,
                    "'extensions.compatObjectFormat' must differ from 'extensions.objectFormat', both use: '{}'",
                    format.name()
                )
            }
        }
    }
}

impl From<ConfigFileError> for RepositoryFormatError {
    fn from(err: ConfigFileError) -> Self {
        Self::Config(err)
    }
}

impl From<FormatVersionError> for RepositoryFormatError {
    fn from(err: FormatVersionError) -> Self {
        Self::UnsupportedVersion(err)
    }
}

impl From<RefFormatError> for RepositoryFormatError {
    fn from(err: RefFormatError) -> Self {
        Self::RefStorage(err)
    }
}
