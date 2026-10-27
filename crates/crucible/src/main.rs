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
mod msg;
mod overlay_host;
mod pick;
mod relaunch;
mod resume;
mod style;
mod tty;

const USAGE: &str = "usage: crucible [--winit | --tty-udev]\n       crucible pick [--rename <name>]\n       crucible launch\n       crucible keys\n       crucible resume [--now [--exit]] --cwd <dir> [-- <command>]\n       crucible relaunch [--app-id <id>] -- <cmdline>\n       crucible msg <request...>\n       crucible overlay-host --socket <path>\n\n  --winit                  run the nested development compositor\n  --tty-udev               run the DRM/KMS backend on a real TTY (multi-output)\n  pick                     the workspace picker (needs $CDE_SOCKET)\n  launch                   the program launcher (needs $CDE_SOCKET)\n  keys                     the read-only settings overlay: effective keybindings (needs $CDE_SOCKET)\n  resume                   restored-terminal banner: Enter runs the old command (--now: run it immediately; --exit: then exit)\n  relaunch                 restored-app placeholder: Enter relaunches the app in its slot\n  msg                      send one request line to the compositor and print the reply\n  overlay-host             a pre-started overlay terminal's program (spawned by CDE)\n  -h, --help               print this usage and exit 0 (`crucible <subcommand> --help` for details)\n\nWith no backend flag, a one-line banner is printed and nothing is started.\n";

/// True when `--help`/`-h` appears anywhere in the arguments (after the program name).
/// Help wins over the backend flags, so `--tty-udev --help` never takes the DRM/KMS device.
fn wants_help(args: &[String]) -> bool {
    args.iter().skip(1).any(|a| a == "--help" || a == "-h")
}

/// True when a subcommand's own `--help`/`-h` is requested. Payload positions never count: for
/// `resume` only arguments before `--` (and not the `--cwd` value), for `pick` not the
/// `--rename` value, and for `msg` only the first argument (the rest is the request line).
fn sub_wants_help(sub: &str, rest: &[String]) -> bool {
    let is_help = |a: &String| a == "--help" || a == "-h";
    if sub == "launch" || sub == "keys" || sub == "overlay-host" {
        return rest.iter().any(is_help);
    }
    if sub == "msg" {
        return rest.first().is_some_and(is_help);
    }
    let value_flag = match sub {
        "resume" => "--cwd",
        "relaunch" => "--app-id",
        _ => "--rename",
    };
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--" if sub == "resume" || sub == "relaunch" => return false,
            f if f == value_flag => i += 1,
            "--help" | "-h" => return true,
            _ => {}
        }
        i += 1;
    }
    false
}

/// Runs a subcommand if `args[1]` names one; returns its exit code. Subcommands never touch the
/// tracing subscriber (stdout belongs to the TUI).
fn run_subcommand(args: &[String]) -> Option<i32> {
    let sub = args.get(1)?.as_str();
    let (usage, run): (&str, fn(&[String]) -> i32) = match sub {
        "pick" => (pick::USAGE, pick::run),
        "launch" => (launch::USAGE, launch::run),
        "keys" => (keys::USAGE, keys::run),
        "resume" => (resume::USAGE, resume::run),
        "relaunch" => (relaunch::USAGE, relaunch::run),
        "msg" => (msg::USAGE, msg::run),
        "overlay-host" => (overlay_host::USAGE, overlay_host::run),
        _ => return None,
    };
    let rest = &args[2..];
    if sub_wants_help(sub, rest) {
        print!("{usage}");
        return Some(0);
    }
    Some(run(rest))
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
    fn help_wins_over_backend_flags() {
        assert!(wants_help(&args(&["crucible", "--tty-udev", "--help"])));
        assert!(wants_help(&args(&["crucible", "-h", "--winit"])));
        assert!(!wants_help(&args(&["crucible", "--winit"])));
        assert!(!wants_help(&args(&["crucible"])));
    }

    #[test]
    fn subcommand_help_never_eats_payloads() {
        assert!(sub_wants_help("pick", &args(&["--help"])));
        assert!(sub_wants_help("pick", &args(&["-h"])));
        assert!(!sub_wants_help("pick", &args(&["--rename", "--help"])));
        assert!(sub_wants_help("resume", &args(&["--help"])));
        assert!(sub_wants_help("resume", &args(&["--cwd", "/x", "-h"])));
        assert!(!sub_wants_help(
            "resume",
            &args(&["--cwd", "/x", "--", "--help"])
        ));
        assert!(!sub_wants_help("resume", &args(&["--cwd", "--help"])));
        assert!(sub_wants_help("relaunch", &args(&["--help"])));
        assert!(sub_wants_help("relaunch", &args(&["--app-id", "x", "-h"])));
        assert!(!sub_wants_help(
            "relaunch",
            &args(&["--app-id", "--help", "--", "x"])
        ));
        assert!(!sub_wants_help("relaunch", &args(&["--", "--help"])));
        assert!(sub_wants_help("launch", &args(&["--help"])));
        assert!(sub_wants_help("launch", &args(&["x", "-h"])));
        assert!(!sub_wants_help("launch", &args(&[])));
        assert!(sub_wants_help("keys", &args(&["--help"])));
        assert!(sub_wants_help("keys", &args(&["x", "-h"])));
        assert!(!sub_wants_help("keys", &args(&[])));
        assert!(sub_wants_help("overlay-host", &args(&["--help"])));
        assert!(sub_wants_help(
            "overlay-host",
            &args(&["--socket", "/x", "-h"])
        ));
        assert!(!sub_wants_help("overlay-host", &args(&["--socket", "/x"])));
        assert!(sub_wants_help("msg", &args(&["--help"])));
        assert!(!sub_wants_help("msg", &args(&["open", "--help"])));
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
