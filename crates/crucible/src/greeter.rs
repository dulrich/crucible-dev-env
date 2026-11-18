//! `crucible greeter` — the compositor in kiosk mode under greetd: a native login panel, the
//! profile background through swaybg, and the greetd login flow. Logs go to stderr (which greetd
//! does not keep) and to a file in [`LOG_DIR`].

use std::fs::File;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tracing_subscriber::fmt::writer::{BoxMakeWriter, MakeWriterExt};

use crucible_compositor::greeter::{GreeterSetup, profile};
use crucible_greetd::fake;

use crate::cli::{Parsed, help_requested};

/// Usage text for `crucible greeter --help`.
pub const USAGE: &str = "usage: crucible greeter [--profile <dir>] [--fake-greetd <file>] [--winit]\n\nRuns CDE as a greetd greeter: a login panel in the profile's style over the profile\nbackground (drawn by /usr/bin/swaybg). Only VT switching and set-layout chords work.\nAfter greetd starts the session the greeter exits 0. Never reads $HOME.\n\n  --profile <dir>      read <dir>/profile.kdl (missing or invalid: the n1-med look on black)\n  --fake-greetd <file> serve the JSON greetd replies in <file> (one per line) in-process\n                       instead of connecting to $GREETD_SOCK (for nested checks)\n  --winit              run nested (development) instead of on the DRM/KMS backend\n  -h, --help           print this usage and exit 0\n\nExit 1 when greetd is unreachable or the exchange fails; exit 2 on a bad argument.\n";

/// The parsed arguments.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Args {
    /// `--profile <dir>`.
    pub profile: Option<PathBuf>,
    /// `--fake-greetd <file>`.
    pub fake: Option<PathBuf>,
    /// `--winit`.
    pub winit: bool,
}

/// Parses the arguments.
///
/// # Errors
/// A message for an unknown argument or a flag without its value.
pub fn parse_args(args: &[String]) -> Result<Parsed<Args>, String> {
    if help_requested(args, &["--profile", "--fake-greetd"], false) {
        return Ok(Parsed::Help);
    }
    let mut out = Args::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--profile" | "--fake-greetd" => {
                let v = it.next().ok_or_else(|| format!("{a} needs a value"))?;
                let slot = if a == "--profile" {
                    &mut out.profile
                } else {
                    &mut out.fake
                };
                *slot = Some(PathBuf::from(v));
            }
            "--winit" => out.winit = true,
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    Ok(Parsed::Run(out))
}

/// Where the greeter writes its log files. greetd does not keep the greeter's stderr, so this is
/// the only record. Readable by everyone: the greeter never logs usernames, passwords or keys.
pub const LOG_DIR: &str = "/tmp/cde-greeter";

/// How many log files [`open_log_file`] keeps, the new one included.
const LOG_KEEP: usize = 5;

/// Opens a new log file `greeter-<secs>-<pid>.log` in `dir`, creating `dir` (0755) when missing
/// and pruning the oldest files down to [`LOG_KEEP`]. `None` (log to stderr only) when `dir` is a
/// symlink, not a directory, not owned by this user, or any step fails: a shared `/tmp` path is
/// never trusted beyond that.
fn open_log_file(dir: &Path, secs: u64, pid: u32) -> Option<File> {
    match std::fs::DirBuilder::new().mode(0o755).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return None,
    }
    let meta = std::fs::symlink_metadata(dir).ok()?;
    if !meta.is_dir() || meta.uid() != rustix::process::geteuid().as_raw() {
        return None;
    }
    let mut old: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("greeter-") && n.ends_with(".log"))
        })
        .collect();
    old.sort();
    let excess = (old.len() + 1).saturating_sub(LOG_KEEP);
    for path in old.iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(dir.join(format!("greeter-{secs:012}-{pid}.log")))
        .ok()
}

/// Sets up logging: stderr, plus a file in [`LOG_DIR`] when one can be opened.
fn init_logging() {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let writer = match open_log_file(Path::new(LOG_DIR), secs, std::process::id()) {
        Some(file) => BoxMakeWriter::new(std::io::stderr.and(Mutex::new(file))),
        None => BoxMakeWriter::new(std::io::stderr),
    };
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
}

/// Connects to greetd, or starts the in-process fake from the reply file.
fn connect(fake_file: Option<&PathBuf>) -> Result<UnixStream, String> {
    match fake_file {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            let replies =
                fake::parse_replies(&text).map_err(|e| format!("{}: {e}", path.display()))?;
            let (client, _server) = fake::connect(replies).map_err(|e| e.to_string())?;
            Ok(client)
        }
        None => {
            let sock = std::env::var_os("GREETD_SOCK")
                .filter(|s| !s.is_empty())
                .ok_or("GREETD_SOCK is not set (run under greetd, or pass --fake-greetd)")?;
            UnixStream::connect(&sock)
                .map_err(|e| format!("cannot connect to {}: {e}", PathBuf::from(sock).display()))
        }
    }
}

/// Entry point.
pub fn run(args: &[String]) -> i32 {
    let args = match parse_args(args) {
        Ok(Parsed::Help) => {
            print!("{USAGE}");
            return 0;
        }
        Ok(Parsed::Run(a)) => a,
        Err(msg) => {
            eprintln!("crucible greeter: {msg}\n{USAGE}");
            return 2;
        }
    };
    init_logging();
    let stream = match connect(args.fake.as_ref()) {
        Ok(s) => s,
        Err(msg) => {
            eprintln!("crucible greeter: {msg}");
            return 1;
        }
    };
    let setup = GreeterSetup {
        profile: profile::load_profile(args.profile.as_deref()),
        stream,
    };
    if args.winit {
        crucible_compositor::winit::run_winit_mode(Some(setup))
    } else {
        crucible_compositor::udev::run_udev_mode(Some(setup))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, empty scratch path under the system temp dir (not created).
    fn scratch(tag: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("crucible-greeter-log-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn log_file_is_created_world_readable_in_a_new_dir() {
        let dir = scratch("new");
        assert!(open_log_file(&dir, 7, 42).is_some());
        let file = dir.join("greeter-000000000007-42.log");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().mode() & 0o777;
        assert_eq!((mode(&dir), mode(&file)), (0o755, 0o644));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn log_files_are_pruned_oldest_first() {
        let dir = scratch("prune");
        for secs in 1..=7 {
            assert!(open_log_file(&dir, secs, 1).is_some());
        }
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names.len(), LOG_KEEP);
        assert_eq!(names[0], "greeter-000000000003-1.log");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_symlinked_log_dir_is_refused() {
        let target = scratch("target");
        std::fs::create_dir(&target).unwrap();
        let link = scratch("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(open_log_file(&link, 1, 1).is_none());
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
        std::fs::remove_file(&link).unwrap();
        std::fs::remove_dir_all(&target).unwrap();
    }

    fn s(list: &[&str]) -> Vec<String> {
        list.iter().map(|x| (*x).to_string()).collect()
    }

    #[test]
    fn parses_flags_and_help() {
        assert_eq!(
            parse_args(&s(&["--profile", "/p", "--winit", "--fake-greetd", "/f"])),
            Ok(Parsed::Run(Args {
                profile: Some("/p".into()),
                fake: Some("/f".into()),
                winit: true,
            }))
        );
        assert_eq!(parse_args(&s(&["--help"])), Ok(Parsed::Help));
        assert!(parse_args(&s(&["--profile"])).is_err());
        assert!(parse_args(&s(&["--tty-udev"])).is_err());
    }
}
