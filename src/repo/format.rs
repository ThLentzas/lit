// TODO: if repo/mod.rs is ends up being not too big we could move the logic there?

use crate::repo::config::{ConfigFile, ConfigFileError, Value, VariableEntry};
use std::error::Error;
use std::fmt;

// TODO: we need to search more on why the config read is enough to determine the repo version and
//  consider it valid. We never walk directories to determine the actual Hash used for reinit etch
#[derive(Debug)]
enum Extension {
    // v0
    // TODO: we need to check what those extensions actually do when present for v0
    //  https://git-scm.com/docs/repository-version/2.39.0b check if changed for later versions
    //  https://git-scm.com/docs/git-config
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
}

impl From<&[u8]> for Extension {
    fn from(value: &[u8]) -> Extension {
        match value {
            // v0
            value if value.eq_ignore_ascii_case(b"noop") => Extension::Noop,
            value if value.eq_ignore_ascii_case(b"preciousObjects") => Extension::PreciousObjects,
            value if value.eq_ignore_ascii_case(b"partialClone") => Extension::PartialClone,
            value if value.eq_ignore_ascii_case(b"worktreeConfig") => Extension::WorktreeConfig,
            // v1
            value if value.eq_ignore_ascii_case(b"noop-v1") => Extension::NoopV1,
            value if value.eq_ignore_ascii_case(b"objectFormat") => Extension::ObjectFormat,
            value if value.eq_ignore_ascii_case(b"refStorage") => Extension::RefStorage,
            value if value.eq_ignore_ascii_case(b"relativeWorktrees") => Extension::RelativeWorktrees,
            value if value.eq_ignore_ascii_case(b"submodulePathConfig") => Extension::SubmodulePathConfig,
            _ => Extension::Unknown,
        }
    }
}

fn verify_v0_extension_value(entry: &VariableEntry<'_>) -> Result<(), RepositoryFormatError> {
    let extension = Extension::from(entry.name());

    match extension {
        Extension::PreciousObjects | Extension::WorktreeConfig => {
            // unknown value for known extension
            if entry.value().to_bool().is_none() {
                return Err(RepositoryFormatError::UnknownExtensionValue(extension));
            }
        }
        // TODO: verify against a git version above 2.42
        // for partial clone, git accepts any non valueless variable
        Extension::PartialClone => {
            if matches!(entry.value(), Value::ImplicitlyTrue) {
                return Err(RepositoryFormatError::UnknownExtensionValue(extension));
            }
        }
        // for noop and unknown the value is ignored
        _ => {}
    }

    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum FormatVersion {
    V0,
    V1,
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

enum RefFormat {
    Files,
    RefTable,
}

enum ObjectFormat {
    Sha1,
    Sha256,
}

// https://git-scm.com/docs/gitrepository-layout#_git_repository_format_versions
// TODO: When reading v1 repos we must also read extensions.* from config. v1 errors when config
//  specifies certain extensions but the repo itself does not impl them, or the value for any known
//  key is not understood by the impl. If no extensions are specified repo version must be set to 0
//  setting it to 1 provides no benefit, and makes the repository incompatible with older implementations
//  of git
pub(super) struct RepositoryFormat {
    version: FormatVersion,
    ref_format: RefFormat,
    object_format: ObjectFormat,
}

impl RepositoryFormat {
    pub(super) fn from_config(cfg: &ConfigFile) -> Result<Self, RepositoryFormatError> {
        let version = match cfg.get_int("core.repositoryformatversion".as_ref()) {
            Ok(None) => FormatVersion::V0,
            Ok(Some(version)) => FormatVersion::try_from(version)?,
            Err(err) => return Err(RepositoryFormatError::Config(err)),
        };

        let extensions = cfg
            .section_entries("extensions")
            .map_err(RepositoryFormatError::Config)?;

        let format = match extensions {
            Some(entries) => {
                match version {
                    // https://github.com/git/git/blob/master/setup.c#L612
                    // comment from author:
                    //  Do not add new extensions to this function. It handles extensions which
                    //  are respected even in v0-format repositories for historical compatibility.
                    FormatVersion::V0 => {
                        for entry in entries {
                            let extension = Extension::from(entry.name());
                            if !extension.is_v0_compatible() {
                                return Err(RepositoryFormatError::V1ExtensionInV0(extension));
                            }
                            verify_v0_extension_value(&entry)?;
                        }
                        RepositoryFormat::default()
                    }
                    // TODO: error V1 without exceptions must be V0
                    //  V1 with unknown extensions also error
                    //  V1 with extension that the value can not be recognized also error
                    FormatVersion::V1 => {}
                }
            }
            None => {
                if matches!(version, FormatVersion::V1) {
                    // TODO: error NoExtensionsInV1
                }
                RepositoryFormat::default()
            }
        };

        Ok(format)
    }

    fn v0() -> Self {
        Self {
            version: FormatVersion::V0,
            ref_format: RefFormat::Files,
            object_format: ObjectFormat::Sha1,
        }
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
pub(crate) enum RepositoryFormatError {
    Config(ConfigFileError),
    UnsupportedVersion(FormatVersionError),
    V1ExtensionInV0(Extension),
    // a value that Git does not recognize
    UnknownExtensionValue(Extension),
    UnknownV1Extension,
}

impl Error for RepositoryFormatError {}

impl fmt::Display for RepositoryFormatError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            RepositoryFormatError::Config(err) => write!(f, "{err}"),
            RepositoryFormatError::UnsupportedVersion(err) => write!(f, "{err}"),
        }
    }
}

impl From<FormatVersionError> for RepositoryFormatError {
    fn from(err: FormatVersionError) -> Self {
        Self::UnsupportedVersion(err)
    }
}
