//! Input handling: pointer, keyboard, and the global Super (Mod4) leader.
//!
//! WS-4 funnels the nested `winit` backend's [`InputEvent`]s into the seat so the compositor becomes
//! interactive:
//!
//! - **Keyboard** ([`CrucibleState::process_keyboard_event`]): every key is run through the seat's
//!   [`keyboard.input`](smithay::input::keyboard::KeyboardHandle::input) filter closure. The closure
//!   computes [`process_keyboard_shortcut`]; a matching `Super`-chord is **intercepted**
//!   ([`FilterResult::Intercept`]) and never reaches the client, while every other key is
//!   **forwarded** ([`FilterResult::Forward`]) to the focused client. The **suppressed-keys**
//!   mechanism records an intercepted *press* keysym and swallows its matching *release* so clients
//!   never see a dangling key-up.
//! - **Pointer** ([`CrucibleState::process_pointer_motion_absolute`] / `..._button` / `..._axis`):
//!   absolute winit motion is mapped into the output, the pointer location and pointer-focus
//!   (surface-under from the [`Space`](smithay::desktop::Space)) are updated so clients get
//!   enter/leave/motion, and buttons/axis frames are delivered. A pointer press also moves keyboard
//!   focus to the window under the cursor.
//!
//! ## Pure, testable shortcut mapping
//! [`process_keyboard_shortcut`] is a pure function of (`modifiers`, `keysym`) → `Option<KeyAction>`
//! with no compositor state, so the binding logic is unit-tested without a running compositor (see
//! the tests at the bottom of this module).
//!
//! The binding defaults are structured as a table inside `process_keyboard_shortcut` so that Phase 3
//! can replace the lookup with a config-driven override. The defaults live here; Phase 3 sources them
//! from `crucible-config`.
//!
//! ## Default binding table (WS-4)
//!
//! | Chord                 | `KeyAction`              | Model op                              |
//! |-----------------------|--------------------------|---------------------------------------|
//! | `Super+Return`        | `Run(terminal)`          | spawn terminal (fills focused pane)   |
//! | `Super+Q`             | `Quit`                   | stop compositor                       |
//! | `Super+H/J/K/L`       | `Focus{Left,Down,Up,Right}` | `World::focus(dir)`              |
//! | `Super+Shift+H/J/K/L` | `Move{Left,Down,Up,Right}`  | `World::move_(dir)`              |
//! | `Super+Backslash`     | `SplitRight`             | `World::split(Right)`                 |
//! | `Super+Minus`         | `SplitDown`              | `World::split(Down)`                  |
//! | `Super+Shift+Return`  | `StackWindow`            | spawn terminal (stacks in pane)       |
//! | `Super+Tab`           | `CycleStackNext`         | `World::cycle_stack(1)`               |
//! | `Super+Shift+Tab`     | `CycleStackPrev`         | `World::cycle_stack(-1)`              |
//! | `Super+Ctrl+H/J/K/L`  | `Resize{Left,Down,Up,Right}` | `World::resize(dir, 0.05)`       |
//! | `Super+]`             | `NextWorkspace`          | `World::cycle_workspace(1)`           |
//! | `Super+[`             | `PrevWorkspace`          | `World::cycle_workspace(-1)`          |
//! | `Super+N`             | `NewWorkspace`           | `World::new_workspace(auto_name)`     |
//! | `Super+Shift+]`       | `MoveWindowToNextWorkspace` | `World::move_window_to_workspace` |
//! | `Super+Shift+[`       | `MoveWindowToPrevWorkspace` | `World::move_window_to_workspace` |
//!
//! `rename-workspace` and the fuzzy workspace picker are **Phase 3** (`// Phase 3: picker/prompt`).
//!
//! ## Scope (WS-4)
//! This is the **winit** event path only — `udev`/libinput is WS-6. Focus targets stay bare
//! [`WlSurface`] (Wayland-only); the `KeyboardFocusTarget`/`PointerFocusTarget` enums that unify
//! layer/popup/X11 focus are WS-5 work (see [`crate::focus`]).

use std::process::Command;
use std::sync::atomic::Ordering;

use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, Event, InputBackend, InputEvent, KeyState,
        KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    },
    desktop::WindowSurfaceType,
    input::{
        keyboard::{FilterResult, Keysym, ModifiersState},
        pointer::{AxisFrame, ButtonEvent, MotionEvent, RelativeMotionEvent},
    },
    reexports::wayland_server::protocol::wl_pointer,
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Serial},
};
use tracing::{debug, error, info, warn};

use crate::{
    element::WindowElement,
    focus::{KeyboardFocusTarget, PointerFocusTarget},
    state::{Backend, CrucibleState},
    winit::WinitData,
};

/// Default terminal command used when `$TERMINAL` is unset, tried in order.
///
/// CDE targets Wayland-native terminals; the first one found on `$PATH` at spawn time is used. The
/// child is launched with `WAYLAND_DISPLAY` pointed at CDE's own socket so it connects back to us.
const DEFAULT_TERMINALS: &[&str] = &["foot", "alacritty", "kitty", "weston-terminal"];

/// A compositor action triggered by an intercepted `Super`-leader chord.
///
/// Produced by [`process_keyboard_shortcut`] and consumed by [`CrucibleState::handle_key_action`].
/// Each variant maps 1:1 to a CDE verb; the dispatch in `handle_key_action` calls the
/// corresponding [`crucible_layout::World`] op through the state's single mutation entry point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyAction {
    /// Quit the compositor cleanly (stop the main loop). `Super+Q`.
    Quit,
    /// Spawn a program. `Super+Return` spawns the terminal; it connects via the Wayland socket
    /// and the shell map path (`new_toplevel`) places it in the focused pane automatically.
    Run(String),

    // ---- directional focus (`Super+H/J/K/L`) -----------------------------------------------
    /// Move pane focus left within the active workspace.
    FocusLeft,
    /// Move pane focus down within the active workspace.
    FocusDown,
    /// Move pane focus up within the active workspace.
    FocusUp,
    /// Move pane focus right within the active workspace.
    FocusRight,

    // ---- pane move (`Super+Shift+H/J/K/L`) -------------------------------------------------
    /// Swap the focused pane with its left neighbour (focus follows).
    MoveLeft,
    /// Swap the focused pane with its down neighbour (focus follows).
    MoveDown,
    /// Swap the focused pane with its up neighbour (focus follows).
    MoveUp,
    /// Swap the focused pane with its right neighbour (focus follows).
    MoveRight,

    // ---- split (`Super+Backslash`, `Super+Minus`) ------------------------------------------
    /// Split the focused pane rightward; the new empty pane becomes focused.
    ///
    /// The user then spawns a terminal (`Super+Return`) which maps into the now-focused pane.
    SplitRight,
    /// Split the focused pane downward; the new empty pane becomes focused.
    SplitDown,

    // ---- stack (`Super+Shift+Return`, `Super+Tab`, `Super+Shift+Tab`) ---------------------
    /// Stack a new terminal window into the focused pane by spawning it.
    ///
    /// Because `World::open_window` is only ever called from the surface-map path (when a client
    /// connects and maps a toplevel), keybinds must not fabricate a surface-less `WindowId`.
    /// Instead, this verb spawns the terminal; the shell map path pushes the new window onto the
    /// focused pane's stack. Plain `Super+Return` does the same thing — they are identical at the
    /// model level; the separate verb exists for discoverability and for Phase 3 to bind
    /// differently if desired.
    StackWindow,
    /// Cycle the focused pane's window stack forward by one (next window).
    CycleStackNext,
    /// Cycle the focused pane's window stack backward by one (previous window).
    CycleStackPrev,

    // ---- resize (`Super+Ctrl+H/J/K/L`) ----------------------------------------------------
    /// Shrink the focused pane leftward (shift the left split edge outward).
    ResizeLeft,
    /// Shrink the focused pane downward (shift the bottom split edge inward).
    ResizeDown,
    /// Shrink the focused pane upward (shift the top split edge outward).
    ResizeUp,
    /// Expand the focused pane rightward (shift the right split edge outward).
    ResizeRight,

    // ---- workspace (`Super+]`, `Super+[`, `Super+N`, `Super+Shift+]`, `Super+Shift+[`) ----
    /// Switch to the next workspace on the focused output (wraps around).
    NextWorkspace,
    /// Switch to the previous workspace on the focused output (wraps around).
    PrevWorkspace,
    /// Create a new workspace on the focused output with an auto-generated name and switch to it.
    ///
    /// The fuzzy picker / interactive naming is **Phase 3** (`// Phase 3: picker/prompt`).
    NewWorkspace,
    /// Move the focused window to the next workspace on the focused output (wraps around).
    ///
    /// No-op when there is no focused window or only one workspace exists.
    MoveWindowToNextWorkspace,
    /// Move the focused window to the previous workspace on the focused output (wraps around).
    ///
    /// No-op when there is no focused window or only one workspace exists.
    MoveWindowToPrevWorkspace,

    // ---- VT switch (udev/DRM only) ---------------------------------------------------------
    /// Switch to virtual terminal `n` (only meaningful on the udev/DRM backend).
    ///
    /// Produced when libinput delivers a `Ctrl+Alt+F<n>` chord (the `XF86Switch_VT_<n>` keysyms).
    /// The nested `winit` backend never sees these, so this arm is a no-op there.
    VtSwitch(i32),
}

/// Maps a modifier+keysym combination to a [`KeyAction`], or `None` to forward the key.
///
/// **Pure** (no compositor state) so the `Super`-leader binding table is unit-testable without a
/// running compositor. Only `Super` (`modifiers.logo`, Mod4) chords are bound; every other key
/// returns `None` and is forwarded to the focused client.
///
/// The bindings are structured as a readable `match` table (modifier pattern × keysym → action)
/// so that **Phase 3** can replace the dispatch with a config-driven lookup without touching
/// `handle_key_action`. The defaults live here; Phase 3 sources them from `crucible-config`.
///
/// See the module-level binding table for the full chord → verb mapping.
pub fn process_keyboard_shortcut(modifiers: ModifiersState, keysym: Keysym) -> Option<KeyAction> {
    // VT switching (`Ctrl+Alt+F<n>`) takes priority over the Super leader and is independent of it.
    // libinput maps `Ctrl+Alt+F<n>` to the `XF86Switch_VT_<n>` keysyms; map those to a VT switch.
    if (Keysym::XF86_Switch_VT_1.raw()..=Keysym::XF86_Switch_VT_12.raw()).contains(&keysym.raw()) {
        let vt = (keysym.raw() - Keysym::XF86_Switch_VT_1.raw() + 1) as i32;
        return Some(KeyAction::VtSwitch(vt));
    }

    // Only the Super (logo / Mod4) leader is bound; everything else forwards to the client.
    if !modifiers.logo {
        return None;
    }

    // Decompose modifier flags once for the table below.
    let shift = modifiers.shift;
    let ctrl = modifiers.ctrl;

    // --- binding table (Phase 3: replace this match with a config-driven lookup) ---
    //
    // Ordering: plain Super first, then Super+Shift, then Super+Ctrl. Within each group,
    // keysyms are listed in the order they appear in the module-level doc table.
    match (shift, ctrl, keysym) {
        // ---- plain Super ---------------------------------------------------------------
        (false, false, Keysym::Return) => Some(KeyAction::Run(terminal_command())),
        // Both lower- and upper-case map to Quit so the bind works regardless of Caps Lock.
        (false, false, Keysym::q | Keysym::Q) => Some(KeyAction::Quit),

        // focus: Super+H/J/K/L
        (false, false, Keysym::h) => Some(KeyAction::FocusLeft),
        (false, false, Keysym::j) => Some(KeyAction::FocusDown),
        (false, false, Keysym::k) => Some(KeyAction::FocusUp),
        (false, false, Keysym::l) => Some(KeyAction::FocusRight),

        // split: Super+Backslash (right) / Super+Minus (down)
        (false, false, Keysym::backslash) => Some(KeyAction::SplitRight),
        (false, false, Keysym::minus) => Some(KeyAction::SplitDown),

        // cycle-stack: Super+Tab (next) — Super+Shift+Tab (prev) handled below
        (false, false, Keysym::Tab) => Some(KeyAction::CycleStackNext),

        // workspace navigation: Super+] next, Super+[ prev, Super+N new
        (false, false, Keysym::bracketright) => Some(KeyAction::NextWorkspace),
        (false, false, Keysym::bracketleft) => Some(KeyAction::PrevWorkspace),
        (false, false, Keysym::n | Keysym::N) => Some(KeyAction::NewWorkspace),

        // ---- Super+Shift -----------------------------------------------------------
        // spawn into current pane (stack-window): Super+Shift+Return
        (true, false, Keysym::Return) => Some(KeyAction::StackWindow),

        // move pane: Super+Shift+H/J/K/L
        (true, false, Keysym::h | Keysym::H) => Some(KeyAction::MoveLeft),
        (true, false, Keysym::j | Keysym::J) => Some(KeyAction::MoveDown),
        (true, false, Keysym::k | Keysym::K) => Some(KeyAction::MoveUp),
        (true, false, Keysym::l | Keysym::L) => Some(KeyAction::MoveRight),

        // cycle-stack prev: Super+Shift+Tab
        (true, false, Keysym::Tab | Keysym::ISO_Left_Tab) => Some(KeyAction::CycleStackPrev),

        // move window to workspace: Super+Shift+] next, Super+Shift+[ prev
        (true, false, Keysym::bracketright) => Some(KeyAction::MoveWindowToNextWorkspace),
        (true, false, Keysym::bracketleft) => Some(KeyAction::MoveWindowToPrevWorkspace),

        // ---- Super+Ctrl ------------------------------------------------------------
        // resize: Super+Ctrl+H/J/K/L
        (false, true, Keysym::h | Keysym::H) => Some(KeyAction::ResizeLeft),
        (false, true, Keysym::j | Keysym::J) => Some(KeyAction::ResizeDown),
        (false, true, Keysym::k | Keysym::K) => Some(KeyAction::ResizeUp),
        (false, true, Keysym::l | Keysym::L) => Some(KeyAction::ResizeRight),

        // ---- unmatched Super chord → forward to client -----------------------------
        _ => None,
    }
}

/// Resolves the terminal command: `$TERMINAL` if set and non-empty, else the first default.
///
/// Pure aside from reading the environment; the actual lookup against `$PATH` happens when the child
/// is spawned in [`CrucibleState::spawn_terminal`].
fn terminal_command() -> String {
    match std::env::var("TERMINAL") {
        Ok(term) if !term.trim().is_empty() => term,
        _ => DEFAULT_TERMINALS[0].to_string(),
    }
}

impl CrucibleState<WinitData> {
    /// Routes a single winit [`InputEvent`] into the seat.
    ///
    /// Handles the events the nested winit backend emits: keyboard keys (through the Super-leader
    /// filter), absolute pointer motion, pointer buttons, and scroll axes. Other events (touch,
    /// gestures, relative motion) are not produced by the winit backend and are ignored here; they
    /// arrive with the udev/libinput path (see [`CrucibleState::process_input_event_libinput`]).
    pub fn process_input_event(&mut self, event: InputEvent<smithay::backend::winit::WinitInput>) {
        type B = smithay::backend::winit::WinitInput;
        match event {
            InputEvent::Keyboard { event } => self.process_keyboard_event::<B>(event),
            InputEvent::PointerMotionAbsolute { event } => {
                self.process_pointer_motion_absolute::<B>(event)
            }
            InputEvent::PointerButton { event } => self.process_pointer_button::<B>(event),
            InputEvent::PointerAxis { event } => self.process_pointer_axis::<B>(event),
            // Winit does not emit these; the udev/libinput backend handles them.
            _ => {}
        }
    }
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Routes a single libinput [`InputEvent`] into the seat (the udev/DRM backend path).
    ///
    /// Generic over the [`InputBackend`] so it works with `LibinputInputBackend`; reuses the **same**
    /// per-event seat handlers the winit path uses ([`Self::process_keyboard_event`],
    /// [`Self::process_pointer_button`], [`Self::process_pointer_axis`]). The difference from the
    /// winit path is the pointer-motion model: real input devices deliver **relative** motion
    /// ([`InputEvent::PointerMotion`]) rather than the absolute motion winit emits, so this routes
    /// motion through [`Self::process_pointer_motion_relative`]. Device add/remove are surfaced to
    /// the caller (which configures libinput / tracks keyboards for LED sync) and otherwise ignored.
    pub fn process_input_event_libinput<I: InputBackend>(&mut self, event: InputEvent<I>) {
        match event {
            InputEvent::Keyboard { event } => self.process_keyboard_event::<I>(event),
            InputEvent::PointerMotion { event } => self.process_pointer_motion_relative::<I>(event),
            InputEvent::PointerButton { event } => self.process_pointer_button::<I>(event),
            InputEvent::PointerAxis { event } => self.process_pointer_axis::<I>(event),
            // Touch/gesture/device events are not handled yet; the udev source inspects
            // DeviceAdded/DeviceRemoved itself for keyboard-LED bookkeeping before calling us.
            _ => {}
        }
    }

    /// Runs a keyboard key through the seat, intercepting `Super`-leader chords.
    ///
    /// The [`keyboard.input`](smithay::input::keyboard::KeyboardHandle::input) filter closure:
    /// - on a **press**, computes [`process_keyboard_shortcut`]; a match is recorded in
    ///   `suppressed_keys` and intercepted, otherwise the key is forwarded;
    /// - on a **release**, if the keysym is suppressed it is removed and intercepted (so the client
    ///   never sees a dangling key-up), otherwise forwarded.
    ///
    /// Any intercepted [`KeyAction`] is dispatched via [`Self::handle_key_action`].
    fn process_keyboard_event<I: InputBackend>(&mut self, evt: I::KeyboardKeyEvent) {
        let keycode = evt.key_code();
        let state = evt.state();
        let serial = SERIAL_COUNTER.next_serial();
        let time = Event::time_msec(&evt);

        // Cloned out so the filter closure does not borrow `self`; written back afterwards.
        let mut suppressed_keys = self.suppressed_keys.clone();
        let keyboard = self.seat.get_keyboard().unwrap();

        let action = keyboard.input(
            self,
            keycode,
            state,
            serial,
            time,
            |_state, modifiers, handle| {
                let keysym = handle.modified_sym();
                debug!(?state, mods = ?modifiers, keysym = keysym.raw(), "key");

                if let KeyState::Pressed = state {
                    // A press that matches a Super-chord is intercepted; record it so we can also
                    // swallow its release and never leak a key-up to the client.
                    let action = process_keyboard_shortcut(*modifiers, keysym);
                    if action.is_some() {
                        suppressed_keys.push(keysym);
                    }
                    // The filter's `Intercept` payload type is `Option<KeyAction>` (a release
                    // intercepts with `None`); wrap the matched action accordingly.
                    match action {
                        Some(action) => FilterResult::Intercept(Some(action)),
                        None => FilterResult::Forward,
                    }
                } else {
                    // Release: swallow it iff its press was intercepted.
                    if suppressed_keys.contains(&keysym) {
                        suppressed_keys.retain(|k| *k != keysym);
                        FilterResult::Intercept(None)
                    } else {
                        FilterResult::Forward
                    }
                }
            },
        );

        self.suppressed_keys = suppressed_keys;

        // `input` returns `Option<FilterResult::Intercept(_)>`; the outer `Option` is `None` when
        // the key was forwarded. A release carries `Some(None)` (intercepted, no action).
        if let Some(Some(action)) = action {
            self.handle_key_action(action);
        }
    }

    /// Dispatches an intercepted [`KeyAction`] to the appropriate model op + apply step.
    ///
    /// Every op goes through the single mutation entry point: infallible verbs via `update_model`,
    /// fallible ones via `with_model`, which applies the re-derived projection only on success and
    /// logs expected no-op errors (e.g. `SameWorkspace`) at debug level so they don't surface as
    /// noise in normal use.
    fn handle_key_action(&mut self, action: KeyAction) {
        use crucible_layout::Direction;

        /// Amount by which a single resize keystroke adjusts the split ratio.
        const RESIZE_DELTA: f64 = 0.05;

        match action {
            // ---- compositor-level actions -----------------------------------------------
            KeyAction::Quit => {
                info!("Super+Q: quitting");
                self.running.store(false, Ordering::SeqCst);
            }
            KeyAction::Run(cmd) => {
                info!(cmd, "Super+Return: spawning terminal into focused pane");
                self.spawn_terminal(&cmd);
            }
            KeyAction::VtSwitch(vt) => {
                // Only the udev/DRM backend owns a session; on winit this is a no-op (the backend's
                // default `change_vt` returns `false`).
                info!(to = vt, "VT switch requested");
                if !self.backend_data.change_vt(vt) {
                    warn!(to = vt, "Backend does not support VT switching");
                }
            }

            // ---- directional focus ------------------------------------------------------
            KeyAction::FocusLeft => {
                self.update_model(|w| w.focus(Direction::Left));
            }
            KeyAction::FocusDown => {
                self.update_model(|w| w.focus(Direction::Down));
            }
            KeyAction::FocusUp => {
                self.update_model(|w| w.focus(Direction::Up));
            }
            KeyAction::FocusRight => {
                self.update_model(|w| w.focus(Direction::Right));
            }

            // ---- pane move --------------------------------------------------------------
            KeyAction::MoveLeft => {
                self.update_model(|w| w.move_(Direction::Left));
            }
            KeyAction::MoveDown => {
                self.update_model(|w| w.move_(Direction::Down));
            }
            KeyAction::MoveUp => {
                self.update_model(|w| w.move_(Direction::Up));
            }
            KeyAction::MoveRight => {
                self.update_model(|w| w.move_(Direction::Right));
            }

            // ---- split ------------------------------------------------------------------
            KeyAction::SplitRight => {
                self.update_model(|w| {
                    w.split(Direction::Right);
                });
            }
            KeyAction::SplitDown => {
                self.update_model(|w| {
                    w.split(Direction::Down);
                });
            }

            // ---- stack-window -----------------------------------------------------------
            // Spawning a terminal causes the client to connect, map a toplevel, and flow through
            // `new_toplevel` → `World::open_window`, which pushes the window onto the focused
            // pane's stack. We must not call `World::open_window` or `World::stack_window`
            // directly from a keybind: doing so allocates a `WindowId` with no backing surface
            // and corrupts the `by_id` map. The spawn IS the stack — it stacks into the focused
            // pane automatically.
            KeyAction::StackWindow => {
                let cmd = terminal_command();
                info!(
                    cmd,
                    "Super+Shift+Return: spawning terminal to stack into focused pane"
                );
                self.spawn_terminal(&cmd);
            }

            // ---- cycle stack ------------------------------------------------------------
            KeyAction::CycleStackNext => {
                self.update_model(|w| w.cycle_stack(1));
            }
            KeyAction::CycleStackPrev => {
                self.update_model(|w| w.cycle_stack(-1));
            }

            // ---- resize -----------------------------------------------------------------
            KeyAction::ResizeLeft => {
                self.update_model(|w| w.resize(Direction::Left, RESIZE_DELTA));
            }
            KeyAction::ResizeDown => {
                self.update_model(|w| w.resize(Direction::Down, RESIZE_DELTA));
            }
            KeyAction::ResizeUp => {
                self.update_model(|w| w.resize(Direction::Up, RESIZE_DELTA));
            }
            KeyAction::ResizeRight => {
                self.update_model(|w| w.resize(Direction::Right, RESIZE_DELTA));
            }

            // ---- workspace navigation ---------------------------------------------------
            KeyAction::NextWorkspace => {
                self.update_model(|w| w.cycle_workspace(1));
            }
            KeyAction::PrevWorkspace => {
                self.update_model(|w| w.cycle_workspace(-1));
            }
            KeyAction::NewWorkspace => {
                // Auto-generate a workspace name from the total number of workspaces on the
                // focused output + 1, so each new workspace gets a unique numeric label
                // (e.g. "2", "3", …). Phase 3 replaces this with an interactive naming prompt
                // via the fuzzy picker.
                // Phase 3: picker/prompt — replace auto-name with user-supplied name from picker.
                let name = {
                    let oid = self.layout.world.focused_output();
                    let n = self
                        .layout
                        .world
                        .output(oid)
                        .map_or(1, |o| o.workspaces.len() + 1);
                    n.to_string()
                };
                info!(name, "Super+N: creating new workspace");
                self.update_model(|w| {
                    w.new_workspace(name);
                });
            }

            // ---- rename-workspace -------------------------------------------------------
            // Phase 3: picker/prompt — wire to an interactive rename prompt. For now this verb
            // has no default binding; the action variant exists so Phase 3 can add a binding
            // without touching the dispatch arm.

            // ---- move window to workspace -----------------------------------------------
            KeyAction::MoveWindowToNextWorkspace => {
                self.move_focused_window_to_adjacent_workspace(1);
            }
            KeyAction::MoveWindowToPrevWorkspace => {
                self.move_focused_window_to_adjacent_workspace(-1);
            }
        }
    }

    /// Moves the currently focused window to the workspace `delta` steps from the active one.
    ///
    /// `delta = 1` → next workspace; `delta = -1` → previous workspace (both wrap). No-op when
    /// there is no focused window, only one workspace exists, or the model rejects the move.
    fn move_focused_window_to_adjacent_workspace(&mut self, delta: i64) {
        use crucible_layout::LayoutError;

        let Some(window_id) = self.layout.world.focused_window() else {
            debug!("move-window-to-workspace: no focused window; ignoring");
            return;
        };

        // Determine the target workspace by cycling the active output's workspace stack.
        let output_id = self.layout.world.focused_output();
        let Some(output) = self.layout.world.output(output_id) else {
            return;
        };
        let ws_count = output.workspaces.len();
        if ws_count < 2 {
            debug!("move-window-to-workspace: only one workspace; ignoring");
            return;
        }
        let active_idx = output.active;
        // Wrap-around index arithmetic: same formula as `cycle_index` in `crucible-layout`.
        let target_idx = ((active_idx as i64 + delta).rem_euclid(ws_count as i64)) as usize;
        let target_ws = output.workspaces[target_idx];

        match self.with_model(|w| w.move_window_to_workspace(window_id, target_ws)) {
            Ok(()) => {}
            Err(LayoutError::SameWorkspace) => {
                debug!("move-window-to-workspace: already on target workspace");
            }
            Err(err) => {
                warn!(%err, "move-window-to-workspace: unexpected error");
            }
        }
    }

    /// Spawns `cmd` as a child connected to CDE's Wayland socket.
    ///
    /// The child inherits the environment but with `WAYLAND_DISPLAY` overridden to CDE's socket
    /// name so it connects to *this* compositor rather than the host session.
    fn spawn_terminal(&self, cmd: &str) {
        let Some(socket) = self.socket_name.clone() else {
            warn!(cmd, "No wayland socket; cannot spawn terminal");
            return;
        };
        info!(cmd, socket, "Super+Return: spawning terminal");
        match Command::new(cmd).env("WAYLAND_DISPLAY", &socket).spawn() {
            Ok(_child) => {}
            Err(err) => error!(cmd, %err, "Failed to spawn terminal"),
        }
    }

    /// Raises `window` in the [`Space`](smithay::desktop::Space) and gives it keyboard focus.
    ///
    /// For an XWayland window this both activates it (the seat drives the X11 `set_input_focus`
    /// through the [`KeyboardFocusTarget::Window`] target) and raises it in the X11 stacking order
    /// via the running [`X11Wm`](smithay::xwayland::X11Wm).
    fn focus_window(&mut self, window: WindowElement) {
        self.space.raise_element(&window, true);
        if let Some(surface) = window.x11_surface() {
            if let Some(xwm) = self.xwm.as_mut() {
                let _ = xwm.raise_window(surface);
            }
        }
        self.set_keyboard_focus(Some(KeyboardFocusTarget::Window(window)));
    }

    /// Maps absolute winit pointer motion into the output and delivers it to the seat.
    ///
    /// Transforms the device position into the output's logical geometry, updates the pointer
    /// location and pointer focus (the surface under the cursor, from the [`Space`]), so the focused
    /// client receives enter/leave/motion. No-op if no output is mapped yet.
    fn process_pointer_motion_absolute<I: InputBackend>(
        &mut self,
        evt: I::PointerMotionAbsoluteEvent,
    ) {
        let Some(output) = self.space.outputs().next().cloned() else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };

        let pos = evt.position_transformed(output_geo.size) + output_geo.loc.to_f64();
        let serial = SERIAL_COUNTER.next_serial();
        let time = evt.time_msec();

        let under = self.surface_under(pos);
        let pointer = self.pointer.clone();
        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: pos,
                serial,
                time,
            },
        );
        pointer.frame(self);
    }

    /// Maps **relative** pointer motion (real input devices, via libinput) into the seat.
    ///
    /// Unlike winit's absolute motion, libinput delivers a motion *delta*. The new pointer location
    /// is the previous location plus the delta, clamped to the union of all mapped outputs so the
    /// cursor cannot leave the visible area (it can cross freely between outputs laid left-to-right).
    /// The seat receives both the relative-motion event (for clients that requested it via the
    /// relative-pointer protocol, gated by [`Backend::HAS_RELATIVE_MOTION`]) and the resulting
    /// absolute motion. No-op until at least one output is mapped.
    fn process_pointer_motion_relative<I: InputBackend>(&mut self, evt: I::PointerMotionEvent) {
        if self.space.outputs().next().is_none() {
            return;
        }

        let serial = SERIAL_COUNTER.next_serial();
        let pointer = self.pointer.clone();

        let mut location = pointer.current_location() + evt.delta();
        location = self.clamp_coords(location);

        let under = self.surface_under(location);
        pointer.relative_motion(
            self,
            under.clone(),
            &RelativeMotionEvent {
                delta: evt.delta(),
                delta_unaccel: evt.delta_unaccel(),
                utime: evt.time(),
            },
        );
        pointer.motion(
            self,
            under,
            &MotionEvent {
                location,
                serial,
                time: evt.time_msec(),
            },
        );
        pointer.frame(self);
    }

    /// Clamps a pointer location to the bounding box of all mapped outputs.
    ///
    /// With outputs laid out left-to-right (see the udev backend), the union is a single horizontal
    /// strip, so this keeps the cursor on-screen while letting it move between adjacent outputs.
    /// Returns `pos` unchanged when no output is mapped.
    fn clamp_coords(&self, pos: Point<f64, Logical>) -> Point<f64, Logical> {
        let mut bbox: Option<Rectangle<i32, Logical>> = None;
        for output in self.space.outputs() {
            if let Some(geo) = self.space.output_geometry(output) {
                bbox = Some(match bbox {
                    Some(acc) => acc.merge(geo),
                    None => geo,
                });
            }
        }
        let Some(bbox) = bbox else {
            return pos;
        };
        let bbox = bbox.to_f64();
        let x = pos.x.clamp(bbox.loc.x, bbox.loc.x + bbox.size.w - 1.0);
        let y = pos.y.clamp(bbox.loc.y, bbox.loc.y + bbox.size.h - 1.0);
        (x, y).into()
    }

    /// Delivers a pointer button to the seat, moving keyboard focus on press.
    fn process_pointer_button<I: InputBackend>(&mut self, evt: I::PointerButtonEvent) {
        let serial = SERIAL_COUNTER.next_serial();
        let button = evt.button_code();
        let state = wl_pointer::ButtonState::from(evt.state());

        if wl_pointer::ButtonState::Pressed == state {
            self.update_keyboard_focus_under_pointer(serial);
        }

        let pointer = self.pointer.clone();
        pointer.button(
            self,
            &ButtonEvent {
                button,
                state: state.try_into().unwrap(),
                serial,
                time: evt.time_msec(),
            },
        );
        pointer.frame(self);
    }

    /// Delivers a scroll axis frame to the seat.
    fn process_pointer_axis<I: InputBackend>(&mut self, evt: I::PointerAxisEvent) {
        let horizontal = evt
            .amount(Axis::Horizontal)
            .unwrap_or_else(|| evt.amount_v120(Axis::Horizontal).unwrap_or(0.0) * 15.0 / 120.0);
        let vertical = evt
            .amount(Axis::Vertical)
            .unwrap_or_else(|| evt.amount_v120(Axis::Vertical).unwrap_or(0.0) * 15.0 / 120.0);
        let horizontal_v120 = evt.amount_v120(Axis::Horizontal);
        let vertical_v120 = evt.amount_v120(Axis::Vertical);

        let mut frame = AxisFrame::new(evt.time_msec()).source(evt.source());
        if horizontal != 0.0 {
            frame = frame
                .relative_direction(Axis::Horizontal, evt.relative_direction(Axis::Horizontal));
            frame = frame.value(Axis::Horizontal, horizontal);
            if let Some(v120) = horizontal_v120 {
                frame = frame.v120(Axis::Horizontal, v120 as i32);
            }
        }
        if vertical != 0.0 {
            frame =
                frame.relative_direction(Axis::Vertical, evt.relative_direction(Axis::Vertical));
            frame = frame.value(Axis::Vertical, vertical);
            if let Some(v120) = vertical_v120 {
                frame = frame.v120(Axis::Vertical, v120 as i32);
            }
        }
        if evt.source() == AxisSource::Finger {
            if evt.amount(Axis::Horizontal) == Some(0.0) {
                frame = frame.stop(Axis::Horizontal);
            }
            if evt.amount(Axis::Vertical) == Some(0.0) {
                frame = frame.stop(Axis::Vertical);
            }
        }

        let pointer = self.pointer.clone();
        pointer.axis(self, frame);
        pointer.frame(self);
    }

    /// Returns the [`PointerFocusTarget`] (and its top-left in global logical coords) under `pos`,
    /// if any.
    ///
    /// Resolution walks the [`Space`]'s mapped windows and dispatches over the window kind so X11
    /// windows resolve to a [`PointerFocusTarget::X11Surface`] and Wayland windows to a
    /// [`PointerFocusTarget::WlSurface`] (see [`WindowElement::surface_under`]).
    fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(PointerFocusTarget, Point<f64, Logical>)> {
        self.space
            .element_under(pos)
            .and_then(|(window, location)| {
                window
                    .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                    .map(|(target, surface_loc)| (target, (location + surface_loc).to_f64()))
            })
    }

    /// Moves keyboard focus to the window under the current pointer location, if any.
    ///
    /// Used on pointer press (click-to-focus). Raises the window in the [`Space`] and assigns the
    /// seat's keyboard focus to it (activating X11 windows through the focus path).
    fn update_keyboard_focus_under_pointer(&mut self, _serial: Serial) {
        let location = self.pointer.current_location();
        // Route the model's focused output to the monitor under the pointer so new windows and the
        // (WS-4) workspace verbs act on the monitor the user is interacting with. Click is a clean,
        // unambiguous focus-change signal; pure pointer-enter routing can follow in WS-4 if desired.
        let output_under = self.space.output_under(location).next().cloned();
        if let Some(output) = output_under {
            self.focus_output(&output);
        }
        if let Some((window, _)) = self
            .space
            .element_under(location)
            .map(|(w, p)| (w.clone(), p))
        {
            self.focus_window(window);
        }
    }
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Returns the seat's current [`KeyboardFocusTarget`], if any.
    pub fn keyboard_focus(&self) -> Option<KeyboardFocusTarget> {
        self.seat.get_keyboard().and_then(|kbd| kbd.current_focus())
    }

    /// Sets (or clears) the seat's keyboard focus to `target`.
    ///
    /// Called from the shell map path (so a newly mapped toplevel/X11 window receives keys) and
    /// from the input path (click-to-focus / the stubbed focus cycle). A fresh serial is allocated
    /// for the focus change. When `target` is an XWayland window the seat's
    /// [`KeyboardTarget`](smithay::input::keyboard::KeyboardTarget) impl drives the X11
    /// `set_input_focus`/take-focus that activates it.
    pub fn set_keyboard_focus(&mut self, target: Option<KeyboardFocusTarget>) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        keyboard.set_focus(self, target, serial);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a [`ModifiersState`] with only `logo` (Super) set.
    fn super_only() -> ModifiersState {
        ModifiersState {
            logo: true,
            ..Default::default()
        }
    }

    /// Builds a [`ModifiersState`] with `logo` + `shift` (Super+Shift).
    fn super_shift() -> ModifiersState {
        ModifiersState {
            logo: true,
            shift: true,
            ..Default::default()
        }
    }

    /// Builds a [`ModifiersState`] with `logo` + `ctrl` (Super+Ctrl).
    fn super_ctrl() -> ModifiersState {
        ModifiersState {
            logo: true,
            ctrl: true,
            ..Default::default()
        }
    }

    // ---- existing bindings (preserved) -------------------------------------------------------

    #[test]
    fn super_return_runs_a_terminal() {
        let action = process_keyboard_shortcut(super_only(), Keysym::Return);
        assert!(matches!(action, Some(KeyAction::Run(_))));
    }

    #[test]
    fn super_q_quits() {
        assert_eq!(
            process_keyboard_shortcut(super_only(), Keysym::q),
            Some(KeyAction::Quit)
        );
        // Upper-case (Shift/Caps) variant also quits.
        assert_eq!(
            process_keyboard_shortcut(super_only(), Keysym::Q),
            Some(KeyAction::Quit)
        );
    }

    // ---- directional focus: Super+H/J/K/L ---------------------------------------------------

    #[test]
    fn super_hjkl_maps_to_directional_focus() {
        assert_eq!(
            process_keyboard_shortcut(super_only(), Keysym::h),
            Some(KeyAction::FocusLeft)
        );
        assert_eq!(
            process_keyboard_shortcut(super_only(), Keysym::j),
            Some(KeyAction::FocusDown)
        );
        assert_eq!(
            process_keyboard_shortcut(super_only(), Keysym::k),
            Some(KeyAction::FocusUp)
        );
        assert_eq!(
            process_keyboard_shortcut(super_only(), Keysym::l),
            Some(KeyAction::FocusRight)
        );
    }

    // ---- pane move: Super+Shift+H/J/K/L -----------------------------------------------------

    #[test]
    fn super_shift_hjkl_maps_to_move() {
        assert_eq!(
            process_keyboard_shortcut(super_shift(), Keysym::h),
            Some(KeyAction::MoveLeft)
        );
        assert_eq!(
            process_keyboard_shortcut(super_shift(), Keysym::j),
            Some(KeyAction::MoveDown)
        );
        assert_eq!(
            process_keyboard_shortcut(super_shift(), Keysym::k),
            Some(KeyAction::MoveUp)
        );
        assert_eq!(
            process_keyboard_shortcut(super_shift(), Keysym::l),
            Some(KeyAction::MoveRight)
        );
        // Upper-case variants (Shift already set, so Keysym may arrive capitalised).
        assert_eq!(
            process_keyboard_shortcut(super_shift(), Keysym::H),
            Some(KeyAction::MoveLeft)
        );
        assert_eq!(
            process_keyboard_shortcut(super_shift(), Keysym::L),
            Some(KeyAction::MoveRight)
        );
    }

    // ---- split: Super+Backslash / Super+Minus -----------------------------------------------

    #[test]
    fn super_backslash_splits_right() {
        assert_eq!(
            process_keyboard_shortcut(super_only(), Keysym::backslash),
            Some(KeyAction::SplitRight)
        );
    }

    #[test]
    fn super_minus_splits_down() {
        assert_eq!(
            process_keyboard_shortcut(super_only(), Keysym::minus),
            Some(KeyAction::SplitDown)
        );
    }

    // ---- stack-window: Super+Shift+Return ---------------------------------------------------

    #[test]
    fn super_shift_return_stacks_window() {
        assert_eq!(
            process_keyboard_shortcut(super_shift(), Keysym::Return),
            Some(KeyAction::StackWindow)
        );
    }

    // ---- cycle stack: Super+Tab / Super+Shift+Tab -------------------------------------------

    #[test]
    fn super_tab_cycles_stack_next() {
        assert_eq!(
            process_keyboard_shortcut(super_only(), Keysym::Tab),
            Some(KeyAction::CycleStackNext)
        );
    }

    #[test]
    fn super_shift_tab_cycles_stack_prev() {
        assert_eq!(
            process_keyboard_shortcut(super_shift(), Keysym::Tab),
            Some(KeyAction::CycleStackPrev)
        );
        // Some keyboard layouts deliver Super+Shift+Tab as ISO_Left_Tab.
        assert_eq!(
            process_keyboard_shortcut(super_shift(), Keysym::ISO_Left_Tab),
            Some(KeyAction::CycleStackPrev)
        );
    }

    // ---- resize: Super+Ctrl+H/J/K/L --------------------------------------------------------

    #[test]
    fn super_ctrl_hjkl_maps_to_resize() {
        assert_eq!(
            process_keyboard_shortcut(super_ctrl(), Keysym::h),
            Some(KeyAction::ResizeLeft)
        );
        assert_eq!(
            process_keyboard_shortcut(super_ctrl(), Keysym::j),
            Some(KeyAction::ResizeDown)
        );
        assert_eq!(
            process_keyboard_shortcut(super_ctrl(), Keysym::k),
            Some(KeyAction::ResizeUp)
        );
        assert_eq!(
            process_keyboard_shortcut(super_ctrl(), Keysym::l),
            Some(KeyAction::ResizeRight)
        );
        // Upper-case variants.
        assert_eq!(
            process_keyboard_shortcut(super_ctrl(), Keysym::H),
            Some(KeyAction::ResizeLeft)
        );
        assert_eq!(
            process_keyboard_shortcut(super_ctrl(), Keysym::L),
            Some(KeyAction::ResizeRight)
        );
    }

    // ---- workspace navigation: Super+] / Super+[ / Super+N ---------------------------------

    #[test]
    fn super_bracketright_next_workspace() {
        assert_eq!(
            process_keyboard_shortcut(super_only(), Keysym::bracketright),
            Some(KeyAction::NextWorkspace)
        );
    }

    #[test]
    fn super_bracketleft_prev_workspace() {
        assert_eq!(
            process_keyboard_shortcut(super_only(), Keysym::bracketleft),
            Some(KeyAction::PrevWorkspace)
        );
    }

    #[test]
    fn super_n_new_workspace() {
        assert_eq!(
            process_keyboard_shortcut(super_only(), Keysym::n),
            Some(KeyAction::NewWorkspace)
        );
        assert_eq!(
            process_keyboard_shortcut(super_only(), Keysym::N),
            Some(KeyAction::NewWorkspace)
        );
    }

    // ---- move window to workspace: Super+Shift+] / Super+Shift+[ ---------------------------

    #[test]
    fn super_shift_bracketright_move_window_next() {
        assert_eq!(
            process_keyboard_shortcut(super_shift(), Keysym::bracketright),
            Some(KeyAction::MoveWindowToNextWorkspace)
        );
    }

    #[test]
    fn super_shift_bracketleft_move_window_prev() {
        assert_eq!(
            process_keyboard_shortcut(super_shift(), Keysym::bracketleft),
            Some(KeyAction::MoveWindowToPrevWorkspace)
        );
    }

    // ---- forward / unmatched guards --------------------------------------------------------

    #[test]
    fn unmatched_super_chord_is_none() {
        // A Super chord with no binding is not intercepted (forwarded to the client).
        assert_eq!(process_keyboard_shortcut(super_only(), Keysym::z), None);
    }

    #[test]
    fn non_super_keys_are_forwarded() {
        // Without the logo modifier, even bound keysyms must forward to the focused client.
        let no_mods = ModifiersState::default();
        assert_eq!(process_keyboard_shortcut(no_mods, Keysym::Return), None);
        assert_eq!(process_keyboard_shortcut(no_mods, Keysym::q), None);
        assert_eq!(process_keyboard_shortcut(no_mods, Keysym::h), None);
        assert_eq!(process_keyboard_shortcut(no_mods, Keysym::backslash), None);
        assert_eq!(process_keyboard_shortcut(no_mods, Keysym::Tab), None);
        assert_eq!(
            process_keyboard_shortcut(no_mods, Keysym::bracketright),
            None
        );
    }

    // ---- terminal fallback -----------------------------------------------------------------

    #[test]
    fn terminal_command_falls_back_to_default_without_env() {
        // SAFETY: single-threaded test; we restore the prior value before returning.
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
}
