# Booting into CDE (session setup)

This page sets up CDE to launch from the tty1 login shell, with a release binary that dev-tree rebuilds do not change, and a clean way back to a plain shell or i3.

## Release install

Build a release binary and install it where the launcher can find it. Keep the previous binary as `crucible.prev` beside the installed one if you want a quick rollback:

```sh
cargo build --release
[ -e ~/.local/bin/crucible ] && cp -p ~/.local/bin/crucible ~/.local/bin/crucible.prev
install -m755 target/release/crucible ~/.local/bin/crucible
~/.local/bin/crucible --version   # confirm the installed build
```

The dev tree's `cargo build` rebuilds then no longer change what boots: the installed copy only changes when you run the install line again.

## Launch from tty1

Add this block to `~/.profile`. It runs the installed launcher, so the host needs no checkout (`scripts/deploy.sh` installs `cde-session`, `crucible` and `cde-wallpaper-rotate` into `~/.local/bin`). It fires only on `/dev/tty1`, only when no Wayland session is already running, and is skipped while `~/.config/cde/no-autolaunch` exists:

```sh
if [ "$(tty)" = /dev/tty1 ] && [ -z "$WAYLAND_DISPLAY" ] && [ ! -e "$HOME/.config/cde/no-autolaunch" ]; then
    exec "$HOME/.local/bin/cde-session"
fi
```

`cde-session` is `scripts/cde.sh` generated at deploy time: `CDE_BIN` defaults to the sibling `crucible`, it never runs `cargo` or `git`, and it logs the deployed build id instead of a git head. On a development checkout you can still `exec` `scripts/cde.sh` directly (it builds the dev tree unless `CDE_BIN` is set).

`cde-session` (and `cde.sh`) picks the compositor mode from `CDE_MODE`, which defaults to `--tty-udev`, so no mode flag is passed here. With `CDE_BIN` set, it skips `cargo build` and runs that binary; it exits 1 if `CDE_BIN` is not an executable file. It still writes the ghostty wrapper (the fallback `$TERMINAL`, used only when `config.kdl` sets no `terminal`; foot is the configured default), logs to `/tmp/cde/`, and sets the first wallpaper.

After a crash or a failed start, read `/tmp/cde/latest.log`.

## Recommended autostart line

Export the session environment to D-Bus and systemd user services so portals and D-Bus-activated apps find CDE's Wayland socket. Add to `~/.config/cde/config.kdl`:

```kdl
autostart "dbus-update-activation-environment --systemd WAYLAND_DISPLAY DISPLAY XDG_CURRENT_DESKTOP XDG_SESSION_TYPE"
```

See [`config.md`](config.md#autostart) for the `autostart` semantics.

## Wallpaper

`cde.sh` sets the first wallpaper at startup. To rotate it under CDE, run `~/.local/bin/cde-wallpaper-rotate` (the installed `scripts/wallpaper-rotate.sh`) from cron (`cde-wallpaper-rotate --help` shows the options and an example line). Set `WALLPAPER_DIR` to the directory of images to rotate through. If you also have an X session that rotates wallpapers with `feh`, give the two cron jobs separate names or state files so they do not overwrite each other.

## Reverting and falling back to i3

1. **If CDE crashes or quits,** the `exec` means you land back at the tty1 login prompt. There is no autologin, so it cannot loop.
2. **To stop the auto-launch:** from another VT (Ctrl+Alt+F2) or over ssh, run `touch ~/.config/cde/no-autolaunch`, then log in on tty1 for a plain shell. To re-enable it, `rm` the file.
3. **If you keep an X session as a fallback** (for example i3), start it from a display manager on another VT (Ctrl+Alt+F7 is typical), or without a display manager run `startx /usr/bin/i3` from a tty that is not running CDE.
4. **Without an X fallback,** the recovery path is a tty shell or ssh.
5. **To roll back to a known-good binary:** reinstall from a previous commit with `git checkout <sha> && cargo build --release && install -m755 target/release/crucible ~/.local/bin/crucible`, or copy back the `crucible.prev` you kept beside the installed binary.
6. **To fully remove the setup,** delete the `.profile` block.
