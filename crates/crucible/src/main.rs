//! Entry point for the Crucible Dev Env (CDE) binary.
//!
//! In later phases this launches the compositor (nested `winit` or `udev`/DRM backend) and the fuzzy
//! workspace picker. For Phase 0 it is a placeholder that reports its identity so the workspace
//! builds and runs end-to-end.

fn main() {
    println!(
        "{} {} — {}",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_DESCRIPTION"),
    );

    // WS-1 recognizes the backend-selection flags the later workstreams will wire up, but the
    // compositor itself is still a stub. See plans/01-compositor-up.md.
    match std::env::args().nth(1).as_deref() {
        Some("--winit") => println!("--winit (nested development backend): not yet implemented"),
        Some("--tty-udev") => println!("--tty-udev (DRM/KMS TTY backend): not yet implemented"),
        _ => println!("pre-alpha: see plans/00-mvp-roadmap.md for what's coming."),
    }
}
