pub mod cli;
mod cmd;

// this should return Result<(), Error>
// it is the equivalent of exit status codes in a C program, 0 -> success, anything else -> failure
fn main() {
    // let mut command = cli::parse();
    // command.execute();

    println!("{}", std::env::current_dir().unwrap().display());
}
