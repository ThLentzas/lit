use std::borrow::Cow;
use std::io::{self, IsTerminal, Write};
use crate::cmd::status::Format;
use crate::repo::path::RepoPath;
use crate::repo::report::{HeadIndexChange, Report, WorkspaceIndexChange};

// TODO: this need to change to 17 when we add diff support
const LABEL_WIDTH: usize = 12;


// let mut name = path;
// // for the a/b relative to root path we display it as a/b/ only if it is a dir
// if stat.mode == os::DIR {
// name.push(b'/');
// }
enum Color {
    Red,
    Green,
    // diff adds Bold, Cyan
}

impl Color {
    // https://en.wikipedia.org/wiki/ANSI_escape_code
    // Read: Select Graphic Rendition parameters
    fn sgr(&self) -> &'static [u8] {
        match self {
            Color::Red => b"\x1b[31m",
            Color::Green => b"\x1b[32m",
        }
    }
}

enum Style {
    Plain,
    Colored(Color),
}

impl Style {
    // the user might have redirected the stdout to another process or a file. we don't want to
    // add the sgr sequence as is, because only the terminal will recognize it and not display it
    fn for_stdout(color: Color) -> Self {
        if io::stdout().is_terminal() {
            Self::Colored(color)
        } else {
            Self::Plain
        }
    }

    // For porcelain output and tests no coloring
    fn plain() -> Self {
        Self::Plain
    }

    fn begin(&self, out: &mut impl Write) -> io::Result<()> {
        match self {
            Style::Plain => Ok(()),
            Style::Colored(color) => out.write_all(color.sgr()),
        }
    }

    fn end(&self, out: &mut impl Write) -> io::Result<()> {
        match self {
            Style::Plain => Ok(()),
            Style::Colored(_) => out.write_all(b"\x1b[m"),
        }
    }
}

pub(super) fn print(report: &Report, format: Format) -> io::Result<()> {
    match format {
        Format::Short => print_short(report),
        Format::Long => print_long(report),

    }
}

fn print_short(_report: &Report) -> io::Result<()> {
    Ok(())
}

fn print_long(report: &Report) -> io::Result<()> {
    let mut writer = io::stdout().lock();
    let staged = report.changes
        .iter()
        .filter_map(|(path, change)| {
            change.head_index.as_ref().map(|ch| {
                let label = match ch {
                    HeadIndexChange::ADDED => "new file:",
                    HeadIndexChange::MODIFIED => "modified:",
                    HeadIndexChange::DELETED => "deleted:",
                };
                (path, label)
            })
        });
    let unstaged = report.changes
        .iter()
        .filter_map(|(path, change)| {
            change.workspace_index.as_ref().map(|ch| {
                let label = match ch {
                    WorkspaceIndexChange::MODIFIED => "modified:",
                    WorkspaceIndexChange::DELETED => "deleted:",
                };
                (path, label)
            })
        });
    let untracked = report.untracked
        .iter()
        .map(|p| (p, ""));

    print_section(
        "Changes to be committed",
        staged,
        Style::for_stdout(Color::Green),
        &mut writer,
    )?;
    print_section(
        "Changes not staged for commit",
        unstaged,
        Style::for_stdout(Color::Red),
        &mut writer,
    )?;
    print_section(
        "Untracked files",
        untracked,
        Style::for_stdout(Color::Red),
        &mut writer,
    )?;
    Ok(())
}

fn print_section<'a, Iter, Writer>(
    heading: &str,
    changes: Iter,
    style: Style,
    out: &mut Writer,
) -> io::Result<()>
where
    Iter: Iterator<Item = (&'a RepoPath, &'static str)>,
    Writer: Write,
{
    let mut changes = changes.peekable();
    // empty section
    if changes.peek().is_none() {
        return Ok(());
    }

    writeln!(out, "{heading}:")?;
    writeln!(out)?;
    for (path, label) in changes {
        out.write_all(b"\t")?;
        style.begin(out)?;
        if !label.is_empty() {
            write!(out, "{label:<LABEL_WIDTH$}")?;
        }
        out.write_all(&stdout_bytes(path.as_bytes()))?;
        style.end(out)?;
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
