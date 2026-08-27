use crate::cmd::status::Format;
use crate::repo::os::FileKind;
use crate::repo::path::RepoPath;
use crate::repo::report::{HeadIndexChange, Report, WorkspaceIndexChange};
use std::io::{self, IsTerminal, Write};
use crate::cmd::print::{self, Printer};

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

pub(super) struct StatusPrinter {
    pub(super) format: Format
}

// TODO: nothing to commit, working tree clean
impl Printer for StatusPrinter {
    type T<'a> = Report;

    fn print(&self, value: &Report) -> io::Result<()> {
        match self.format {
            Format::Short => print_short(value),
            Format::Long => print_long(value),
        }
    }
}

fn print_short(_report: &Report) -> io::Result<()> {
    Ok(())
}

fn print_long(report: &Report) -> io::Result<()> {
    // if we don't acquire the lock, everytime we write stdout would have to acquire the lock.
    let mut writer = io::stdout().lock();
    
    let staged = report.changes.iter().filter_map(|(path, change)| {
        change.head_index.as_ref().map(|ch| {
            let label = match ch {
                HeadIndexChange::Added => "new file:",
                HeadIndexChange::Modified => "modified:",
                HeadIndexChange::Deleted => "deleted:",
            };
            (path.as_bytes(), label)
        })
    });
    let unstaged = report.changes.iter().filter_map(|(path, change)| {
        change.workspace_index.as_ref().map(|ch| {
            let label = match ch {
                WorkspaceIndexChange::Modified => "modified:",
                WorkspaceIndexChange::Deleted => "deleted:",
            };
            (path.as_bytes(), label)
        })
    });

    let mut untracked: Vec<Vec<u8>> = report
        .untracked
        .iter()
        .map(|(path, kind)| display_name(path, kind))
        .collect();
    untracked.sort();
    let untracked = untracked.iter().map(|p| (p.as_slice(), ""));

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

fn print_section<'a, Iter, W>(
    heading: &str,
    changes: Iter,
    style: Style,
    out: &mut W,
) -> io::Result<()>
where
    Iter: Iterator<Item = (&'a [u8], &'static str)>,
    W: Write,
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
        out.write_all(&print::stdout_path_bytes(path))?;
        style.end(out)?;
        out.write_all(b"\n")?;
    }
    writeln!(out)?;

    Ok(())
}

// sounds like we print something, but we actually get the name we will display in print()
fn display_name(path: &RepoPath, kind: &FileKind) -> Vec<u8> {
    let mut bytes = path.as_bytes().to_vec();
    if *kind == FileKind::Directory {
        bytes.push(b'/');
    }
    bytes
}
