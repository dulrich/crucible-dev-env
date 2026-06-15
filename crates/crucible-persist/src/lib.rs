//! Workspace persistence for Crucible Dev Env (CDE).
//!
//! The **Workspace** is CDE's unit of persistence (there is no separate "Session"). This crate
//! serializes the layout tree from [`crucible_layout`] to KDL on a short auto-save cadence, surfaces
//! saved workspaces to the fuzzy startup picker, and restores them.
//!
//! MVP restore fidelity is **layout + cwd + relaunch command** for external terminals (cwd captured
//! via `/proc/<pid>/cwd` or OSC 7); true scrollback persistence waits on the built-in terminal.
//! Resurrected commands are gated behind a "press Enter to run" safety banner so a restart never
//! silently re-executes a command. Implementation lands in Phase 3; see `plans/`.
