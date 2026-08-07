use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use crate::cmd::add::Add;
use crate::cmd::Command;
use crate::cmd::commit::Commit;
use crate::cmd::init::Init;
use crate::cmd::status::Status;
use crate::cmd::cat_file::CatFile;

// TODO: if we use clap we need to make sure that we parse as env::args_os() and not env::args
pub(super) fn parse() -> Command {
    let mut args = env::args().skip(1).into_iter().peekable();
    
    match args.next().unwrap().as_str() {
        "init" => Command::Init(Init::new(&mut args)),
        // if any path is empty -> Error
        "add" => Command::Add(Add { paths: vec![PathBuf::from(args.next().unwrap())]} ),
        "commit" => Command::Commit(Commit{}),
        "status" => Command::Status(Status{}),
        "cat-file" => {
            let obj_type = OsString::from(args.next().unwrap());
            let oid = OsString::from(args.next().unwrap());
            Command::CatFile(CatFile { obj_type, oid })
        }
        _ => todo!(),
    }
}

