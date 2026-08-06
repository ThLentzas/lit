pub mod cli;
pub mod cmd;
mod repo;

// TODO: every struct field that is called bytes changed it to inner, because it is a Vec<u8> it is obvious
// a vec of bytes
// TODO: scoped commits, is it possible to have a pull request command?
fn main() {
    // combine those to something like cli::run()?
    let mut command = cli::parse();
    command.execute();
}

// TODO: write a doc where we explain each command and how it works and why we made those choices
// TODO: write the new parser methods that are common in the next rewrite
// TODO: review every single import and why we declare them that way in terms of access level
// TODO: When I learn about const fn need to look again everything.
// TODO: should Io error variants be merged if another variant which is also an enum has an Io variant too?
// Workspace::Io and Workspace::OsError::Io
// TODO: add a rustfmt file