use std::ffi::OsString;
use crate::repo::config;
use crate::repo::lockfile::Lockfile;
use crate::repo::Repository;

pub(crate) struct Config {
    pub(crate) name: OsString,
    pub(crate) value: OsString,
}

impl Config {
    pub(crate) fn execute(&self) {
        // let repo = Repository::discover().unwrap();
        // let mut config = config::Config::new(repo.config_path()).unwrap();
        // // let mut lock  = Lockfile::acquire(&config.local).unwrap();
        // // config.load().unwrap();
        // // config.set(&self.name, &self.value);
        // lock.write(&config.serialize()).unwrap();
        // lock.commit().unwrap();
    }
}