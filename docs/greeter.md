# The CDE greeter

`crucible greeter` is an optional graphical login screen for CDE. It runs under the `cde-login` daemon (which speaks the [greetd](https://sr.ht/~kennylevinsen/greetd/) greeter protocol) as the `_greetd` user: the CDE compositor in kiosk mode, drawing its own login panel in the CDE look. Logging in on tty1 with `cde-session` stays supported and is still the default.

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
position "lower"                 // center | upper | lower | upper-left | upper-right | lower-left | lower-right
blank-after 600                  // idle seconds before the screens blank, 1 or more
session "cde-session"            // the command greetd starts
keyboard { layouts "us" "es"; }  // xkb layouts in group order
bind "Super+E" "set-layout us"   // only set-layout binds are accepted
```

- `background` is a colour or an absolute image path; the profile never holds `stock` or `user`, which the publisher resolves to a copied image file.
- `palette` holds the panel colours resolved from the style (for `terminal`, the user's `theme` colours). Without it the style's own palette is used (`terminal` falls back to `n1-med`).
- `position` is the screen region whose centre the panel is centred on (clamped to stay on the output); `blank-after` is the idle timeout in whole seconds, published from the config's `idle { blank-after }` (600 when it sets none). The greeter never locks, and the key press that wakes blanked screens is consumed.
- The fallback look is style `n1-med` on colour `#000000`, alpha 1.0, position `lower`, blank after 600 s, layout `us`, session `cde-session` and the default binds.

## Background

The greeter runs `/usr/bin/swaybg -o '*' -i <image> -m fill` or `/usr/bin/swaybg -o '*' -c <rrggbb>` once its Wayland socket is up. When swaybg is missing it logs a warning; the screen is then cleared to the profile colour (black under an image) and the panel still draws.

The stock CDE background image may change on a major release.

## Panel and keys

The panel sits at the profile's `position` (default `lower`) on every output; keyboard focus is global. It shows the prompt ("Username", then the PAM prompts), the input field and a message line (errors, PAM info lines, and "Authenticating…" while greetd is answering).

- Printable characters type into the field; a secret prompt shows one bullet per character, never the text.
- `Backspace` deletes the last character.
- `Enter` submits the field; the field is then cleared.
- `Escape` cancels back to the username prompt.
- Keys are ignored while greetd is answering (a wrong password takes about two seconds).

Only two kinds of chords work, the same ones as on the lock screen: VT switching (`Ctrl+Alt+F<n>`) and `set-layout` binds. Every other chord (for example `Super+Return`) does nothing. IPC `exec` requests follow the same rule. With two or more layouts in the profile, the panel shows the active layout name at the right of the prompt row, updated on each `set-layout`.

## Logging

The greeter logs to stderr and to a file `/tmp/cde-greeter/greeter-<unix-seconds>-<pid>.log`. greetd does not keep the greeter's stderr, so the file is the only record. The greeter keeps the five newest files. The directory is created mode 0755 and the files 0644, so any user can read them: usernames, passwords and keys never appear in the logs. If `/tmp/cde-greeter` is a symlink, not a directory, or owned by another user, the greeter logs to stderr only. `RUST_LOG` sets the level (default `info`). After the screens blank, the first key press is logged once, with whether it woke them, never its key.

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

The login daemon is `cde-login`, which replaces greetd. It runs as root from `cde-login.service`, starts the greeter as `_greetd` on VT 7, and starts the user's session on VT 8 in graphics mode. It switches to the session only once CDE reports its first frame ready (no console, no empty frame), and on logout it returns to the greeter on VT 7. tty1 keeps its getty and stays a text-login fallback (`cde-session` autostarts only from a tty1 login). Keep the `greetd` package installed but disabled: it provides the `_greetd` account and is the rollback. Nothing changes until you enable `cde-login`.

1. Make `/opt/cde` root-owned: `sudo install -d -o root -g root -m 0755 /opt/cde`, or for an existing user-owned one `sudo chown -R root:root /opt/cde`.
2. Create the profile directory (you write it, the greeter user `_greetd` reads it): `sudo install -d -o <you> -g _greetd -m 0750 /var/lib/cde-greeter`
3. Run `scripts/deploy.sh <host>`. It stages the files in `~/.local/share/cde/opt-staging` and prints one `sudo sh -c '...'` command that installs them into `/opt/cde`; run it. Start CDE once (or reload its config) so the profile is published.
4. Install the unit and the PAM files:

   ```sh
   sudo install -m 0644 dist/systemd/cde-login.service /etc/systemd/system/
   sudo install -m 0644 dist/pam.d/cde-login dist/pam.d/cde-login-greeter /etc/pam.d/
   sudo systemctl daemon-reload
   ```

5. Switch over. Both services claim `display-manager.service`, so disable greetd first: `sudo systemctl disable greetd`, then `sudo systemctl enable cde-login`, then reboot.

Migration from a greetd setup: `/etc/greetd/config.toml` is no longer read. The greeter command and the VTs are `cde-login` flags (see `cde-login --help`), and the defaults match the old config.

Rollback: `sudo systemctl disable cde-login && sudo systemctl enable greetd`, then reboot.

Every release on a greeter host leaves one sudo install step: the deploy prints it and reports `sudo install pending` until the installed `--build` matches.

Security note: `/opt/cde` is root-owned because `cde-login` runs as root and the greeter runs as `_greetd`; a user-writable `/opt/cde` would be a path to root.

### What gets published, and when

On start and on every config reload, when the config has a `greeter` block and `/var/lib/cde-greeter` exists and is writable, the CDE session writes `profile.kdl` there (atomically, and only when it changed). With no `greeter` block, or without the directory, it writes nothing. The background resolves as follows:

- `color`: no image; the profile holds the colour.
- `stock`: the profile points at `/opt/cde/share/backgrounds/stock.png`, installed by the deploy where the greeter user can read it.
- `user`: the session copies the first image of your wallpaper state (`${XDG_STATE_HOME:-~/.local/state}/wallpaper-rotate/current`, the first output's image in `separate` mode) to `/var/lib/cde-greeter/background.<ext>` and points the profile at the copy. It also keeps an empty `user-background` marker in the directory while the background is `user` (removed otherwise). With the marker present, `cde-wallpaper-rotate` refreshes the copy on every rotation. If a rotation changes the image's extension, the profile keeps the old path until the next config apply republishes it. Without a wallpaper state the greeter shows black.

The deploy also installs the stock image to `${XDG_DATA_HOME:-~/.local/share}/cde/backgrounds/stock.png` on every host (the lock screen's `stock` background). Where `/opt/cde` exists it never writes there: it stages the files in `~/.local/share/cde/opt-staging` and prints one `sudo sh -c '...'` command (or `deploy: <host>: /opt/cde is current`); elsewhere it prints `deploy: <host>: /opt/cde absent; greeter install skipped`.
