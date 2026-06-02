pub mod cli;
mod cmd;

// this should return Result<(), Error>
// it is the equivalent of exit status codes in a C program, 0 -> success, anything else -> failure
// toDo: everywhere that we expect a path make it work with Git's concept of pathspec
fn main() {
    // combine those to something like cli::run()?
    let mut command = cli::parse();
    command.execute();
}
