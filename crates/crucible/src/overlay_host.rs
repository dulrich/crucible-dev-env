//! `crucible overlay-host --socket <path>`: a pre-warmed overlay terminal's program. The
//! compositor starts the overlay terminal ahead of time running this; it binds `<path>`, waits for
//! one connection carrying one mode line, removes the socket, and then runs the requested overlay
//! (`pick`, `launch` or `keys`) in-process with the same arguments the cold path passes. It draws
//! nothing before the mode arrives.
//!
//! Mode line (one line, tab-separated, no escaping): `<request-epoch-ms>\t<mode>[\t<arg>…]\n`.
//! The compositor never sends a tab or newline inside a field (the picker's rename value is
//! already field-encoded).

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;

/// Usage text for `crucible overlay-host --help`.
pub const USAGE: &str = "usage: crucible overlay-host --socket <path>\n\nA pre-started overlay terminal's program (spawned by CDE; not for interactive use). Binds\n<path>, waits for one mode line `<epoch-ms>\\t<mode>[\\t<arg>...]` (mode: pick, launch, keys),\nremoves the socket, then runs that overlay in-process.\n";

/// A parsed mode line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModeLine {
    /// The request's wall-clock time, ms since the UNIX epoch.
    pub epoch_ms: u128,
    /// `pick`, `launch` or `keys`.
    pub mode: String,
    /// The subcommand arguments (`--rename <encoded>` for pick).
    pub args: Vec<String>,
}

/// Formats a mode line (without the trailing newline); `None` if a field holds a tab or newline.
#[cfg(test)]
pub fn format_mode_line(line: &ModeLine) -> Option<String> {
    let mut out = format!("{}\t{}", line.epoch_ms, line.mode);
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
/// mode, and arguments on `launch`/`keys`; `pick` accepts nothing or `--rename <value>`.
pub fn parse_mode_line(raw: &str) -> Result<ModeLine, String> {
    let raw = raw.strip_suffix('\n').unwrap_or(raw);
    let mut fields = raw.split('\t');
    let epoch = fields.next().unwrap_or_default();
    let epoch_ms: u128 = epoch
        .parse()
        .map_err(|_| format!("bad request epoch {epoch:?}"))?;
    let mode = fields.next().ok_or("missing mode")?.to_string();
    let args: Vec<String> = fields.map(str::to_string).collect();
    match (mode.as_str(), args.len()) {
        ("launch" | "keys", 0) => {}
        ("pick", 0) => {}
        ("pick", 2) if args[0] == "--rename" => {}
        ("pick" | "launch" | "keys", _) => return Err(format!("bad arguments for {mode}")),
        _ => return Err(format!("unknown mode {mode:?}")),
    }
    Ok(ModeLine {
        epoch_ms,
        mode,
        args,
    })
}

fn parse_args(args: &[String]) -> Result<PathBuf, String> {
    match args {
        [flag, path] if flag == "--socket" && !path.is_empty() => Ok(PathBuf::from(path)),
        _ => Err("expected --socket <path>".into()),
    }
}

/// Runs the subcommand; returns the exit code.
pub fn run(args: &[String]) -> i32 {
    let path = match parse_args(args) {
        Ok(p) => p,
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
    match mode.mode.as_str() {
        "pick" => crate::pick::run(&mode.args),
        "launch" => crate::launch::run(&mode.args),
        _ => crate::keys::run(&mode.args),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(epoch: u128, mode: &str, args: &[&str]) -> ModeLine {
        ModeLine {
            epoch_ms: epoch,
            mode: mode.into(),
            args: args.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn mode_line_round_trips() {
        for l in [
            line(1_700_000_000_123, "pick", &[]),
            line(5, "pick", &["--rename", "my ws%3A1"]),
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
            Ok(PathBuf::from("/x.sock"))
        );
        assert!(parse_args(&a(&[])).is_err());
        assert!(parse_args(&a(&["--socket"])).is_err());
        assert!(parse_args(&a(&["/x.sock"])).is_err());
    }
}
