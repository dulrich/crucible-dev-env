//! `crucible overlay-host --socket <path>`: a pre-warmed overlay terminal's program. The
//! compositor starts the overlay terminal ahead of time running this; it binds `<path>`, waits for
//! one connection carrying one mode line, removes the socket, and then runs the requested overlay
//! (`pick`, `launch` or `keys`) in-process with the same arguments the cold path passes. It draws
//! nothing before the mode arrives.
//!
//! Mode line (one line, tab-separated, no escaping): `<request-epoch-ms>\t<mode>[\t<arg>…]\n`.
//! The compositor never sends a tab or newline inside a field (the picker's rename and root values
//! are already field-encoded). An empty `--root` value is a trailing empty field
//! (`…\tpick\t--root\t\n`); a missing value (`…\tpick\t--root`) is rejected.

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;

use crucible_persist::PickTarget;

use crate::{Parsed, help_requested};

/// Usage text for `crucible overlay-host --help`.
pub const USAGE: &str = "usage: crucible overlay-host --socket <path>\n\nA pre-started overlay terminal's program (spawned by CDE; not for interactive use). Binds\n<path>, waits for one mode line `<epoch-ms>\\t<mode>[\\t<arg>...]` (mode: pick, launch, keys),\nremoves the socket, then runs that overlay in-process.\n";

/// Which overlay a mode line asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayMode {
    /// `pick`.
    Pick,
    /// `launch`.
    Launch,
    /// `keys`.
    Keys,
}

impl OverlayMode {
    /// The wire spelling (the compositor writes it; the host only parses it).
    #[cfg(test)]
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            OverlayMode::Pick => "pick",
            OverlayMode::Launch => "launch",
            OverlayMode::Keys => "keys",
        }
    }

    /// Parses the exact wire spelling; anything else is `None`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<OverlayMode> {
        match name {
            "pick" => Some(OverlayMode::Pick),
            "launch" => Some(OverlayMode::Launch),
            "keys" => Some(OverlayMode::Keys),
            _ => None,
        }
    }
}

/// A parsed mode line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModeLine {
    /// The request's wall-clock time, ms since the UNIX epoch.
    pub epoch_ms: u128,
    /// The requested overlay.
    pub mode: OverlayMode,
    /// The subcommand arguments (`--rename <encoded>` or `--root <encoded-or-empty>` for pick).
    pub args: Vec<String>,
}

/// Formats a mode line (without the trailing newline); `None` if a field holds a tab or newline.
#[cfg(test)]
pub fn format_mode_line(line: &ModeLine) -> Option<String> {
    let mut out = format!("{}\t{}", line.epoch_ms, line.mode.as_str());
    for a in &line.args {
        if a.contains(['\t', '\n', '\r']) {
            return None;
        }
        out.push('\t');
        out.push_str(a);
    }
    Some(out)
}

/// Parses one mode line (a trailing `\n` is allowed). Rejects a non-integer epoch, an unknown
/// mode, and arguments on `launch`/`keys`; `pick` accepts nothing, `--rename <value>` or `--root <value>` (the value may be empty).
pub fn parse_mode_line(raw: &str) -> Result<ModeLine, String> {
    let raw = raw.strip_suffix('\n').unwrap_or(raw);
    let mut fields = raw.split('\t');
    let epoch = fields.next().unwrap_or_default();
    let epoch_ms: u128 = epoch
        .parse()
        .map_err(|_| format!("bad request epoch {epoch:?}"))?;
    let name = fields.next().ok_or("missing mode")?;
    let mode = OverlayMode::from_name(name).ok_or_else(|| format!("unknown mode {name:?}"))?;
    let args: Vec<String> = fields.map(str::to_string).collect();
    // Pick arguments go through the shared `pick` grammar (shape only: the value stays encoded and
    // is decoded when the picker runs).
    let ok = match mode {
        OverlayMode::Launch | OverlayMode::Keys => args.is_empty(),
        OverlayMode::Pick => PickTarget::parse_encoded(&args).is_ok(),
    };
    if !ok {
        return Err(format!("bad arguments for {name}"));
    }
    Ok(ModeLine {
        epoch_ms,
        mode,
        args,
    })
}

fn parse_args(args: &[String]) -> Result<Parsed<PathBuf>, String> {
    if help_requested(args, &[], false) {
        return Ok(Parsed::Help);
    }
    match args {
        [flag, path] if flag == "--socket" && !path.is_empty() => {
            Ok(Parsed::Run(PathBuf::from(path)))
        }
        _ => Err("expected --socket <path>".into()),
    }
}

/// Runs the subcommand; returns the exit code.
pub fn run(args: &[String]) -> i32 {
    let path = match parse_args(args) {
        Ok(Parsed::Run(p)) => p,
        Ok(Parsed::Help) => {
            print!("{USAGE}");
            return 0;
        }
        Err(e) => {
            eprintln!("crucible overlay-host: {e}\n\n{USAGE}");
            return 2;
        }
    };
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("crucible overlay-host: cannot bind {}: {e}", path.display());
            return 1;
        }
    };
    let accepted = listener.accept();
    let _ = std::fs::remove_file(&path);
    drop(listener);
    let mut line = String::new();
    match accepted {
        Ok((stream, _)) => {
            if let Err(e) = BufReader::new(stream).read_line(&mut line) {
                eprintln!("crucible overlay-host: read failed: {e}");
                return 1;
            }
        }
        Err(e) => {
            eprintln!("crucible overlay-host: accept failed: {e}");
            return 1;
        }
    }
    let mode = match parse_mode_line(&line) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("crucible overlay-host: {e}");
            return 2;
        }
    };
    crate::latency::set_request_epoch(mode.epoch_ms);
    match mode.mode {
        OverlayMode::Pick => crate::pick::run(&mode.args),
        OverlayMode::Launch => crate::launch::run(&mode.args),
        OverlayMode::Keys => crate::keys::run(&mode.args),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(epoch: u128, mode: &str, args: &[&str]) -> ModeLine {
        ModeLine {
            epoch_ms: epoch,
            mode: OverlayMode::from_name(mode).unwrap(),
            args: args.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn mode_names_round_trip_and_unknown_names_are_rejected() {
        for m in [OverlayMode::Pick, OverlayMode::Launch, OverlayMode::Keys] {
            assert_eq!(OverlayMode::from_name(m.as_str()), Some(m));
        }
        for bad in ["", "Pick", "bogus", "pick ", "settings"] {
            assert_eq!(OverlayMode::from_name(bad), None, "{bad:?}");
        }
        assert_eq!(
            parse_mode_line("12\tbogus"),
            Err("unknown mode \"bogus\"".to_string())
        );
    }

    #[test]
    fn mode_line_round_trips() {
        for l in [
            line(1_700_000_000_123, "pick", &[]),
            line(5, "pick", &["--rename", "my ws%3A1"]),
            line(6, "pick", &["--root", "/home/u"]),
            line(6, "pick", &["--root", ""]),
            line(7, "launch", &[]),
            line(9, "keys", &[]),
        ] {
            let text = format_mode_line(&l).unwrap();
            assert_eq!(parse_mode_line(&format!("{text}\n")), Ok(l.clone()));
            assert_eq!(parse_mode_line(&text), Ok(l));
        }
        assert_eq!(
            format_mode_line(&line(1, "pick", &["--rename", "a\tb"])),
            None
        );
    }

    #[test]
    fn malformed_mode_lines_are_rejected() {
        for bad in [
            "",
            "\n",
            "abc\tpick",
            "-1\tpick",
            "12",
            "12\tbogus",
            "12\tlaunch\tx",
            "12\tkeys\t--rename\tx",
            "12\tpick\t--rename",
            "12\tpick\t--root",
            "12\tkeys\t--root\tx",
            "12\tpick\t--other\tx",
        ] {
            assert!(parse_mode_line(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn socket_flag_is_required() {
        let a = |l: &[&str]| l.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(
            parse_args(&a(&["--socket", "/x.sock"])),
            Ok(Parsed::Run(PathBuf::from("/x.sock")))
        );
        assert!(parse_args(&a(&[])).is_err());
        assert!(parse_args(&a(&["--socket"])).is_err());
        assert!(parse_args(&a(&["/x.sock"])).is_err());
    }

    #[test]
    fn help_is_recognised_anywhere() {
        let a = |l: &[&str]| l.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(parse_args(&a(&["--help"])), Ok(Parsed::Help));
        assert_eq!(parse_args(&a(&["--socket", "/x", "-h"])), Ok(Parsed::Help));
        assert!(parse_args(&a(&["--socket", "/x"])).is_ok_and(|p| p != Parsed::Help));
    }
}
