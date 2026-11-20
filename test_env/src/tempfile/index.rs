use std::{io::{self, Write}, path::Path};
use tempfile::NamedTempFile;

pub struct TempIndexFile {
    file: NamedTempFile,
}

impl TempIndexFile {
    pub fn path(&self) -> &Path {
        self.file.path()
    }

    pub fn close(self) -> io::Result<()> {
        self.file.close()
    }

    pub fn with_content(bytes: &[u8]) -> io::Result<Self> {
        let mut tempfile = NamedTempFile::new()?;
        tempfile.write_all(bytes)?;
        
        Ok(Self { file: tempfile })
    }
}
