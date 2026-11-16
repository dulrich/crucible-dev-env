//! `crucible greeter` — the compositor in kiosk mode under greetd: a native login panel, the
//! profile background through swaybg, and the greetd login flow. Logs go to stderr only.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use crucible_compositor::greeter::{GreeterSetup, crucible_greetd::fake, profile};

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
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
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
