# Supported terminals

Crucible Dev Env (CDE) supports four terminals. This is the one list; each terminal has its own page with its theme coverage, settings and key-binding carve-out. Measurements are in [`docs/spikes/terminal-matrix/WP-1-FINDINGS.md`](../spikes/terminal-matrix/WP-1-FINDINGS.md), settings in [`docs/config.md`](../config.md).

## Support levels

- **full** — styled by the CDE `theme`, padding and focus ring matched, exec, cwd and restore pass, measured in the matrix, and able to host the pre-started overlay terminal.
- **basic** — spawns, exec and restore pass, styled by the `theme`; no daemon or overlay guarantees.

Anything else is unsupported and is not listed.

## The list

| Terminal | Version | Level | Modes | Measurements |
| --- | --- | --- | --- | --- |
| [cterm](cterm.md) | 0.5.1 | **full** | standalone, daemon | [matrix](../spikes/terminal-matrix/WP-1-FINDINGS.md#matrix) |
| [foot](foot.md) | 1.28.0 | **full** | standalone, server (`foot --server` + `footclient`) | [matrix](../spikes/terminal-matrix/WP-1-FINDINGS.md#matrix) |
| [urxvt](urxvt.md) | rxvt-unicode 9.31 | **full** (standalone) | standalone; daemon (`urxvtd` + `urxvtc`) not rated | [matrix](../spikes/terminal-matrix/WP-1-FINDINGS.md#matrix) |
| [ghostty](ghostty.md) | 1.3.1 (stale data) | **basic** | standalone | [matrix, ghostty row](../spikes/terminal-matrix/WP-1-FINDINGS.md#matrix) |

- urxvt is **full** in standalone mode because exec, cwd, padding, restore and the overlay host all pass there; its daemon mode is not rated because the restore check was inconclusive (the setgid daemon's working directory is unreadable on the measurement host).
- ghostty is **basic** because its only data is from 2026-10-31 and is marked stale: ghostty is not installed on the measurement host (host-a) and was not reinstalled, so none of the full-level checks were re-run.

Every measured mode passes the exec flag, cwd flag and `{gap}` padding-argument checks ([integration checks](../spikes/terminal-matrix/WP-1-FINDINGS.md#integration-checks)). The numbers were taken on one host with a 1-minute load average above 2.0, so read them as relative, not absolute.

## One theme for all of them

A `theme` block in `config.kdl` renders a fragment per terminal into `~/.config/cde/terminal-theme/`; see [`theme` in docs/config.md](../config.md#theme). CDE also adds built-in padding arguments per terminal (`tiling.terminal-args`) so the focus ring sits in the terminal's own padding.
