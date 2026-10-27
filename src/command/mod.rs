pub(crate) mod add;
pub(crate) mod cat_file;
pub(crate) mod commit;
pub(crate) mod config;
pub(crate) mod init;
pub(crate) mod print;
pub(crate) mod status;
pub(crate) mod error;

use crate::command::add::{Add, AddError};
use crate::command::config::Config;
use crate::command::init::{Init, InitError};
use crate::repo::{Repository, RepositoryError};
use clap::Subcommand;
use core::fmt;
use std::error::Error;

#[derive(Debug, PartialEq, Eq)]
enum RepositoryRequirement {
    None,
    Any,           // valid metadata, bare no bare does not matter
    NeedsWorkTree, // valid metadata and worktree
}

// TODO: should all commands consume self since they are one and done?
#[derive(Debug, Subcommand)]
pub(super) enum Command {
    Init(Init),
    Add(Add),
    // Commit(Commit),
    // Status(Status),
    // CatFile(CatFile),
    Config(Config),
}

impl Command {
    // TODO: error handling
    // TODO: we need to move the discovery logic to the dispatcher for commands that can't be executed
    //  in bare repos.
    pub(super) fn execute(self) -> Result<(), CommandError> {
        match &self {
            Command::Init(init) => init.execute()?,
            Command::Add(add) => {
                let repository = Repository::discover()?;
                if self.repo_requirement() == RepositoryRequirement::NeedsWorkTree
                    && repository.is_bare()
                {
                    return Err(CommandError::WorktreeRequired);
                }
                add.execute(&repository)?
            }
            // Command::Commit(command) => command.execute().unwrap(),
            // Command::Status(command) => command.execute().unwrap(),
            // Command::CatFile(command) => command.execute().unwrap(),
            Command::Config(cmd) => cmd.execute(),
        }
        Ok(())
    }

    fn repo_requirement(&self) -> RepositoryRequirement {
        match self {
            Self::Init(_) => RepositoryRequirement::None,
            Self::Add(_) => RepositoryRequirement::NeedsWorkTree,
            Self::Config(_) => RepositoryRequirement::Any,
        }
    }
}

#[derive(Debug)]
pub(crate) enum CommandError {
    Init(InitError),
    Repository(RepositoryError),
    Add(AddError),
    WorktreeRequired,
}

impl Error for CommandError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Init(source) => Some(source),
            Self::Add(source) => Some(source),
            Self::Repository(source) => Some(source),
            Self::WorktreeRequired => None,
        }
    }
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // TODO: check with and without a message, the message in this case has to be some top
            //  level generic message
            Self::Init(_) => write!(f, ""),
            Self::Add(_) => write!(f, ""),
            Self::Repository(_) => write!(f, ""),
            Self::WorktreeRequired => write!(f, "this operation must be run in a work tree"),
        }
    }
}

impl From<InitError> for CommandError {
    fn from(err: InitError) -> Self {
        Self::Init(err)
    }
}

impl From<RepositoryError> for CommandError {
    fn from(err: RepositoryError) -> Self {
        Self::Repository(err)
    }
}

impl From<AddError> for CommandError {
    fn from(err: AddError) -> Self {
        Self::Add(err)
    }
}
