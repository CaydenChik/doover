//! T3 — snapshot engine round-trip matrix (doover-implementation-plan.md §3).
//! Written before the snapshot module exists; drives its design.
//!
//! Every test operates inside its own tempdir jail: a store root and a
//! separate "world" directory whose contents get snapshotted, mangled, and
//! restored.

use doover_core::snapshot::{EntryKind, Limits, Root, Store, StoreOptions};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

struct Jail {
    _tmp: tempfile::TempDir,
    store: Store,
    world: PathBuf,
}

fn jail() -> Jail {
    jail_with(StoreOptions { force_copy: false })
}

fn jail_with(opts: StoreOptions) -> Jail {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open_with(tmp.path().join("store"), opts).unwrap();
    let world = tmp.path().join("world");
    fs::create_dir_all(&world).unwrap();
    Jail {
        _tmp: tmp,
        store,
        world,
    }
}

fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// Strict-by-default snapshot: most scenarios must produce ZERO warnings — a
/// spurious (or vanished) warning is a failure. Tests that expect gaps call
/// `snapshot()` directly and assert their warnings explicitly.
fn snap(store: &Store, path: &Path, limits: Option<&Limits>) -> doover_core::snapshot::Manifest {
    let m = store.snapshot(path, limits).unwrap();
    assert!(
        m.warnings.is_empty(),
        "expected a warning-free snapshot of {}, got: {:?}",
        path.display(),
        m.warnings
    );
    m
}

/// Full recursive fingerprint: path → (type marker, content, mode bits).
fn fingerprint(root: &Path) -> BTreeMap<PathBuf, (String, Vec<u8>, u32)> {
    let mut map = BTreeMap::new();
    let meta = fs::symlink_metadata(root);
    if meta.is_err() {
        return map; // absent
    }
    for entry in walkdir::WalkDir::new(root).sort_by_file_name() {
        let entry = entry.unwrap();
        let rel = entry.path().strip_prefix(root).unwrap().to_path_buf();
        let meta = fs::symlink_metadata(entry.path()).unwrap();
        let mode = meta.permissions().mode() & 0o7777;
        if meta.file_type().is_symlink() {
            let target = fs::read_link(entry.path()).unwrap();
            map.insert(
                rel,
                (
                    "symlink".into(),
                    target.into_os_string().into_encoded_bytes().to_vec(),
                    0,
                ),
            );
        } else if std::os::unix::fs::FileTypeExt::is_fifo(&meta.file_type()) {
            map.insert(rel, ("fifo".into(), Vec::new(), mode));
        } else if meta.is_dir() {
            map.insert(rel, ("dir".into(), Vec::new(), mode));
        } else {
            map.insert(rel, ("file".into(), fs::read(entry.path()).unwrap(), mode));
        }
    }
    map
}

// --- basic round trips --------------------------------------------------------

#[test]
fn file_round_trip_content_mode_mtime() {
    let j = jail();
    let f = j.world.join("cfg.ini");
    write(&f, "alpha=1\n");
    fs::set_permissions(&f, fs::Permissions::from_mode(0o640)).unwrap();
    let want_mtime = fs::metadata(&f).unwrap().modified().unwrap();

    let m = snap(&j.store, &f, None);
    assert_eq!(m.root, Root::Present);

    fs::write(&f, "clobbered").unwrap();
    fs::set_permissions(&f, fs::Permissions::from_mode(0o600)).unwrap();
    j.store.restore(&m).unwrap();

    assert_eq!(fs::read_to_string(&f).unwrap(), "alpha=1\n");
    assert_eq!(
        fs::metadata(&f).unwrap().permissions().mode() & 0o7777,
        0o640
    );
    assert_eq!(fs::metadata(&f).unwrap().modified().unwrap(), want_mtime);
}

#[test]
fn empty_file_round_trip() {
    let j = jail();
    let f = j.world.join("empty");
    write(&f, "");
    let m = snap(&j.store, &f, None);
    fs::write(&f, "no longer empty").unwrap();
    j.store.restore(&m).unwrap();
    assert_eq!(fs::read(&f).unwrap(), Vec::<u8>::new());
}

#[test]
fn absent_marker_round_trip_deletes_created_file() {
    let j = jail();
    let f = j.world.join("did-not-exist.txt");
    let m = snap(&j.store, &f, None);
    assert_eq!(m.root, Root::Absent);

    write(&f, "the action created me");
    j.store.restore(&m).unwrap();
    assert!(
        !f.exists(),
        "restore of an absent marker must delete the path"
    );
}

#[test]
fn absent_marker_round_trip_deletes_created_dir() {
    let j = jail();
    let d = j.world.join("newdir");
    let m = snap(&j.store, &d, None);
    write(&d.join("sub/file.txt"), "x");
    j.store.restore(&m).unwrap();
    assert!(!d.exists());
}

#[test]
fn dir_tree_round_trip_exact() {
    let j = jail();
    let d = j.world.join("project");
    write(&d.join("a.txt"), "A");
    write(&d.join("sub dir/файл.txt"), "unicode + spaces");
    write(&d.join("sub dir/deep/📸.dat"), "emoji");
    fs::create_dir_all(d.join("empty-dir")).unwrap();
    fs::set_permissions(d.join("a.txt"), fs::Permissions::from_mode(0o600)).unwrap();
    let before = fingerprint(&d);
    let want_mtime = fs::metadata(d.join("sub dir/файл.txt"))
        .unwrap()
        .modified()
        .unwrap();

    let m = snap(&j.store, &d, None);

    // mangle: modify, delete, add
    fs::write(d.join("a.txt"), "MANGLED").unwrap();
    fs::remove_dir_all(d.join("sub dir/deep")).unwrap();
    write(&d.join("intruder.txt"), "added after snapshot");
    fs::remove_dir(d.join("empty-dir")).unwrap();

    j.store.restore(&m).unwrap();
    assert_eq!(
        fingerprint(&d),
        before,
        "restore must reproduce the exact tree"
    );
    assert_eq!(
        fs::metadata(d.join("sub dir/файл.txt"))
            .unwrap()
            .modified()
            .unwrap(),
        want_mtime,
        "mtimes must survive tree round-trips too"
    );
}

#[test]
fn symlink_round_trip_not_followed() {
    let j = jail();
    let d = j.world.join("tree");
    write(&d.join("real.txt"), "target content");
    std::os::unix::fs::symlink("real.txt", d.join("link")).unwrap();
    std::os::unix::fs::symlink("/nowhere/dangling", d.join("dangler")).unwrap();
    let before = fingerprint(&d);

    let m = snap(&j.store, &d, None);
    // the symlink itself is an entry; its target file appears once, not twice
    let link_entries: Vec<_> = m
        .entries
        .iter()
        .filter(|e| matches!(e.kind, EntryKind::Symlink { .. }))
        .collect();
    assert_eq!(link_entries.len(), 2, "both symlinks captured as symlinks");

    fs::remove_file(d.join("link")).unwrap();
    write(&d.join("link"), "replaced by regular file");
    fs::remove_file(d.join("dangler")).unwrap();
    j.store.restore(&m).unwrap();
    assert_eq!(fingerprint(&d), before);
}

#[test]
fn restore_recreates_missing_parent_dirs() {
    let j = jail();
    let f = j.world.join("deep/nested/dir/file.txt");
    write(&f, "content");
    let m = snap(&j.store, &f, None);
    fs::remove_dir_all(j.world.join("deep")).unwrap();
    j.store.restore(&m).unwrap();
    assert_eq!(fs::read_to_string(&f).unwrap(), "content");
}

// --- store properties ---------------------------------------------------------

#[test]
fn dedup_same_content_is_one_object() {
    let j = jail();
    write(&j.world.join("one.txt"), "identical bytes");
    write(&j.world.join("two.txt"), "identical bytes");
    j.store.snapshot(&j.world.join("one.txt"), None).unwrap();
    j.store.snapshot(&j.world.join("two.txt"), None).unwrap();
    j.store.snapshot(&j.world.join("one.txt"), None).unwrap(); // repeat
    assert_eq!(j.store.object_count().unwrap(), 1);
}

#[test]
fn hardlinked_files_both_captured() {
    let j = jail();
    let d = j.world.join("tree");
    write(&d.join("original.txt"), "shared inode");
    fs::hard_link(d.join("original.txt"), d.join("alias.txt")).unwrap();

    let m = snap(&j.store, &d, None);
    fs::remove_dir_all(&d).unwrap();
    j.store.restore(&m).unwrap();

    // restored as two independent files with equal content (link-ness is
    // documented as not preserved)
    assert_eq!(
        fs::read_to_string(d.join("original.txt")).unwrap(),
        "shared inode"
    );
    assert_eq!(
        fs::read_to_string(d.join("alias.txt")).unwrap(),
        "shared inode"
    );
    assert_eq!(j.store.object_count().unwrap(), 1, "same content deduped");
}

#[test]
fn forced_plain_copy_backend_round_trips() {
    let j = jail_with(StoreOptions { force_copy: true });
    let d = j.world.join("tree");
    write(&d.join("x.txt"), "copied not cloned");
    let before = fingerprint(&d);
    let m = snap(&j.store, &d, None);
    fs::remove_dir_all(&d).unwrap();
    j.store.restore(&m).unwrap();
    assert_eq!(fingerprint(&d), before);
}

// --- corruption ---------------------------------------------------------------

#[test]
fn corrupted_object_refused_and_destination_untouched() {
    let j = jail();
    let f = j.world.join("precious.txt");
    write(&f, "original precious content");
    let m = snap(&j.store, &f, None);

    // flip a byte in the stored object
    let object = j.store.object_paths().unwrap().pop().expect("one object");
    let mut bytes = fs::read(&object).unwrap();
    fs::set_permissions(&object, fs::Permissions::from_mode(0o644)).unwrap();
    bytes[0] ^= 0xFF;
    fs::write(&object, bytes).unwrap();

    fs::write(&f, "current state").unwrap();
    let err = j.store.restore(&m).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("corrupt") || msg.contains("mismatch"),
        "error must say what went wrong, got: {msg}"
    );
    // fail-closed: verification happens before any mutation
    assert_eq!(fs::read_to_string(&f).unwrap(), "current state");
}

// --- limits ---------------------------------------------------------------------

#[test]
fn limits_truncate_and_report() {
    let j = jail();
    let d = j.world.join("big");
    for i in 0..20 {
        write(&d.join(format!("f{i:02}.txt")), &format!("content {i}"));
    }
    let limits = Limits {
        max_files: 5,
        max_bytes: u64::MAX,
    };
    let m = snap(&j.store, &d, Some(&limits));
    assert!(m.truncated, "exceeding max_files must set truncated");
    assert!(m.skipped > 0, "skipped count must be reported");
    let captured = m
        .entries
        .iter()
        .filter(|e| matches!(e.kind, EntryKind::File { .. }))
        .count();
    assert!(captured <= 5, "captured {captured} files, cap was 5");
}

// --- special files (audit 2026-07-06: FIFO ingestion used to hang forever) -----

#[test]
fn fifo_in_tree_round_trips_without_hanging() {
    let j = jail_with(StoreOptions { force_copy: true }); // copy path = the hang path
    let d = j.world.join("tree");
    write(&d.join("normal.txt"), "fine");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(d.join("pipe.fifo"))
            .status()
            .unwrap()
            .success()
    );
    let before = fingerprint(&d);

    // must return promptly instead of blocking on opening the fifo
    let (tx, rx) = std::sync::mpsc::channel();
    let store = j.store;
    let d2 = d.clone();
    std::thread::spawn(move || {
        let m = store.snapshot(&d2, None);
        let _ = tx.send((store, m));
    });
    let (store, m) = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("snapshot must not hang on a fifo");
    let m = m.unwrap();

    fs::remove_dir_all(&d).unwrap();
    store.restore(&m).unwrap();
    assert_eq!(fingerprint(&d), before, "fifo must be recreated");
}

#[test]
fn socket_is_skipped_with_warning_not_a_hang() {
    let j = jail();
    let d = j.world.join("tree");
    write(&d.join("normal.txt"), "fine");
    let _listener = std::os::unix::net::UnixListener::bind(d.join("live.sock")).unwrap();

    let m = j.store.snapshot(&d, None).unwrap();
    assert!(
        m.warnings.iter().any(|w| w.contains("live.sock")),
        "skipping a socket must be reported: {:?}",
        m.warnings
    );

    fs::remove_dir_all(&d).unwrap();
    let report = j.store.restore(&m).unwrap();
    assert_eq!(fs::read_to_string(d.join("normal.txt")).unwrap(), "fine");
    assert!(!d.join("live.sock").exists(), "sockets cannot be recreated");
    drop(report);
}

#[test]
fn unreadable_subdir_warns_instead_of_failing_snapshot() {
    let j = jail();
    let d = j.world.join("tree");
    write(&d.join("ok.txt"), "readable");
    write(&d.join("locked/secret.txt"), "unreachable");
    fs::set_permissions(d.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();

    let result = j.store.snapshot(&d, None);
    // teardown safety before asserting
    fs::set_permissions(d.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();

    let m = result.expect("one unreadable subdir must not kill the snapshot");
    assert!(
        m.warnings.iter().any(|w| w.contains("locked")),
        "the gap must be reported loudly: {:?}",
        m.warnings
    );
    assert!(
        m.entries
            .iter()
            .any(|e| e.rel == std::path::Path::new("ok.txt")),
        "readable content still captured"
    );
}

// --- limits (audit round 6: max_bytes was never exercised) -----------------------

#[test]
fn max_bytes_limit_truncates() {
    let j = jail();
    let d = j.world.join("big");
    for i in 0..10 {
        write(&d.join(format!("f{i}.bin")), &"x".repeat(1000));
    }
    let limits = Limits {
        max_files: u64::MAX,
        max_bytes: 2500,
    };
    let m = j.store.snapshot(&d, Some(&limits)).unwrap();
    assert!(m.truncated, "byte budget exceeded must set truncated");
    assert!(m.skipped > 0);
    let bytes: u64 = m
        .entries
        .iter()
        .filter_map(|e| match &e.kind {
            EntryKind::File { len, .. } => Some(*len),
            _ => None,
        })
        .sum();
    assert!(bytes <= 2500, "captured {bytes} bytes over a 2500 budget");
}

// --- round-2 audit regressions ---------------------------------------------------

#[test]
fn empty_present_manifest_restore_is_a_noop_not_destruction() {
    // a socket at the snapshot ROOT: Present, zero entries, warned. Restoring
    // it must leave the live target alone (audit round 2: it was deleted, then
    // the swap errored — data loss).
    let j = jail();
    let sock = j.world.join("live.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();

    let m = j.store.snapshot(&sock, None).unwrap();
    assert_eq!(m.root, Root::Present);
    assert!(m.entries.is_empty());
    assert!(!m.warnings.is_empty(), "uncapturable root must warn");

    let report = j.store.restore(&m).expect("no-op restore must succeed");
    assert!(
        sock.exists(),
        "restore must not destroy the un-restorable target"
    );
    assert!(
        report.warnings.iter().any(|w| w.contains("no-op")),
        "the no-op must be reported: {:?}",
        report.warnings
    );
}

#[test]
fn fifo_mode_survives_round_trip() {
    let j = jail();
    let d = j.world.join("tree");
    write(&d.join("f.txt"), "x");
    let fifo = d.join("pipe");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    // 0o622 is altered by any usual umask, so mkfifo-without-chmod fails this
    fs::set_permissions(&fifo, fs::Permissions::from_mode(0o622)).unwrap();
    let before = fingerprint(&d);

    let m = snap(&j.store, &d, None);
    fs::remove_dir_all(&d).unwrap();
    j.store.restore(&m).unwrap();
    assert_eq!(
        fingerprint(&d),
        before,
        "fifo mode must survive (umask must not mask it)"
    );
}

#[test]
fn failed_restore_leaves_target_intact_and_no_staging() {
    // force the staging build to fail (read-only parent) and prove the
    // current state survives untouched — the point of stage-then-swap
    let j = jail();
    let d = j.world.join("sub");
    write(&d.join("f.txt"), "original");
    let m = snap(&j.store, &d.join("f.txt"), None);
    fs::write(d.join("f.txt"), "current-state").unwrap();
    fs::set_permissions(&d, fs::Permissions::from_mode(0o555)).unwrap();

    let result = j.store.restore(&m);
    fs::set_permissions(&d, fs::Permissions::from_mode(0o755)).unwrap(); // teardown safety

    assert!(result.is_err(), "read-only parent must fail the restore");
    assert_eq!(
        fs::read_to_string(d.join("f.txt")).unwrap(),
        "current-state",
        "failed restore must leave the target exactly as it was"
    );
    let staging: Vec<_> = fs::read_dir(&d)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".doover-"))
        .collect();
    assert!(
        staging.is_empty(),
        "staging droppings after failure: {staging:?}"
    );
}

#[test]
fn orphaned_staging_is_detectable() {
    let j = jail();
    let orphan = j.world.join(".doover-restore-999-0");
    fs::create_dir_all(&orphan).unwrap();
    write(&j.world.join("normal.txt"), "x");
    let found = doover_core::snapshot::orphaned_staging(&j.world).unwrap();
    assert_eq!(found, vec![orphan], "doctor needs to find crash leftovers");
}

#[test]
fn concurrent_ingestion_same_content_is_race_free() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("store");
    let world = tmp.path().join("world");
    fs::create_dir_all(&world).unwrap();
    for i in 0..8 {
        write(
            &world.join(format!("f{i}.txt")),
            "identical content across all files",
        );
    }
    let world2 = world.clone();
    let root2 = root.clone();

    let t1 = std::thread::spawn(move || {
        let store = Store::open_with(root2, StoreOptions::default()).unwrap();
        for _ in 0..25 {
            store.snapshot(&world2, None).unwrap();
        }
    });
    let store = Store::open_with(&root, StoreOptions::default()).unwrap();
    for _ in 0..25 {
        store.snapshot(&world, None).unwrap();
    }
    t1.join().expect("concurrent ingester must not panic");

    assert_eq!(
        store.object_count().unwrap(),
        1,
        "same content = one object, despite racing"
    );
    let leftovers: Vec<_> = fs::read_dir(root.join("tmp"))
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert!(leftovers.is_empty(), "tmp droppings after racing ingesters");
}

// --- restore staging hygiene ----------------------------------------------------

#[test]
fn restore_leaves_no_staging_droppings() {
    let j = jail();
    let d = j.world.join("proj");
    write(&d.join("f.txt"), "content");
    let m = snap(&j.store, &d, None);
    fs::write(d.join("f.txt"), "changed").unwrap();
    j.store.restore(&m).unwrap();

    let leftovers: Vec<_> = fs::read_dir(&j.world)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".doover-"))
        .collect();
    assert!(leftovers.is_empty(), "staging droppings: {leftovers:?}");
}

// --- extended attributes (skipped on filesystems without support) --------------

#[test]
fn xattr_round_trip_when_supported() {
    let j = jail();
    let f = j.world.join("tagged.txt");
    write(&f, "content");
    if xattr::set(&f, "user.doover.test", b"marker").is_err() {
        eprintln!("skipping: filesystem does not support user xattrs");
        return;
    }
    let m = snap(&j.store, &f, None);
    fs::remove_file(&f).unwrap();
    write(&f, "replaced");
    j.store.restore(&m).unwrap();
    assert_eq!(fs::read_to_string(&f).unwrap(), "content");
    let value = xattr::get(&f, "user.doover.test").unwrap();
    assert_eq!(value.as_deref(), Some(b"marker".as_slice()));
}

// --- scale (reflink-gated for the sparse giant) ---------------------------------

#[test]
fn sparse_large_file_round_trip() {
    let j = jail();
    if !j.store.supports_reflink() {
        eprintln!("skipping: store filesystem has no copy-on-write support");
        return;
    }
    let f = j.world.join("disk.img");
    let file = fs::File::create(&f).unwrap();
    file.set_len(5 * 1024 * 1024 * 1024).unwrap(); // 5 GB sparse
    drop(file);
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut fh = fs::OpenOptions::new().write(true).open(&f).unwrap();
        fh.write_all(b"HEAD").unwrap();
        fh.seek(SeekFrom::End(-4)).unwrap();
        fh.write_all(b"TAIL").unwrap();
    }
    let m = snap(&j.store, &f, None);
    fs::write(&f, "tiny now").unwrap();
    j.store.restore(&m).unwrap();

    let meta = fs::metadata(&f).unwrap();
    assert_eq!(meta.len(), 5 * 1024 * 1024 * 1024);
    let mut head = vec![0u8; 4];
    let mut tail = vec![0u8; 4];
    use std::io::{Read, Seek, SeekFrom};
    let mut fh = fs::File::open(&f).unwrap();
    fh.read_exact(&mut head).unwrap();
    fh.seek(SeekFrom::End(-4)).unwrap();
    fh.read_exact(&mut tail).unwrap();
    assert_eq!(&head, b"HEAD");
    assert_eq!(&tail, b"TAIL");
}

#[test]
fn large_tree_round_trip() {
    let j = jail();
    let n: usize = std::env::var("DOOVER_T3_TREE_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);
    let d = j.world.join("forest");
    for i in 0..n {
        // two levels of fan-out, mixed content so dedup doesn't trivialize it
        write(
            &d.join(format!("dir{:02}/f{i:05}.txt", i % 100)),
            &format!("file number {i}"),
        );
    }
    let before = fingerprint(&d);
    let m = snap(&j.store, &d, None);
    fs::remove_dir_all(&d).unwrap();
    j.store.restore(&m).unwrap();
    assert_eq!(fingerprint(&d), before);
}
