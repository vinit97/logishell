# logishell

A Linux terminal app for Logitech keyboards and mice, written in Rust.
Supports Bluetooth, Bolt, Unifying, and direct USB through each device's available
features. Physically tested on MX Master 4 and MX Mechanical Mini over Bolt;
other devices may have limited support. See [device support](docs/devices.md).

## install

Requires Linux with systemd and udev, Rust 1.88+, and a C compiler/linker.
Run as your normal user:

```sh
git clone https://github.com/vinit97/logishell.git
cd logishell
./setup.sh
```

The script installs `~/.local/bin/logishell`, device and virtual-input permissions,
and the login service. It uses sudo for permissions and takes no options.
[Installation details](packaging/README.md).

## use

| Command | Purpose |
| --- | --- |
| `logishell` or `logishell status` | Device status and battery |
| `logishell setup` | Settings, button/key actions, details, pairing, and connections |
| `logishell config` | Read/change settings and manage bindings |
| `logishell daemon` | Run saved button/key bindings |
| `logishell help` | Command help, including `help config set` |

Setup offers action presets for remappable controls, haptic strength, and
thumb-wheel direction and left/right actions on supported devices.
Settings, bindings, and nicknames stay in a draft until **Apply changes**.
Pairing and connection changes take effect immediately.
Setup reuses device and settings reads until you choose Refresh or change a connection.

Direct commands also accept device IDs and saved nicknames:

```sh
logishell config get 'MX Master 4'
logishell config set 'MX Master 4' dpi 1600
logishell config set 'MX Master 4' thumb-wheel-interval 150
```

## configuration

Saved settings and bindings live in `~/.config/logishell/config.toml`.
Changes saved through commands or setup reload the running daemon automatically.
After editing the file yourself, run `logishell config check`, then
`logishell config apply` for hardware settings or `logishell config reload`
for a running daemon's bindings. The daemon runs bindings only.

See [configuration and automation](docs/automation.md) for TOML examples,
setting values and reset behavior. [AGENTS.md](AGENTS.md) has command guidance for agents.

## uninstall

```sh
./uninstall.sh
```

Removes the CLI, login service, and bundled permissions. Keeps configuration
and pairings.

## development

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked --release
```

MIT licensed. Not affiliated with Logitech. Independently implemented;
OpenLogi inspired the project. [Protocol references](docs/devices.md#protocol-references).
