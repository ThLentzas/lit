pub(crate) mod init;
pub(crate) mod add;
pub(crate) mod commit;
pub(crate) mod status;
pub(crate) mod cat_file;
mod print;

use crate::cmd::add::Add;
use crate::cmd::cat_file::CatFile;
use crate::cmd::commit::Commit;
use crate::cmd::init::Init;
use crate::cmd::status::Status;

// init creates repository structure
// add creates/updates the index
// commit consumes the index
pub(super) enum Command {
    Init(Init),
    Add(Add),
    Commit(Commit),
    Status(Status),
    CatFile(CatFile)
}

impl Command {
    // TODO: error handling
    pub(super) fn execute(&mut self) {
        match self {
            Command::Init(cmd) => cmd.execute().unwrap(),
            Command::Add(cmd) => cmd.execute().unwrap(),
            Command::Commit(cmd) => cmd.execute().unwrap(),
            Command::Status(cmd) => cmd.execute().unwrap(),
            Command::CatFile(cmd) => cmd.execute().unwrap(),
        }
    }
}