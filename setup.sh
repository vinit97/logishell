#!/usr/bin/env bash
set -euo pipefail

fail() { printf 'logishell: %s\n' "$*" >&2; exit 1; }
[[ $# == 0 ]] || fail 'Run ./setup.sh without arguments.'
[[ $EUID != 0 ]] || fail 'Run this script as your normal user, without sudo.'
[[ $(uname -s) == Linux ]] || fail 'Linux is required.'
[[ ${HOME:-} == /* && $HOME != / ]] || fail 'HOME must name your home directory.'
for dependency in cargo sudo udevadm modprobe systemctl getent id groupadd gpasswd; do
    command -v "$dependency" >/dev/null || fail "Required command not found: $dependency"
done
udev_version=$(udevadm --version)
[[ $udev_version =~ ^[0-9]+$ ]] && (( 10#$udev_version >= 247 )) ||
    fail 'systemd/udev 247 or newer is required for account-specific device permissions.'
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source "$repo/packaging/access.sh"
staging=$(mktemp -d)
binary=
trap 'rm -rf -- "$staging"; if [[ -n $binary ]]; then rm -f -- "$binary"; fi' EXIT
rules=(72-logishell.rules 72-logishell-remap.rules)
managed_group=false
for rule in "${rules[@]}"; do
    sed "s/@LOGISHELL_UID@/$EUID/g" "$repo/packaging/$rule.in" > "$staging/$rule"
    target=/etc/udev/rules.d/$rule
    if [[ -e $target || -L $target ]]; then
        [[ -f $target && ! -L $target ]] && cmp -s "$staging/$rule" "$target" ||
            fail "Refusing to replace a modified rule or another account's installation: $target"
        managed_group=true
    fi
done
legacy_rules=()
for rule in 70-logishell.rules 70-logishell-remap.rules; do
    target=/etc/udev/rules.d/$rule
    if [[ -e $target || -L $target ]]; then
        [[ -f $target && ! -L $target ]] && cmp -s "$repo/packaging/legacy/$rule" "$target" ||
            fail "Refusing to replace a different file: $target"
        legacy_rules+=("$target")
    fi
done
legacy_cargo=false
if [[ -e $HOME/.cargo/bin/logishell || -L $HOME/.cargo/bin/logishell ]]; then
    installed=$(cargo install --list --root "$HOME/.cargo")
    [[ $'\n'$installed == *$'\nlogishell v'* ]] ||
        fail "Unregistered executable: $HOME/.cargo/bin/logishell. Move or remove it, then run setup again."
    legacy_cargo=true
fi
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
inspect_access_group || fail 'Refusing unsafe input-access group.'
[[ $access_group_present == false || $managed_group == true ]] ||
    fail "Refusing to adopt an existing group without recognized logishell rules: $access_group"
systemctl --user show-environment >/dev/null

cargo build --locked --release --manifest-path "$repo/Cargo.toml" --target-dir "$repo/target"
if [[ -e $service || -e $legacy_service ]]; then
    # Release diversion before changing raw-device permissions. The user manager
    # obtains its new supplementary group at the required reboot.
    systemctl --user stop logishell.service
fi
install -d "$HOME/.local/bin"
binary=$(mktemp "$HOME/.local/bin/.logishell.XXXXXX")
install -m 0755 "$repo/target/release/logishell" "$binary"
mv -fT -- "$binary" "$HOME/.local/bin/logishell"
printf 'Granting UID %s persistent raw Logitech and virtual-input access.\n' "$EUID"
printf 'This account can control input across local sessions, including while inactive.\n'
for rule in "${rules[@]}"; do
    sudo install -m 0644 "$staging/$rule" "/etc/udev/rules.d/$rule"
done
# Install the recognizable rules first so an interrupted group creation can be
# retried without adopting an unrelated pre-existing group.
if [[ $access_group_present == false ]]; then
    sudo groupadd --system "$access_group"
fi
inspect_access_group || fail 'Refusing unsafe input-access group.'
[[ $access_group_present == true ]] || fail 'Device-access group was not created.'
sudo gpasswd --add "$access_user" "$access_group"
if (( ${#legacy_rules[@]} )); then
    sudo rm -- "${legacy_rules[@]}"
fi
sudo modprobe uinput
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=misc --sysname-match=uinput
install -Dm 0644 "$repo/packaging/logishell.service" "$service"
rm -f -- "$legacy_service"
systemctl --user daemon-reload
systemctl --user reenable logishell.service
if [[ $legacy_cargo == true ]]; then
    cargo uninstall --root "$HOME/.cargo" logishell
fi

printf 'Installed logishell for UID %s with startup at login.\n' "$EUID"
printf 'Reboot to activate group membership and revoke handles and permissions from older rules.\n'
printf 'The login service is enabled and will start after reboot.\n'
case :$PATH: in
    *:"$HOME/.local/bin":*) ;;
    *) printf 'Add %s/.local/bin to your PATH to run logishell by name.\n' "$HOME" ;;
esac
