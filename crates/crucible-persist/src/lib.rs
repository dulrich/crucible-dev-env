//! Workspace persistence for Crucible Dev Env (CDE).
//!
//! The **Workspace** is CDE's unit of persistence (there is no separate "Session"). This crate
//! holds the pure persistence core:
//!
//! - [`WorkspaceRecord`] and friends: the saved shape of a workspace (topology from
//!   [`crucible_layout`] plus per-pane window stacks), with [`WorkspaceRecord::from_world`] and
//!   [`WorkspaceRecord::layout`] / [`WorkspaceRecord::panes`] so a restore can
//!   `insert_workspace` then replay stacks with `open_window_at` / `set_stack_focus`;
//! - the KDL workspace file format ([`WorkspaceRecord::to_kdl`] / [`WorkspaceRecord::from_kdl`]);
//! - the on-disk [`store`] (atomic save, load, list) under `$XDG_STATE_HOME/cde/workspaces/`;
//! - `/proc` cwd + foreground-command capture ([`capture`]) for external terminals;
//! - the auto-save throttle ([`should_save`]).
//!
//! MVP restore fidelity is **layout + cwd + relaunch command** for external terminals; true
//! scrollback persistence waits on the built-in terminal. Resurrected commands are gated behind a
//! "press Enter to run" safety banner so a restart never silently re-executes a command. All
//! compositor wiring lives elsewhere; nothing here spawns processes or runs timers.

pub mod capture;
mod error;
mod format;
mod policy;
mod record;
pub mod store;

pub use capture::{TerminalCapture, ancestors, capture_terminal, children_of};
pub use error::PersistError;
pub use format::FORMAT_VERSION;
pub use policy::should_save;
pub use record::{PaneRecord, RecordNode, WindowRecord, WorkspaceRecord};
