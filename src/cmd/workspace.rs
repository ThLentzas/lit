use std::fs;
use std::path::PathBuf;

pub(super) struct Workspace {
    pub(super) cwd: PathBuf
}

impl Workspace {
    pub(super) fn list_files(&self) -> Vec<PathBuf> {
        fs::read_dir(&self.cwd).unwrap()
            // equivalent
            // filter(|entry| {
            //         let entry = entry.as_ref().unwrap();
            //         let name = entry.file_name();
            //         name != ".git"
            //     })
            //     .map(|entry| {
            //         entry.unwrap().path()
            //     })
            //     .collect();
            .filter_map(|entry| {
                // if the entry is an Err, ok() turns it into None, and ? returns that None
                // immediately, which tells filter_map to skip it.
                let entry = entry.ok()?;
                if entry.file_name() == ".git" {
                    None
                } else {
                    Some(entry.path())
                }

            })
            .collect()
    }
}