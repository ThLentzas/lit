use std::env;
use std::path::PathBuf;
use crate::cmd::add::Add;
use crate::cmd::Command;
use crate::cmd::commit::Commit;
use crate::cmd::init::Init;
use crate::cmd::status::Status;

// TODO: if we use clap we need to make sure that we parse as env::args_os() and not env::args
// args() forces every argument to be valid utf-9 but in our case what we want is a platform OsString
// we know that unix allow pretty every byte sequence that does not contain NUL and / and windows
// uses WTF-16. We don't want to reject non-utf8 paths.
pub(super) fn parse() -> Command {
    let mut args = env::args().skip(1).into_iter().peekable();
    
    match args.next().unwrap().as_str() {
        "init" => Command::Init(Init::new(&mut args)),
        // if any path is empty -> Error
        "add" => Command::Add(Add { paths: vec![PathBuf::from(args.next().unwrap())]} ),
        "commit" => Command::Commit(Commit{}),
        "status" => Command::Status(Status{}),
        _ => todo!(),
    }
}

