//! Cross-module ship-safety: resolve → snapshot → run the REAL command →
//! restore, asserting recovery. These are the end-to-end guarantees the whole
//! product exists to provide; a unit test on any single module cannot prove
//! them. (The hook adapter, step 5, will drive this same flow from JSON.)
//!
//! Prime invariant under test: whenever the resolver reports a scope WITHOUT
//! has_unknown, snapshotting that scope must actually make the command
//! recoverable. If it can't, the resolver must have said unknown.

use doover_core::registry::Registry;
use doover_core::resolver::{Ctx, normalize_lexical, resolve};
use doover_core::snapshot::{Manifest, Store, StoreOptions};
use std::fs;
use std::path::Path;

struct World {
    tmp: tempfile::TempDir,
    store: Store,
}

fn world() -> World {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open_with(tmp.path().join(".store"), StoreOptions::default()).unwrap();
    World { tmp, store }
}

impl World {
    fn cwd(&self) -> std::path::PathBuf {
        normalize_lexical(self.tmp.path())
    }

    fn store_object_count(&self) -> u64 {
        self.store.object_count().unwrap()
    }

    /// Resolve, snapshot the scoped paths, run the command for real in cwd,
    /// then restore. Returns whether the resolver claimed certainty.
    fn run_and_undo(&self, command: &str) -> bool {
        let cwd = self.cwd();
        let home = cwd.join(".home");
        fs::create_dir_all(&home).unwrap();
        let registry = Registry::builtin().unwrap();
        let ctx = Ctx {
            cwd: &cwd,
            home: &home,
        };
        let r = resolve(command, &registry, &ctx);

        let manifests: Vec<Manifest> = r
            .paths
            .iter()
            .map(|p| self.store.snapshot(p, None).unwrap())
            .collect();

        let status = std::process::Command::new("bash")
            .args(["--noprofile", "--norc", "-c", command])
            .current_dir(&cwd)
            .status()
            .unwrap();
        assert!(status.success() || status.code().is_some(), "command ran");

        for m in &manifests {
            self.store.restore(m).unwrap();
        }
        !r.has_unknown
    }
}

fn read(p: &Path) -> Option<String> {
    fs::read_to_string(p).ok()
}

#[test]
fn rm_recursive_directory_is_fully_recoverable() {
    let w = world();
    let cwd = w.cwd();
    fs::create_dir_all(cwd.join("build/sub")).unwrap();
    fs::write(cwd.join("build/a.txt"), "A").unwrap();
    fs::write(cwd.join("build/sub/b.txt"), "B").unwrap();

    let certain = w.run_and_undo("rm -rf build");
    assert!(certain, "a plain recursive rm must be a certain scope");
    assert_eq!(read(&cwd.join("build/a.txt")).as_deref(), Some("A"));
    assert_eq!(read(&cwd.join("build/sub/b.txt")).as_deref(), Some("B"));
}

#[test]
fn redirect_truncation_is_recoverable() {
    let w = world();
    let cwd = w.cwd();
    fs::write(cwd.join("notes.txt"), "original\ncontent\n").unwrap();
    let certain = w.run_and_undo("echo clobbered > notes.txt");
    assert!(certain, "echo + redirect is a certain scope");
    assert_eq!(
        read(&cwd.join("notes.txt")).as_deref(),
        Some("original\ncontent\n")
    );
}

#[test]
fn glob_delete_is_recoverable() {
    let w = world();
    let cwd = w.cwd();
    for n in ["a.bak", "b.bak", "keep.txt"] {
        fs::write(cwd.join(n), n).unwrap();
    }
    let certain = w.run_and_undo("rm -- *.bak");
    assert!(certain);
    assert_eq!(read(&cwd.join("a.bak")).as_deref(), Some("a.bak"));
    assert_eq!(read(&cwd.join("b.bak")).as_deref(), Some("b.bak"));
}

/// The load-bearing safety property (audit rounds 6+7): operations THROUGH a
/// symlink affect data outside the lexical scope. Round 7's unified rule
/// scopes the resolved target alongside every symlink, so these are now
/// CERTAIN and fully recoverable — strictly stronger than round 6's
/// unknown-fallback. "Certain but unrecoverable" remains the forbidden state.
#[test]
fn through_symlink_delete_is_certain_and_recoverable() {
    let w = world();
    let cwd = w.cwd();
    fs::create_dir_all(cwd.join("real")).unwrap();
    fs::write(cwd.join("real/precious.txt"), "irreplaceable").unwrap();
    std::os::unix::fs::symlink("real", cwd.join("link")).unwrap();

    let certain = w.run_and_undo("rm -rf link/");
    assert!(certain, "link + target scoping keeps this a certain scope");
    assert_eq!(
        read(&cwd.join("real/precious.txt")).as_deref(),
        Some("irreplaceable"),
        "the through-symlink deletion must be fully undone"
    );
}

/// Round-7 twin: WRITES through a file symlink clobber the target's content.
#[test]
fn write_through_file_symlink_is_certain_and_recoverable() {
    let w = world();
    let cwd = w.cwd();
    fs::write(cwd.join("target.txt"), "precious original").unwrap();
    std::os::unix::fs::symlink("target.txt", cwd.join("linkfile")).unwrap();

    let certain = w.run_and_undo("echo clobbered > linkfile");
    assert!(certain, "redirect to a symlink is a certain scope");
    assert_eq!(
        read(&cwd.join("target.txt")).as_deref(),
        Some("precious original"),
        "the clobbered target content must be restored"
    );
}

/// Round-8 finding 1 (data loss): a truncating write through a HARDLINK hits
/// the shared inode, so every name sees the clobber. Restore must recover the
/// sibling name too, not just the one doover snapshotted.
#[test]
fn write_through_hardlink_recovers_all_names() {
    let w = world();
    let cwd = w.cwd();
    fs::write(cwd.join("original.txt"), "precious shared inode").unwrap();
    fs::hard_link(cwd.join("original.txt"), cwd.join("alias.txt")).unwrap();

    let certain = w.run_and_undo("echo clobbered > alias.txt");
    assert!(certain);
    assert_eq!(
        read(&cwd.join("alias.txt")).as_deref(),
        Some("precious shared inode")
    );
    assert_eq!(
        read(&cwd.join("original.txt")).as_deref(),
        Some("precious shared inode"),
        "the hardlinked sibling shares the inode and must also be restored"
    );
}

/// Round-8 finding 2: a plain `rm link` deletes only the link. The target
/// tree — possibly huge or sensitive — must NOT be copied into the store.
#[test]
fn plain_symlink_delete_does_not_snapshot_the_target() {
    let w = world();
    let cwd = w.cwd();
    fs::create_dir_all(cwd.join("real")).unwrap();
    fs::write(cwd.join("real/secret.key"), "sensitive").unwrap();
    std::os::unix::fs::symlink("real", cwd.join("link")).unwrap();

    let before = w.store_object_count();
    let certain = w.run_and_undo("rm link");
    let after = w.store_object_count();
    assert!(certain);
    // a symlink carries no file content: snapshotting only the link adds ZERO
    // objects. Any growth means the target tree was pulled into the store.
    assert_eq!(
        after,
        before,
        "plain rm of a symlink pulled target content into the store ({} new objects)",
        after - before
    );
    assert_eq!(
        read(&cwd.join("real/secret.key")).as_deref(),
        Some("sensitive"),
        "target untouched"
    );
}

#[test]
fn plain_symlink_delete_restores_the_link() {
    // `rm link` (no slash) removes the symlink itself, not the target — this
    // IS a certain, recoverable scope
    let w = world();
    let cwd = w.cwd();
    fs::create_dir_all(cwd.join("real")).unwrap();
    fs::write(cwd.join("real/keep.txt"), "safe").unwrap();
    std::os::unix::fs::symlink("real", cwd.join("link")).unwrap();

    let certain = w.run_and_undo("rm link");
    assert!(certain);
    assert!(
        cwd.join("link")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        read(&cwd.join("real/keep.txt")).as_deref(),
        Some("safe"),
        "target untouched"
    );
}
