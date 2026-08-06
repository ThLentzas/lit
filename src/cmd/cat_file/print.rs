use std::io::{self, Write};
use crate::cmd::print;
use crate::repo::object::{Object, Signature};
use crate::repo::object::mode::Mode;

pub(super) fn print(out: &mut impl Write, object: &Object) -> io::Result<()> {
    match object {
        Object::Blob(content) => out.write_all(content),
        Object::Tree(entries) => {
            let mut buf = Vec::new();
            for entry in entries {
                let mode = entry.mode;
                if mode.is_directory() {
                    buf.push(b'0');
                }
                buf.extend_from_slice(mode.as_octal_bytes());
                buf.push(b' ');
                buf.extend_from_slice(write_mode(&entry.mode).as_bytes());
                buf.push(b' ');
                buf.extend_from_slice(entry.oid.to_hex().as_bytes());
                buf.push(b' ');
                buf.extend_from_slice(&print::stdout_path_bytes(&entry.name));
                buf.push(b'\n');
            }
            // many ways to do drop the last '\n', we could call buf.pop() after the loop, we could
            // also make the iterator peekable, Git writes it though
            out.write_all(&buf)
        }
        Object::Commit(commit) => {
            let mut buf = Vec::new();
            buf.extend_from_slice(b"tree ");
            buf.extend_from_slice(commit.root_id.to_hex().as_bytes());
            buf.push(b'\n');

            for parent in commit.parents.iter() {
                buf.extend_from_slice(b"parent ");
                buf.extend_from_slice(parent.to_hex().as_bytes());
                buf.push(b'\n');
            }

            write_signature(&mut buf, "author", &commit.author);
            write_signature(&mut buf, "committer", &commit.committer);
            buf.push(b'\n');

            buf.extend_from_slice(commit.message.as_bytes());
            out.write_all(&buf)
        }
    }
}

fn write_signature(buf: &mut Vec<u8>, user: &str, signature: &Signature) {
    buf.extend_from_slice(user.as_bytes());
    buf.push(b' ');
    buf.extend_from_slice(signature.name.as_bytes());
    buf.extend_from_slice(b" <");
    buf.extend_from_slice(signature.email.as_bytes());
    buf.extend_from_slice(b"> ");
    buf.extend_from_slice(signature.timestamp.to_string().as_bytes());
    buf.push(b'\n');
}

fn write_mode(mode: &Mode) -> &str {
    if mode.is_directory() { "tree" } else { "blob" }
}