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
        let repo = Repository::discover().unwrap();
        let cfg_path = repo.config_path();
        let mut lock  = Lockfile::acquire(&cfg_path).unwrap();
        let cfg = config::ConfigFile::new(cfg_path).unwrap();
        let cfg = cfg.set(&self.name, &self.value).unwrap();
        lock.write(&cfg.serialize()).unwrap();
        lock.commit().unwrap();
    }
}