use std::error::Error;
use std::fmt;

// eprintln!("{}", Report::new(&err))
// The idiomatic way of reporting errors is to call Display for parent and call source() to get the
// rest. Don't just delegate to their Display impl inside the parent's.
//
// Experimental in std:: https://doc.rust-lang.org/std/error/struct.Report.html
#[derive(Debug)]
pub(crate) struct ErrorReport<'a> {
    error: &'a dyn Error,
}

// TODO: should this be in lib.rs?
impl<'a> ErrorReport<'a> {
    pub(crate) fn new(error: &'a dyn Error) -> Self {
        Self { error }
    }
}

impl fmt::Display for ErrorReport<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.error)?;

        let mut source = self.error.source();
        while let Some(cause) = source {
            write!(f, "\n\tcaused by: {cause}")?;
            source = cause.source();
        }

        Ok(())
    }
}
