extern crate core;

pub mod cli;
mod cmd;
pub mod hex;

// this should return Result<(), Error>
// it is the equivalent of exit status codes in a C program, 0 -> success, anything else -> failure
// toDo: everywhere that we expect a path make it work with Git's concept of pathspec
fn main() {
    // combine those to something like cli::run()?
    let mut command = cli::parse();
    command.execute();
}

// TODO: write a doc where we explain each command and how it works and why we made those choices
// TODO: write the new parser methods that are common in the next rewrite