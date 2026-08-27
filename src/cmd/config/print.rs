use std::io;
use crate::cmd::print::Printer;
use crate::repo::config::ConfigEntry;

pub(super) struct ConfigPrinter {

}

impl Printer for ConfigPrinter {
    type T<'a> = ConfigEntry<'a>;

    fn print<'a>(&self, value: &ConfigEntry<'a>) -> io::Result<()> {

        Ok(())
    }
}