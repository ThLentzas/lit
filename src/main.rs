extern crate core;

mod cli;
mod cmd;
mod repo;

// TODO: every struct field that is called bytes changed it to inner, because it is a Vec<u8> it is obvious
//  a vec of bytes
// TODO: scoped commits, is it possible to have a pull request command?
// TODO: we need to test in config .lock that if our program panics the file actually gets deleted
// TODO: when we read env variables that will treat as paths we need to make sure that apart from 
//  having valid bytes, they don't contain NUL?
// TODO: top level errors return Result<(), CommandError> make it the same way io::Result works
//  also do this of reach command error
fn main() {
    cli::run();
}

// TODO: write a doc where we explain each command, how it works and why we made those choices
// TODO: write the new parser methods that are common in the next rewrite
// TODO: review every single import and why we declare them that way in terms of access level
// TODO: When I learn about const fn need to look again everything.
// TODO: should Io error variants be merged if another variant which is also an enum has an Io variant too?
// Workspace::Io and Workspace::OsError::Io
// TODO: add a rustfmt file
// TODO: check the visibility of all the mods again
// TODO: can we eventually make a git port where someone can run a lit command their git directory
//  will be ported to lit and then they can continue working?
// TODO: what are objects stored in packfiles?
// TODO: for consistency we need to use the same formatting in all write!() either ({foo}) or 
//  ({}, foo)
// TODO: need to review all the info we expose via fmt::Display impls. Need to double check what we
//  need to preserve internally for debug and what the user sees
// TODO: should we set a global rule to use ReadableByte for everything printable in stdout/err?