mod init;
mod add;
mod commit;
mod status;

use crate::cmd::init::Init;
use crate::command::{Add, Commit};
use crate::command::status::Status;

// init creates repository structure
// add creates/updates the index
// commit consumes the index
pub(super) enum Command {
    Init(Init),
    Add(Add),
    Commit(Commit),
    Status(Status),
}

impl Command {
    // toDo: error handling
    pub(super) fn execute(&mut self) {
        match self {
            Command::Init(cmd) => cmd.execute().unwrap(),
            Command::Add(cmd) => cmd.execute().unwrap(),
            Command::Commit(cmd) => cmd.execute().unwrap(),
            Command::Status(cmd) => cmd.execute().unwrap(),
        }
    }
}