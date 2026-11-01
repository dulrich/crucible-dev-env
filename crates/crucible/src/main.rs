//! Entry point for the Crucible Dev Env (CDE) binary.
//!
//! Parses the backend-selection flag, sets up tracing, and dispatches to the matching compositor
//! backend. `--winit` runs the nested development backend; `--tty-udev` runs the DRM/KMS backend on
//! a real TTY across multiple outputs. The `pick`, `resume` and `msg` subcommands are the
//! client-side tools the compositor spawns (and scripts can call).

mod client;
mod fuzzy;
mod keys;
mod latency;
mod launch;
mod mem;
mod msg;
mod overlay;
mod overlay_host;
mod pick;
mod relaunch;
mod resume;
mod style;
mod table;
mod tty;

const USAGE: &str = "usage: crucible [--winit | --tty-udev]\n       crucible pick [--rename <name> | --root <path>]\n       crucible launch\n       crucible keys\n       crucible resume [--now [--exit]] --cwd <dir> [-- <command>]\n       crucible relaunch [--app-id <id>] -- <cmdline>\n       crucible mem [--pid <n>]\n       crucible msg <request...>\n       crucible overlay-host --socket <path>\n\n  --winit                  run the nested development compositor\n  --tty-udev               run the DRM/KMS backend on a real TTY (multi-output)\n  pick                     the workspace picker (needs $CDE_SOCKET)\n  launch                   the program launcher (needs $CDE_SOCKET)\n  keys                     the read-only settings overlay: effective keybindings (needs $CDE_SOCKET)\n  resume                   restored-terminal banner: Enter runs the old command (--now: run it immediately; --exit: then exit)\n  relaunch                 restored-app placeholder: Enter relaunches the app in its slot\n  mem                      real memory (PSS, swap) per app of the running CDE\n  msg                      send one request line to the compositor and print the reply\n  overlay-host             a pre-started overlay terminal's program (spawned by CDE)\n  -h, --help               print this usage and exit 0 (`crucible <subcommand> --help` for details)\n\nWith no backend flag, a one-line banner is printed and nothing is started.\n";

/// True when `--help`/`-h` appears anywhere in the arguments (after the program name).
/// Help wins over the backend flags, so `--tty-udev --help` never takes the DRM/KMS device.
fn wants_help(args: &[String]) -> bool {
    args.iter().skip(1).any(|a| a == "--help" || a == "-h")
}

/// What a subcommand's parser found: a request for its usage, or the parsed arguments.
#[derive(Debug, PartialEq, Eq)]
pub enum Parsed<T> {
    /// `--help`/`-h` in a non-payload position: print the usage, exit 0, no side effect.
    Help,
    /// The parsed arguments to run with.
    Run(T),
}

/// True when `--help`/`-h` appears in a non-payload position of a subcommand's arguments. Payload
/// positions never count: the value of any of `value_flags` (for example `--rename`), and, when
/// `stop_at_dashdash`, everything from `--` on.
pub fn help_requested(args: &[String], value_flags: &[&str], stop_at_dashdash: bool) -> bool {
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--" if stop_at_dashdash => return false,
            f if value_flags.contains(&f) => i += 1,
            "--help" | "-h" => return true,
            _ => {}
        }
        i += 1;
    }
    false
}

/// A subcommand: its name, its `--help` text and its entry point. The entry point's parser
/// recognises `--help`/`-h` itself, printing the usage and returning 0 before any side effect.
type Subcommand = (&'static str, &'static str, fn(&[String]) -> i32);

const SUBCOMMANDS: &[Subcommand] = &[
    ("pick", pick::USAGE, pick::run),
    ("launch", launch::USAGE, launch::run),
    ("keys", keys::USAGE, keys::run),
    ("resume", resume::USAGE, resume::run),
    ("relaunch", relaunch::USAGE, relaunch::run),
    ("mem", mem::USAGE, mem::run),
    ("msg", msg::USAGE, msg::run),
    ("overlay-host", overlay_host::USAGE, overlay_host::run),
];

/// Runs a subcommand if `args[1]` names one; returns its exit code. Subcommands never touch the
/// tracing subscriber (stdout belongs to the TUI).
fn run_subcommand(args: &[String]) -> Option<i32> {
    let sub = args.get(1)?.as_str();
    let (_, _, run) = SUBCOMMANDS.iter().find(|(name, _, _)| *name == sub)?;
    Some(run(&args[2..]))
}

/// True when `--build` or `--version` appears in the top-level arguments (after the program name);
/// help is checked first, so `--help` wins.
fn wants_build(args: &[String]) -> bool {
    args.iter()
        .skip(1)
        .any(|a| a == "--build" || a == "--version")
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(code) = run_subcommand(&args) {
        std::process::exit(code);
    }
    if wants_help(&args) {
        print!("{USAGE}");
        return;
    }
    if wants_build(&args) {
        println!("{}", env!("FLEET_BUILD_ID"));
        return;
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match args.get(1).map(String::as_str) {
        Some("--winit") => crucible_compositor::winit::run_winit(),
        Some("--tty-udev") => crucible_compositor::udev::run_udev(),
        _ => {
            println!(
                "{} {} — {}",
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
                env!("CARGO_PKG_DESCRIPTION"),
            );
            println!(
                "pass --winit to start the nested development compositor, or --tty-udev to run on \
                 a TTY (DRM/KMS, multi-output). Configuration: ~/.config/cde/config.kdl."
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn build_and_version_flags_are_detected_after_the_program_name() {
        assert!(wants_build(&args(&["crucible", "--build"])));
        assert!(wants_build(&args(&["crucible", "--version"])));
        assert!(!wants_build(&args(&["crucible", "--winit"])));
        assert!(!wants_build(&args(&["--build"])));
    }

    #[test]
    fn help_wins_over_backend_flags() {
        assert!(wants_help(&args(&["crucible", "--tty-udev", "--help"])));
        assert!(wants_help(&args(&["crucible", "-h", "--winit"])));
        assert!(!wants_help(&args(&["crucible", "--winit"])));
        assert!(!wants_help(&args(&["crucible"])));
    }

    #[test]
    fn help_requested_skips_the_value_flag_and_stops_at_dashdash() {
        assert!(help_requested(&args(&["--help"]), &[], false));
        assert!(help_requested(&args(&["x", "-h"]), &[], false));
        assert!(!help_requested(&args(&[]), &[], false));
        assert!(!help_requested(
            &args(&["--rename", "--help"]),
            &["--rename", "--root"],
            false
        ));
        assert!(help_requested(
            &args(&["--rename", "x", "-h"]),
            &["--rename", "--root"],
            false
        ));
        assert!(!help_requested(
            &args(&["--root", "--help"]),
            &["--rename", "--root"],
            false
        ));
        assert!(help_requested(
            &args(&["--root", "", "--help"]),
            &["--rename", "--root"],
            false
        ));
        assert!(!help_requested(&args(&["--", "-h"]), &[], true));
        assert!(help_requested(&args(&["--", "-h"]), &[], false));
    }

    #[test]
    fn every_subcommand_usage_names_the_subcommand() {
        for (name, usage, _) in SUBCOMMANDS {
            assert!(
                usage.starts_with(&format!("usage: crucible {name}")),
                "{name}"
            );
        }
    }

    #[test]
    fn unknown_first_arg_is_not_a_subcommand() {
        assert!(run_subcommand(&args(&["crucible", "--winit"])).is_none());
        assert!(run_subcommand(&args(&["crucible"])).is_none());
        assert_eq!(
            run_subcommand(&args(&["crucible", "msg", "--help"])),
            Some(0)
        );
    }
}
