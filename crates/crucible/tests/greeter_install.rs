//! Runs the real `scripts/greeter-install.sh` with `sh` against a scratch tree through
//! `--dest-root`, with stub binaries that answer `--build`.
//!
//! The script installs from its own directory and its invocation guard accepts only a path ending
//! `scripts/greeter-install.sh`, so each test copies the script to `<tmp>/scripts/` with
//! `lib/build-id.sh` beside it and lays the payload out next to the script. Outside git the script's
//! own build id is `unknown`, which the stubs print (the mismatch case prints something else).
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::UNIX_EPOCH;

const FILES: [&str; 6] = [
    "crucible",
    "cde-login",
    "stock.png",
    "dist/systemd/cde-login.service",
    "dist/pam.d/cde-login",
    "dist/pam.d/cde-login-greeter",
];

struct Tree {
    root: PathBuf,
}

impl Tree {
    /// A scratch tree with the script, its helper and a full payload; `dest/opt/cde` exists.
    fn new(name: &str, id: &str) -> Tree {
        let root = std::env::temp_dir().join(format!("cde-gi-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let scripts = root.join("scripts");
        fs::create_dir_all(scripts.join("lib")).unwrap();
        fs::copy(
            repo.join("scripts/greeter-install.sh"),
            scripts.join("greeter-install.sh"),
        )
        .unwrap();
        fs::copy(
            repo.join("scripts/lib/build-id.sh"),
            scripts.join("lib/build-id.sh"),
        )
        .unwrap();
        for f in FILES {
            let p = scripts.join(f);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            if f == "crucible" || f == "cde-login" {
                let body = format!("#!/bin/sh\n# {f}\necho {id}\n");
                fs::write(&p, body).unwrap();
                fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
            } else {
                fs::write(&p, format!("payload {f}\n")).unwrap();
            }
        }
        fs::create_dir_all(root.join("dest/opt/cde")).unwrap();
        Tree { root }
    }

    fn dest(&self) -> PathBuf {
        self.root.join("dest")
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new("sh")
            .arg(self.root.join("scripts/greeter-install.sh"))
            .arg("--dest-root")
            .arg(self.dest())
            .args(args)
            .output()
            .expect("spawn sh")
    }

    /// Every file under dest with its mtime (ns) and bytes, for before/after comparison.
    fn snapshot(&self) -> Vec<(PathBuf, u128, Vec<u8>)> {
        fn walk(dir: &Path, out: &mut Vec<(PathBuf, u128, Vec<u8>)>) {
            let mut ents: Vec<_> = fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().path())
                .collect();
            ents.sort();
            for p in ents {
                if p.is_dir() {
                    walk(&p, out);
                } else {
                    let m = fs::metadata(&p).unwrap().modified().unwrap();
                    let ns = m.duration_since(UNIX_EPOCH).unwrap().as_nanos();
                    out.push((p.clone(), ns, fs::read(&p).unwrap()));
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.dest(), &mut out);
        out
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn mode(p: &Path) -> u32 {
    fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn first_install_creates_leaf_dirs_and_second_run_changes_nothing() {
    let t = Tree::new("first", "unknown");
    let d = t.dest();
    let out = t.run(&[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(text(&out).contains("systemctl restart cde-login.service"));
    for (rel, m) in [
        ("opt/cde/bin/crucible", 0o755),
        ("opt/cde/bin/cde-login", 0o755),
        ("opt/cde/share/backgrounds/stock.png", 0o644),
        ("etc/systemd/system/cde-login.service", 0o644),
        ("etc/pam.d/cde-login", 0o644),
        ("etc/pam.d/cde-login-greeter", 0o644),
    ] {
        assert!(d.join(rel).is_file(), "{rel} missing");
        assert_eq!(mode(&d.join(rel)), m, "{rel} mode");
    }
    assert!(
        !d.join("opt/cde/bin/crucible.prev").exists(),
        "no prev on a first install"
    );
    let before = t.snapshot();
    let out = t.run(&[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(text(&out).contains("nothing to do"), "{}", text(&out));
    assert!(!text(&out).contains("restart"), "{}", text(&out));
    assert_eq!(t.snapshot(), before, "second run changed files");
    assert!(!d.join("opt/cde/bin/crucible.prev").exists());
    assert!(!d.join("opt/cde/bin/cde-login.prev").exists());
}

#[test]
fn missing_source_or_wrong_build_id_refuses_before_writing() {
    let t = Tree::new("missing", "unknown");
    fs::remove_file(t.root.join("scripts/dist/pam.d/cde-login-greeter")).unwrap();
    let out = t.run(&[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(
        text(&out).contains("greeter-install: missing staged file"),
        "{}",
        text(&out)
    );
    assert_eq!(t.snapshot(), Vec::new(), "wrote before refusing");

    let t = Tree::new("badid", "unknown");
    let p = t.root.join("scripts/cde-login");
    fs::write(&p, "#!/bin/sh\necho not-the-id\n").unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    let out = t.run(&[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(
        text(&out).contains("--build printed 'not-the-id'"),
        "{}",
        text(&out)
    );
    assert_eq!(t.snapshot(), Vec::new(), "wrote before refusing");
    assert!(!t.dest().join("opt/cde/bin").exists());
}

#[test]
fn rollback_restores_the_previous_pair_and_keeps_prev() {
    let t = Tree::new("rollback", "unknown");
    let bin = t.dest().join("opt/cde/bin");
    fs::create_dir_all(&bin).unwrap();
    for n in ["crucible", "cde-login"] {
        fs::write(bin.join(n), "new\n").unwrap();
        fs::write(bin.join(format!("{n}.prev")), "old\n").unwrap();
    }
    let out = t.run(&["--rollback"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    for n in ["crucible", "cde-login"] {
        assert_eq!(fs::read_to_string(bin.join(n)).unwrap(), "old\n");
        assert_eq!(
            fs::read_to_string(bin.join(format!("{n}.prev"))).unwrap(),
            "old\n"
        );
        assert_eq!(mode(&bin.join(n)), 0o755);
    }
    // One .prev missing: refuse before touching anything.
    fs::write(bin.join("crucible"), "newer\n").unwrap();
    fs::remove_file(bin.join("cde-login.prev")).unwrap();
    let out = t.run(&["--rollback"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert_eq!(fs::read_to_string(bin.join("crucible")).unwrap(), "newer\n");
}
