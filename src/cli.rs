use crate::command::{Command, CommandError};
use clap::Parser;

// TODO: https://git-scm.com/docs/git, top level options
#[derive(Debug, Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

pub(super) fn run() -> Result<(), CommandError> {
    let cli = Cli::parse();
    cli.command.execute()
}
