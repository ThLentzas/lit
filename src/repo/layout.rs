use crate::repo::os::{IoError, OsPath};
use std::{fs, io};

// describes how the resolved worktree is connected to the metadata directory
#[derive(Debug, PartialEq, Eq)]
enum MetadataPlacement {
    // Nothing connects metadata with the worktree, no `.lit` dir, no pointer file, they are separate
    // like `LIT_DIR` and `LIT_WORK_TREE` can name two completely unrelated directories. It is also
    // used for bare repos
    Unlinked,
    // Ordinary `<worktree>/.lit` dir
    Embedded,
    // `<worktree>/.lit` is a pointer file to metadata
    Separate { pointer_file: OsPath },
}

impl MetadataPlacement {
    // we can't naively compare paths without resolution first
    //
    // 1. `worktree_dir` has the `core.worktree` value resolved based on metadata dir
    //      metadata_dir = /projects/app/.lit
    //      core.worktree = ..
    //      worktree = metadata_dir.join(worktree_dir) -> /projects/app/.lit/..
    //
    //      let entry = dir.join_unchecked(".lit"); which creates /projects/app/.lit/../.lit
    //      the comparison then fails, and we end up with `Direct` which
    //      is incorrect because /projects/app/.lit/../.lit normalizes to /projects/app/.lit so
    //      the result should be `Embedded`. core.worktree is /projects/app
    // 2. `pointer_file` is symlink `projects/app/.lit` pointing to `/storage/app-metadata`
    //      worktree is `core.worktree` = `projects/app/src/..`
    //      entry = projects/app/src/../.lit
    //      the comparison fails, and we get `Direct` instead of `Separate`. The actual worktree
    //      path is `projects/app` and `/projects/app/.lit is a pointer file
    // 3. worktree is reach via symlink
    //      `/home/user/app` is a symlink to `/projects/app
    //      `LIT_DIR` = /projects/app/.lit
    //      `LIT_WORK_TREE` = /home/user/app
    //
    //      we compare /projects/app/.lit to /home/user/app/.lit again getting `Direct` instead of
    //      `Embedded`
    //
    // all arguments are absolute canonical paths
    fn from_discovery(
        metadata_dir: &OsPath,
        worktree_dir: Option<&OsPath>,
        pointer_file: Option<OsPath>,
    ) -> Result<Self, IoError> {
        // bare repos, no relationship to classify
        let Some(worktree_dir) = worktree_dir else {
            return Ok(Self::Unlinked);
        };

        let entry = worktree_dir.join_unchecked(".lit");
        let metadata = match fs::metadata(&entry) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok(Self::Unlinked);
            }
            Err(err) => return Err(IoError::new("stat", Some(entry), err)),
        };

        // Note: `entry` is still not in canonical form, we need to either call `fs::canonicalize()`
        // or compare using `OsPath::same_canonical_path_with()`. It is important to do that because
        // `entry` can be a symlink to a pointer, and we don't want to compare the symlink's path
        // with `pointer_file` but target's path. check test case p07
        if metadata.is_dir() {
            // if entry exists and is a dir compare with the metadata dir
            if metadata_dir.same_canonical_path_with(&entry)? {
                return Ok(Self::Embedded);
            }
        } else if metadata.is_file() {
            // this is case 2 mentioned in the comment above
            if let Some(pointer_file) = pointer_file
                && pointer_file.same_canonical_path_with(&entry)?
            {
                return Ok(Self::Separate { pointer_file });
            }
        }
        Ok(Self::Unlinked)
    }
}

#[derive(Debug)]
pub(crate) struct Layout {
    // directory containing the repository metadata (HEAD, config, objects, ...)
    metadata_dir: OsPath,
    // root of the working tree for non-bare
    // None for bare
    worktree_dir: Option<OsPath>,
    // describes how the metadata is connected to the worktree
    placement: MetadataPlacement,
}

impl Layout {
    // the worktree root for non-bare or the metadata directory for bare
    pub(crate) fn root(&self) -> &OsPath {
        self.worktree_dir.as_ref().unwrap_or(&self.metadata_dir)
    }

    pub(crate) fn metadata_dir(&self) -> &OsPath {
        &self.metadata_dir
    }

    pub(crate) fn worktree_dir(&self) -> Option<&OsPath> {
        self.worktree_dir.as_ref()
    }

    // link is always absolute by construction
    pub(crate) fn pointer_file(&self) -> Option<&OsPath> {
        if let MetadataPlacement::Separate { pointer_file } = &self.placement {
            return Some(pointer_file);
        }
        None
    }

    pub(crate) fn is_bare(&self) -> bool {
        self.worktree_dir.is_none()
    }

    pub(super) fn from_discovery(
        metadata_dir: OsPath,
        worktree_dir: Option<OsPath>,
        pointer_file: Option<OsPath>,
    ) -> Result<Self, IoError> {
        let placement =
            MetadataPlacement::from_discovery(&metadata_dir, worktree_dir.as_ref(), pointer_file)?;

        Ok(Self {
            metadata_dir,
            worktree_dir,
            placement,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::os;
    use sealed_test::prelude::*;
    use tempfile::Builder;

    // No worktree
    #[test]
    fn p01() {
        let metadata_dir = OsPath::new_unchecked("/demo/metadata");

        let placement = MetadataPlacement::from_discovery(&metadata_dir, None, None).unwrap();

        assert_eq!(placement, MetadataPlacement::Unlinked)
    }

    // `<worktree>/.lit` does not exist -> `worktree_dir` and `metadata_dir` are not related. We can
    // still discover them via config values and env vars. Such case is `LIT_DIR` and `LIT_WORK_TREE`
    // mention unrelated paths. `Destination::needs_worktree_config()` handles such cases
    #[test]
    fn p02() {
        let metadata_dir = OsPath::new_unchecked("/demo/metadata");
        let worktree_dir = OsPath::new_unchecked("/projects/lit");

        let placement =
            MetadataPlacement::from_discovery(&metadata_dir, Some(&worktree_dir), None).unwrap();

        assert_eq!(placement, MetadataPlacement::Unlinked)
    }

    // `metadata` = /projects/lit/.lit
    // `worktree` = /projects/lit/
    // The `entry` canonicalizes to a directory, and it is equal to the metadata directory
    #[test]
    fn p03() {
        let temp = tempfile::tempdir().unwrap();
        // we create the `.lit` directory inside `temp` with 0 added randomness in the name
        let metadata_dir = Builder::new()
            .prefix(".lit")
            .rand_bytes(0)
            .tempdir_in(temp.path())
            .unwrap();
        let metadata_dir = OsPath::new_unchecked(metadata_dir.path());
        let worktree_dir = OsPath::new_unchecked(temp.path());

        let placement =
            MetadataPlacement::from_discovery(&metadata_dir, Some(&worktree_dir), None).unwrap();

        assert_eq!(placement, MetadataPlacement::Embedded);
        // explicit call to report potential errors during `drop()`
        temp.close().unwrap();
    }

    // `metadata_dir` and `worktree_dir` are unrelated but `entry` which is `<worktree_dir>/.lit`
    // exists as a symlink that points to `metadata_dir`
    // `metadata` = /storage/app-metadata
    // `worktree` = /projects/app
    // `entry` = /projects/app/.lit -> /storage/app-metadata
    #[test]
    fn p04() {
        let temp1_dir = tempfile::tempdir().unwrap();
        let temp2_dir = tempfile::tempdir().unwrap();
        let worktree_dir = OsPath::new_unchecked(temp1_dir.path());
        let metadata_dir = OsPath::new_unchecked(temp2_dir.path());
        let link = worktree_dir.join_unchecked(".lit");
        os::symlink(&metadata_dir, &link).unwrap();

        let placement =
            MetadataPlacement::from_discovery(&metadata_dir, Some(&worktree_dir), None).unwrap();

        assert_eq!(placement, MetadataPlacement::Embedded);
        // explicit call to report potential errors during `drop()`
        temp1_dir.close().unwrap();
        temp2_dir.close().unwrap();
    }

    // `entry` exists and is pointer file
    //
    // `metadata` = /storage/app-metadata
    // `worktree` = /projects/app
    // `pointer_file` = /projects/app/.lit -> /storage/app-metadata
    // `entry` = /projects/app/.lit
    //
    // similar to the above case, just different branch in the `MetadataPlacement::from_discovery()`
    // logic. Placement does not reread the pointer's content, Repository::discover() already resolved
    // it, we can assume it is `litdir: /storage/app-metadata`
    #[test]
    fn p05() {
        let temp_dir = tempfile::tempdir().unwrap();
        let worktree_dir = OsPath::new_unchecked(temp_dir.path());
        let metadata_dir = OsPath::new_unchecked("/storage/metadata");
        let tempfile = Builder::new()
            .prefix(".lit")
            .rand_bytes(0)
            .tempfile_in(&worktree_dir)
            .unwrap();
        let pointer_file = OsPath::new_unchecked(tempfile.path());

        let placement = MetadataPlacement::from_discovery(
            &metadata_dir,
            Some(&worktree_dir),
            Some(pointer_file.clone()),
        )
        .unwrap();

        assert_eq!(placement, MetadataPlacement::Separate { pointer_file });
        // explicit call to report potential errors during `drop()`
        // tempfile is also cleaned since it lives inside `temp_dir`
        temp_dir.close().unwrap();
    }

    // `entry` is a symlink to the pointer file
    // same as p07 but now `entry` is not a `pointer_file` itself but a symlink to one
    //
    // `metadata` = /storage/app-metadata
    // `worktree` = /projects/app
    // `pointer_file` = /pointers/lit
    // `entry` = /projects/app/.lit -> /pointers/lit
    #[test]
    fn p07() {
        let temp_dir = tempfile::tempdir().unwrap();
        let pointer_dir = tempfile::tempdir().unwrap();
        let worktree_dir = OsPath::new_unchecked(temp_dir.path());
        let metadata_dir = OsPath::new_unchecked("/storage/app-metadata");
        let link = worktree_dir.join_unchecked(".lit");
        let tempfile = Builder::new()
            .prefix(".lit")
            .rand_bytes(0)
            // don't try `TempDir::new().unwrap()` because the directory gets dropped immediately
            .tempfile_in(pointer_dir.path())
            .unwrap();
        let pointer_file = OsPath::new_unchecked(tempfile.path());
        os::symlink(&pointer_file, &link).unwrap();

        let placement = MetadataPlacement::from_discovery(
            &metadata_dir,
            Some(&worktree_dir),
            Some(pointer_file.clone()),
        )
        .unwrap();

        assert_eq!(placement, MetadataPlacement::Separate { pointer_file });
        // explicit call to report potential errors during `drop()`
        temp_dir.close().unwrap();
        // tempfile is also cleaned since it lives inside `pointer_dir`
        pointer_dir.close().unwrap();
    }

    // `entry` exists but is some unrelated directory
    //
    // `metadata` = /storage/app-metadata
    // `worktree` = /projects/app
    // `pointer_file` = /projects/lit/.lit
    // `entry` = /projects/app/.lit is a different directory
    //
    // the directory branch compares `/storage/app-metadata` != `/projects/app/.lit`
    // Merely having `.lit` directory does not connect it to the selected metadata
    #[test]
    fn p08() {
        let temp1_dir = tempfile::tempdir().unwrap();
        // Note: don't try to use `let _ =` because `_` is not a binding it's a wildcard pattern and
        // drop happens immediately which deletes `temp_dir`
        // this creates `entry` so the fs::metadata() does not fail
        let _dir = Builder::new()
            .prefix(".lit")
            .rand_bytes(0)
            .tempdir_in(temp1_dir.path())
            .unwrap();
        let worktree_dir = OsPath::new_unchecked(temp1_dir.path());
        let temp2_dir = tempfile::tempdir().unwrap();
        let metadata_dir = Builder::new()
            .prefix("metadata")
            .rand_bytes(0)
            .tempdir_in(temp2_dir.path())
            .unwrap();

        let metadata_dir = OsPath::new_unchecked(metadata_dir.path());
        let placement =
            MetadataPlacement::from_discovery(&metadata_dir, Some(&worktree_dir), None).unwrap();

        assert_eq!(placement, MetadataPlacement::Unlinked);
        // explicit call to report potential errors during `drop()`
        // `_dir` and `metadata_dir` live inside `temp1_dir` and `temp2_dir` and are cleaned automatically
        // from the recursive walks of their respective directories
        temp1_dir.close().unwrap();
        temp2_dir.close().unwrap();
    }

    // `entry` exists but is a regular file
    //
    // `metadata` = /storage/app-metadata
    // `worktree` = /projects/app
    // `pointer_file` = /foo/.lit
    // `entry` = /projects/app/.lit is a file
    //
    // the directory branch compares `/foo/.lit` != `/projects/app/.lit`
    #[test]
    fn p09() {
        let temp1_dir = tempfile::tempdir().unwrap();
        // Note: don't try to use `let _ =` because `_` is not a binding it's a wildcard pattern and
        // drop happens immediately which deletes `temp_dir`
        // this creates `entry` so the fs::metadata() does not fail
        let _file = Builder::new()
            .prefix(".lit")
            .rand_bytes(0)
            .tempfile_in(temp1_dir.path())
            .unwrap();
        let worktree_dir = OsPath::new_unchecked(temp1_dir.path());
        let temp2_dir = tempfile::tempdir().unwrap();
        let pointer_file = Builder::new()
            .prefix(".lit")
            .rand_bytes(0)
            .tempfile_in(temp2_dir.path())
            .unwrap();
        let pointer_file = OsPath::new_unchecked(pointer_file.path());
        let metadata_dir = OsPath::new_unchecked("/storage/metadata");

        let placement = MetadataPlacement::from_discovery(
            &metadata_dir,
            Some(&worktree_dir),
            Some(pointer_file),
        )
        .unwrap();

        assert_eq!(placement, MetadataPlacement::Unlinked);
        // explicit call to report potential errors during `drop()`
        // `_file` and `pointer_file` live inside `temp1_dir` and `temp2_dir` and are cleaned automatically
        // from the recursive walks of their respective directories
        temp1_dir.close().unwrap();
        temp2_dir.close().unwrap();
    }

    // `entry` is a dangling symlink
    #[test]
    fn p10() {
        let temp_dir = tempfile::tempdir().unwrap();
        let worktree_dir = OsPath::new_unchecked(temp_dir.path());
        let metadata_dir = OsPath::new_unchecked("/storage/app-metadata");
        let link = worktree_dir.join_unchecked(".lit");
        let missing = OsPath::new_unchecked("/missing");
        // dangling symlink
        os::symlink(&missing, &link).unwrap();

        let placement =
            MetadataPlacement::from_discovery(&metadata_dir, Some(&worktree_dir), None).unwrap();

        assert_eq!(placement, MetadataPlacement::Unlinked);
        // explicit call to report potential errors during `drop()`
        temp_dir.close().unwrap();
    }
}
