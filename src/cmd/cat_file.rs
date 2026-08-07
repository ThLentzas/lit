mod print;

use crate::cmd::cat_file::print::CatFilePrinter;
use crate::cmd::print::Printer;
use crate::repo::db::{Database, DbError};
use crate::repo::object::{ObjectType, Oid};
use crate::repo::{DiscoverError, Repository};
use std::ffi::OsString;
use std::io;

pub(crate) struct CatFile {
    pub(crate) obj_type: OsString,
    pub(crate) oid: OsString,
}

impl CatFile {
    // TODO: we need to support the shortest prefix that is unique and since it is unique we can
    // return that object, jon in his impl mentions using glob around 32:00
    // if the user wrote cat-file <oid> this prints a message like "either provide the type or use -p flag"
    pub(super) fn execute(&self) -> Result<(), CatFileError> {
        let repo = Repository::discover()?;
        let db = Database {
            path: repo.db_path(),
        };

        let oid = self
            .oid
            .to_str()
            .and_then(|s| Oid::from_hex(s).ok())
            .ok_or_else(|| CatFileError::BadOid(self.oid.clone()))?;
        let actual = self
            .obj_type
            .to_str()
            .and_then(ObjectType::try_from_str)
            .ok_or_else(|| CatFileError::UnknownType(self.obj_type.clone()))?;

        match db.load(&oid)? {
            Some(object) => {
                let expected = object.obj_type();
                if expected != actual {
                    return Err(CatFileError::TypeMisMatch { expected, actual });
                }
                let printer = CatFilePrinter{};
                printer.print(&object)?;
            }
            None => return Err(CatFileError::NotFound(oid)),
        }

        Ok(())
    }
}

#[derive(Debug)]
pub(super) enum CatFileError {
    RepoError(DiscoverError),
    UnknownType(OsString),
    BadOid(OsString),
    DbError(DbError),
    NotFound(Oid),
    TypeMisMatch {
        expected: ObjectType,
        actual: ObjectType,
    },
    Io(io::Error),
}

impl From<DiscoverError> for CatFileError {
    fn from(err: DiscoverError) -> Self {
        CatFileError::RepoError(err)
    }
}

impl From<DbError> for CatFileError {
    fn from(err: DbError) -> Self {
        CatFileError::DbError(err)
    }
}

impl From<io::Error> for CatFileError {
    fn from(err: io::Error) -> Self {
        CatFileError::Io(err)
    }
}
