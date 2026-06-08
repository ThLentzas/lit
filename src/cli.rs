use crate::cmd::{Add, Command, Commit, Init};
use std::env;
use std::path::PathBuf;

// toDo: write a proper parser, dont pass the command args as fn args
pub(super) fn parse() -> Command {
    let args = env::args();
    let n = args.into_iter().len();
    let mut args = env::args().skip(1).into_iter().peekable();
    if args.peek().is_none() {
        // toDo: Error
    }
    
    match args.next().unwrap().as_str() {
        "init" => Command::Init(Init::new(&mut args)),
        "add" => Command::Add(Add { path: PathBuf::from(args.next().unwrap()) } ),
        "commit" => Command::Commit(Commit{}),
        _ => todo!(),
    }
}
