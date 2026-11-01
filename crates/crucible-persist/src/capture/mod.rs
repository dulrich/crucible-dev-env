//! `/proc` capture of a terminal window's working directory and foreground command, and the one
//! window classifier ([`window_record`]) that turns a managed window into its saved record.
//!
//! Pure parsers over `/proc` text plus a thin filesystem edge that takes a [`CaptureEnv`]
//! (normally the real `/proc` and the user's claude home; tests point it at a fixture tree).
//!
//! Layout: [`procfs`] reads `/proc`, [`shell`] quotes and joins argv, [`terminal`] decides a
//! terminal's foreground command through a table of program policies, [`claude`] holds the one
//! program-specific policy (resuming a `claude` session), and [`classify`] turns a window into its
//! saved record.

use std::path::PathBuf;

mod classify;
mod claude;
#[cfg(test)]
mod fake_proc;
mod procfs;
mod shell;
mod terminal;

pub use classify::{SpawnClass, WindowRecordInput, relaunch_wrapper_of, window_record};
pub use claude::{claude_resume_argv, claude_session_id};
pub use procfs::{
    ProcInfo, ProcSnapshot, Stat, ancestors, children_of, parse_children, parse_cmdline,
    parse_stat, read_cmdline,
};
pub use shell::{is_shell_program, join_command, shell_quote};
pub use terminal::{TerminalCapture, capture_terminal};

/// Where capture reads from: the `/proc` tree and the claude home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureEnv {
    /// The process tree root (normally `/proc`; tests point it at a fixture tree).
    pub proc_root: PathBuf,
    /// `$CLAUDE_CONFIG_DIR`, else `$HOME/.claude`. A home that does not exist (or has no
    /// `sessions/`) simply reads nothing: every program is then captured by its plain argv.
    pub claude_home: PathBuf,
}

impl CaptureEnv {
    /// `<claude_home>/sessions`, where claude keeps its per-pid session records.
    #[must_use]
    pub fn sessions_dir(&self) -> PathBuf {
        self.claude_home.join("sessions")
    }
}
