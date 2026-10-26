//! The compositor-independent CDE verb set.
//!
//! [`Action`] is produced by [`crate::chord::resolve`] from a `Super`-leader chord and consumed by
//! the compositor's `handle_key_action` (`crucible-compositor::input::execute`), which executes
//! each verb through the single model-mutation entry point (`update_model`/`with_model`). Verbs
//! that used to be one variant per direction/sign (`FocusLeft`, `FocusDown`, …) are parameterized
//! over [`crucible_layout::Direction`] instead, so the compositor needs exactly one execution arm
//! per verb (`reviews/2026-07-30-tn-code-review.md` F1, F3).

use crucible_layout::Direction;

/// Default terminal command used when `$TERMINAL` is unset, tried in order.
///
/// CDE targets Wayland-native terminals; the compositor tries these against `$PATH` in order at
/// spawn time and launches the child with `WAYLAND_DISPLAY` pointed at CDE's own socket so it
/// connects back to the compositor.
pub const DEFAULT_TERMINALS: &[&str] = &["foot", "alacritty", "kitty", "weston-terminal"];

/// A CDE verb triggered by an intercepted `Super`-leader chord (or, for [`Action::VtSwitch`], a
/// libinput `Ctrl+Alt+F<n>` chord independent of the leader).
///
/// Produced by [`crate::chord::resolve`]. `Quit`, `RunTerminal`, `Run`, `OpenPicker`, `OpenLauncher`,
/// `OpenSettings`, `RenameWorkspace` and `VtSwitch` are
/// compositor-level (spawn a process, stop the loop, open UI, or ask the backend to switch VTs); every
/// other variant names a `crucible_layout::World` operation the compositor applies through its
/// single mutation entry point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Quit the compositor cleanly (stop the main loop). `Super+Shift+Q`.
    Quit,
    /// Close the focused pane if it is empty and not the workspace's last pane (`World::close_pane`).
    /// Never closes windows; a refused close is a no-op. `Super+W`.
    ClosePane,
    /// Spawn the configured terminal into the focused pane (the window joins the pane's stack). The
    /// compositor picks the terminal (config `terminal`, then `$TERMINAL`, then a default list).
    /// Because `World::open_window` is only called from the surface-map path, a keybind never
    /// fabricates a window: the spawned client's toplevel is placed by the shell map path.
    RunTerminal,
    /// Run a real command line (non-empty) via `/bin/sh -c 'exec <cmd>'` into the focused pane.
    /// The window joins the pane's stack through the shell map path.
    Run(String),
    /// Run a command line (non-empty) in a new terminal: the configured terminal starts
    /// `crucible resume --now --cwd <dir> -- <cmd>`. No default chord.
    RunInTerminal(String),

    /// Move pane focus in `Direction` within the active workspace. `Super+H/J/K/L`.
    Focus(Direction),
    /// Swap the focused pane with its `Direction` neighbour (focus follows). `Super+Shift+H/J/K/L`.
    Move(Direction),
    /// Split the focused pane along `Direction`; the new empty pane becomes focused.
    /// `Super+Backslash` (`Right`) / `Super+Minus` (`Down`).
    ///
    /// The user then spawns a terminal (`Super+Shift+Return`) which maps into the now-focused pane.
    Split(Direction),
    /// Adjust the focused pane's split ratio along `Direction`. `Super+Ctrl+H/J/K/L`.
    ///
    /// The step size is a compositor-edge constant, not chord policy (see `execute::RESIZE_DELTA`
    /// in `crucible-compositor`) — the leader table only names *which* edge moves, not by how much.
    Resize(Direction),

    /// Cycle the focused pane's window stack by `delta` steps (wraparound). `Super+Tab` (`1`) /
    /// `Super+Shift+Tab` (`-1`, including the `ISO_Left_Tab` keysym some layouts deliver).
    CycleStack(i64),
    /// Switch the focused output's active workspace by `delta` (wraparound). `Super+]` (`1`) /
    /// `Super+[` (`-1`).
    CycleWorkspace(i64),
    /// Create a new workspace on the focused output with an auto-generated name and switch to it.
    /// `Super+N`.
    ///
    /// The fuzzy picker / interactive naming is **Phase 3**.
    NewWorkspace,
    /// Move the focused window `delta` workspaces from the active one on the focused output
    /// (wraparound). `Super+Shift+]` (`1`) / `Super+Shift+[` (`-1`).
    ///
    /// No-op when there is no focused window or only one workspace exists.
    MoveWindowToWorkspace(i64),
    /// Close the focused output's active workspace: its record is saved first, then its windows
    /// are ended. Refused on the output's last workspace or while a restore is pending.
    /// `Super+Shift+W`.
    CloseWorkspace,
    /// Switch to the workspace pinned to slot `n` (0-9); an unpinned slot is a no-op.
    /// `Super+<digit>`.
    Slot(u8),
    /// Pin the active workspace to slot `n` (0-9). `Super+Shift+<digit>`.
    PinSlot(u8),
    /// Move the focused output's active workspace `delta` outputs along the output order
    /// (wraparound). `Super+Ctrl+]` (`1`) / `Super+Ctrl+[` (`-1`). No-op with one output.
    MoveWorkspaceToOutput(i64),

    /// Open the fuzzy workspace picker. `Super+P`. Implemented by WP-5; until then the compositor
    /// only logs that it is not available.
    OpenPicker,
    /// Open the program launcher (`crucible launch`). `Super+Return`.
    OpenLauncher,
    /// Open the read-only settings overlay (`crucible keys`). `Super+comma`.
    OpenSettings,
    /// Rename the active workspace (interactive prompt). `Super+Shift+R`. Implemented by WP-5.
    RenameWorkspace,

    /// Switch to virtual terminal `n` (only meaningful on the udev/DRM backend).
    ///
    /// Produced when libinput delivers a `Ctrl+Alt+F<n>` chord (the `XF86Switch_VT_<n>` keysyms),
    /// independently of the `Super` leader. The nested `winit` backend never sees these, so this
    /// verb is routed to a no-op there.
    VtSwitch(i32),
}

impl std::fmt::Display for Action {
    /// The exact `binds` action grammar (`parse_action` accepts every form but `vt-switch`).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let dir = |d: &Direction| match d {
            Direction::Left => "left",
            Direction::Right => "right",
            Direction::Up => "up",
            Direction::Down => "down",
        };
        match self {
            Self::Quit => f.write_str("quit"),
            Self::ClosePane => f.write_str("close-pane"),
            Self::RunTerminal => f.write_str("run-terminal"),
            Self::Run(cmd) => write!(f, "run {cmd}"),
            Self::RunInTerminal(cmd) => write!(f, "run-in-terminal {cmd}"),
            Self::Focus(d) => write!(f, "focus {}", dir(d)),
            Self::Move(d) => write!(f, "move {}", dir(d)),
            Self::Split(d) => write!(f, "split {}", dir(d)),
            Self::Resize(d) => write!(f, "resize {}", dir(d)),
            Self::CycleStack(n) => write!(f, "cycle-stack {n}"),
            Self::CycleWorkspace(n) => write!(f, "cycle-workspace {n}"),
            Self::NewWorkspace => f.write_str("new-workspace"),
            Self::MoveWindowToWorkspace(n) => write!(f, "move-window-to-workspace {n}"),
            Self::CloseWorkspace => f.write_str("close-workspace"),
            Self::Slot(n) => write!(f, "slot {n}"),
            Self::PinSlot(n) => write!(f, "pin-slot {n}"),
            Self::MoveWorkspaceToOutput(n) => write!(f, "move-workspace-to-output {n}"),
            Self::OpenPicker => f.write_str("open-picker"),
            Self::OpenLauncher => f.write_str("open-launcher"),
            Self::OpenSettings => f.write_str("open-settings"),
            Self::RenameWorkspace => f.write_str("rename-workspace"),
            Self::VtSwitch(n) => write!(f, "vt-switch {n}"),
        }
    }
}

/// Resolves the terminal command: `$TERMINAL` if set and non-empty, else the first default.
///
/// Pure aside from reading the environment; the actual lookup against `$PATH` happens when the
/// compositor spawns the child.
#[must_use]
pub fn terminal_command() -> String {
    match std::env::var("TERMINAL") {
        Ok(term) if !term.trim().is_empty() => term,
        _ => DEFAULT_TERMINALS[0].to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serializes the two tests below: `$TERMINAL` is process-global state, and `cargo test` runs
    /// this crate's tests on multiple threads, so mutating it from two tests concurrently would be
    /// racy (`resolve`'s own tests avoid the hazard entirely by never asserting on the exact
    /// string — see `chord::tests::super_return_runs_a_terminal`).
    static TERMINAL_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn terminal_command_falls_back_to_default_without_env() {
        let _guard = TERMINAL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("TERMINAL").ok();
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("TERMINAL");
        }
        assert_eq!(terminal_command(), DEFAULT_TERMINALS[0]);
        #[allow(unsafe_code)]
        unsafe {
            match prev {
                Some(v) => std::env::set_var("TERMINAL", v),
                None => std::env::remove_var("TERMINAL"),
            }
        }
    }

    #[test]
    fn terminal_command_prefers_env_when_set_and_non_empty() {
        let _guard = TERMINAL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("TERMINAL").ok();
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("TERMINAL", "my-term");
        }
        assert_eq!(terminal_command(), "my-term");
        #[allow(unsafe_code)]
        unsafe {
            match prev {
                Some(v) => std::env::set_var("TERMINAL", v),
                None => std::env::remove_var("TERMINAL"),
            }
        }
    }
}
