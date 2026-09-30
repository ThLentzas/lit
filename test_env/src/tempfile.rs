use std::ffi::OsStr;
use std::io;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use tempfile::NamedTempFile;

pub struct TempConfigFile {
    file: NamedTempFile,
}

impl TempConfigFile {
    pub fn empty() -> io::Result<Self> {
        Ok(Self {
            file: NamedTempFile::new()?,
        })
    }

    pub fn path(&self) -> &Path {
        self.file.path()
    }

    pub fn close(self) -> io::Result<()> {
        self.file.close()
    }
}

#[derive(Default)]
pub struct Builder<'a> {
    settings: Vec<(&'a OsStr, Option<&'a OsStr>)>,
}

impl<'a> Builder<'a> {
    pub fn new() -> Builder<'a> {
        Self::default()
    }

    pub fn setting<K, V>(mut self, key: &'a K, value: Option<&'a V>) -> Self
    where
        // "foo" or any string literal is &'static str but our parameter is &K so matching the argument
        // we get:
        //  Parameter: &K
        //  Argument:  &str -> K = str which is unsized
        //  str is unsize even though &str is sized
        //
        // Generic type parameters have an implicit `Sized` bound.
        //
        // When `key` is defined as `key: K`, `K` matches the entire argument type
        // key: K + argument &str -> K = &str, no need for `?Sized`
        // key: &K + argument &str -> K = str
        //
        // `key: K` takes its argument by value, it depends on the K on what happens with ownership
        // if `K` is a reference the argument value is that reference. Share refs impl `Copy`, owned
        // types move.
        K: AsRef<OsStr> + ?Sized,
        V: AsRef<OsStr> + ?Sized,
    {
        self.settings.push((key.as_ref(), value.map(AsRef::as_ref)));
        self
    }

    pub fn build(self) -> io::Result<TempConfigFile> {
        let mut tempfile = NamedTempFile::new()?;
        for setting in self.settings {
            let key = setting.0.as_bytes();
            let mut parts = key.splitn(2, |&byte| byte == b'.');
            let section = parts.next().unwrap();
            let name = parts.next().unwrap();

            tempfile.write_all(b"[")?;
            tempfile.write_all(section)?;
            tempfile.write_all(b"]\n\t")?;
            tempfile.write_all(name)?;
            if let Some(value) = setting.1 {
                tempfile.write_all(b" = ")?;
                tempfile.write_all(value.as_bytes())?;
            }
            tempfile.write_all(b"\n")?;
        }
        Ok(TempConfigFile { file: tempfile })
    }
}
