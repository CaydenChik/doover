//! Read-only inspection: compare a recorded manifest against the live
//! filesystem, per entry (step 8, `doover diff`).
//!
//! This is strictly informational — it never mutates anything and shares its
//! notion of "changed" with the undo conflict oracle (content hash for files,
//! target for symlinks, kind for everything), so what `diff` reports as
//! modified is exactly what `undo` would flag as a conflict.
//!
//! Robustness (audit round 13): an informational command must degrade, never
//! abort. One unreadable file marks that line `Unreadable` and the walk
//! continues; a root whose identity changed (a dir now a symlink) is reported
//! and the walk stops, so child paths are never stat'd THROUGH an impostor
//! (which would both mislead and hash an unbounded, unrelated tree); a
//! truncated pre-manifest flags the whole report `partial`.

use crate::snapshot::{EntryKind, Manifest, Root, SnapshotError, hash_file};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

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
    /// Present and same kind, but we could not read it to compare (e.g.
    /// permission denied). Reported, not fatal.
    Unreadable,
}

impl PathStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unchanged => "unchanged",
            Self::Modified => "modified",
            Self::Missing => "missing",
            Self::TypeChanged => "type-changed",
            Self::Created => "created",
            Self::Unreadable => "unreadable",
        }
    }
}

#[derive(Debug)]
pub struct DiffLine {
    pub path: PathBuf,
    pub status: PathStatus,
}

#[derive(Debug)]
pub struct DiffReport {
    pub lines: Vec<DiffLine>,
    /// The recorded manifest was truncated by limits, so this comparison
    /// covers only part of the captured tree — never imply total coverage.
    pub partial: bool,
}

/// Per-entry status of `m` against the world as it is right now.
pub fn diff_manifest(m: &Manifest) -> Result<DiffReport, SnapshotError> {
    let partial = m.truncated;
    if m.root == Root::Absent {
        // the path did not exist when captured; one line says it all
        let status = if m.path.symlink_metadata().is_ok() {
            PathStatus::Created
        } else {
            PathStatus::Unchanged
        };
        return Ok(DiffReport {
            lines: vec![DiffLine {
                path: m.path.clone(),
                status,
            }],
            partial,
        });
    }

    // Evaluate the recorded root first. If its very identity changed —
    // type-changed (a dir replaced by a symlink/file) or gone — then every
    // child entry is relative to a root that no longer means what it meant.
    // Stat'ing `root/child` would resolve THROUGH the impostor (a symlink to
    // an unrelated, possibly huge tree): misleading statuses and unbounded
    // hashing of data the action never touched. Report the root and stop.
    let root_kind = m
        .entries
        .iter()
        .find(|e| e.rel.as_os_str().is_empty())
        .map(|e| &e.kind);
    if let Some(kind) = root_kind {
        let root_status = status_of_entry(&m.path, kind)?;
        if matches!(root_status, PathStatus::TypeChanged | PathStatus::Missing) {
            return Ok(DiffReport {
                lines: vec![DiffLine {
                    path: m.path.clone(),
                    status: root_status,
                }],
                partial,
            });
        }
    }

    let mut lines = Vec::with_capacity(m.entries.len());
    for entry in &m.entries {
        // rel is empty for the root itself; join("") would grow a trailing
        // slash and stat("…/file/") is ENOTDIR — the intact root would read
        // as Missing
        let abs = if entry.rel.as_os_str().is_empty() {
            m.path.clone()
        } else {
            m.path.join(&entry.rel)
        };
        let status = status_of_entry(&abs, &entry.kind)?;
        lines.push(DiffLine { path: abs, status });
    }
    Ok(DiffReport { lines, partial })
}

/// Compare one live path against a recorded entry kind. Never follows links
/// (a file replaced by a symlink is a type change, matching the restore path)
/// and never aborts: an unreadable object is `Unreadable`, not an error.
fn status_of_entry(abs: &Path, kind: &EntryKind) -> Result<PathStatus, SnapshotError> {
    let meta = match abs.symlink_metadata() {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(PathStatus::Missing),
        // lstat itself failed (e.g. a parent dir lost search permission): we
        // cannot judge, so say so rather than pretend it is missing
        Err(_) => return Ok(PathStatus::Unreadable),
    };
    let ft = meta.file_type();
    let status = match kind {
        EntryKind::File { hash, .. } => {
            if !ft.is_file() {
                PathStatus::TypeChanged
            } else {
                match hash_file(abs) {
                    Ok(h) if &h == hash => PathStatus::Unchanged,
                    Ok(_) => PathStatus::Modified,
                    // present, still a file, but unreadable — do not abort the
                    // whole report over one locked file
                    Err(_) => PathStatus::Unreadable,
                }
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
            } else {
                match std::fs::read_link(abs) {
                    Ok(t) if &t == target => PathStatus::Unchanged,
                    Ok(_) => PathStatus::Modified,
                    Err(_) => PathStatus::Unreadable,
                }
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
            {
                PathStatus::TypeChanged
            }
        }
    };
    Ok(status)
}
