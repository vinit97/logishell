# logishell

Independent Rust terminal utility for Logitech keyboards and mice on Linux.
Keep the project name lowercase and the implementation lean.
Install as a standalone user package: executable in `~/.local/bin`, service in
`~/.local/share/systemd/user`, user config in `~/.config/logishell`.

## terminal commands

Use ordinary terminal output and check exit codes: `0` success, `1` command
failure, `2` invalid arguments. `setup` is interactive; agents should use the
commands below. Select devices by unambiguous name, saved alias, or ID.
Use `logishell config` to change configurable settings instead of editing TOML
or source code. Read the setting back after changing it.

| Command | Purpose |
| --- | --- |
| `logishell status` | Connected and paired devices, state, battery |
| `logishell config get 'MX Master 4'` | Device ID, current settings, supported values, named controls and saved actions |
| `logishell config get 'MX Master 4' dpi` | Read just one setting; faster than loading all device details |
| `logishell config set 'MX Master 4' dpi 1600` | Change hardware and save; `--temporary` skips saving, with no automatic revert |
| `logishell config set 'MX Master 4' thumb-wheel-interval 200` | Save the thumb-wheel repeat interval in milliseconds and reload bindings |
| `logishell config check` | Validate saved config without changing hardware |
| `logishell config apply [device]` | Apply saved hardware settings |
| `logishell config reload` | Load manual TOML edits into the running daemon |
| `logishell config reset` | Back up and clear saved config; leave hardware and pairings intact |
| `logishell config bind 'MX Master 4' back key:ctrl+c` | Save a supported control binding |
| `logishell config unbind 'MX Master 4' back` | Remove a saved binding |
| `logishell help config set` | Command help; use `logishell help` for the overview |

Config lives only at `~/.config/logishell/config.toml`. Use the Device ID from
`config get` for TOML keys. Preserve unrelated entries, validate edits, then
apply hardware settings or reload bindings as appropriate. Check failures and
read settings back; applying several changes can partially succeed.
Saved CLI changes (`set`, `alias`, `bind`, `unbind`, `reset`) and setup's final
Apply automatically reload a running daemon. Manual TOML edits need explicit
`config reload`. A daemon need not be running to save changes.
See [configuration examples](docs/automation.md).

## development rules

- Implement from protocol documentation; do not copy another application's code.
- Support devices by detected capabilities, without a model allowlist. Keep status observational; never invent battery readings or claim untested hardware is verified.
- Keep roots to `status`, `setup`, `config`, `daemon`, and `help`. Pairing and connection management belong in setup. Do not add alternate help flags or config paths.
- Setup stages settings, bindings, and nicknames until final Apply; going back or discarding must not write or reload them.
- The daemon runs bindings only. Load config at startup and reload requests, never poll the file or automatically apply hardware settings. Stopping must release virtual input and restore temporary diversion.
- No GUI, telemetry, firmware updates, automatic downloads, diagnostics commands, or shell-completion generation.
- Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test`.
- Keep tests focused on distinct failure modes; extend existing cases instead of duplicating coverage across unit and integration tests.
- Automated tests must not change host permissions, pair/unpair devices, or enable services.
- Setup grants raw-device access only to the explicitly trusted installing UID. Never grant it to all seat users.
