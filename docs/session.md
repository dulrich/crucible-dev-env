# Booting into CDE (session setup)

This page sets up CDE to launch from the tty1 login shell, with a release binary that dev-tree rebuilds do not change, and a clean way back to a plain shell or i3.

## Release install

Build a release binary and install it where the launcher can find it:

```sh
cargo build --release
install -m755 target/release/crucible ~/.local/bin/crucible
```

The dev tree's `cargo build` rebuilds then no longer change what boots: the installed copy only changes when you run the install line again.

## Launch from tty1

Add this block to `~/.profile`. It fires only on `/dev/tty1`, only when no Wayland session is already running, and is skipped while `~/.config/cde/no-autolaunch` exists:

```sh
if [ "$(tty)" = /dev/tty1 ] && [ -z "$WAYLAND_DISPLAY" ] && [ ! -e "$HOME/.config/cde/no-autolaunch" ]; then
    CDE_BIN="$HOME/.local/bin/crucible" exec crucible-dev-env/scripts/cde.sh
fi
```

`cde.sh` picks the compositor mode from `CDE_MODE`, which defaults to `--tty-udev`, so no mode flag is passed here. With `CDE_BIN` set, it skips `cargo build` and runs that binary; it exits 1 if `CDE_BIN` is not an executable file. It still writes the ghostty wrapper, logs to `/tmp/cde/`, and sets the first wallpaper.

After a crash or a failed start, read `/tmp/cde/latest.log`.

## Recommended autostart line

Export the session environment to D-Bus and systemd user services so portals and D-Bus-activated apps find CDE's Wayland socket. Add to `~/.config/cde/config.kdl`:

```kdl
autostart "dbus-update-activation-environment --systemd WAYLAND_DISPLAY DISPLAY XDG_CURRENT_DESKTOP XDG_SESSION_TYPE"
```

See [`config.md`](config.md#autostart) for the `autostart` semantics.

## Wallpaper

`cde.sh` sets the first wallpaper at startup. To rotate it under CDE, use the cron line host-b already has (`cron-run wallpaper-rotate … scripts/wallpaper-rotate.sh`). host-a's current cron entry is the i3 `feh` line, so it needs the CDE line added under its own job id (e.g. `cron-run wallpaper-rotate-cde …`), because `cron-run` keys its health file by job id and the two lines would otherwise overwrite each other's state.

## Reverting and falling back to i3

1. **If CDE crashes or quits,** the `exec` means you land back at the tty1 login prompt. There is no autologin, so it cannot loop.
2. **To stop the auto-launch:** from another VT (Ctrl+Alt+F2) or over ssh, run `touch ~/.config/cde/no-autolaunch`, then log in on tty1 for a plain shell. To re-enable it, `rm` the file.
3. **i3 on host-a:** use lightdm on vt7 (Ctrl+Alt+F7) while it is enabled, or `sudo systemctl enable --now lightdm` if you disabled it. Without a display manager, run `startx /usr/bin/i3` from a tty that is not running CDE.
4. **host-b has no X fallback.** The recovery path there is a tty shell or ssh.
5. **To roll back to a known-good binary:** reinstall from a previous commit with `git checkout <sha> && cargo build --release && install -m755 target/release/crucible ~/.local/bin/crucible`, or keep a `crucible.prev` copy beside the installed binary and copy it back.
6. **To fully remove the setup,** delete the `.profile` block.
