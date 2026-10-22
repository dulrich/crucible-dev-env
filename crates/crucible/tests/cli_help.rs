//! A bare `--help` (alone or beside a backend flag) prints usage and exits 0 without starting a
//! compositor backend — in particular `--tty-udev --help` must not take the DRM/KMS device.
use std::process::Command;

#[test]
fn help_prints_usage_and_starts_no_backend() {
    let dir = std::env::temp_dir().join(format!("cde-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for args in [
        &["--help"][..],
        &["-h"][..],
        &["--winit", "--help"][..],
        &["--tty-udev", "--help"][..],
        &["--help", "--tty-udev"][..],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_crucible"))
            .args(args)
            .current_dir(&dir)
            .output()
            .expect("spawn crucible");
        assert_eq!(out.status.code(), Some(0), "{args:?}: {out:?}");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("usage: crucible"), "{args:?}: {text}");
    }
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        0,
        "help created files"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
