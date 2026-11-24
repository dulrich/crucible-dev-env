//! The `CDE_SOCKET` IPC protocol: a pure codec shared with the `crucible` binary, plus the
//! compositor's non-blocking Unix-socket server driven by calloop.
//!
//! ## Wire format (plan `03-persistence`, Public Interfaces)
//! UTF-8, one request line → one response line (`\n`-terminated). A request is a verb, optionally
//! followed by **one space** and an argument (the rest of the line). Responses are `ok`,
//! `ok <payload>` or `err <reason>`.
//!
//! **Encoding:** every name, path and reason — in requests and responses alike — goes through
//! [`encode_field`], which percent-encodes exactly `%`, `:`, TAB, LF and CR (`%25`, `%3A`, `%09`,
//! `%0A`, `%0D`); every other byte passes through. [`decode_field`] is its exact inverse. The same
//! rule encodes the argument of `crucible pick --rename <current>`. The codec itself lives in
//! `crucible_persist::field` and is re-exported here.
//!
//! | Request (`Request`)        | Response                                                  |
//! |----------------------------|-----------------------------------------------------------|
//! | `candidates`               | `ok ` + TAB-separated [`Candidate`]s (`kind:name[:path]`) |
//! | `open <name>`              | `ok` / `err <reason>`                                     |
//! | `open-folder <path>`       | `ok` / `err <reason>`                                     |
//! | `new <name>`               | `ok` / `err <reason>`                                     |
//! | `rename <name>`            | `ok` / `err <reason>`                                     |
//! | `root <path>` / `root -`   | `ok` / `err <reason>` (set / clear the active workspace's root) |
//! | `bindings`                 | `ok ` + TAB-separated `chord:action` pairs (fields encoded) |
//! | `save`                     | `ok`                                                      |
//! | `run <command>`            | `ok` (spawned) / `err <reason>`                           |
//! | `relaunch`                 | `ok` (placeholder swapped for its app) / `err <reason>`   |
//! | `close-pid <n>`            | `ok <count>` / `err no window for pid <n>`                |
//! | `capture <path>`           | `ok <w>x<h>` (PNG of the focused output written) / `err <reason>` |
//!
//! `relaunch` is the only request that depends on the caller: the compositor reads the peer PID
//! (`SO_PEERCRED`) and accepts it only from an app relaunch placeholder (`crucible relaunch`).
//!
//! An empty candidate list is answered with a bare `ok`.
//!
//! **The `root -` sentinel:** `root -` clears the root, and any other `root <path>` sets it. That
//! is unambiguous because an admitted root is absolute (`check_root`): a set root never encodes
//! to `-`. The typed sender enforces it: [`Request::to_line`] refuses a `SetRoot(Some(_))` that is
//! relative (`-` included) or not valid UTF-8 with an [`EncodeError`], never rewriting it to a
//! clear or a lossy path. The raw `crucible msg` path sends its line as typed.
//!
//! The pure items ([`encode_field`], [`decode_field`], [`Request`], [`Response`], [`Candidate`])
//! are what the `crucible pick`/`msg` client side imports; the socket server ([`bind_socket`],
//! [`CrucibleState::start_ipc`]) is compositor-only.

mod env;
mod payload;
mod server;
mod wire;

pub use env::{
    LAUNCHER_TUI_ENV, OVERLAY_ACCENT_ENV, OVERLAY_STYLE_ENV, OVERLAY_SURROUND_ENV, SOCKET_ENV,
    SPAWN_EPOCH_ENV, TERMINAL_ENV, TERMINAL_EXEC_FLAG_ENV,
};
pub use payload::{
    Candidate, CandidateKind, OutputInfo, format_bindings, format_candidates, format_outputs,
    parse_bindings, parse_candidates, parse_outputs,
};
pub use server::{Outbox, Reply, Ticket, bind_socket, socket_path, take_lines};
pub use wire::{EncodeError, Request, Response, decode_field, encode_field};
