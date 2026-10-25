//! Entry point for the Crucible Dev Env (CDE) binary.
//!
//! Parses the backend-selection flag, sets up tracing, and dispatches to the matching compositor
//! backend. `--winit` runs the nested development backend; `--tty-udev` runs the DRM/KMS backend on
//! a real TTY across multiple outputs.

const USAGE: &str = "usage: crucible [--winit | --tty-udev]\n\n  --winit                  run the nested development compositor\n  --tty-udev               run the DRM/KMS backend on a real TTY (multi-output)\n  -h, --help               print this usage and exit 0\n\nWith no backend flag, a one-line banner is printed and nothing is started.\n";

/// True when `--help`/`-h` appears anywhere in the arguments (after the program name).
/// Help wins over the backend flags, so `--tty-udev --help` never takes the DRM/KMS device.
fn wants_help(args: &[String]) -> bool {
    args.iter().skip(1).any(|a| a == "--help" || a == "-h")
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
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
}
