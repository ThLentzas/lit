use super::Format;
use super::report::{HeadIndexChange, Report, WorkspaceIndexChange};
use std::borrow::Cow;
use std::io::{self, IsTerminal, Write};

const LABEL_WIDTH: usize = 12;

enum Color {
    Red,
    Green,
}

struct Style {
    colorize: bool,
}

impl Style {
    fn auto() -> Self {
        Self { colorize: io::stdout().is_terminal() }
    }

    // For porcelain output and tests no coloring
    pub(crate) fn plain() -> Self {
        Self { colorize: false }
    }
}

pub(super) fn print(report: &Report, format: Format) -> io::Result<()> {
    match format {
        Format::Short => print_short(report),
        Format::Long => print_long(report),
    }
}

fn print_short(report: &Report) -> io::Result<()> {
    Ok(())
}

fn print_long(report: &Report) -> io::Result<()> {
    let mut writer = io::stdout().lock();

    print_changes(
        "Changes to be committed",
        report.changes.iter().filter_map(|(path, change)| {
            change.head_index.as_ref().map(|ch| {
                (
                    path,
                    match ch {
                        HeadIndexChange::ADDED => "new file:",
                        HeadIndexChange::MODIFIED => "modified:",
                        HeadIndexChange::DELETED => "deleted:",
                    },
                )
            })
        }),
        Color::Green,
        &mut writer,
    )?;
    print_changes(
        "Changes not staged for commit",
        report.changes.iter().filter_map(|(path, change)| {
            change.workspace_index.as_ref().map(|ch| {
                (
                    path,
                    match ch {
                        WorkspaceIndexChange::MODIFIED => "modified:",
                        WorkspaceIndexChange::DELETED => "deleted:",
                    },
                )
            })
        }),
        Color::Red,
        &mut writer,
    )?;
    print_changes(
        "Untracked files",
        report.untracked.iter().map(|p| (p, "")),
        Color::Red,
        &mut writer,
    )?;
    Ok(())
}

fn print_changes<'a, Iter, Writer>(
    message: &str,
    changes: Iter,
    color: Color,
    out: &mut Writer,
) -> io::Result<()>
where
    Iter: Iterator<Item = (&'a Vec<u8>, &'static str)>,
    Writer: Write
{
    let mut changes = changes.peekable();
    // empty section
    if changes.peek().is_none() {
        return Ok(());
    }
    writeln!(out, "{message}:")?;
    writeln!(out)?;
    for (path, label) in changes {
        out.write(b"\t")?;
        if !label.is_empty() {
            write!(out, "{label:<LABEL_WIDTH$}")?;
        }
        out.write_all(&stdout_bytes(path))?;
        out.write_all(b"\n")?;
    }
    writeln!(out)?;

    Ok(())
}

fn stdout_bytes(path: &[u8]) -> Cow<'_, [u8]> {
    if !path.iter().any(|&b| needs_quoting(b)) {
        return Cow::Borrowed(path);
    }

    let mut out = Vec::with_capacity(path.len() + 2);
    out.push(b'"');
    for &b in path {
        match b {
            0x07 => out.extend_from_slice(b"\\a"),
            0x08 => out.extend_from_slice(b"\\b"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\n' => out.extend_from_slice(b"\\n"),
            0x0b => out.extend_from_slice(b"\\v"),
            0x0c => out.extend_from_slice(b"\\f"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            // printable ASCII stays as is
            0x20..=0x7e => out.push(b),
            // everything else remaining controls (incl. ESC 0x1b), DEL, all bytes >= 0x80 become
            // 3-digit octal
            _ => {
                out.push(b'\\');
                out.push(b'0' + ((b >> 6) & 0o7));
                out.push(b'0' + ((b >> 3) & 0o7));
                out.push(b'0' + (b & 0o7));
            }
        }
    }
    out.push(b'"');
    Cow::Owned(out)
}

fn needs_quoting(b: u8) -> bool {
    b.is_ascii_control() // includes 0x7f
        || b >= 0x80 // non-ASCII, valid UTF-8 or not
        || b == b'"'
        || b == b'\\'
}
