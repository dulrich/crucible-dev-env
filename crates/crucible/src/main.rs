//! Entry point for the Crucible Dev Env (CDE) binary.
//!
//! Parses the backend-selection flag, sets up tracing, and dispatches to the matching compositor
//! backend. `--winit` runs the nested development backend; `--tty-udev` runs the DRM/KMS backend on
//! a real TTY across multiple outputs.

use crucible_compositor::state::CompositorOptions;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    let env = std::env::var("CDE_FOCUS_FOLLOWS_MOUSE").ok();
    let options = parse_options(args.get(2..).unwrap_or(&[]), env.as_deref());

    match args.get(1).map(String::as_str) {
        Some("--winit") => crucible_compositor::winit::run_winit(options),
        Some("--tty-udev") => crucible_compositor::udev::run_udev(options),
        _ => {
            println!(
                "{} {} — {}",
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
                env!("CARGO_PKG_DESCRIPTION"),
            );
            println!(
                "pass --winit to start the nested development compositor, or --tty-udev to run on \
                 a TTY (DRM/KMS, multi-output). Optional after the backend flag: \
                 --no-focus-follows-mouse (or env CDE_FOCUS_FOLLOWS_MOUSE=0)."
            );
        }
    }
}

/// Parses compositor options from the arguments after the backend flag plus the
/// `CDE_FOCUS_FOLLOWS_MOUSE` env value.
///
/// Focus-follows-mouse is on unless `--no-focus-follows-mouse` appears in `rest` or the env value
/// is `0`/`false`/`off` (case-insensitive). Pre-phase-3 stopgap; KDL config replaces it.
fn parse_options(rest: &[String], focus_follows_mouse_env: Option<&str>) -> CompositorOptions {
    let flag_off = rest.iter().any(|a| a == "--no-focus-follows-mouse");
    let env_off = focus_follows_mouse_env.is_some_and(|v| {
        let v = v.trim();
        v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off")
    });
    CompositorOptions {
        focus_follows_mouse: !(flag_off || env_off),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn focus_follows_mouse_defaults_on() {
        assert!(parse_options(&[], None).focus_follows_mouse);
        assert!(parse_options(&args(&["--other"]), None).focus_follows_mouse);
    }

    #[test]
    fn flag_disables_anywhere_after_backend() {
        assert!(!parse_options(&args(&["--no-focus-follows-mouse"]), None).focus_follows_mouse);
        assert!(
            !parse_options(&args(&["--x", "--no-focus-follows-mouse"]), Some("1"))
                .focus_follows_mouse
        );
    }

    #[test]
    fn env_off_values_disable() {
        for v in ["0", "false", "FALSE", "off", "Off", " off "] {
            assert!(!parse_options(&[], Some(v)).focus_follows_mouse, "{v}");
        }
    }

    #[test]
    fn other_env_values_keep_it_on() {
        for v in ["1", "true", "on", "", "no", "yes"] {
            assert!(parse_options(&[], Some(v)).focus_follows_mouse, "{v}");
        }
    }
}
