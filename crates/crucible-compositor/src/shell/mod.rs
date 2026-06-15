//! Wayland shell protocol handlers (xdg-shell and friends).
//!
//! Future home of the shell-protocol glue: `xdg_shell` toplevel/popup lifecycle, surface commit
//! handling, window-rule application, and the bridge from Wayland surface state into the pure
//! `crucible_layout` model (creating/destroying windows in the focused pane). Keeps protocol I/O
//! at the edge so the layout core stays compositor-free.
//!
//! Implemented in a later workstream (WS-3); this is a WS-1 stub.
