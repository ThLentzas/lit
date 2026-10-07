pub(crate) mod init;
pub(crate) mod add;
pub(crate) mod commit;
pub(crate) mod status;
pub(crate) mod cat_file;
pub(crate) mod print;
pub(crate) mod config;

use crate::command::config::Config;
use crate::command::init::Init;
use clap::Subcommand;

enum RepositoryRequirement {
    None,
    Any, // valid metadata, bare no bare does not matter
    NeedsWorktree // valid metadata and worktree
}

// TODO: should all commands consume self since they are one and done?
#[derive(Debug, Subcommand)]
pub(super) enum Command {
    Init(Init),
    // Add(Add),
    // Commit(Commit),
    // Status(Status),
    // CatFile(CatFile),
    Config(Config)
}

impl Command {
    // TODO: error handling
    // TODO: we need to move the discovery logic to the dispatcher for commands that can't be executed
    //  in bare repos.
    pub(super) fn execute(self) {
        match self {
            Command::Init(cmd) => cmd.execute().unwrap(),
            // Command::Add(command) => command.execute().unwrap(),
            // Command::Commit(command) => command.execute().unwrap(),
            // Command::Status(command) => command.execute().unwrap(),
            // Command::CatFile(command) => command.execute().unwrap(),
            Command::Config(cmd) => cmd.execute(),
        }
    }

    fn repo_requirement(&self) -> RepositoryRequirement {
        match self {
            Self::Init(_) => RepositoryRequirement::None,
            Self::Config(_) => RepositoryRequirement::Any,
        }
    }
}