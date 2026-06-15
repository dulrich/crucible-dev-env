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
    println!("pre-alpha: see plans/00-mvp-roadmap.md for what's coming.");
}
