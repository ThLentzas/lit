use crate::repo::os::FileKind;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub(crate) enum Mode {
    Regular = 0o100644,
    Executable = 0o100755,
    Symlink = 0o120000,
    Directory = 0o040000,
}

impl Mode {
    pub(crate) fn is_regular(self) -> bool {
        matches!(self, Self::Regular)
    }

    pub(crate) fn is_executable(self) -> bool {
        matches!(self, Self::Executable)
    }

    pub(crate) fn is_symlink(self) -> bool {
        matches!(self, Self::Symlink)
    }

    pub(crate) fn is_directory(self) -> bool {
        matches!(self, Self::Directory)
    }
    
    pub(crate) fn from_raw(value: u32) -> Option<Self> {
        match value {
            0o100644 => Some(Mode::Regular),
            0o100755 => Some(Mode::Executable),
            0o120000 => Some(Mode::Symlink),
            0o040000 => Some(Mode::Directory),
            _ => None,
        }
    }

    pub(crate) fn as_octal_bytes(&self) -> &[u8] {
        // won't work because we return a ref to a local variable that gets dropped
        // format!("{:o}", *self as u32).as_bytes()
        match self {
            Mode::Regular => b"100644",
            Mode::Executable => b"100755",
            Mode::Symlink => b"120000",
            Mode::Directory => b"040000",
        }
    }
}

#[derive(Debug)]
pub(crate) struct UnsupportedFileType;

impl TryFrom<FileKind> for Mode {
    type Error = UnsupportedFileType;

    fn try_from(value: FileKind) -> Result<Self, Self::Error> {
        match value {
            FileKind::Regular(false) => Ok(Mode::Regular),
            FileKind::Regular(true) => Ok(Mode::Executable),
            FileKind::Symlink => Ok(Mode::Symlink),
            FileKind::Directory => Ok(Mode::Directory),
            FileKind::Other => Err(UnsupportedFileType),
        }
    }
}
