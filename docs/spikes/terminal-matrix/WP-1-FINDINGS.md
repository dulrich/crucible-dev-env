# Terminal matrix: measured standalone and daemon modes (2026-11-13)

Measured on host-a by `docs/spikes/terminal-matrix/matrix.sh` (driver build id of this tree). Raw per-mode output (`<mode>.res`, `<mode>.log`) lives in the gitignored spike scratch directory; every number below was copied from it. The previous single-run 2026-11 spike (an internal note, not published) is the only source for ghostty.

## Method

For each mode the driver starts a fresh nested `crucible --winit` (release build, own `XDG_*` directories under `/tmp/cde-41wp1`, parent `WAYLAND_DISPLAY`) through `crucible msg run-on scratch-41-wp1 <script>`, so the nested compositor and every test window live in a background scratch workspace, never in the focused one. The compositor's `terminal` is the real terminal binary (cterm) or a thin `sh` wrapper named like it (foot, footclient, urxvt, urxvtc) that records its pid and argv and then `exec`s the real binary, with `terminal-args` carrying the padding argument (`gap` 9). cterm is configured directly because its window grant is bound to the spawned executable and a wrapper script is rejected (`peer executable differs from the granted executable`). The 50 MB file is generated text. Runs serialize on a `flock` lock file.

Conditions and their n:

- **Spawn-to-first-frame latency**, n=5 per cell. Cold = the mode's daemon is not running (a fresh daemon per run); warm = daemon resident. Standalone modes have no resident process, so they have a single series reported under warm and no cold cell.
- **PSS** with 1, 5 and 10 windows, n=1 per cell (each count is one sampling of the live process set, 2 s after the last window mapped). The set is every window process and its descendants plus, for daemon modes, the daemon or server; the overlay-host process the compositor keeps warm is excluded from standalone sets (for daemon modes the daemon is counted whole). RSS is listed beside it because it is readable for every process.
- **Idle CPU**, n=1: `utime+stime` from `/proc/<pid>/stat` summed over the same set, over 60 s with 5 windows open (the shells are idle).
- **Throughput**, n=3: wall time of `cat` of a 50 MB file in a new window (`run-in-terminal`).
- **Responsiveness under load**, n=3: first-frame latency of a new window while another window of the same mode floods output (`cat` of the 50 MB file in a loop), measured 4 s into the flood.
- **Integration**, pass/fail with a note per terminal.

First-frame method, per mode (they are not interchangeable):

| Mode | Cold | Warm and under load |
|---|---|---|
| cterm standalone | compositor `spawn→map` line | compositor `spawn→map` line |
| cterm daemon | compositor `spawn→map` line (identity grant ties the daemon's window to the spawn) | same |
| foot standalone | compositor `spawn→map` line | same |
| foot `--server` + footclient | compositor line; the cold server is started by the wrapper, so its start is included | the foot server's own `WAYLAND_DEBUG` log (`ack_configure`, then `attach`, then `commit`), because the resident server is not an ancestor of the spawn; the server runs with debug on only for these two conditions |
| urxvt standalone | compositor `spawn→map` line (the X11 window is matched by pid) | same |
| urxvtd + urxvtc | XWayland window-tree poll (`xwininfo -root -tree`, 15 to 25 ms resolution) | same; the compositor cannot match the daemon's windows to the spawn |

The compositor line has 1 ms resolution (integer milliseconds). The foot-server debug method resolves about 0.1 ms. The urxvt-daemon poll method is coarser and its numbers carry its own overhead, so they are not directly comparable with the other rows at the millisecond level. A client-side `WAYLAND_DEBUG` cross-check against the compositor line was not run (not measured: budget).

## Conditions recorded

- Host host-a, kernel 7.2.12+deb13-amd64. CPU governor `powersave` on every core in every run.
- crucible build id `ff98ea0c62f82e9089841fe1bd36d9f5478233bb` (release build of this tree). cterm 0.5.1, build id `ec76972d3916511bba4d345792607ccae2d75a61`. foot 1.28.0 (also footclient), urxvt/urxvtd/urxvtc rxvt-unicode 9.31 (setgid `utmp`).
- llama-server **was resident** in every run (1 to 3 processes, embedding models, idle while measuring).
- Load average at run start: cterm 4.83, cterm-daemon 2.63, foot 2.95, foot-server 2.74, urxvt 2.61, urxvt-daemon 3.70 (1-min); end-of-run values 2.6 to 3.7. The host never sat at or below the 2.0 limit between samples (other sessions, the flood phases of the driver itself, and background services). The driver waited up to 10 s per sample and then took it anyway; it flagged each such sample in the log. Per-mode counts, wait rounds then samples taken over the limit: cterm 44/22, cterm-daemon 98/49, foot 142/71, foot-server 196/98, urxvt 240/120, urxvt-daemon 294/147 (cumulative across gate calls, not only measured samples). **No sample was cleanly discarded and repeated under the 2.0 rule; the whole matrix was measured at 1-min load above 2.0.** Read the numbers as relative within one host state, not absolute.
- The nested compositor's workspace is hidden, so its windows receive no frame callbacks. Latency and throughput therefore exclude vsync pacing and describe process and protocol work, not on-screen presentation.
- The nested compositor was a release build with default logging, so the 1 ms compositor line carries no debug overhead.

## Matrix

Latency in ms, median (min to max). PSS in MiB, from `/proc/<pid>/smaps_rollup`. "per window" is the PSS (or RSS where PSS is unreadable) growth between 5 and 10 windows divided by 5.

| Mode | Cold latency (n=5) | Warm latency (n=5) | Latency under flood (n=3) | PSS base, 1 / 5 / 10 windows (n=1) | per window | RSS 1 / 5 / 10 windows | Idle CPU, 5 windows (n=1) | 50 MB throughput s (n=3) |
|---|---|---|---|---|---|---|---|---|
| cterm standalone | n/a (no resident process) | 7 (6 to 7) | 6 (6 to 6) | none / 15.8 / 54.5 / 102.2 | 9.5 | 31.7 / 127.4 / 246.9 | 0.000 % (0 ticks) | 0.330 (0.327 to 0.335) |
| cterm daemon | 9 (8 to 12) | 6 (6 to 6) | 4 (3 to 5) | 18.1 / 29.7 / 52.7 / 81.2 | 5.7 | 50.4 / 121.5 / 210.4 | 0.000 % (0 ticks) | 0.311 (0.299 to 0.326) |
| foot standalone | n/a | 9 (9 to 10) | 10 (10 to 11) | none / 14.5 / 70.3 / 139.0 | 13.7 | 34.0 / 170.5 / 341.0 | 0.000 % (0 ticks) | 0.381 (0.374 to 0.400) |
| foot `--server` + footclient | 107 (107 to 108), includes server start | 11.7 (11.1 to 12.0) | 16.3 (15.2 to 18.3) | 2.7 / 15.0 / 61.3 / 119.1 | 11.6 | 35.0 / 123.1 / 233.0 | 0.150 % (9 ticks) | 0.364 (0.351 to 0.381) |
| urxvt standalone | n/a | 40 (38 to 41) | 39 (39 to 41) | n/a (setgid, unreadable) | n/a (RSS: 39.1) | 39.2 / 197.3 / 392.7 | 0.033 % (2 ticks) | 0.439 (0.421 to 0.504) |
| urxvtd + urxvtc | 142 (140 to 146), poll method | 14 (14 to 14), poll method | 13 (12 to 13), poll method | n/a (setgid, unreadable) | n/a (RSS: 14.0) | 59.8 / 112.4 / 182.2 | 0.017 % (1 tick) | 0.442 (0.435 to 0.462) |
| ghostty 1.3.1 | not measured (not installed) | 360.4 (n=5) **stale** | not measured | 56.8 to 58.7 at 1 window **stale** | not measured | 167.3 **stale** | not measured | 1.728 (n=3) **stale** |

Notes on the cells:

- cterm daemon "PSS base" 18.1 MiB is the resident daemon with no user window. In this nested setup the overlay host is a standalone cterm window (the overlay arguments do not carry `mode=daemon`), so the daemon starts on the first terminal window and the cold series is a genuine cold start.
- foot-server base 2.7 MiB is the idle server (the warm overlay host window lives in the server in this mode and is counted).
- urxvt and urxvtd are setgid on this host: `smaps_rollup` of the urxvt processes is unreadable (EACCES). Those cells are recorded as `n/a (setgid, unreadable)`, never as zero. The readable part of the process set (the shells, 2 to 4 MiB each; for 10 windows 52.9 MiB standalone and 56.6 MiB with the daemon) is only a lower bound that omits the urxvt processes themselves, and is not a PSS of the terminal; RSS (readable) is the only comparable figure. RSS counts shared pages in every process, so it is not comparable to PSS (foot standalone: 34 MiB RSS but 14.5 MiB PSS at 1 window).
- The first debug run of foot standalone (not the table run, load flagged) showed 10 windows at 407 MiB PSS; the table run shows 139 MiB. The cause of that one outlier was not investigated, and the table values are the recorded run.
- Idle CPU is `utime+stime` ticks over 60 s divided by 60: every mode is at or below 0.15 % of one core. At n=1 and 100 Hz tick resolution (9 ticks at most) the differences between modes are not distinguishable.
- ghostty is not installed and was not reinstalled. The ghostty row is the 2026-10-31 data of the previous 2026-11 spike (ghostty 1.3.1, client-side `WAYLAND_DEBUG` latency, one idle window, `crucible mem` PSS) and is **stale**: a different method, an older compositor and a different foot version. Do not compare it numerically with the rows above.

## Integration checks

| Check | cterm (standalone and daemon) | foot (standalone and server) | urxvt (standalone and daemon) |
|---|---|---|---|
| Exec flag through `run-in-terminal` | pass, `-e` (both) | pass, `-e` (both) | pass, `-e` (both) |
| cwd flag, run directly against the nested display | pass, `--working-directory` (both) | pass, `--working-directory` (both) | pass, `-cd` (both) |
| `{gap}` padding argument accepted | pass, `-o "pad={gap}x{gap} center"` substituted to `pad=9x9 center`, terminal mapped and stayed up (both) | pass, same argument on foot and footclient | pass, `-b {gap}` substituted to `-b 9`; for urxvtc the client forks away so liveness is not checked, the daemon-side window mapped |
| Restore after a nested `crucible` restart (saved workspace, window root set, nested compositor restarted on the same state directory; the restored window's shell cwd equals the saved root) | pass (both) | pass (both) | standalone pass; daemon **inconclusive**: the restored windows' cwd could not be read because the setgid daemon's `/proc/<pid>/cwd` is unreadable |
| Overlay host runs in the terminal (the compositor keeps one warm overlay-host window in the configured terminal) | pass, compositor map line 6 ms (standalone); daemon mode starts the overlay host as a standalone cterm window | pass; standalone map line 9 ms; the server hosts it in server mode, so the map line is not attributable and the evidence is a live `crucible overlay-host` process in the run | pass; standalone map line 42 ms; the daemon hosts it in daemon mode, evidence is the live process |

Padding was checked at the argument level only: the visual result (padding actually drawn, cursor and cell grid alignment) was not inspected (not measured: needs a screen check). The overlay check proves the host process runs; opening an actual overlay in each terminal was not exercised.

## Implications for support levels

Facts only; the README texts come later.

- Every measured mode passes the exec flag, cwd flag and padding-argument checks, so all three terminals are usable as the CDE terminal through the existing `terminal`, `exec-flag` and `terminal-args` settings.
- First-frame latency spans two orders of magnitude between modes on this host: single-digit to low-double-digit ms for cterm (standalone and daemon), foot standalone, foot-server warm and urxvtc warm; about 40 ms for urxvt standalone (X11 through XWayland); 107 to 142 ms when a daemon or server must be started first. A resident daemon cuts urxvt's warm first frame from about 40 ms to about 14 ms (poll method, so a coarse comparison) but made foot's slightly slower (9 to 11.7 ms) and cterm's about equal (7 to 6 ms).
- Under a flooding neighbour window the new-window latency did not grow for any mode (cterm daemon 4 ms, foot 10 ms, urxvt 39 ms, urxvtc 13 ms); foot-server grew from 11.7 to 16.3 ms, the largest increase and the only one beyond the min-max spread. Whether the flood competed for the daemon's single thread was not isolated.
- Memory: a resident daemon or server lowers the PSS per added window for cterm (9.5 to 5.7 MiB, with an 18.1 MiB resident daemon) and foot (13.7 to 11.6 MiB, with a 2.7 MiB server); the 10-window total is lower with a daemon for cterm (81.2 against 102.2 MiB) and foot (119.1 against 139.0 MiB). urxvt's PSS cannot be read on a host where it is setgid, so only RSS compares (39.1 against 14.0 MiB per window, standalone against daemon).
- Idle CPU with 5 windows is at or below 0.15 % of one core for every mode; throughput of 50 MB differs by under 0.15 s between modes (0.31 to 0.44 s) in a hidden workspace.
- Gaps: ghostty has only stale data, urxvt PSS and urxvtd restore are unreadable here, the visual padding was not inspected, cold figures for the standalone modes do not exist by definition, and everything was measured on a host whose 1-min load stayed above 2.0 with llama-server resident and a `powersave` governor. The low-end hosts were not run.
