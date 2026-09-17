#!/usr/bin/env bash
set -euo pipefail

fail() { printf 'logishell: %s\n' "$*" >&2; exit 1; }
[[ $# == 0 ]] || fail 'Run ./setup.sh without arguments.'
[[ $EUID != 0 ]] || fail 'Run this script as your normal user, without sudo.'
[[ $(uname -s) == Linux ]] || fail 'Linux is required.'
[[ ${HOME:-} == /* && $HOME != / ]] || fail 'HOME must name your home directory.'
for dependency in cargo sudo udevadm modprobe systemctl; do
    command -v "$dependency" >/dev/null || fail "Required command not found: $dependency"
done
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
legacy_cargo=false
if [[ -e $HOME/.cargo/bin/logishell || -L $HOME/.cargo/bin/logishell ]]; then
    installed=$(cargo install --list --root "$HOME/.cargo")
    [[ $'\n'$installed == *$'\nlogishell v'* ]] ||
        fail "Unregistered executable: $HOME/.cargo/bin/logishell. Move or remove it, then run setup again."
    legacy_cargo=true
fi
for rule in 70-logishell.rules 70-logishell-remap.rules; do
    target=/etc/udev/rules.d/$rule
    if [[ -e $target || -L $target ]]; then
        [[ ! -L $target ]] && cmp -s "$repo/packaging/$rule" "$target" ||
            fail "Refusing to replace a different file: $target"
    fi
done
service=$HOME/.local/share/systemd/user/logishell.service
legacy_service=$HOME/.config/systemd/user/logishell.service
for existing in "$service" "$legacy_service"; do
    if [[ -e $existing || -L $existing ]]; then
        [[ ! -L $existing ]] && {
            cmp -s "$repo/packaging/logishell.service" "$existing" ||
                cmp -s <(sed 's@%h/.local/bin/logishell@%h/.cargo/bin/logishell@' "$repo/packaging/logishell.service") "$existing"
        } ||
            fail "Refusing to replace a different file: $existing"
    fi
done
systemctl --user show-environment >/dev/null

cargo build --locked --release --manifest-path "$repo/Cargo.toml" --target-dir "$repo/target"
install -d "$HOME/.local/bin"
binary=$(mktemp "$HOME/.local/bin/.logishell.XXXXXX")
trap 'rm -f -- "$binary"' EXIT
install -m 0755 "$repo/target/release/logishell" "$binary"
mv -fT -- "$binary" "$HOME/.local/bin/logishell"
sudo install -m 0644 "$repo/packaging/70-logishell.rules" /etc/udev/rules.d/70-logishell.rules
sudo modprobe uinput
sudo install -m 0644 "$repo/packaging/70-logishell-remap.rules" /etc/udev/rules.d/70-logishell-remap.rules
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=misc --sysname-match=uinput
install -Dm 0644 "$repo/packaging/logishell.service" "$service"
rm -f -- "$legacy_service"
systemctl --user daemon-reload
systemctl --user reenable logishell.service
systemctl --user restart logishell.service
systemctl --user is-active --quiet logishell.service
if [[ $legacy_cargo == true ]]; then
    cargo uninstall --root "$HOME/.cargo" logishell
fi

printf 'Installed logishell with device access, virtual input, and startup at login.\n'
printf 'Reconnect Logitech devices or receivers to refresh device access.\n'
case :$PATH: in
    *:"$HOME/.local/bin":*) ;;
    *) printf 'Add %s/.local/bin to your PATH to run logishell by name.\n' "$HOME" ;;
esac
