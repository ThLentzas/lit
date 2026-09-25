mod print;

use crate::command::cat_file::print::CatFilePrinter;
use crate::command::print::Printer;
use crate::repo::db::DatabaseError;
use crate::repo::object::ObjectType;
use crate::repo::object::oid::Oid;
use crate::repo::os::{IoError, IoErrorContext};
use crate::repo::{Repository, RepositoryError};
use core::fmt;
use std::error::Error;
use std::ffi::OsString;
use std::path::Path;

#[derive(Debug)]
pub(crate) struct CatFile {
    pub(crate) obj_type: OsString,
    pub(crate) oid: OsString,
}

impl CatFile {
    // if the user wrote cat-file <oid> this prints a message like "either provide the type or use -p flag"
    pub(super) fn execute(&self) -> Result<(), CatFileError> {
        let repo = Repository::discover()?;
        let db = repo.database();

        let oid = self
            .oid
            .to_str()
            .ok_or_else(|| CatFileError::BadOid(self.oid.clone()))?;
        let oid = if oid.len() < 40 {
            db.resolve_oid_prefix(oid)?
        } else {
            Oid::from_hex(oid).map_err(|_| CatFileError::BadOid(self.oid.clone()))?
        };

        let actual = self
            .obj_type
            .to_str()
            .and_then(ObjectType::try_from_str)
            .ok_or_else(|| CatFileError::UnknownType(self.obj_type.clone()))?;

        let object = db.load(&oid)?;
        {
            let expected = object.obj_type();
            if expected != actual {
                return Err(CatFileError::TypeMismatch { expected, actual });
            }
            let printer = CatFilePrinter;
            printer.print(&object).with_context::<&Path>("write", None)?;
        }

        Ok(())
    }
}

#[derive(Debug)]
pub(super) enum CatFileError {
    Repository(RepositoryError),
    UnknownType(OsString),
    BadOid(OsString),
    Database(DatabaseError),
    NotFound(Oid),
    TypeMismatch {
        expected: ObjectType,
        actual: ObjectType,
    },
    // occurs when trying to print to the terminal, compared to the other Io variants we had there
    // is no path
    Io(IoError),
}

impl Error for CatFileError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Repository(source) => Some(source),
            Self::UnknownType(_) => None,
            Self::BadOid(_) => None,
            Self::Database(source) => Some(source),
            Self::NotFound(_) => None,
            Self::TypeMismatch { .. } => None,
            Self::Io(source) => Some(source),
        }
    }
}

impl fmt::Display for CatFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Repository(_) => write!(f, "could not discover repository"),
            Self::UnknownType(obj_type) => {
                write!(f, "unknown object type '{}'", obj_type.to_string_lossy())
            }
            Self::BadOid(oid) => {
                write!(f, "invalid object id '{}'", oid.to_string_lossy())
            }
            Self::Database(err) => write!(f, "{err}"),
            Self::NotFound(oid) => {
                write!(f, "object {} not found", oid.to_hex())
            }
            Self::TypeMismatch { expected, actual } => {
                write!(f, "object type mismatch: expected {expected}, got {actual}")
            }
            Self::Io(_) => write!(f, "could not write to stdout")
        }
    }
}

impl From<RepositoryError> for CatFileError {
    fn from(err: RepositoryError) -> Self {
        Self::Repository(err)
    }
}

impl From<DatabaseError> for CatFileError {
    fn from(err: DatabaseError) -> Self {
        Self::Database(err)
    }
}

impl From<IoError> for CatFileError {
    fn from(err: IoError) -> Self {
        Self::Io(err)
    }
}
