use crate::command::{
    add::AddEvent,
    add::RefreshChange,
    print::{Printer, ReadableBytes},
};
use std::io::{self, StdoutLock, Write};

pub(super) struct AddPrinter {
    out: StdoutLock<'static>,
}

impl AddPrinter {
    pub(super) fn lock() -> Self {
        Self {
            out: io::stdout().lock(),        }
    }
}

impl Printer for AddPrinter {
    type T<'a> = AddEvent<'a>;

    fn print<'a>(&mut self, event: &AddEvent) -> io::Result<()> {
        match event {
            AddEvent::Added(path) => {
                writeln!(&mut self.out, "add '{}'", ReadableBytes(path.as_bytes()))
            }
            AddEvent::Removed(path) => writeln!(
                &mut self.out,
                "remove '{}'",
                ReadableBytes(path.as_bytes())
            ),
            AddEvent::Ignored(path) => writeln!(
                &mut self.out,
                "ignored '{}'",
                ReadableBytes(path.as_bytes())
            ),
            AddEvent::RefreshHeader => self
                .out
                .write_all(b"unstage changes after refreshing the index:\n"),
            AddEvent::Refresh(path, change) => match change {
                RefreshChange::Modified => {
                    writeln!(&mut self.out, "M '{}'", ReadableBytes(path.as_bytes()))
                }
                RefreshChange::Deleted => {
                    writeln!(&mut self.out, "D '{}'", ReadableBytes(path.as_bytes()))
                }
                RefreshChange::TypeChanged => {
                    writeln!(&mut self.out, "T '{}'", ReadableBytes(path.as_bytes()))
                }
            },
        }
    }
}
