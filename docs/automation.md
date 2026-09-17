# configuration for scripts and agents

```sh
logishell status
logishell config get 'MX Master 4'
```

Commands accept a device name, saved alias, or ID. Copy the **Device ID** from
`config get` for TOML keys; pairing or changing transport can change it.
`config get` shows settings, supported values, and named controls with their
saved actions. The identifier in brackets is accepted by `config bind`.
Default means no logishell binding is saved; saved actions need a running daemon.
Read any warnings before making changes. Status keeps to connection and battery.

Commands print readable text. Check exit codes: `0` success,
`1` command failure, `2` invalid arguments. Pairing and connections use `setup`.

## edit, validate, and apply

Configuration lives at `~/.config/logishell/config.toml`. Replace `<device-id>`
with your device's ID and keep only supported settings and controls:

```toml
schema_version = 1

[devices."<device-id>"]
alias = "mouse"

[devices."<device-id>".settings]
dpi = "1600"
smartshift-sensitivity = "20"

[devices."<device-id>".bindings]
back = "key:ctrl+c"
```

Setting values are strings. Aliases must be unique and use ASCII letters, numbers,
hyphens, or underscores. Preserve other devices when editing the file.

| Command | Effect |
| --- | --- |
| `config check` | Validate the file; does not check connected hardware |
| `config apply [device]` | Write saved hardware settings; omit device to apply all detected saved entries |
| `config reload` | Load manual TOML edits into a running daemon |
| `config reset` | Back up the file, then clear saved settings, aliases, and bindings |

Prefix commands with `logishell`.
Hardware settings are never applied automatically. Reset leaves physical settings
and pairings intact. Saved CLI changes and setup's final Apply reload a running
daemon; manual TOML edits require `config reload`. The daemon never polls the file.
Applying can partially succeed: read the result and verify with `config get`.
Absent devices are skipped. Removing a saved setting does not reset its hardware.

## setting values

| Key | String values |
| --- | --- |
| `dpi` | Device-reported range and steps from `config get` |
| `wheel-mode` | `ratchet`, `free-spin` |
| `smartshift` | `on`, `off` |
| `smartshift-sensitivity` | `1`–`254`; `255` disables automatic disengagement |
| `scroll-invert` | `on`, `off` |
| `thumb-wheel-invert` | `on`, `off`; native horizontal scrolling direction |
| `fn-lock` | `on`: media row; `off`: F1–F12 without Fn |
| `backlight` | `on`, `off` |
| `haptic-strength` | `0`–`100` percent; preserves whether haptics are enabled |

On/off values also accept `true`/`false`. Choose either `smartshift` or
`smartshift-sensitivity` in a file. Explicit `wheel-mode` is applied last.
Applying haptic strength plays a short test pulse when enabled and supported;
selecting a draft value does not vibrate or change the device.

`logishell config set mouse dpi 1600` changes and saves immediately;
`--temporary` changes hardware without saving and does not automatically revert.
`config bind mouse back key:ctrl+c` saves a binding; `config unbind mouse back`
removes it. Sources must be reported controls, including quoted `cid:0xNNNN` keys.
Actions include `key:ctrl+c`, `mouse:middle`, and `media:play-pause`.
Supported thumb wheels also accept `thumb-left` and `thumb-right`, for example
`config bind mouse thumb-left media:volume-down`. Assigning either direction
replaces native horizontal scrolling while the daemon runs; an unassigned
direction shows **No action**. Remove both bindings to restore native scrolling.
Bindings require the daemon and [input permission](../packaging/README.md).

Use `logishell config set mouse thumb-wheel-interval 250` to slow repeated
thumb-wheel shortcuts, and `logishell config get mouse thumb-wheel-interval` to
read it back. The command saves and reloads the daemon automatically; interval
changes update the running worker without restarting its input device. This allows
at most one thumb-wheel action every 250 ms while turning, with an immediate first
press and no queued presses after stopping. The interval applies to both
directions together; button bindings and native scrolling are unaffected.
Values are `0`–`5000` milliseconds; omitted or `0` keeps the original repeat rate.
This daemon setting cannot use `--temporary`. It is stored as
`thumb_wheel_interval_ms` directly in the device table, outside `settings`.
