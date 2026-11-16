# The CDE greeter

`crucible greeter` is an optional graphical login screen for CDE. It runs under [greetd](https://sr.ht/~kennylevinsen/greetd/) as greetd's greeter user: the CDE compositor in kiosk mode, drawing its own login panel in the CDE look. Logging in on tty1 with `cde-session` stays supported and is still the default.

In greeter mode the compositor loads no workspaces, runs no autostart, uses none of the user's binds and saves no session. It never reads `$HOME` or `$XDG_CONFIG_HOME` (the greeter user cannot read the user's home anyway): everything it shows comes from the profile directory given with `--profile`.

## Command line

```
crucible greeter [--profile <dir>] [--fake-greetd <file>] [--winit]
```

- `--profile <dir>` reads `<dir>/profile.kdl` (below). Without it, or when the file is missing, unreadable or invalid, the greeter uses the fallback look and logs one warning.
- `--fake-greetd <file>` replaces greetd with an in-process fake for testing: `<file>` holds one JSON greetd reply per line (blank lines and `#` comment lines are skipped), served in order, one per request. When the replies run out the connection closes and the greeter exits 1.
- `--winit` runs nested inside another Wayland session (development and testing). Without it the greeter runs on the DRM/KMS backend, as greetd starts it.

Without `--fake-greetd` the greeter connects to `$GREETD_SOCK`, which greetd sets. A missing or unreachable socket is an error on stderr and exit 1. After greetd accepts `start_session` the greeter exits 0, and greetd starts the session command. A failed exchange with greetd exits 1 (greetd then restarts the greeter).

## Profile

`<dir>/profile.kdl` is published by the user's CDE session from the config's `greeter` block and `theme` block (see the deploy section below). Every node is optional; a missing node keeps the fallback value, while an unknown node or a bad value makes the whole file invalid (fallback).

```kdl
style "terminal"                 // terminal | n1-dark | n1-med | n1-light
background { image "/var/lib/cde-greeter/background.png"; }   // or: background { color "#000000"; }
palette { bg "#1d1f21"; text "#c5c8c6"; line "#666666"; accent "#81a2be"; }
alpha 0.85                       // panel fill, 0.0..=1.0 (below 1.0 is the glass look)
session "cde-session"            // the command greetd starts
keyboard { layouts "us" "es"; }  // xkb layouts in group order
bind "Super+E" "set-layout us"   // only set-layout binds are accepted
```

- `background` is a colour or an absolute image path; the profile never holds `stock` or `user`, which the publisher resolves to a copied image file.
- `palette` holds the panel colours resolved from the style (for `terminal`, the user's `theme` colours). Without it the style's own palette is used (`terminal` falls back to `n1-med`).
- The fallback look is style `n1-med` on colour `#000000`, alpha 1.0, layout `us`, session `cde-session` and the default binds.

## Background

The greeter runs `/usr/bin/swaybg -o '*' -i <image> -m fill` or `/usr/bin/swaybg -o '*' -c <rrggbb>` once its Wayland socket is up. When swaybg is missing it logs a warning; the screen is then cleared to the profile colour (black under an image) and the panel still draws.

The stock CDE background image may change on a major release.

## Panel and keys

The panel is centred on every output; keyboard focus is global. It shows the prompt ("Username", then the PAM prompts), the input field and a message line (errors, PAM info lines, and "Authenticating…" while greetd is answering).

- Printable characters type into the field; a secret prompt shows one bullet per character, never the text.
- `Backspace` deletes the last character.
- `Enter` submits the field; the field is then cleared.
- `Escape` cancels back to the username prompt.
- Keys are ignored while greetd is answering (a wrong password takes about two seconds).

Only two kinds of chords work, the same ones as on the lock screen: VT switching (`Ctrl+Alt+F<n>`) and `set-layout` binds. Every other chord (for example `Super+Return`) does nothing. IPC `exec` requests follow the same rule.

## Logging

The greeter logs to stderr only, which greetd sends to its journal (`journalctl -u greetd`). `RUST_LOG` sets the level (default `info`). Passwords never appear in logs.

## Nested test

Run it nested in a background scratch workspace (see [nested-testing.md](nested-testing.md)), with a profile and a reply script in a short temp directory:

```sh
mkdir -p /tmp/cde-greeter/run /tmp/cde-greeter/state /tmp/cde-greeter/config /tmp/cde-greeter/profile
chmod 700 /tmp/cde-greeter/run
printf 'style "n1-med"\nbackground { color "#203040"; }\nalpha 0.8\n' > /tmp/cde-greeter/profile/profile.kdl
printf '%s\n' '{"type":"auth_message","auth_message_type":"secret","auth_message":"Password:"}' '{"type":"success"}' '{"type":"success"}' > /tmp/cde-greeter/replies
crucible msg run-on scratch-greeter env vblank_mode=0 WAYLAND_DISPLAY="$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" XDG_RUNTIME_DIR=/tmp/cde-greeter/run XDG_STATE_HOME=/tmp/cde-greeter/state XDG_CONFIG_HOME=/tmp/cde-greeter/config crucible greeter --winit --profile /tmp/cde-greeter/profile --fake-greetd /tmp/cde-greeter/replies
CDE_SOCKET=/tmp/cde-greeter/run/cde/wayland-1.sock crucible msg capture /tmp/cde-greeter/panel.png
```

That script answers a username with a password prompt, accepts the password, then accepts `start_session`: typing any username and password into the nested window logs in, and the greeter exits 0. Stop it early with `crucible msg close-pid <pid>`.

## Setup

See the deploy section (added with the system install).
