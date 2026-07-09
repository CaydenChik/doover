//! D3 safety floor (permanent): across a broad corpus of real agent commands,
//! nothing that actually destroys local files or external state may classify
//! as SILENTLY safe — it must be at least `has_unknown` (defensive fallback +
//! journaled gap) or an explicit high severity. This locks the invariant so a
//! future rule can never quietly downgrade a dangerous command to safe.

use doover_core::registry::Registry;
use doover_core::resolver::{Ctx, Severity, resolve};

#[test]
fn no_dangerous_command_is_silently_safe() {
    let reg = Registry::builtin().unwrap();
    let j = tempfile::tempdir().unwrap();
    let home = j.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let ctx = Ctx {
        cwd: j.path(),
        home: &home,
    };

    // commands that modify local files OR external/system state — every one
    // must snapshot, or fall back (has_unknown), or be flagged. None silent.
    let dangerous = [
        "rsync --delete src/ dst/",
        "rsync -a src/ dst/",
        "tar -xf a.tar",
        "tar xzf a.tgz",
        "unzip -o a.zip",
        "patch -p1 < d.diff",
        "perl -i -pe 's/a/b/' f",
        "perl -pi.bak -e 's/a/b/' f",
        "npm ci",
        "make clean",
        "cargo clean",
        "redis-cli FLUSHALL",
        "psql -c 'DROP TABLE t'",
        "mysql -e 'DROP DATABASE d'",
        "dropdb prod",
        "kubectl delete pod web",
        "docker rm -f web",
        "docker rmi img",
        "docker volume rm data",
        "docker system prune -af",
        "apt-get remove nginx",
        "pip uninstall -y requests",
        "systemctl stop nginx",
        "kill -9 1234",
    ];
    for cmd in dangerous {
        let r = resolve(cmd, &reg, &ctx);
        let handled = r.severity >= Severity::Externalizing || r.has_unknown;
        assert!(
            handled,
            "`{cmd}` is SILENTLY safe (sev {:?}, unk {}) — a dangerous command \
             must snapshot, fall back, or be flagged",
            r.severity, r.has_unknown
        );
    }
}
