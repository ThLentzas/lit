use std::io::{self, Write};
use crate::cmd::print::{self, Printer};
use crate::repo::object::{Object, Signature};
use crate::repo::object::mode::Mode;

pub(super) struct CatFilePrinter;

impl Printer for CatFilePrinter {
    type T<'a> = Object;

    fn print(&self, value: &Object) -> io::Result<()> {
        // for Tree and commit we could create a buff and then call out.write_all(buf) without
        // acquiring the lock, but it is unnecessary
        let mut out = io::stdout().lock();

        match value {
            Object::Blob(content) => out.write_all(content),
            Object::Tree(entries) => {
                for entry in entries {
                    let mode = entry.mode;

                    if mode.is_directory() {
                        out.write_all(b"0")?;
                    }
                    out.write_all(mode.as_octal_bytes())?;
                    out.write_all(b" ")?;
                    out.write_all(write_mode(&entry.mode).as_bytes())?;
                    out.write_all(b" ")?;
                    out.write_all(entry.oid.to_hex().as_bytes())?;
                    out.write_all(b" ")?;
                    out.write_all(&print::stdout_path_bytes(&entry.name))?;
                    out.write_all(b"\n")?;
                }
                Ok(())
            }
            Object::Commit(commit) => {
                out.write_all(b"tree ")?;
                out.write_all(commit.root_id.to_hex().as_bytes())?;
                out.write_all(b"\n")?;

                for parent in &commit.parents {
                    out.write_all(b"parent ")?;
                    out.write_all(parent.to_hex().as_bytes())?;
                    out.write_all(b"\n")?;
                }

                write_signature(&mut out, "author", &commit.author)?;
                write_signature(&mut out, "committer", &commit.committer)?;

                out.write_all(b"\n")?;
                out.write_all(commit.message.as_bytes())
            }
        }
    }
}

fn write_signature(out: &mut impl Write, user: &str, signature: &Signature) -> io::Result<()> {
    out.write_all(user.as_bytes())?;
    out.write_all(b" ")?;
    out.write_all(signature.name.as_bytes())?;
    out.write_all(b" <")?;
    out.write_all(signature.email.as_bytes())?;
    out.write_all(b"> ")?;
    out.write_all(signature.timestamp.to_string().as_bytes())?;
    out.write_all(b"\n")?;

    Ok(())
}

fn write_mode(mode: &Mode) -> &str {
    if mode.is_directory() { "tree" } else { "blob" }
}