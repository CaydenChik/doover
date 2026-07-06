//! Content-addressed snapshot store.
//!
//! Pre-action state is captured into `<store>/objects/<hh>/<blake3>` using
//! copy-on-write clones where the filesystem supports them (`clonefile` on
//! APFS, `FICLONE` on Btrfs/XFS via the `reflink-copy` crate) and plain copies
//! everywhere else. Ingestion clones *first* and hashes the frozen clone, so a
//! concurrently mutating source can never poison the store with a wrong hash.
//!
//! Restore is fail-closed: every referenced object is re-hashed and verified
//! **before** the first byte of the destination is touched; a corrupt store
//! refuses loudly instead of "restoring" garbage.
//!
//! File metadata (mode, mtime, xattrs) lives in the [`Manifest`], not in the
//! object store — objects are pure content, which is what makes deduplication
//! sound. Hardlink identity is not preserved: links restore as independent
//! files with identical content.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("io error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("store object {hash} is corrupt (content does not match its hash); restore refused")]
    CorruptObject { hash: String },
    #[error("store object {hash} is missing; restore refused")]
    MissingObject { hash: String },
}

fn io_err(path: &Path, source: io::Error) -> SnapshotError {
    SnapshotError::Io {
        path: path.display().to_string(),
        source,
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct StoreOptions {
    /// Skip reflink attempts and always plain-copy (also settable via the
    /// DOOVER_FORCE_COPY env var through [`Store::open`]).
    pub force_copy: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_files: u64,
    pub max_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Root {
    /// The path existed when snapshotted.
    Present,
    /// The path did not exist: restoring deletes whatever is there now.
    Absent,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EntryKind {
    File {
        hash: String,
        len: u64,
        mode: u32,
        mtime: SystemTime,
        xattrs: Vec<(String, Vec<u8>)>,
    },
    Dir {
        mode: u32,
    },
    Symlink {
        target: PathBuf,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    /// Path relative to the snapshot root; empty for the root itself.
    pub rel: PathBuf,
    pub kind: EntryKind,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Manifest {
    /// Absolute path this snapshot captured.
    pub path: PathBuf,
    pub root: Root,
    /// Walk order: parents before children.
    pub entries: Vec<Entry>,
    /// True when limits stopped the capture before completion.
    pub truncated: bool,
    /// Files seen but not captured because of limits.
    pub skipped: u64,
}

#[derive(Debug, Default)]
pub struct RestoreReport {
    pub files_restored: u64,
    pub warnings: Vec<String>,
}

pub struct Store {
    objects: PathBuf,
    tmp: PathBuf,
    opts: StoreOptions,
    seq: AtomicU64,
}

impl Store {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, SnapshotError> {
        let force_copy = std::env::var_os("DOOVER_FORCE_COPY").is_some();
        Self::open_with(root, StoreOptions { force_copy })
    }

    pub fn open_with(root: impl Into<PathBuf>, opts: StoreOptions) -> Result<Self, SnapshotError> {
        let root = root.into();
        let objects = root.join("objects");
        let tmp = root.join("tmp");
        fs::create_dir_all(&objects).map_err(|e| io_err(&objects, e))?;
        fs::create_dir_all(&tmp).map_err(|e| io_err(&tmp, e))?;
        Ok(Self {
            objects,
            tmp,
            opts,
            seq: AtomicU64::new(0),
        })
    }

    /// Capture the pre-action state of `path` (file, directory tree, symlink,
    /// or an absent-marker when the path does not exist).
    pub fn snapshot(
        &self,
        path: &Path,
        limits: Option<&Limits>,
    ) -> Result<Manifest, SnapshotError> {
        let mut manifest = Manifest {
            path: path.to_path_buf(),
            root: Root::Present,
            entries: Vec::new(),
            truncated: false,
            skipped: 0,
        };
        let meta = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                manifest.root = Root::Absent;
                return Ok(manifest);
            }
            Err(e) => return Err(io_err(path, e)),
        };

        if meta.file_type().is_symlink() {
            manifest.entries.push(Entry {
                rel: PathBuf::new(),
                kind: EntryKind::Symlink {
                    target: fs::read_link(path).map_err(|e| io_err(path, e))?,
                },
            });
            return Ok(manifest);
        }
        if meta.is_file() {
            manifest
                .entries
                .push(self.file_entry(path, PathBuf::new(), &meta)?);
            return Ok(manifest);
        }

        // directory tree
        let mut files: u64 = 0;
        let mut bytes: u64 = 0;
        for item in walkdir::WalkDir::new(path).sort_by_file_name() {
            let item = item.map_err(|e| SnapshotError::Io {
                path: path.display().to_string(),
                source: e.into(),
            })?;
            let rel = item
                .path()
                .strip_prefix(path)
                .unwrap_or(item.path())
                .to_path_buf();
            let meta = item.metadata().map_err(|e| SnapshotError::Io {
                path: item.path().display().to_string(),
                source: e.into(),
            })?;
            if meta.file_type().is_symlink() {
                manifest.entries.push(Entry {
                    rel,
                    kind: EntryKind::Symlink {
                        target: fs::read_link(item.path()).map_err(|e| io_err(item.path(), e))?,
                    },
                });
            } else if meta.is_dir() {
                manifest.entries.push(Entry {
                    rel,
                    kind: EntryKind::Dir {
                        mode: meta.permissions().mode() & 0o7777,
                    },
                });
            } else {
                if let Some(l) = limits {
                    if files + 1 > l.max_files || bytes + meta.len() > l.max_bytes {
                        manifest.truncated = true;
                        manifest.skipped += 1;
                        continue;
                    }
                }
                files += 1;
                bytes += meta.len();
                manifest
                    .entries
                    .push(self.file_entry(item.path(), rel, &meta)?);
            }
        }
        Ok(manifest)
    }

    /// Restore the exact captured state, replacing whatever is at the path
    /// now. Fails before touching anything if the store is corrupt. Callers
    /// wanting conflict detection diff current state against the manifest
    /// first (undo engine, step 6).
    pub fn restore(&self, manifest: &Manifest) -> Result<RestoreReport, SnapshotError> {
        let mut report = RestoreReport::default();
        let target_root = &manifest.path;

        if manifest.root == Root::Absent {
            remove_any(target_root)?;
            return Ok(report);
        }

        // fail-closed verification pass, deduped by hash
        let mut verified = std::collections::BTreeSet::new();
        for entry in &manifest.entries {
            if let EntryKind::File { hash, .. } = &entry.kind {
                if verified.contains(hash) {
                    continue;
                }
                let object = self.object_path(hash);
                if !object.exists() {
                    return Err(SnapshotError::MissingObject { hash: hash.clone() });
                }
                if hash_file(&object)? != *hash {
                    return Err(SnapshotError::CorruptObject { hash: hash.clone() });
                }
                verified.insert(hash.clone());
            }
        }

        remove_any(target_root)?;
        if let Some(parent) = target_root.parent() {
            fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
        }

        let mut dir_modes: Vec<(PathBuf, u32)> = Vec::new();
        for entry in &manifest.entries {
            let dest = if entry.rel.as_os_str().is_empty() {
                target_root.clone()
            } else {
                target_root.join(&entry.rel)
            };
            match &entry.kind {
                EntryKind::Dir { mode } => {
                    fs::create_dir_all(&dest).map_err(|e| io_err(&dest, e))?;
                    dir_modes.push((dest, *mode));
                }
                EntryKind::Symlink { target } => {
                    std::os::unix::fs::symlink(target, &dest).map_err(|e| io_err(&dest, e))?;
                }
                EntryKind::File {
                    hash,
                    mode,
                    mtime,
                    xattrs,
                    ..
                } => {
                    let object = self.object_path(hash);
                    self.copy_out(&object, &dest)?;
                    // clones inherit the object's read-only mode: make the
                    // destination writable before touching times/xattrs, then
                    // apply the recorded mode last
                    fs::set_permissions(&dest, fs::Permissions::from_mode(0o600))
                        .map_err(|e| io_err(&dest, e))?;
                    let fh = fs::OpenOptions::new()
                        .write(true)
                        .open(&dest)
                        .map_err(|e| io_err(&dest, e))?;
                    fh.set_times(fs::FileTimes::new().set_modified(*mtime))
                        .map_err(|e| io_err(&dest, e))?;
                    drop(fh);
                    for (name, value) in xattrs {
                        if let Err(e) = xattr::set(&dest, name, value) {
                            report.warnings.push(format!(
                                "xattr {name} not restored on {}: {e}",
                                dest.display()
                            ));
                        }
                    }
                    fs::set_permissions(&dest, fs::Permissions::from_mode(*mode))
                        .map_err(|e| io_err(&dest, e))?;
                    report.files_restored += 1;
                }
            }
        }
        // apply directory modes deepest-first so restrictive modes can't block
        // their own children
        for (dir, mode) in dir_modes.into_iter().rev() {
            fs::set_permissions(&dir, fs::Permissions::from_mode(mode))
                .map_err(|e| io_err(&dir, e))?;
        }
        Ok(report)
    }

    /// Probe whether the store's filesystem supports copy-on-write clones.
    pub fn supports_reflink(&self) -> bool {
        let a = self.tmp_path();
        let b = self.tmp_path();
        let ok = fs::write(&a, b"probe").is_ok() && reflink_copy::reflink(&a, &b).is_ok();
        let _ = fs::remove_file(&a);
        let _ = fs::remove_file(&b);
        ok
    }

    pub fn object_count(&self) -> Result<u64, SnapshotError> {
        Ok(self.object_paths()?.len() as u64)
    }

    pub fn object_paths(&self) -> Result<Vec<PathBuf>, SnapshotError> {
        let mut out = Vec::new();
        for item in walkdir::WalkDir::new(&self.objects) {
            let item = item.map_err(|e| SnapshotError::Io {
                path: self.objects.display().to_string(),
                source: e.into(),
            })?;
            if item.file_type().is_file() {
                out.push(item.path().to_path_buf());
            }
        }
        Ok(out)
    }

    fn file_entry(
        &self,
        src: &Path,
        rel: PathBuf,
        meta: &fs::Metadata,
    ) -> Result<Entry, SnapshotError> {
        let (hash, len) = self.ingest(src)?;
        Ok(Entry {
            rel,
            kind: EntryKind::File {
                hash,
                len,
                mode: meta.permissions().mode() & 0o7777,
                mtime: meta.modified().map_err(|e| io_err(src, e))?,
                xattrs: read_xattrs(src),
            },
        })
    }

    /// Clone-then-hash: the clone freezes the content, so the recorded hash is
    /// correct even if the source mutates mid-snapshot.
    fn ingest(&self, src: &Path) -> Result<(String, u64), SnapshotError> {
        let tmp = self.tmp_path();
        if self.opts.force_copy {
            fs::copy(src, &tmp).map_err(|e| io_err(src, e))?;
        } else {
            reflink_copy::reflink_or_copy(src, &tmp).map_err(|e| io_err(src, e))?;
        }
        let hash = hash_file(&tmp)?;
        let len = fs::metadata(&tmp).map_err(|e| io_err(&tmp, e))?.len();
        let object = self.object_path(&hash);
        if object.exists() {
            let _ = fs::remove_file(&tmp);
        } else {
            let parent = object.parent().expect("object path has parent");
            fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
            fs::rename(&tmp, &object).map_err(|e| io_err(&object, e))?;
            // objects are immutable content: drop write bits
            let _ = fs::set_permissions(&object, fs::Permissions::from_mode(0o444));
        }
        Ok((hash, len))
    }

    fn copy_out(&self, object: &Path, dest: &Path) -> Result<(), SnapshotError> {
        if self.opts.force_copy {
            fs::copy(object, dest).map_err(|e| io_err(dest, e))?;
        } else {
            reflink_copy::reflink_or_copy(object, dest).map_err(|e| io_err(dest, e))?;
        }
        Ok(())
    }

    fn object_path(&self, hash: &str) -> PathBuf {
        let prefix = hash.get(0..2).unwrap_or("xx");
        self.objects.join(prefix).join(hash)
    }

    fn tmp_path(&self) -> PathBuf {
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        self.tmp.join(format!("{}-{n}-{nanos}", std::process::id()))
    }
}

fn read_xattrs(path: &Path) -> Vec<(String, Vec<u8>)> {
    let Ok(names) = xattr::list(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for name in names {
        if let Ok(Some(value)) = xattr::get(path, &name) {
            out.push((name.to_string_lossy().into_owned(), value));
        }
    }
    out
}

fn remove_any(path: &Path) -> Result<(), SnapshotError> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err(path, e)),
        Ok(meta) => {
            let result = if meta.is_dir() && !meta.file_type().is_symlink() {
                fs::remove_dir_all(path)
            } else {
                fs::remove_file(path)
            };
            result.map_err(|e| io_err(path, e))
        }
    }
}

const MMAP_THRESHOLD: u64 = 1024 * 1024;

fn hash_file(path: &Path) -> Result<String, SnapshotError> {
    let len = fs::metadata(path).map_err(|e| io_err(path, e))?.len();
    let mut hasher = blake3::Hasher::new();
    if len >= MMAP_THRESHOLD {
        hasher
            .update_mmap_rayon(path)
            .map_err(|e| io_err(path, e))?;
    } else {
        hasher.update(&fs::read(path).map_err(|e| io_err(path, e))?);
    }
    Ok(hasher.finalize().to_hex().to_string())
}
