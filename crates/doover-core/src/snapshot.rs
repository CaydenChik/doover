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
//! sound. Hardlink identity WITHIN a snapshotted directory tree is not
//! preserved (linked files restore as independent copies with identical
//! content — harmless, since both names are captured). But a single-file
//! restore whose current target still has nlink>1 rewrites content in place to
//! preserve the shared inode, so a truncating write through one name
//! (`> alias`) recovers every hardlinked sibling (audit round 8).

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
    /// Named pipe. Recreated empty on restore (its transient contents are not
    /// data we can or should capture); never opened during snapshot, which is
    /// what used to hang the engine.
    Fifo {
        mode: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    /// Path relative to the snapshot root; empty for the root itself.
    pub rel: PathBuf,
    pub kind: EntryKind,
}

/// Version stamped into every newly-written [`Manifest`]. Bump when the
/// serialized shape changes incompatibly; readers refuse anything newer than
/// they understand. Legacy JSON without the field deserializes as 0.
pub const MANIFEST_SCHEMA: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Manifest {
    /// Serialization schema version (see [`MANIFEST_SCHEMA`]).
    #[serde(default)]
    pub schema: u32,
    /// Absolute path this snapshot captured.
    pub path: PathBuf,
    pub root: Root,
    /// Walk order: parents before children.
    pub entries: Vec<Entry>,
    /// True when limits stopped the capture before completion.
    pub truncated: bool,
    /// Files seen but not captured because of limits.
    pub skipped: u64,
    /// Loud record of anything the snapshot could not fully capture:
    /// unreadable subtrees, skipped special files (sockets/devices), fifos.
    /// A non-empty list means coverage has gaps the caller must surface.
    pub warnings: Vec<String>,
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
}

/// Process-global uniquifier for tmp/staging names: two `Store` handles on the
/// same root (concurrent hook invocations in one process) must never collide.
fn next_unique() -> u64 {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
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
        Ok(Self { objects, tmp, opts })
    }

    /// Capture the pre-action state of `path` (file, directory tree, symlink,
    /// or an absent-marker when the path does not exist).
    pub fn snapshot(
        &self,
        path: &Path,
        limits: Option<&Limits>,
    ) -> Result<Manifest, SnapshotError> {
        let mut manifest = Manifest {
            schema: MANIFEST_SCHEMA,
            path: path.to_path_buf(),
            root: Root::Present,
            entries: Vec::new(),
            truncated: false,
            skipped: 0,
            warnings: Vec::new(),
        };
        let meta = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                manifest.root = Root::Absent;
                return Ok(manifest);
            }
            Err(e) => return Err(io_err(path, e)),
        };

        // single non-directory root
        if !meta.is_dir() || meta.file_type().is_symlink() {
            match self.classify(path, PathBuf::new(), &meta)? {
                Captured::Entry(entry) => manifest.entries.push(entry),
                Captured::Skipped(reason) => {
                    // the root itself is uncapturable (e.g. a device node):
                    // record an absent-style marker is wrong, so warn and leave
                    // entries empty — restore of an empty Present manifest is a
                    // no-op, and the loud warning routes the caller to the
                    // unknown policy
                    manifest
                        .warnings
                        .push(format!("{}: {reason}", path.display()));
                }
            }
            return Ok(manifest);
        }

        // directory tree — tolerate per-entry errors (unreadable subdirs) by
        // recording a warning and continuing, never aborting the whole snapshot
        let mut files: u64 = 0;
        let mut bytes: u64 = 0;
        let walker = walkdir::WalkDir::new(path).sort_by_file_name();
        for item in walker {
            let item = match item {
                Ok(i) => i,
                Err(e) => {
                    let at = e
                        .path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| path.display().to_string());
                    manifest.warnings.push(format!("{at}: unreadable ({e})"));
                    continue;
                }
            };
            let rel = item
                .path()
                .strip_prefix(path)
                .unwrap_or(item.path())
                .to_path_buf();
            let meta = match item.metadata() {
                Ok(m) => m,
                Err(e) => {
                    manifest
                        .warnings
                        .push(format!("{}: unreadable ({e})", item.path().display()));
                    continue;
                }
            };
            if meta.is_dir() && !meta.file_type().is_symlink() {
                manifest.entries.push(Entry {
                    rel,
                    kind: EntryKind::Dir {
                        mode: meta.permissions().mode() & 0o7777,
                    },
                });
                continue;
            }
            if meta.is_file() && !meta.file_type().is_symlink() {
                if let Some(l) = limits {
                    if files + 1 > l.max_files || bytes + meta.len() > l.max_bytes {
                        manifest.truncated = true;
                        manifest.skipped += 1;
                        continue;
                    }
                }
                files += 1;
                bytes += meta.len();
            }
            match self.classify(item.path(), rel, &meta)? {
                Captured::Entry(entry) => manifest.entries.push(entry),
                Captured::Skipped(reason) => manifest
                    .warnings
                    .push(format!("{}: {reason}", item.path().display())),
            }
        }
        Ok(manifest)
    }

    /// Turn one filesystem object into a manifest entry, or decide it can't be
    /// captured. Never opens fifos/sockets/devices for I/O.
    fn classify(
        &self,
        abs: &Path,
        rel: PathBuf,
        meta: &fs::Metadata,
    ) -> Result<Captured, SnapshotError> {
        use std::os::unix::fs::FileTypeExt;
        let ft = meta.file_type();
        let mode = meta.permissions().mode() & 0o7777;
        if ft.is_symlink() {
            return Ok(Captured::Entry(Entry {
                rel,
                kind: EntryKind::Symlink {
                    target: fs::read_link(abs).map_err(|e| io_err(abs, e))?,
                },
            }));
        }
        if ft.is_dir() {
            return Ok(Captured::Entry(Entry {
                rel,
                kind: EntryKind::Dir { mode },
            }));
        }
        if ft.is_file() {
            return Ok(Captured::Entry(self.file_entry(abs, rel, meta)?));
        }
        if ft.is_fifo() {
            // recreated empty; never opened (opening a fifo for read blocks)
            return Ok(Captured::Entry(Entry {
                rel,
                kind: EntryKind::Fifo { mode },
            }));
        }
        // sockets, block/char devices: not restorable data
        let what = if ft.is_socket() {
            "socket"
        } else if ft.is_block_device() {
            "block device"
        } else if ft.is_char_device() {
            "char device"
        } else {
            "special file"
        };
        Ok(Captured::Skipped(format!(
            "skipped {what} (cannot snapshot)"
        )))
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

        // fail-closed verification pass, deduped by hash — before any mutation
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

        // A Present manifest with zero entries means the root existed but was
        // uncapturable (socket/device — see the snapshot warnings). There is
        // nothing to rebuild, and destroying the live object would turn a
        // coverage gap into data loss: warn and leave the target alone.
        if manifest.entries.is_empty() {
            report.warnings.push(format!(
                "{}: nothing was capturable at snapshot time; restore is a no-op",
                target_root.display()
            ));
            return Ok(report);
        }

        // Hardlink preservation: if the manifest is a single regular file and
        // the current target is still a regular file with more than one link,
        // stage-then-swap would rebuild a FRESH inode and orphan the sibling
        // names (which a truncating write like `> alias` clobbered through the
        // shared inode). Rewrite the content IN PLACE so every hardlinked name
        // recovers. Restore of a recovery is acceptable to do non-atomically —
        // the pre-restore state is already the clobbered content.
        if let [entry] = manifest.entries.as_slice() {
            if entry.rel.as_os_str().is_empty() {
                if let EntryKind::File {
                    hash, mode, mtime, ..
                } = &entry.kind
                {
                    if fs::symlink_metadata(target_root)
                        .ok()
                        .filter(|m| m.file_type().is_file())
                        .map(|m| std::os::unix::fs::MetadataExt::nlink(&m))
                        .is_some_and(|n| n > 1)
                    {
                        let object = self.object_path(hash);
                        self.write_in_place(target_root, &object, *mode, *mtime)?;
                        report.files_restored += 1;
                        return Ok(report);
                    }
                }
            }
        }

        let parent = target_root
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| SnapshotError::Io {
                path: target_root.display().to_string(),
                source: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "cannot restore a filesystem root",
                ),
            })?;
        fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;

        // Build the whole restored tree into a sibling staging path first, so
        // every fallible operation (copy, chmod, set_times, xattr, mkfifo)
        // happens while the current target is still fully intact. Only once
        // staging succeeds do we swap — remove target, rename staging in —
        // which is metadata-only and on the same filesystem. A crash mid-build
        // leaves the target untouched; a failure is cleaned up.
        let staging = self.staging_path(parent);
        let build = self.build_into(&staging, manifest, &mut report);
        if let Err(e) = build {
            let _ = remove_any(&staging);
            return Err(e);
        }

        if let Err(e) = remove_any(target_root) {
            let _ = remove_any(&staging);
            return Err(e);
        }
        if let Err(e) = fs::rename(&staging, target_root) {
            let _ = remove_any(&staging);
            return Err(io_err(target_root, e));
        }
        Ok(report)
    }

    /// Materialize `manifest` at `base` (a fresh staging path). Directory modes
    /// are applied deepest-first so a restrictive mode can't block writing its
    /// own children.
    fn build_into(
        &self,
        base: &Path,
        manifest: &Manifest,
        report: &mut RestoreReport,
    ) -> Result<(), SnapshotError> {
        let mut dir_modes: Vec<(PathBuf, u32)> = Vec::new();
        for entry in &manifest.entries {
            let dest = if entry.rel.as_os_str().is_empty() {
                base.to_path_buf()
            } else {
                base.join(&entry.rel)
            };
            match &entry.kind {
                EntryKind::Dir { mode } => {
                    fs::create_dir_all(&dest).map_err(|e| io_err(&dest, e))?;
                    dir_modes.push((dest, *mode));
                }
                EntryKind::Symlink { target } => {
                    if let Some(p) = dest.parent() {
                        fs::create_dir_all(p).map_err(|e| io_err(p, e))?;
                    }
                    std::os::unix::fs::symlink(target, &dest).map_err(|e| io_err(&dest, e))?;
                }
                EntryKind::Fifo { mode } => {
                    if let Some(p) = dest.parent() {
                        fs::create_dir_all(p).map_err(|e| io_err(p, e))?;
                    }
                    make_fifo(&dest, *mode)?;
                    // mkfifo's mode argument is masked by the umask; apply the
                    // recorded mode explicitly
                    fs::set_permissions(&dest, fs::Permissions::from_mode(*mode))
                        .map_err(|e| io_err(&dest, e))?;
                }
                EntryKind::File {
                    hash,
                    mode,
                    mtime,
                    xattrs,
                    ..
                } => {
                    if let Some(p) = dest.parent() {
                        fs::create_dir_all(p).map_err(|e| io_err(p, e))?;
                    }
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
        for (dir, mode) in dir_modes.into_iter().rev() {
            fs::set_permissions(&dir, fs::Permissions::from_mode(mode))
                .map_err(|e| io_err(&dir, e))?;
        }
        Ok(())
    }

    /// Truncate-and-rewrite an existing file's content without replacing its
    /// inode, so hardlinked siblings recover too. Only used on the nlink>1
    /// path; the object bytes are read from the (already hash-verified) store.
    fn write_in_place(
        &self,
        dest: &Path,
        object: &Path,
        mode: u32,
        mtime: SystemTime,
    ) -> Result<(), SnapshotError> {
        // ensure we can open for writing regardless of the clobbered mode
        fs::set_permissions(dest, fs::Permissions::from_mode(0o600))
            .map_err(|e| io_err(dest, e))?;
        let bytes = fs::read(object).map_err(|e| io_err(object, e))?;
        let fh = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(dest)
            .map_err(|e| io_err(dest, e))?;
        {
            use std::io::Write;
            let mut w = &fh;
            w.write_all(&bytes).map_err(|e| io_err(dest, e))?;
        }
        fh.set_times(fs::FileTimes::new().set_modified(mtime))
            .map_err(|e| io_err(dest, e))?;
        drop(fh);
        fs::set_permissions(dest, fs::Permissions::from_mode(mode)).map_err(|e| io_err(dest, e))?;
        Ok(())
    }

    fn staging_path(&self, parent: &Path) -> PathBuf {
        parent.join(format!(
            ".doover-restore-{}-{}",
            std::process::id(),
            next_unique()
        ))
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
        self.tmp
            .join(format!("{}-{}", std::process::id(), next_unique()))
    }
}

enum Captured {
    Entry(Entry),
    Skipped(String),
}

/// Crash leftovers from an interrupted restore swap. `doover doctor` surfaces
/// these; their contents are a fully-built restore image and may aid recovery.
pub fn orphaned_staging(dir: &Path) -> Result<Vec<PathBuf>, SnapshotError> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| io_err(dir, e))? {
        let entry = entry.map_err(|e| io_err(dir, e))?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(".doover-restore-")
        {
            out.push(entry.path());
        }
    }
    out.sort();
    Ok(out)
}

fn make_fifo(path: &Path, mode: u32) -> Result<(), SnapshotError> {
    use std::os::unix::ffi::OsStrExt;
    let cpath = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        io_err(
            path,
            io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"),
        )
    })?;
    // SAFETY: cpath is a valid NUL-terminated C string for the duration of the call.
    let rc = unsafe { libc::mkfifo(cpath.as_ptr(), mode as libc::mode_t) };
    if rc != 0 {
        return Err(io_err(path, io::Error::last_os_error()));
    }
    Ok(())
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
