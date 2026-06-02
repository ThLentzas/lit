use crate::cmd::{Command, Init};
use std::env;

// toDo: write a proper parser, dont pass the command args as fn args
pub(super) fn parse() -> Command {
    let mut args = env::args().skip(1).into_iter().peekable();
    if args.peek().is_none() {
        // toDo: Error
    }
    
    match args.next().unwrap().as_str() {
        "init" => Command::Init(Init::new(&mut args)),
        _ => todo!(),
    }
}
