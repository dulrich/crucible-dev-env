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
//! - the named workspace [`slots`] (`Super+<digit>` pins) in `$XDG_STATE_HOME/cde/slots.kdl`, a sibling of the store;
//! - the [`session`] record (open workspaces per output, focus) in `$XDG_STATE_HOME/cde/session.kdl`, also a sibling;
//! - `/proc` cwd + foreground-command capture ([`capture`]) for external terminals;
//! - the one typed `crucible <sub>` argv contract ([`invocation`]: resume, relaunch, pick, launch,
//!   keys, overlay-host) and its field codec ([`field`]);
//! - program resolution for spawns ([`program`]: executable check, child `PATH`, pre-flight);
//! - restore planning ([`restore`]: one spawn job per saved window);
//! - the auto-save throttle ([`should_save`]).
//!
//! MVP restore fidelity is **layout + cwd + relaunch command** for external terminals; true
//! scrollback persistence waits on the built-in terminal. Resurrected commands are gated behind a
//! "press Enter to run" safety banner so a restart never silently re-executes a command. All
//! compositor wiring lives elsewhere; nothing here spawns processes or runs timers.

pub mod capture;
mod error;
pub mod field;
mod format;
pub mod invocation;
mod policy;
pub mod program;
mod record;
pub mod restore;
pub mod session;
pub mod slots;
pub mod store;

pub use capture::{
    CaptureEnv, ProcAnchor, ProcInfo, ProcSnapshot, SpawnClass, TerminalCapture, WindowRecordInput,
    ancestors, capture_anchor, capture_shell, capture_terminal, children_of, relaunch_wrapper_of,
    window_record,
};
pub use error::PersistError;
pub use format::{FORMAT_VERSION, is_session_key};
pub use invocation::{
    CdeWrapper, Invocation, PickTarget, RelaunchWrapper, ResumeMode, ResumeWrapper,
    crucible_subcommand, is_cde_process,
};
pub use policy::should_save;
pub use program::{
    CrucibleExeError, DELETED_SUFFIX, ExeRemoved, child_path, crucible_exe, is_executable_file,
    own_exe_name, preflight_command, reinstalled_exe, resolve_program, same_program, strip_deleted,
};
pub use record::{PaneRecord, RecordNode, WindowRecord, WorkspaceRecord};
pub use restore::{SpawnJob, prepare_lost, prepare_restore, resolve_cwd};
