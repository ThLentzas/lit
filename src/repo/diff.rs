use crate::repo::Repository;
use crate::repo::config::ConfigFileError;
use crate::repo::db::DatabaseError;
use crate::repo::index::Index;
use crate::repo::object::Object;
use crate::repo::object::mode::Mode;
use crate::repo::os::{FileKind, IoError};
use crate::repo::repo_path::RepoPath;
use crate::repo::report::{Report, ReportError, WorkspaceIndexChange};
use similar::{Algorithm, DiffOp};
use std::ops::Range;

const DEFAULT_UNIFIED_FORMAT_CONTEXT: usize = 3;

// TODO: all the comments on how things work and design move it to the doc
// `context` are the unchanged lines shown around changes, default value for unified format is `3`
// each hunk includes up to 3 unchanged lines before and after the changed region
//
// header follows this format: @@ -old_start,old_count +new_start,new_count @@
//  @@ -1,6 +1,6 @@
//      -1, 6: starts at line 1 of the old file and covers 6 lines
//      +1, 6: starts at line 1 of the new file and covers 6 lines
//  counts include context:
//      old count = unchanged + deleted
//      new count = unchanged + inserted
//
//  count is not an ending line number, -10, 6 covers the old lines 10 - 15
//
// a hunk is a region/area where the files differ
// a file can have zero, one or multiple hunks. The number of hunks is determined by the number of
// consecutive unchanged lines between changes.
pub(crate) struct Hunk {
    old_range: Range<usize>,
    new_range: Range<usize>,
    // Each operation contains positions which we use to retrieve the actual lines.
    //
    // let old_lines: Vec<&[u8]> = vec![b"apple\n", b"banana\n", b"cherry\n"];
    // let new_lines: Vec<&[u8]> = vec![
    //     b"apple\n",
    //     b"blueberry\n",
    //     b"mango\n",
    //     b"cherry\n",
    // ];
    //
    // [
    //     DiffOp::Equal {
    //         old_index: 0,
    //         new_index: 0,
    //         len: 1,
    //     },
    //     DiffOp::Replace {
    //         old_index: 1,
    //         old_len: 1,
    //         new_index: 1,
    //         new_len: 2,
    //     },
    //     DiffOp::Equal {
    //         old_index: 2,
    //         new_index: 3,
    //         len: 1,
    //     },
    // ]
    //
    //  Operation	            Retrieve	   Display
    // First Equal	        old_lines[0..1]	    apple  // (0, 0, 1) starting at index 0 at the old file and index 0 at the new file there is a region of 1 identical lines.
    // Replace: old part	old_lines[1..2]	   -banana
    // Replace: new part	new_lines[1..3]	 +blueberry, +mango
    // Last Equal	        old_lines[2..3]	    cherry
    ops: Vec<DiffOp>,
}

pub(crate) struct FileVersion {
    mode: Mode,
    content: Vec<u8>,
}

// DiffFile represents the version , Patch describes the changes between two versions
pub(crate) struct Patch {
    pub(crate) path: RepoPath,
    pub(crate) old_version: Option<FileVersion>,
    pub(crate) new_version: Option<FileVersion>,
    pub(crate) hunks: Vec<Hunk>,
}

// Both algos use the Myers' approach. The difference is whether they take shortcuts when the search
// becomes to expensive. Both produce a valid script. When the input is large Myers Algorithm::Myers
// would use heuristics that could sacrifice minimality while Algorithm::RawMyers can spend much
// longer finding the minimum.
// The --minimal flag will use Algorithm::RawMyers explicitly

// default diff compares tracked file versions in the index against the workspace.
pub(crate) fn unstaged(
    repo: &Repository,
    // TODO: we need to address this on the caller, because if refresh_index is false we should not
    //  pass &mut Index but &Index
    index: &mut Index,
    refresh_index: bool,
) -> Result<Vec<Patch>, DiffError> {
    let workspace = repo.workspace().unwrap();
    let cfg = repo.config()?;
    let db = repo.database();
    // TODO: apply refreshes
    //  https://git-scm.com/docs/git-config#Documentation/git-config.txt-diffautoRefreshIndex
    let report = Report::unstaged_changes(&workspace, &index, &cfg, refresh_index)?;
    let mut patches = Vec::with_capacity(report.unstaged.len());

    for (path, change) in report.unstaged {
        let index_entry = index.get(&path).unwrap();
        let Object::Blob(content) = db.load(index_entry.oid())? else {
            // TODO: this is an error?
            continue;
        };
        let old_content = content;
        let (new_mode, new_content) = match change {
            WorkspaceIndexChange::Modified(node) => {
                // we can't match on `index_entry.mode` because this is the old mode of the entry
                // and the new one might be different
                // if `index_entry.mode` is a symlink but the new mode is `Regular` we should use
                // `read_file()` not `read_symlink()` and vice versa.
                //
                // this is a TOCTOU race condition the entry might have changed from when we recorded
                // metadata to when we make the `read` call
                // we could also make a new `stat()` call but that does not change anything, the gap
                // is still there
                let new_mode = match Mode::try_from(node.kind) {
                    Ok(mode) => mode,
                    Err(_) => return Err(DiffError::UnsupportedFileType(path)),
                };
                // if kind is FileKind::Other, `try_from()` would return an error
                // if kind is FileKind::Directory, this can't actually happen because workspace scan
                // does not record directories
                // it is safe to call `read_file()` in the else block
                let new_content = if matches!(&node.kind, FileKind::Symlink) {
                    workspace.read_link(&path)?
                } else {
                    workspace.read_file(&path)?
                };
                (Some(new_mode), Some(new_content))
            }
            WorkspaceIndexChange::Deleted => (None, None),
        };
        let old_lines: Vec<&[u8]> = old_content.split_inclusive(|&byte| byte == b'\n').collect();
        // When the entry is deleted from the workspace we pass an empty slice, `new_content` remains
        // `None` and we will use that in the patch to indicate deletion.
        // We treat a missing new version as a valid deletion. We still need to build hunks showing
        // which lines were removed. This is why we pass an empty slice, we treat deletion as `delete
        // every old line`. For an empty file being deleted, there no content hunks. Patch's new: None
        // will signal to the printer to emit the file-deletion header.
        let new_lines: Vec<&[u8]> = new_content
            .as_ref()
            .map(|content| content.split_inclusive(|&byte| byte == b'\n').collect())
            .unwrap_or_default();
        // returns the list of operations describing how to transform sequence `a` into sequence `b`
        // `Replace` means replace this range of `a` with this range of `b`
        let ops = similar::capture_diff_slices(Algorithm::Myers, &old_lines, &new_lines);
        let old_version = Some(FileVersion {
            mode: index_entry.mode,
            content: old_content,
        });
        let new_version = new_mode.map(|mode| FileVersion {
            mode,
            // safe, when mode is `Some`, new_content is also `Some`
            content: new_content.unwrap(),
        });
        let patch = Patch {
            path,
            old_version,
            new_version,
            hunks: merge_overlapping_changes(ops, DEFAULT_UNIFIED_FORMAT_CONTEXT),
        };
        patches.push(patch);
    }
    Ok(patches)
}

// we merge nearby changes into one hunk when their context regions overlap or touch. This way we
// keep nearby edits together without printing the same context twice.
// if we kept overlapping hunks separate, the same unchanged line would appear twice
//
// Without merging:
//   @@ -1,5 +1,5 @@
//      start
//      -color=red
//      +color=blue
//      alpha
//      beta
//      shared
//   @@ -5,5 +5,5 @@
//      shared
//      gamma
//      delta
//      -workers=2
//      +workers=4
//      end
//
// `shared` is the same line 5, printed twice:
//     - It belongs to the first change’s three trailing context lines: alpha, beta, shared.
//     - It belongs to the second change’s three leading context lines: shared, gamma, delta.
//   @@ -1,9 +1,9 @@
//      start
//      -color=red
//      +color=blue
//      alpha
//      beta
//      shared
//      gamma
//      delta
//      -workers=2
//      +workers=4
//      end
//
// This is the duplication the merging rule prevents. Properly separated hunks have a gap between
// their context regions, so they don’t repeat lines
fn merge_overlapping_changes(ops: Vec<DiffOp>, context: usize) -> Vec<Hunk> {
    similar::group_diff_ops(ops, context)
        .into_iter()
        .map(|ops| {
            let first = ops.first().unwrap();
            let last = ops.last().unwrap();
            let old_range = first.old_range().start..last.old_range().end;
            let new_range = first.new_range().start..last.new_range().end;
            Hunk {
                old_range,
                new_range,
                ops,
            }
        })
        .collect()
}

pub(crate) enum DiffError {
    Config(ConfigFileError),
    Report(ReportError),
    Io(IoError),
    Database(DatabaseError),
    // TODO: for unsupported file types git only includes the path of the file, not the actual type
    UnsupportedFileType(RepoPath),
}

impl From<ConfigFileError> for DiffError {
    fn from(err: ConfigFileError) -> Self {
        Self::Config(err)
    }
}

impl From<ReportError> for DiffError {
    fn from(err: ReportError) -> Self {
        Self::Report(err)
    }
}

impl From<IoError> for DiffError {
    fn from(err: IoError) -> Self {
        Self::Io(err)
    }
}

impl From<DatabaseError> for DiffError {
    fn from(err: DatabaseError) -> Self {
        Self::Database(err)
    }
}
