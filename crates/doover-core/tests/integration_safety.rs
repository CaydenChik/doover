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

/// The load-bearing safety property (audit round 6): a delete THROUGH a
/// directory symlink destroys data outside the lexical scope. The resolver
/// must classify it unknown — and this test proves that either the data comes
/// back OR the resolver admitted uncertainty. It must never be "certain but
/// unrecoverable".
#[test]
fn through_symlink_delete_is_never_certain_and_lossy() {
    let w = world();
    let cwd = w.cwd();
    fs::create_dir_all(cwd.join("real")).unwrap();
    fs::write(cwd.join("real/precious.txt"), "irreplaceable").unwrap();
    std::os::unix::fs::symlink("real", cwd.join("link")).unwrap();

    let certain = w.run_and_undo("rm -rf link/");
    let recovered = read(&cwd.join("real/precious.txt")).as_deref() == Some("irreplaceable");
    assert!(
        recovered || !certain,
        "through-symlink delete was reported as a certain scope but the data is gone"
    );
    // our chosen resolution is to fail safe to unknown (so the future hook
    // engine snapshots the cwd instead); assert that explicitly
    assert!(
        !certain,
        "trailing-slash symlink must be classified unknown"
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
