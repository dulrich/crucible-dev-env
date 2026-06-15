//! Entry point for the Crucible Dev Env (CDE) binary.
//!
//! Parses the backend-selection flag, sets up tracing, and dispatches to the matching compositor
//! backend. `--winit` runs the nested development backend; `--tty-udev` runs the DRM/KMS backend on
//! a real TTY across multiple outputs.

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match std::env::args().nth(1).as_deref() {
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
                 a TTY (DRM/KMS, multi-output)."
            );
        }
    }
}
