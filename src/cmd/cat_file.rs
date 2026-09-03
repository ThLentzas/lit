mod print;

use crate::cmd::cat_file::print::CatFilePrinter;
use crate::cmd::print::Printer;
use crate::repo::db::{Database, DbError};
use crate::repo::object::ObjectType;
use crate::repo::object::oid::Oid;
use crate::repo::{DiscoverError, Repository};
use core::fmt;
use std::error::Error;
use std::ffi::OsString;
use std::io;

#[derive(Debug)]
pub(crate) struct CatFile {
    pub(crate) obj_type: OsString,
    pub(crate) oid: OsString,
}

impl CatFile {
    // if the user wrote cat-file <oid> this prints a message like "either provide the type or use -p flag"
    pub(super) fn execute(&self) -> Result<(), CatFileError> {
        let repo = Repository::discover()?;
        let db = Database {
            path: repo.db_path(),
        };

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

        match db.load(&oid)? {
            Some(object) => {
                let expected = object.obj_type();
                if expected != actual {
                    return Err(CatFileError::TypeMismatch { expected, actual });
                }
                let printer = CatFilePrinter;
                printer.print(&object)?;
            }
            None => return Err(CatFileError::NotFound(oid)),
        }

        Ok(())
    }
}

#[derive(Debug)]
pub(super) enum CatFileError {
    Repository(DiscoverError),
    UnknownType(OsString),
    BadOid(OsString),
    Database(DbError),
    NotFound(Oid),
    TypeMismatch {
        expected: ObjectType,
        actual: ObjectType,
    },
    Io(io::Error),
}

impl Error for CatFileError {}

impl fmt::Display for CatFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CatFileError::Repository(err) => write!(f, "{err}"),
            CatFileError::UnknownType(obj_type) => {
                write!(f, "unknown object type '{}'", obj_type.to_string_lossy())
            }
            CatFileError::BadOid(oid) => {
                write!(f, "invalid object id '{}'", oid.to_string_lossy())
            }
            CatFileError::Database(err) => write!(f, "{err}"),
            CatFileError::NotFound(oid) => {
                write!(f, "object {} not found", oid.to_hex())
            }
            CatFileError::TypeMismatch { expected, actual } => {
                write!(f, "object type mismatch: expected {expected}, got {actual}")
            }
            CatFileError::Io(err) => write!(f, "{err}"),
        }
    }
}

impl From<DiscoverError> for CatFileError {
    fn from(err: DiscoverError) -> Self {
        CatFileError::Repository(err)
    }
}

impl From<DbError> for CatFileError {
    fn from(err: DbError) -> Self {
        CatFileError::Database(err)
    }
}

impl From<io::Error> for CatFileError {
    fn from(err: io::Error) -> Self {
        CatFileError::Io(err)
    }
}
