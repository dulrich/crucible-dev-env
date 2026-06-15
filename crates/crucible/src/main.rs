//! Entry point for the Crucible Dev Env (CDE) binary.
//!
//! Parses the backend-selection flag, sets up tracing, and dispatches to the matching compositor
//! backend. WS-2 wires up `--winit` (the nested development backend); `--tty-udev` (DRM/KMS) lands
//! in WS-6 and is still a stub.

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match std::env::args().nth(1).as_deref() {
        Some("--winit") => crucible_compositor::winit::run_winit(),
        Some("--tty-udev") => {
            // The DRM/KMS TTY backend arrives in WS-6; see plans/01-compositor-up.md.
            println!("--tty-udev (DRM/KMS TTY backend): not yet implemented");
        }
        _ => {
            println!(
                "{} {} — {}",
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
                env!("CARGO_PKG_DESCRIPTION"),
            );
            println!("pass --winit to start the nested development compositor.");
        }
    }
}
