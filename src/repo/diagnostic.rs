use std::error::Error;
use std::fmt;

// eprintln!("{}", Report::new(&err))
// The idiomatic way of reporting errors is to call Display for parent and call source() to get the
// rest. Don't just delegate to their Display impl inside the parent's.
//
// Experimental in std:: https://doc.rust-lang.org/std/error/struct.Report.html
#[derive(Debug)]
pub(crate) struct Report<'a> {
    error: &'a dyn Error,
}

impl<'a> Report<'a> {
    pub(crate) fn new(error: &'a dyn Error) -> Self {
        Self { error }
    }
}

impl fmt::Display for Report<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.error)?;

        let mut current = self.error.source();
        while let Some(cause) = current {
            write!(f, "\n\tcaused by: {cause}")?;
            current = cause.source();
        }

        Ok(())
    }
}
