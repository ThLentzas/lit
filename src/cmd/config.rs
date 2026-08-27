mod print;

use crate::repo::Repository;
use crate::repo::config::ConfigFile;
use crate::repo::lockfile::Lockfile;
use clap::{Args, Subcommand};
use std::ffi::OsString;

#[derive(Debug, Args)]
struct DisplayOption {
    #[arg(short = 'z', long)]
    null: bool,
    #[arg(long)]
    name_only: bool,
    #[arg(long)]
    show_names: bool,
    #[arg(long)]
    no_show_names: bool,
    #[arg(long)]
    show_origin: bool,
    #[arg(long)]
    show_scope: bool,
}

#[derive(Debug, Args)]
struct Set {
    #[arg(long)]
    all: bool,
    name: OsString,
    value: OsString,
}

#[derive(Debug, Args)]
struct Get {
    #[arg(long)]
    all: bool,
    // flatten affects how arguments appear on the command line while preserving a nested structure
    //
    // lit config get --all --show-names --null user.name
    //
    // flatten allows us to map this command to:
    //  get.all
    //  get.name
    //  get.display.show-names
    //  get.display.null
    //
    //  If we had a Vec<DisplayOption> then the command changes interface to:
    //      lit config get --all --display null --display names
    //  this is different from Git's interface
    #[command(flatten)]
    display_options: DisplayOption,
    name: OsString,
}

#[derive(Debug, Subcommand)]
enum Action {
    Get(Get),
    Set(Set),
}

#[derive(Debug, Args)]
pub(crate) struct Config {
    #[command(subcommand)]
    action: Action
}

impl Config {
    pub(crate) fn execute(&self) {
        let repo = Repository::discover().unwrap();
        let cfg_path = repo.config_path();

        match &self.action {
            Action::Get(get) => {
                let cfg = ConfigFile::new(&cfg_path).unwrap();
                let _entry = cfg.get(&get.name).unwrap();
            }
            Action::Set(set) => {
                let mut lock  = Lockfile::acquire(&cfg_path).unwrap();
                let cfg = ConfigFile::new(&cfg_path).unwrap();
                let cfg = cfg.set(&set.name, &set.value).unwrap();
                lock.write(&cfg.serialize()).unwrap();
                lock.commit().unwrap();
            }
        }
    }
}