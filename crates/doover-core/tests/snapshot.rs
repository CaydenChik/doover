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
        max_duration: None,
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

// --- hardlink preservation (audit round 8) ---------------------------------------

#[test]
fn restore_of_hardlinked_file_preserves_the_inode() {
    use std::os::unix::fs::MetadataExt;
    let j = jail();
    let f = j.world.join("original.txt");
    write(&f, "shared content");
    let alias = j.world.join("alias.txt");
    fs::hard_link(&f, &alias).unwrap();
    let inode_before = fs::metadata(&f).unwrap().ino();

    let m = snap(&j.store, &f, None);
    // truncate through the shared inode (as `> alias` would)
    fs::write(&alias, "clobbered via sibling").unwrap();
    assert_eq!(fs::read_to_string(&f).unwrap(), "clobbered via sibling");

    j.store.restore(&m).unwrap();

    // both names recovered AND still the same inode (link intact)
    assert_eq!(fs::read_to_string(&f).unwrap(), "shared content");
    assert_eq!(fs::read_to_string(&alias).unwrap(), "shared content");
    assert_eq!(
        fs::metadata(&f).unwrap().ino(),
        inode_before,
        "inode must be preserved"
    );
    assert_eq!(
        fs::metadata(&f).unwrap().nlink(),
        2,
        "hardlink must survive restore"
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
        max_duration: None,
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

// --- audit round 15: restore must not write outside the target tree ----------

/// A manifest whose `rel` escapes the root (`..`) must be REFUSED, not
/// materialized outside the staging dir. `rel` comes from a stored manifest
/// (journal JSON on disk); a corrupted or tampered one could otherwise turn
/// `undo` — a write primitive — into an arbitrary-path write. The hash side is
/// already fail-closed (a traversing hash fails content-verify); `rel` needs
/// the same discipline. Defense-in-depth / corruption robustness.
#[test]
fn restore_refuses_a_manifest_whose_rel_escapes_the_root() {
    let j = jail();
    let d = j.world.join("proj");
    write(&d.join("x.txt"), "legit");
    let mut m = snap(&j.store, &d, None);

    // repoint the child entry outside the root, keeping its (valid) object
    let child = m
        .entries
        .iter_mut()
        .find(|e| !e.rel.as_os_str().is_empty())
        .expect("a child entry");
    child.rel = PathBuf::from("../escape.txt");

    let escapee = j.world.join("escape.txt");
    let result = j.store.restore(&m);

    assert!(
        result.is_err(),
        "restore of a traversing rel must be refused"
    );
    assert!(
        !escapee.exists(),
        "restore wrote OUTSIDE the target tree: {}",
        escapee.display()
    );
}

/// The absolute-path variant: an entry `rel` that is itself absolute would,
/// under a naive `base.join(abs)`, discard `base` entirely and write at the
/// absolute location. Must also be refused.
#[test]
fn restore_refuses_a_manifest_whose_rel_is_absolute() {
    let j = jail();
    let d = j.world.join("proj");
    write(&d.join("x.txt"), "legit");
    let mut m = snap(&j.store, &d, None);

    let victim = j.world.join("victim.txt");
    let child = m
        .entries
        .iter_mut()
        .find(|e| !e.rel.as_os_str().is_empty())
        .unwrap();
    child.rel = victim.clone(); // absolute

    let result = j.store.restore(&m);
    assert!(result.is_err(), "absolute rel must be refused");
    assert!(
        !victim.exists(),
        "restore wrote to an absolute path outside the tree"
    );
}

// --- time budget (bench D1) --------------------------------------------------

/// A snapshot must stop at its wall-clock budget and mark the manifest
/// `truncated` (a loud, partial coverage gap) rather than run unbounded into
/// the harness hook timeout. Determinism: 2,000 files cannot be captured in
/// ~0 ms on any machine, so a spent (ZERO) budget truncates for certain — no
/// wall-clock race.
#[test]
fn snapshot_stops_at_a_time_budget_and_marks_partial() {
    let j = jail();
    let d = j.world.join("big");
    for i in 0..2000 {
        write(&d.join(format!("f{i:04}.txt")), "x");
    }

    let spent = Limits {
        max_files: u64::MAX,
        max_bytes: u64::MAX,
        max_duration: Some(std::time::Duration::ZERO),
    };
    let m = j.store.snapshot(&d, Some(&spent)).unwrap();
    assert!(
        m.truncated,
        "a spent time budget must truncate the snapshot"
    );
    assert!(
        m.entries.len() < 2001,
        "capture must be partial, got {}",
        m.entries.len()
    );
    assert!(
        m.warnings
            .iter()
            .any(|w| w.to_lowercase().contains("budget")),
        "a time-budget cutoff must be a loud warning: {:?}",
        m.warnings
    );

    // a generous budget captures the whole tree, no truncation
    let roomy = Limits {
        max_files: u64::MAX,
        max_bytes: u64::MAX,
        max_duration: Some(std::time::Duration::from_secs(120)),
    };
    let full = j.store.snapshot(&d, Some(&roomy)).unwrap();
    assert!(
        !full.truncated,
        "a 120s budget must not truncate 2000 tiny files"
    );
    assert_eq!(full.entries.len(), 2001, "root dir + 2000 files");
}

// --- D2: disk exhaustion hygiene ----------------------------------------------

/// A failed ingest (ENOSPC, unwritable objects dir) must not leak its partial
/// tmp file for an hour until clean_tmp reaps it — repeated failures would
/// stack partial copies on an already-strained disk.
#[test]
fn failed_ingest_leaves_no_tmp_droppings() {
    let j = jail();
    let f = j.world.join("f.txt");
    write(&f, "content");
    let store_root = j.world.parent().unwrap().join("store");
    let objects = store_root.join("objects");
    fs::set_permissions(&objects, fs::Permissions::from_mode(0o555)).unwrap();
    let result = j.store.snapshot(&f, None);
    fs::set_permissions(&objects, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        result.is_err(),
        "unwritable objects dir must fail the snapshot"
    );
    let tmp = store_root.join("tmp");
    assert_eq!(
        fs::read_dir(&tmp).unwrap().count(),
        0,
        "failed ingest must clean its tmp file immediately"
    );
}

#[test]
fn free_bytes_reports_a_sane_value() {
    let j = jail();
    let free = doover_core::snapshot::free_bytes(&j.world);
    assert!(
        free.is_some_and(|b| b > 0),
        "free_bytes on a live fs: {free:?}"
    );
}

/// D4: store objects are COPIES OF USER FILES — never world-readable.
#[test]
fn store_objects_are_owner_only() {
    let j = jail();
    let f = j.world.join("secret.txt");
    write(&f, "api_key=hunter2");
    j.store.snapshot(&f, None).unwrap();
    let obj = j.store.object_paths().unwrap().into_iter().next().unwrap();
    let mode = fs::metadata(&obj).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o400,
        "objects must be 0400 (owner read-only), got {mode:o}"
    );
}

// --- regenerable build dirs are skipped, never the root ------------------------

/// Real-world dogfooding: a Rust repo is 99.5% `target/`. A defensive snapshot
/// that walks it burns the entire time budget on regenerable artifacts and
/// captures almost none of the user's actual source. Skip the known build dirs.
#[test]
fn snapshot_skips_regenerable_build_dirs_but_records_them() {
    let j = jail();
    let proj = j.world.join("proj");
    write(&proj.join("src/main.rs"), "fn main() {}");
    write(&proj.join("README.md"), "docs");
    for junk in ["target", "node_modules", "__pycache__", ".venv"] {
        for i in 0..5 {
            write(&proj.join(junk).join(format!("j{i}.bin")), "junk");
        }
    }
    // no git repo here -> name-only fallback
    let skips = doover_core::snapshot::SkipPolicy::new(
        ["target", "node_modules", "__pycache__", ".venv"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        None,
    );
    let m = j.store.snapshot_scoped(&proj, None, &[], &skips).unwrap();

    let rels: Vec<String> = m
        .entries
        .iter()
        .map(|e| e.rel.to_string_lossy().into_owned())
        .collect();
    assert!(
        rels.iter().any(|r| r.contains("main.rs")),
        "source captured"
    );
    assert!(rels.iter().any(|r| r.contains("README")), "docs captured");
    for junk in ["target", "node_modules", "__pycache__", ".venv"] {
        assert!(
            !rels.iter().any(|r| r.starts_with(junk)),
            "{junk} must be skipped, got {rels:?}"
        );
    }
    // honest: the manifest records exactly what it chose not to capture
    assert_eq!(
        m.skipped_dirs.len(),
        4,
        "skipped dirs recorded: {:?}",
        m.skipped_dirs
    );
    // and it is NOT a protection gap (no alarm warnings for a deliberate policy)
    assert!(
        m.warnings.is_empty(),
        "skipping is policy, not a gap: {:?}",
        m.warnings
    );
}

/// The root exception: `rm -rf target` explicitly names it, so it must be
/// captured in full. Skipping applies to build dirs found INSIDE a tree, never
/// to the tree the user actually pointed at.
#[test]
fn an_explicitly_targeted_build_dir_is_still_fully_captured() {
    let j = jail();
    let target = j.world.join("proj/target");
    for i in 0..5 {
        write(&target.join(format!("artifact{i}.o")), "build output");
    }
    let skips = doover_core::snapshot::SkipPolicy::new(vec!["target".to_string()], None);
    let m = j.store.snapshot_scoped(&target, None, &[], &skips).unwrap();
    assert_eq!(m.entries.len(), 6, "root + 5 files fully captured");
    assert!(m.skipped_dirs.is_empty());
}

/// The skip-list has teeth on BOTH sides of the round trip. A snapshot that
/// skipped `target/` must still (a) compare equal to the unchanged live tree
/// (or undo would refuse on every real project), and (b) restore WITHOUT
/// deleting target/ — we never captured it, so we leave it exactly as it is.
#[test]
fn skipped_dirs_survive_the_restore_and_do_not_trip_the_conflict_oracle() {
    let j = jail();
    let proj = j.world.join("proj");
    write(&proj.join("src/main.rs"), "v1");
    write(&proj.join("target/app.o"), "build output");
    let skips = doover_core::snapshot::SkipPolicy::new(vec!["target".to_string()], None);
    let m = j.store.snapshot_scoped(&proj, None, &[], &skips).unwrap();
    assert_eq!(m.skipped_dirs.len(), 1);

    // (a) an unchanged world must NOT read as changed just because target/
    // exists on disk and not in the manifest
    assert!(
        j.store.state_matches(&m).unwrap(),
        "a snapshot with skipped dirs must still match the unchanged world"
    );

    // the command mangles the source AND writes new build output
    write(&proj.join("src/main.rs"), "MANGLED");
    write(&proj.join("target/app.o"), "newer build output");
    write(&proj.join("target/extra.o"), "another artifact");

    // (b) restore brings back the source and LEAVES target/ alone
    j.store.restore(&m).unwrap();
    assert_eq!(
        fs::read_to_string(proj.join("src/main.rs")).unwrap(),
        "v1",
        "source restored"
    );
    assert_eq!(
        fs::read_to_string(proj.join("target/app.o")).unwrap(),
        "newer build output",
        "skipped dir passed through untouched, not reverted"
    );
    assert!(
        proj.join("target/extra.o").exists(),
        "skipped dir must NOT be deleted by the swap"
    );
}

// --- the gitignore gate: name alone is a guess, git's ignore is a declaration --

fn git_repo(root: &Path, gitignore: &str) {
    fs::create_dir_all(root.join(".git")).unwrap(); // enough for repo-root detection
    write(&root.join(".gitignore"), gitignore);
}

/// THE data-loss path the gate closes: a directory whose NAME looks like build
/// output but which git tracks (so it is real, irreplaceable source) must be
/// captured. Skipping it would mean undo could not bring it back.
#[test]
fn a_build_named_dir_that_git_tracks_is_still_captured() {
    let j = jail();
    let proj = j.world.join("proj");
    git_repo(&proj, "target/\nnode_modules/\n"); // build/ NOT ignored
    write(&proj.join("src/main.rs"), "source");
    write(
        &proj.join("build/generate.sh"),
        "REAL SOURCE that lives in build/",
    );
    write(&proj.join("target/app.o"), "artifact");
    write(&proj.join("node_modules/dep.js"), "dep");
    write(&proj.join(".env"), "API_KEY=secret"); // gitignored by nothing here

    let policy = doover_core::snapshot::SkipPolicy::new(
        ["target", "node_modules", "build", "dist"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        Some(&proj),
    );
    let m = j.store.snapshot_scoped(&proj, None, &[], &policy).unwrap();
    let rels: Vec<String> = m
        .entries
        .iter()
        .map(|e| e.rel.to_string_lossy().into_owned())
        .collect();
    let has = |p: &str| rels.iter().any(|r| r == p);

    assert!(has("src/main.rs"), "source captured");
    assert!(
        has("build/generate.sh"),
        "a build-NAMED dir that git TRACKS is real source — must be captured, got {rels:?}"
    );
    assert!(!has("target/app.o"), "gitignored + build name -> skipped");
    assert!(
        !has("node_modules/dep.js"),
        "gitignored + build name -> skipped"
    );
    assert_eq!(m.skipped_dirs.len(), 2, "only target/ and node_modules/");
}

/// The flagship case must survive the gate: `.env` is gitignored but is NOT a
/// build dir, so it is exactly what doover exists to protect.
#[test]
fn a_gitignored_secret_is_still_captured() {
    let j = jail();
    let proj = j.world.join("proj");
    git_repo(&proj, ".env\ntarget/\n");
    write(&proj.join(".env"), "API_KEY=secret");
    write(&proj.join("target/app.o"), "artifact");

    let policy = doover_core::snapshot::SkipPolicy::new(vec!["target".to_string()], Some(&proj));
    let m = j.store.snapshot_scoped(&proj, None, &[], &policy).unwrap();
    let rels: Vec<String> = m
        .entries
        .iter()
        .map(|e| e.rel.to_string_lossy().into_owned())
        .collect();
    assert!(
        rels.iter().any(|r| r == ".env"),
        "gitignored but not a build dir: capture it — this is the whole pitch"
    );
    assert!(!rels.iter().any(|r| r.starts_with("target")));
}

/// Outside a git repo there is no disposability signal, so the name list is all
/// we have: fall back to name-only (documented) rather than reintroduce the
/// 5-second stall for every non-git project.
#[test]
fn outside_a_git_repo_the_name_list_still_applies() {
    let j = jail();
    let proj = j.world.join("plain"); // no .git anywhere
    write(&proj.join("index.js"), "source");
    write(&proj.join("node_modules/dep.js"), "dep");

    let policy = doover_core::snapshot::SkipPolicy::new(vec!["node_modules".to_string()], None);
    let m = j.store.snapshot_scoped(&proj, None, &[], &policy).unwrap();
    let rels: Vec<String> = m
        .entries
        .iter()
        .map(|e| e.rel.to_string_lossy().into_owned())
        .collect();
    assert!(rels.iter().any(|r| r == "index.js"));
    assert!(!rels.iter().any(|r| r.starts_with("node_modules")));
}
