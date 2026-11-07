//! The compositor-independent CDE verb set.
//!
//! [`Action`] is produced by [`crate::chord::resolve`] from a `Super`-leader chord and consumed by
//! the compositor's `handle_key_action` (`crucible-compositor::input::execute`), which executes
//! each verb through the single model-mutation entry point (`update_model`/`with_model`). Verbs
//! that used to be one variant per direction/sign (`FocusLeft`, `FocusDown`, …) are parameterized
//! over [`crucible_layout::Direction`] instead, so the compositor needs exactly one execution arm
//! per verb.
//!
//! ## The verb grammar
//!
//! [`Action`]'s `Display` and `FromStr` impls are the one verb grammar: `Display` prints it and
//! `FromStr` parses it back. The argument-free verbs (and their [`Area`]) live in the `NULLARY`
//! table; every verb that takes an argument has an explicit arm in each impl. `vt-switch <n>` is
//! printed but deliberately **not** parsed: VT switches come only from libinput's `Ctrl+Alt+F<n>`
//! keysyms, never from a bind or an IPC `exec`. The bind-only `none` (unbind) is
//! [`crate::bindings::BindTarget`].

use std::str::FromStr;

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
/// `OpenSettings`, `ToggleHeader`, `RenameWorkspace` and `VtSwitch` are
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
    /// Politely ask the focused pane's current window to close (xdg `close` / X11 `WM_DELETE_WINDOW`; an X11
    /// window without it is destroyed). `Super+Shift+C`.
    CloseWindow,
    /// Switch the keyboard to the layout named `<name>` in config `keyboard { layouts … }`
    /// (selected by its position in that list). A name not in the list is a logged no-op.
    /// Defaults: `Super+E` -> `us`, `Super+S` -> `es`.
    SetLayout(String),
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
    /// Run a command line (non-empty) in the background into workspace `workspace`: its first
    /// toplevel joins that workspace's focused pane (a missing workspace is created, not
    /// activated, and is ephemeral). No output's active workspace changes. No default chord.
    RunOn {
        /// The target workspace name (one word).
        workspace: String,
        /// The verbatim command line.
        command: String,
    },

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
    /// Move the focused output's active workspace by `delta` places within its stack, clamped at
    /// the ends (a no-op there, not a wrap). Unbound by default.
    ReorderWorkspace(i64),
    /// Create a new workspace on the focused output with an auto-generated name and switch to it.
    /// `Super+N`.
    ///
    /// The name is auto-generated; renaming is [`Action::RenameWorkspace`].
    NewWorkspace,
    /// Move the focused window `delta` workspaces from the active one on the focused output
    /// (wraparound). `Super+Shift+]` (`1`) / `Super+Shift+[` (`-1`).
    ///
    /// No-op when there is no focused window or only one workspace exists.
    MoveWindowToWorkspace(i64),
    /// Mark the focused window; pressed again elsewhere, move the marked window into the focused
    /// pane (any workspace or output); pressed again in the marked window's own pane, clear the
    /// mark. `Super+M`.
    ///
    /// No-op when there is no mark and no focused window.
    MarkWindow,
    /// Like [`Action::MarkWindow`], but swaps: the marked window takes the focused pane's active
    /// window's place and that window takes the marked one's. With no mark it marks; into an empty
    /// pane it moves; in the marked window's own pane it clears. `Super+Shift+M`.
    SwapMarkedWindow,
    /// Capture the focused terminal's current directory as the active workspace's root (the folder
    /// new terminals and apps start in). `Super+Shift+D`.
    ///
    /// Changes nothing and fails when the focused window is not a terminal with a known pid.
    CaptureWorkspaceRoot,
    /// Open the workspace-root prompt in the picker overlay (prefilled with the current root; an
    /// empty answer clears it). `Super+Ctrl+Shift+D`. Not the picker's `root` config folders.
    SetWorkspaceRoot,
    /// Close the focused output's active workspace: its record is saved first, then its windows
    /// are ended. Refused on the output's last workspace or while a restore is pending.
    /// `Super+Shift+W`.
    CloseWorkspace,
    /// Delete the focused output's active workspace: its windows are ended and its saved record is
    /// moved to the `deleted/` trash (no final save), and its slot pins are cleared. Refused on the
    /// output's last workspace or while a restore is pending. Unbound by default (destructive).
    DeleteWorkspace,
    /// Save and close every open workspace (as [`Action::CloseWorkspace`] does), leaving one fresh
    /// seed workspace per output. Unbound by default.
    CloseAllWorkspaces,
    /// Switch to the workspace pinned to slot `n` (0-9); an unpinned slot is a no-op.
    /// `Super+<digit>`.
    Slot(u8),
    /// Pin the active workspace to slot `n` (0-9). `Super+Shift+<digit>`.
    PinSlot(u8),
    /// Move the focused output's active workspace `delta` outputs along the output order
    /// (wraparound). `Super+Ctrl+]` (`1`) / `Super+Ctrl+[` (`-1`). No-op with one output.
    MoveWorkspaceToOutput(i64),

    /// Open the fuzzy workspace picker. `Super+P`.
    OpenPicker,
    /// Open the program launcher (`crucible launch`). `Super+Return`.
    OpenLauncher,
    /// Open the read-only settings overlay (`crucible keys`). `Super+comma`.
    OpenSettings,
    /// Show/hide the workspace header strip (`header { mode "toggle" }` only; a no-op otherwise).
    /// `Super+B`.
    ToggleHeader,
    /// Toggle fullscreen for the focused window (`World::toggle_fullscreen`): it covers its output
    /// with no header, gaps or other panes. `Super+F`.
    ToggleFullscreen,
    /// Rename the active workspace (interactive prompt). `Super+Shift+R`.
    RenameWorkspace,
    /// Spawn the configured `lock-command` (a screen locker, e.g. `swaylock`) detached. The lock
    /// itself starts only when the locker makes its `ext-session-lock-v1` request. `Super+Escape`.
    Lock,

    /// Switch to virtual terminal `n` (only meaningful on the udev/DRM backend).
    ///
    /// Produced when libinput delivers a `Ctrl+Alt+F<n>` chord (the `XF86Switch_VT_<n>` keysyms),
    /// independently of the `Super` leader. The nested `winit` backend never sees these, so this
    /// verb is routed to a no-op there.
    VtSwitch(i32),
    /// Capture the focused output and deliver it per config `screenshot { destination }`.
    /// `Super+Print`.
    Screenshot,
    /// Start the interactive region snip. `Super+Shift+Print`.
    Snip,
}

/// The argument-free verbs: the action, its grammar word and its [`Area`]. `Display`, `FromStr`
/// and [`Action::area`] all consult this one table.
#[rustfmt::skip]
const NULLARY: &[(Action, &str, Area)] = &[
    (Action::Quit, "quit", Area::SessionAndSystem),
    (Action::ClosePane, "close-pane", Area::PanesAndWindows),
    (Action::CloseWindow, "close-window", Area::PanesAndWindows),
    (Action::RunTerminal, "run-terminal", Area::LaunchAndOverlays),
    (Action::NewWorkspace, "new-workspace", Area::Workspaces),
    (Action::MarkWindow, "mark-window", Area::PanesAndWindows),
    (Action::SwapMarkedWindow, "swap-marked-window", Area::PanesAndWindows),
    (Action::CaptureWorkspaceRoot, "capture-workspace-root", Area::Workspaces),
    (Action::SetWorkspaceRoot, "set-workspace-root", Area::Workspaces),
    (Action::CloseWorkspace, "close-workspace", Area::Workspaces),
    (Action::DeleteWorkspace, "delete-workspace", Area::Workspaces),
    (Action::CloseAllWorkspaces, "close-all-workspaces", Area::Workspaces),
    (Action::OpenPicker, "open-picker", Area::LaunchAndOverlays),
    (Action::OpenLauncher, "open-launcher", Area::LaunchAndOverlays),
    (Action::OpenSettings, "open-settings", Area::LaunchAndOverlays),
    (Action::ToggleHeader, "toggle-header", Area::Outputs),
    (Action::ToggleFullscreen, "toggle-fullscreen", Area::PanesAndWindows),
    (Action::RenameWorkspace, "rename-workspace", Area::Workspaces),
    (Action::Lock, "lock", Area::SessionAndSystem),
    (Action::Screenshot, "screenshot", Area::LaunchAndOverlays),
    (Action::Snip, "snip", Area::LaunchAndOverlays),
];

/// The direction words, shared by `Display` and `FromStr`.
const DIRECTIONS: [(Direction, &str); 4] = [
    (Direction::Left, "left"),
    (Direction::Right, "right"),
    (Direction::Up, "up"),
    (Direction::Down, "down"),
];

fn direction_word(d: Direction) -> &'static str {
    DIRECTIONS
        .iter()
        .find(|(dir, _)| *dir == d)
        .map_or("", |(_, word)| word)
}

impl std::fmt::Display for Action {
    /// The exact action grammar (`FromStr` accepts every form but `vt-switch`).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some((_, word, _)) = NULLARY.iter().find(|(action, ..)| action == self) {
            return f.write_str(word);
        }
        let dir = |d: &Direction| direction_word(*d);
        match self {
            Self::SetLayout(name) => write!(f, "set-layout {name}"),
            Self::Run(cmd) => write!(f, "run {cmd}"),
            Self::RunInTerminal(cmd) => write!(f, "run-in-terminal {cmd}"),
            Self::RunOn { workspace, command } => write!(f, "run-on {workspace} {command}"),
            Self::Focus(d) => write!(f, "focus {}", dir(d)),
            Self::Move(d) => write!(f, "move {}", dir(d)),
            Self::Split(d) => write!(f, "split {}", dir(d)),
            Self::Resize(d) => write!(f, "resize {}", dir(d)),
            Self::CycleStack(n) => write!(f, "cycle-stack {n}"),
            Self::CycleWorkspace(n) => write!(f, "cycle-workspace {n}"),
            Self::ReorderWorkspace(n) => write!(f, "reorder-workspace {n}"),
            Self::MoveWindowToWorkspace(n) => write!(f, "move-window-to-workspace {n}"),
            Self::Slot(n) => write!(f, "slot {n}"),
            Self::PinSlot(n) => write!(f, "pin-slot {n}"),
            Self::MoveWorkspaceToOutput(n) => write!(f, "move-workspace-to-output {n}"),
            Self::VtSwitch(n) => write!(f, "vt-switch {n}"),
            // Every nullary verb returned from `NULLARY` above.
            _ => unreachable!("{self:?} has no NULLARY row"),
        }
    }
}

/// The functional area a verb belongs to; the settings overlay groups its rows by it. The
/// declaration order is the fixed display order (`Ord` follows it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Area {
    /// Pane focus, movement, splitting, resizing, window close/mark, fullscreen.
    PanesAndWindows,
    /// Cycling a pane's window stack.
    Stacks,
    /// Creating, switching, reordering, closing and naming workspaces; slots; workspace roots.
    Workspaces,
    /// Per-output verbs.
    Outputs,
    /// Spawning terminals and programs, opening the picker, launcher and settings.
    LaunchAndOverlays,
    /// Quit, lock, keyboard layout, VT switching.
    SessionAndSystem,
}

impl Area {
    /// Every area in display order.
    pub const ALL: [Area; 6] = [
        Area::PanesAndWindows,
        Area::Stacks,
        Area::Workspaces,
        Area::Outputs,
        Area::LaunchAndOverlays,
        Area::SessionAndSystem,
    ];

    /// The heading shown for this area.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Area::PanesAndWindows => "Panes & windows",
            Area::Stacks => "Stacks",
            Area::Workspaces => "Workspaces",
            Area::Outputs => "Outputs",
            Area::LaunchAndOverlays => "Launch & overlays",
            Area::SessionAndSystem => "Session & system",
        }
    }
}

impl Action {
    /// The functional area of this verb: the `NULLARY` row for an argument-free verb, else the
    /// argument verb's own arm. The test-side `every_variant` guard keeps the classification
    /// complete (a new verb fails to compile there until it is listed).
    #[must_use]
    pub fn area(&self) -> Area {
        if let Some((.., area)) = NULLARY.iter().find(|(action, ..)| action == self) {
            return *area;
        }
        match self {
            Self::Focus(_) | Self::Move(_) | Self::Split(_) | Self::Resize(_) => {
                Area::PanesAndWindows
            }
            Self::CycleStack(_) => Area::Stacks,
            Self::CycleWorkspace(_)
            | Self::ReorderWorkspace(_)
            | Self::MoveWindowToWorkspace(_)
            | Self::Slot(_)
            | Self::PinSlot(_) => Area::Workspaces,
            Self::MoveWorkspaceToOutput(_) => Area::Outputs,
            Self::Run(_) | Self::RunInTerminal(_) | Self::RunOn { .. } => Area::LaunchAndOverlays,
            Self::SetLayout(_) | Self::VtSwitch(_) => Area::SessionAndSystem,
            // Every nullary verb returned from `NULLARY` above.
            _ => unreachable!("{self:?} has no NULLARY row"),
        }
    }

    /// The lock rule: whether this action may run while the session is locked. Only VT switching
    /// does; every other key goes to the locker and every acting IPC request is refused. The
    /// compositor's keyboard filter and its action gate share this one predicate.
    #[must_use]
    pub fn allowed_while_locked(&self) -> bool {
        matches!(self, Self::VtSwitch(_))
    }

    /// The snip rule: whether this action may run while snip mode is active. Only `snip` (which
    /// toggles the mode off) and VT switching do, from chords and IPC alike.
    #[must_use]
    pub fn allowed_while_snipping(&self) -> bool {
        matches!(self, Self::Snip | Self::VtSwitch(_))
    }
}

/// Why an action string does not parse ([`Action`]'s `FromStr`). The message is the reason only;
/// callers add context (the `binds` chord, the IPC request).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ActionParseError(pub String);

impl FromStr for Action {
    type Err = ActionParseError;

    /// Parses the action grammar printed by `Display` (every form but `vt-switch`). `run` and
    /// `run-in-terminal` take the rest of the text verbatim (trimmed).
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let bad = ActionParseError;
        let words: Vec<&str> = text.split_whitespace().collect();
        let direction = |arg: Option<&&str>| -> Result<Direction, ActionParseError> {
            match arg.map(|s| s.to_ascii_lowercase()) {
                Some(word) => DIRECTIONS
                    .iter()
                    .find(|(_, name)| *name == word)
                    .map(|(dir, _)| *dir)
                    .ok_or_else(|| {
                        bad(format!(
                            "`{word}` is not a direction (left, right, up, down)"
                        ))
                    }),
                None => Err(bad("expected a direction (left, right, up, down)".into())),
            }
        };
        let delta = |arg: Option<&&str>| -> Result<i64, ActionParseError> {
            let arg = arg.ok_or_else(|| bad("expected a signed integer step".into()))?;
            arg.parse::<i64>()
                .map_err(|_| bad(format!("`{arg}` is not an integer")))
        };
        let slot_index = |arg: Option<&&str>| -> Result<u8, ActionParseError> {
            let arg = arg.ok_or_else(|| bad("expected a slot number 0-9".into()))?;
            match arg.parse::<u8>() {
                Ok(n) if n <= 9 => Ok(n),
                Ok(_) => Err(bad(format!("slot `{arg}` is out of range (0-9)"))),
                Err(_) => Err(bad(format!("`{arg}` is not a slot number 0-9"))),
            }
        };
        // `nullary` rejects any argument; `unary` rejects a second one.
        let nullary = |action: Self| match words.get(1) {
            None => Ok(action),
            Some(extra) => Err(bad(format!("unexpected argument `{extra}`"))),
        };
        let unary = |action: Self| match words.get(2) {
            None => Ok(action),
            Some(extra) => Err(bad(format!("unexpected argument `{extra}`"))),
        };
        // The command is the rest of the original text after the verb, verbatim.
        let command = |verb: &str| -> Result<String, ActionParseError> {
            let rest = text.trim().get(verb.len()..).unwrap_or("").trim();
            if rest.is_empty() {
                Err(bad(format!("`{verb}` needs a command")))
            } else {
                Ok(rest.to_string())
            }
        };

        let Some(verb) = words.first() else {
            return Err(bad("empty action".into()));
        };
        let verb = verb.to_ascii_lowercase();
        if let Some((action, ..)) = NULLARY.iter().find(|(_, word, _)| *word == verb) {
            return nullary(action.clone());
        }
        match verb.as_str() {
            "set-layout" => match words.get(1) {
                Some(name) => unary(Self::SetLayout((*name).to_string())),
                None => Err(bad("expected a layout name".into())),
            },
            "run" => command("run").map(Self::Run),
            "run-in-terminal" => command("run-in-terminal").map(Self::RunInTerminal),
            // `run-on <workspace> <command…>`: one workspace word, then the verbatim command.
            "run-on" => {
                let rest = command("run-on")?;
                match rest.split_once(char::is_whitespace) {
                    Some((workspace, cmd)) if !cmd.trim().is_empty() => Ok(Self::RunOn {
                        workspace: workspace.to_string(),
                        command: cmd.trim().to_string(),
                    }),
                    _ => Err(bad("`run-on` needs a workspace and a command".into())),
                }
            }
            "stack-window" => Err(bad(
                "`stack-window` was retired: every launch already stacks into the focused pane; use `run-terminal`".into(),
            )),
            "focus" => unary(Self::Focus(direction(words.get(1))?)),
            "move" => unary(Self::Move(direction(words.get(1))?)),
            "split" => unary(Self::Split(direction(words.get(1))?)),
            "resize" => unary(Self::Resize(direction(words.get(1))?)),
            "cycle-stack" => unary(Self::CycleStack(delta(words.get(1))?)),
            "cycle-workspace" => unary(Self::CycleWorkspace(delta(words.get(1))?)),
            "reorder-workspace" => unary(Self::ReorderWorkspace(delta(words.get(1))?)),
            "move-window-to-workspace" => unary(Self::MoveWindowToWorkspace(delta(words.get(1))?)),
            "move-workspace-to-output" => unary(Self::MoveWorkspaceToOutput(delta(words.get(1))?)),
            "slot" => unary(Self::Slot(slot_index(words.get(1))?)),
            "pin-slot" => unary(Self::PinSlot(slot_index(words.get(1))?)),
            other => Err(bad(format!("unknown action `{other}`"))),
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

    /// Compile-time completeness guard (no wildcard): a new [`Action`] variant fails to compile
    /// here until it is classified as an argument verb or a `NULLARY` one.
    fn is_argument_verb(action: &Action) -> bool {
        match action {
            Action::SetLayout(_)
            | Action::Run(_)
            | Action::RunInTerminal(_)
            | Action::RunOn { .. }
            | Action::Focus(_)
            | Action::Move(_)
            | Action::Split(_)
            | Action::Resize(_)
            | Action::CycleStack(_)
            | Action::CycleWorkspace(_)
            | Action::ReorderWorkspace(_)
            | Action::MoveWindowToWorkspace(_)
            | Action::Slot(_)
            | Action::PinSlot(_)
            | Action::MoveWorkspaceToOutput(_)
            | Action::VtSwitch(_) => true,
            Action::Quit
            | Action::ClosePane
            | Action::CloseWindow
            | Action::RunTerminal
            | Action::NewWorkspace
            | Action::MarkWindow
            | Action::SwapMarkedWindow
            | Action::CaptureWorkspaceRoot
            | Action::SetWorkspaceRoot
            | Action::CloseWorkspace
            | Action::DeleteWorkspace
            | Action::CloseAllWorkspaces
            | Action::OpenPicker
            | Action::OpenLauncher
            | Action::OpenSettings
            | Action::ToggleHeader
            | Action::ToggleFullscreen
            | Action::RenameWorkspace
            | Action::Lock
            | Action::Screenshot
            | Action::Snip => false,
        }
    }

    /// Every `NULLARY` verb plus one sample per argument verb (directions/deltas varied).
    fn every_variant() -> Vec<Action> {
        let mut all: Vec<Action> = NULLARY.iter().map(|(action, ..)| action.clone()).collect();
        all.extend([
            Action::SetLayout("es".into()),
            Action::Run("echo 50%:a b".into()),
            Action::RunInTerminal("htop -d 5".into()),
            Action::RunOn {
                workspace: "scratch-a-wp-4".into(),
                command: "echo 50%:a  b".into(),
            },
            Action::Focus(Direction::Left),
            Action::Move(Direction::Right),
            Action::Split(Direction::Up),
            Action::Resize(Direction::Down),
            Action::CycleStack(-1),
            Action::CycleWorkspace(1),
            Action::ReorderWorkspace(-1),
            Action::MoveWindowToWorkspace(2),
            Action::Slot(3),
            Action::PinSlot(0),
            Action::MoveWorkspaceToOutput(-2),
            Action::VtSwitch(3),
        ]);
        all
    }

    #[test]
    fn nullary_table_and_argument_verbs_partition_every_variant() {
        for action in every_variant() {
            assert_eq!(
                !is_argument_verb(&action),
                NULLARY.iter().any(|(n, ..)| *n == action),
                "{action}"
            );
        }
    }

    #[test]
    fn every_bindable_variant_round_trips_through_display_and_from_str() {
        for action in every_variant() {
            if matches!(action, Action::VtSwitch(_)) {
                continue;
            }
            assert_eq!(
                action.to_string().parse::<Action>(),
                Ok(action.clone()),
                "{action}"
            );
        }
    }

    #[test]
    fn vt_switch_displays_but_does_not_parse() {
        assert_eq!(Action::VtSwitch(3).to_string(), "vt-switch 3");
        assert_eq!(
            "vt-switch 3".parse::<Action>(),
            Err(ActionParseError("unknown action `vt-switch`".into()))
        );
    }

    #[test]
    fn areas_cover_one_representative_per_area_in_a_stable_order() {
        let reps = [
            (Action::Split(Direction::Right), Area::PanesAndWindows),
            (Action::CycleStack(1), Area::Stacks),
            (Action::DeleteWorkspace, Area::Workspaces),
            (Action::ReorderWorkspace(1), Area::Workspaces),
            (Action::MoveWorkspaceToOutput(1), Area::Outputs),
            (Action::OpenSettings, Area::LaunchAndOverlays),
            (Action::Lock, Area::SessionAndSystem),
        ];
        for (action, area) in reps {
            assert_eq!(action.area(), area, "{action}");
        }
        assert!(Area::ALL.windows(2).all(|w| w[0] < w[1]));
        let labels: Vec<_> = Area::ALL.iter().map(|a| a.label()).collect();
        assert_eq!(
            labels,
            [
                "Panes & windows",
                "Stacks",
                "Workspaces",
                "Outputs",
                "Launch & overlays",
                "Session & system"
            ]
        );
        for action in every_variant() {
            assert!(Area::ALL.contains(&action.area()), "{action}");
        }
    }

    #[test]
    fn only_vt_switch_is_allowed_while_locked() {
        for action in every_variant() {
            assert_eq!(
                action.allowed_while_locked(),
                matches!(action, Action::VtSwitch(_)),
                "{action}"
            );
        }
    }

    #[test]
    fn only_snip_and_vt_switch_are_allowed_while_snipping() {
        for action in every_variant() {
            assert_eq!(
                action.allowed_while_snipping(),
                matches!(action, Action::Snip | Action::VtSwitch(_)),
                "{action}"
            );
        }
    }

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

    #[test]
    fn run_on_takes_one_workspace_word_then_the_verbatim_command() {
        assert_eq!(
            "run-on scratch  foot -e  sh".parse::<Action>(),
            Ok(Action::RunOn {
                workspace: "scratch".into(),
                command: "foot -e  sh".into(),
            })
        );
        assert_eq!(
            Action::RunOn {
                workspace: "s".into(),
                command: "foot".into()
            }
            .area(),
            Area::LaunchAndOverlays
        );
        assert!("run-on".parse::<Action>().is_err());
        assert!("run-on scratch".parse::<Action>().is_err());
        assert!("run-on scratch   ".parse::<Action>().is_err());
    }
}
