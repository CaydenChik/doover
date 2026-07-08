//! Read-only inspection: compare a recorded manifest against the live
//! filesystem, per entry (step 8, `doover diff`).
//!
//! This is strictly informational — it never mutates anything and shares its
//! notion of "changed" with the undo conflict oracle (content hash for files,
//! target for symlinks, kind for everything), so what `diff` reports as
//! modified is exactly what `undo` would flag as a conflict.

use crate::snapshot::{EntryKind, Manifest, Root, SnapshotError, hash_file};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathStatus {
    /// Matches the recorded state.
    Unchanged,
    /// Same kind, different content (file hash or symlink target).
    Modified,
    /// Recorded but absent now.
    Missing,
    /// Present but a different kind of filesystem object.
    TypeChanged,
    /// Recorded as absent, exists now.
    Created,
}

impl PathStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unchanged => "unchanged",
            Self::Modified => "modified",
            Self::Missing => "missing",
            Self::TypeChanged => "type-changed",
            Self::Created => "created",
        }
    }
}

#[derive(Debug)]
pub struct DiffLine {
    pub path: PathBuf,
    pub status: PathStatus,
}

/// Per-entry status of `m` against the world as it is right now.
pub fn diff_manifest(m: &Manifest) -> Result<Vec<DiffLine>, SnapshotError> {
    if m.root == Root::Absent {
        // the path did not exist when captured; one line says it all
        let status = if m.path.symlink_metadata().is_ok() {
            PathStatus::Created
        } else {
            PathStatus::Unchanged
        };
        return Ok(vec![DiffLine {
            path: m.path.clone(),
            status,
        }]);
    }

    let mut out = Vec::with_capacity(m.entries.len());
    for entry in &m.entries {
        // rel is empty for the root itself; join("") would grow a trailing
        // slash and stat("…/file/") is ENOTDIR — the intact root would read
        // as Missing
        let abs = if entry.rel.as_os_str().is_empty() {
            m.path.clone()
        } else {
            m.path.join(&entry.rel)
        };
        // never follow links: a file replaced by a symlink is a type change,
        // exactly as the restore path treats it
        let meta = match abs.symlink_metadata() {
            Ok(meta) => meta,
            Err(_) => {
                out.push(DiffLine {
                    path: abs,
                    status: PathStatus::Missing,
                });
                continue;
            }
        };
        let ft = meta.file_type();
        let status = match &entry.kind {
            EntryKind::File { hash, .. } => {
                if !ft.is_file() {
                    PathStatus::TypeChanged
                } else if &hash_file(&abs)? != hash {
                    PathStatus::Modified
                } else {
                    PathStatus::Unchanged
                }
            }
            EntryKind::Dir { .. } => {
                if ft.is_dir() {
                    PathStatus::Unchanged
                } else {
                    PathStatus::TypeChanged
                }
            }
            EntryKind::Symlink { target } => {
                if !ft.is_symlink() {
                    PathStatus::TypeChanged
                } else if std::fs::read_link(&abs).ok().as_deref() != Some(target) {
                    PathStatus::Modified
                } else {
                    PathStatus::Unchanged
                }
            }
            EntryKind::Fifo { .. } => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::FileTypeExt;
                    if ft.is_fifo() {
                        PathStatus::Unchanged
                    } else {
                        PathStatus::TypeChanged
                    }
                }
                #[cfg(not(unix))]
                PathStatus::TypeChanged
            }
        };
        out.push(DiffLine { path: abs, status });
    }
    Ok(out)
}
