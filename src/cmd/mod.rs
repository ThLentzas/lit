pub(crate) mod init;
pub(crate) mod add;
pub(crate) mod commit;
pub(crate) mod status;
pub(crate) mod cat_file;
mod print;
pub(crate) mod config;

use crate::cmd::add::Add;
use crate::cmd::cat_file::CatFile;
use crate::cmd::commit::Commit;
use crate::cmd::config::Config;
use crate::cmd::init::Init;
use crate::cmd::status::Status;

// TODO: should all commands consume self since they are one and done?
pub(super) enum Command {
    Init(Init),
    Add(Add),
    Commit(Commit),
    Status(Status),
    CatFile(CatFile),
    Config(Config)
}

// TODO: should we type def Result<(), CommandError>?
impl Command {
    // TODO: error handling
    pub(super) fn execute(self) {
        match self {
            Command::Init(cmd) => cmd.execute().unwrap(),
            Command::Add(cmd) => cmd.execute().unwrap(),
            Command::Commit(cmd) => cmd.execute().unwrap(),
            Command::Status(cmd) => cmd.execute().unwrap(),
            Command::CatFile(cmd) => cmd.execute().unwrap(),
            Command::Config(cmd) => cmd.execute(),
        }
    }
}