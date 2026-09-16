use crate::cmd::Command;
use clap::Parser;
use rand::RngExt;
use rand::distr::Alphanumeric;

// TODO: this is not the right place for those methods, will be moved after
pub(crate) fn generate(len: u8) -> String {
    (0..len)
        .map(|_| rand::rng().sample(Alphanumeric) as char)
        .collect()
}

pub(crate) fn with_prefix(prefix: &str) -> String {
    // some optimal default for the length of the suffix
    with_prefix_and_len(prefix, 8)
}

pub(crate) fn with_prefix_and_len(prefix: &str, len: u8) -> String {
    let suffix: String = (0..len)
        .map(|_| rand::rng().sample(Alphanumeric) as char)
        .collect();
    format!("{prefix}_{suffix}")
}

pub(crate) fn with_suffix(suffix: &str) -> String {
    // some optimal default for the length of the prefix
    with_suffix_and_len(suffix, 8)
}

pub(crate) fn with_suffix_and_len(suffix: &str, len: u8) -> String {
    let prefix: String = (0..len)
        .map(|_| rand::rng().sample(Alphanumeric) as char)
        .collect();
    format!("{prefix}_{suffix}")
}

// TODO: https://git-scm.com/docs/git, top level options
#[derive(Debug, Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

pub(super) fn run() {
    let cli = Cli::parse();
    cli.command.execute();
}
