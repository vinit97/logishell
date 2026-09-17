# installation

Linux with systemd/udev 247+, Rust 1.88+, and a C compiler/linker are required. The scripts do not
install toolchains or system packages. The account tools `getent`, `groupadd`,
`gpasswd`, and `groupdel` must be available. Run as your normal user:

```sh
./setup.sh
```

Setup installs the binary at `~/.local/bin/logishell`, the login service at
`~/.local/share/systemd/user/logishell.service`, and both permission rules.
It takes no options and may ask for your sudo password.
Keep `~/.local/bin` on `PATH`. The script works from any directory; Cargo uses
the lockfile and downloads build dependencies when needed. Running setup again
updates the executable. Setup enables the login service and stops it until you
reboot, so it starts with the new access-group membership. Older Cargo installations and
the former service file in `~/.config/systemd/user` are migrated automatically.

Setup refuses symlinked or modified permission rules and rules installed for
another account. Exact older bundled rules are migrated automatically. Reboot
after setup to activate group membership and clear existing device handles,
ACLs, and cached udev tags that can retain access from older active-seat rules.

## device access

The HID rules give the installing account raw read/write access to Logitech USB
and Bluetooth interfaces, including receivers. Setup creates the system group
`logishell-<UID>` with only that account as a supplementary member. Both HID and
virtual-input nodes remain owned by root, with this group and mode `0660`,
replacing active-seat access grants. Setup rejects an existing group with other
members, another group sharing its numeric GID, or any account using it as a
primary group. A preexisting group is accepted only with recognized rules from
an earlier installation for the same account. Other users keep ordinary keyboard
and mouse input.

Installing these rules with sudo trusts every process running as that account
to change supported devices and receivers and inject input into the machine.
This authority persists while the account is inactive or accessed remotely;
virtual input can reach another user's active session. The installation supports
one trusted account per machine, without session or same-account isolation.

## control remapping

The separate virtual-input rule allows the installing account to generate
keyboard and mouse events through `/dev/uinput`. Setup loads `uinput` and refreshes
its permissions; rebooting applies access to all connected Logitech devices.

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
then removes recognized rules for the installing account, including exact legacy
rules, with sudo. Its private access group is removed when recognized current
rules were removed and no modified or other-account rules remain. Stop any
foreground daemon first.
Saved configuration, pairings, and hardware settings stay intact. Repeated
uninstall handles absent components and does not require Cargo.

Modified rules, rules for another account, and groups whose exclusive membership
cannot be verified are kept; a modified service must be handled manually.
Reboot after uninstall: removing rules and a group does not reset existing device
permissions, running processes' group membership, or open device handles. Uninstall does not
unload a module other applications might be using.
