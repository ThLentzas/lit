mod print;

use crate::repo::db::{Database, DbError};
use crate::repo::object::{ObjectType, Oid};
use crate::repo::{RepoError, Repository};
use std::ffi::OsString;
use std::io;

pub(crate) struct CatFile {
    pub(crate) obj_type: OsString,
    pub(crate) oid: OsString,
}

impl CatFile {
    pub(super) fn execute(&self) -> Result<(), CatFileError> {
        let repo = Repository::discover()?;
        let db = Database {
            path: repo.db_path(),
        };
        let mut out = io::stdout().lock();

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
                print::print(&mut out, &object)?
            }
            None => return Err(CatFileError::NotFound(oid)),
        }

        Ok(())
    }
}

#[derive(Debug)]
pub(super) enum CatFileError {
    RepoError(RepoError),
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

impl From<RepoError> for CatFileError {
    fn from(err: RepoError) -> Self {
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
