# installation

Linux with systemd and udev, Rust 1.88+, and a C compiler/linker are required. The scripts do not
install toolchains or system packages. Run as your normal user:

```sh
./setup.sh
```

Setup installs the binary at `~/.local/bin/logishell`, the login service at
`~/.local/share/systemd/user/logishell.service`, and both permission rules.
It takes no options and may ask for your sudo password.
Keep `~/.local/bin` on `PATH`. The script works from any directory; Cargo uses
the lockfile and downloads build dependencies when needed. Running setup again
updates the executable and restarts the service. Older Cargo installations and
the former service file in `~/.config/systemd/user` are migrated automatically.

## device access

The HID rule gives the active local user raw read/write access to Logitech USB
and Bluetooth interfaces, including receivers. Reconnect devices after setup
to refresh permissions. Users without an active local seat may need
administrator-managed access.

## control remapping

The separate virtual-input rule allows the active local user to generate
keyboard and mouse events through `/dev/uinput`. Setup loads `uinput` and refreshes
its permissions. Installing permission rules requires sudo.

## automatic remapping at login

The user service runs saved bindings at login; it does not apply hardware
settings. CLI/setup saves reload a running daemon automatically. After editing
`~/.config/logishell/config.toml` directly, run `logishell config reload`.
Stop the service before running a foreground daemon.

```sh
systemctl --user status logishell.service
journalctl --user -u logishell.service -f
systemctl --user stop logishell.service
```

If the logs report failed diversion cleanup or controls already diverted, stop
the service, switch the affected device off and back on, then run
`systemctl --user start logishell.service`. A running service alone does not
confirm that bindings are active; check the logs for device errors. Avoid running
another remapping controller for the same device at the same time.

## removal

```sh
./uninstall.sh
```

Uninstall stops/removes the service before removing the binary,
then removes the bundled rules with sudo. Stop any foreground daemon first.
Saved configuration, pairings, and hardware settings stay intact. Repeated
uninstall handles absent components and does not require Cargo.

Modified rules are kept; a modified service must be handled manually. Setup also
refuses to overwrite unrecognized files. Reconnect devices to clear old HID
permissions. Reboot to clear existing `/dev/uinput` permissions; uninstall does
not unload a module other applications might be using.
