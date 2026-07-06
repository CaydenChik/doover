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

    let m = j.store.snapshot(&f, None).unwrap();
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
    let m = j.store.snapshot(&f, None).unwrap();
    fs::write(&f, "no longer empty").unwrap();
    j.store.restore(&m).unwrap();
    assert_eq!(fs::read(&f).unwrap(), Vec::<u8>::new());
}

#[test]
fn absent_marker_round_trip_deletes_created_file() {
    let j = jail();
    let f = j.world.join("did-not-exist.txt");
    let m = j.store.snapshot(&f, None).unwrap();
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
    let m = j.store.snapshot(&d, None).unwrap();
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

    let m = j.store.snapshot(&d, None).unwrap();

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
}

#[test]
fn symlink_round_trip_not_followed() {
    let j = jail();
    let d = j.world.join("tree");
    write(&d.join("real.txt"), "target content");
    std::os::unix::fs::symlink("real.txt", d.join("link")).unwrap();
    std::os::unix::fs::symlink("/nowhere/dangling", d.join("dangler")).unwrap();
    let before = fingerprint(&d);

    let m = j.store.snapshot(&d, None).unwrap();
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
    let m = j.store.snapshot(&f, None).unwrap();
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

    let m = j.store.snapshot(&d, None).unwrap();
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
    let m = j.store.snapshot(&d, None).unwrap();
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
    let m = j.store.snapshot(&f, None).unwrap();

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
    let m = j.store.snapshot(&d, Some(&limits)).unwrap();
    assert!(m.truncated, "exceeding max_files must set truncated");
    assert!(m.skipped > 0, "skipped count must be reported");
    let captured = m
        .entries
        .iter()
        .filter(|e| matches!(e.kind, EntryKind::File { .. }))
        .count();
    assert!(captured <= 5, "captured {captured} files, cap was 5");
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
    let m = j.store.snapshot(&f, None).unwrap();
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
    let m = j.store.snapshot(&f, None).unwrap();
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
    let m = j.store.snapshot(&d, None).unwrap();
    fs::remove_dir_all(&d).unwrap();
    j.store.restore(&m).unwrap();
    assert_eq!(fingerprint(&d), before);
}
