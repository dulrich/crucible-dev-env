//! The environment variables CDE exports to the processes it spawns (the spawn contract).

/// Environment variable carrying the IPC socket path to every child CDE spawns.
pub const SOCKET_ENV: &str = "CDE_SOCKET";

/// Environment variable carrying the terminal program to every child CDE spawns.
pub const TERMINAL_ENV: &str = "CDE_TERMINAL";

/// Environment variable carrying the terminal's exec flag (`-e`) to every child CDE spawns.
pub const TERMINAL_EXEC_FLAG_ENV: &str = "CDE_TERMINAL_EXEC_FLAG";

/// Environment variable carrying the configured overlay style, set on overlay spawns only.
pub const OVERLAY_STYLE_ENV: &str = "CDE_OVERLAY_STYLE";

/// Environment variable carrying the overlay TUI's highlight accent as lowercase `#rrggbb`, set on
/// overlay spawns only when the overlay style is N1 and the resolved overlay `accent` differs from
/// the style's own (see `config::overlay_tui_accent`). Unset (removed) otherwise.
pub const OVERLAY_ACCENT_ENV: &str = "CDE_OVERLAY_ACCENT";

/// Environment variable set to `default` on overlay spawns whose terminal's default background is
/// the N1 palette `bg` (see `OverlayConfig::surround_is_default_bg`), so the overlay TUI paints its
/// surround with SGR 49 and `overlay.alpha` applies. Unset (removed) otherwise.
pub const OVERLAY_SURROUND_ENV: &str = "CDE_OVERLAY_SURROUND";

/// Environment variable carrying the `launcher { tui … }` entries (program names / `*` globs)
/// joined by `:`, set on overlay spawns only. Unset or empty means no configured entries (the
/// launcher's built-in list still applies). Entries never contain `/` or `:`.
pub const LAUNCHER_TUI_ENV: &str = "CDE_LAUNCHER_TUI";

/// Environment variable carrying the wall-clock spawn time (ms since the UNIX epoch), set on
/// overlay spawns only, so the overlay can log its spawn-to-first-usable-frame latency.
pub const SPAWN_EPOCH_ENV: &str = "CDE_SPAWN_EPOCH_MS";
