use crate::command::print::{self, Printer};
use crate::repo::object::mode::Mode;
use crate::repo::object::{Object, Signature};
use std::io::{self, StdoutLock, Write};

// TODO: maybe all the print logic for every command should be a submodule inside each command
pub(super) struct CatFilePrinter {
    // we don't pass a generic for any writter, printer always print in stdout
    writter: StdoutLock<'static>,
}

impl CatFilePrinter {
    // TODO: maybe this needs a better name?
    pub(super) fn lock() -> Self {
        Self {
            // acquiring the lock allows us to write to stdout without being interrupted in a multithreaded
            // environment, it does not prevent another process from writing to stdout
            // if we print out frequently calling `print!()` it can have a cost in performance since every
            // call needs to acquire and release the lock.
            writter: io::stdout().lock(),
        }
    }

    fn write_signature(&mut self, user: &str, signature: &Signature) -> io::Result<()> {
        self.writter.write_all(user.as_bytes())?;
        self.writter.write_all(b" ")?;
        self.writter.write_all(signature.name.as_bytes())?;
        self.writter.write_all(b" <")?;
        self.writter.write_all(signature.email.as_bytes())?;
        self.writter.write_all(b"> ")?;
        self.writter
            .write_all(signature.timestamp.to_string().as_bytes())?;
        self.writter.write_all(b"\n")?;

        Ok(())
    }
}

impl Printer for CatFilePrinter {
    type T<'a> = Object;

    fn print(&mut self, value: &Object) -> io::Result<()> {
        // for Tree and commit we could create a buff and then call out.write_all(buf) without
        // acquiring the lock, but it is unnecessary
        match value {
            Object::Blob(content) => self.writter.write_all(content),
            Object::Tree(entries) => {
                for entry in entries {
                    let mode = entry.mode;

                    if mode.is_directory() {
                        self.writter.write_all(b"0")?;
                    }
                    self.writter.write_all(mode.as_octal_bytes())?;
                    self.writter.write_all(b" ")?;
                    self.writter.write_all(write_mode(&entry.mode).as_bytes())?;
                    self.writter.write_all(b" ")?;
                    self.writter.write_all(entry.oid.to_hex().as_bytes())?;
                    self.writter.write_all(b" ")?;
                    self.writter
                        .write_all(&print::stdout_path_bytes(&entry.name))?;
                    self.writter.write_all(b"\n")?;
                }
                Ok(())
            }
            Object::Commit(commit) => {
                self.writter.write_all(b"tree ")?;
                self.writter.write_all(commit.root_id.to_hex().as_bytes())?;
                self.writter.write_all(b"\n")?;

                for parent in &commit.parents {
                    self.writter.write_all(b"parent ")?;
                    self.writter.write_all(parent.to_hex().as_bytes())?;
                    self.writter.write_all(b"\n")?;
                }

                self.write_signature("author", &commit.author)?;
                self.write_signature("committer", &commit.committer)?;

                self.writter.write_all(b"\n")?;
                self.writter.write_all(commit.message.as_bytes())
            }
        }
    }
}

fn write_mode(mode: &Mode) -> &str {
    if mode.is_directory() { "tree" } else { "blob" }
}
