#!/usr/bin/env bash
set -euo pipefail

fail() { printf 'logishell: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null || fail "Required command not found: $1"; }
if [[ ${1:-} == --help && $# == 1 ]]; then
    cat <<'EOF'
Usage: ./uninstall.sh

Remove the user executable, login service, and bundled device-access rules.
Removing installed rules needs sudo. Configuration and device pairings are kept.
EOF
    exit 0
fi
[[ $# == 0 ]] || fail 'Use ./uninstall.sh without arguments, or --help.'
[[ $EUID != 0 ]] || fail 'Run this script as your normal user, without sudo.'
[[ $(uname -s) == Linux ]] || fail 'Linux is required.'
[[ ${HOME:-} == /* && $HOME != / ]] || fail 'HOME must name your home directory.'
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
service=$HOME/.local/share/systemd/user/logishell.service
rules=()
for rule in 70-logishell.rules 70-logishell-remap.rules; do
    target=/etc/udev/rules.d/$rule
    if [[ -e $target || -L $target ]]; then
        if [[ ! -L $target ]] && cmp -s "$repo/packaging/$rule" "$target"; then
            rules+=("$target")
        else
            printf 'Keeping modified or unrecognized rule: %s\n' "$target" >&2
        fi
    fi
done
if (( ${#rules[@]} )); then
    need sudo
    need udevadm
fi

if [[ -e $service || -L $service ]]; then
    [[ ! -L $service ]] && cmp -s "$repo/packaging/logishell.service" "$service" ||
        fail "Refusing to remove a different service: $service"
    need systemctl
    systemctl --user disable --now logishell.service
    rm -- "$service"
    systemctl --user daemon-reload
fi

runtime_root=${XDG_RUNTIME_DIR:-/tmp}
[[ $runtime_root == /* ]] || runtime_root=/tmp
daemon_lock=$runtime_root/logishell-$EUID/remap.lock
if [[ -e $daemon_lock ]]; then
    need flock
    flock --nonblock "$daemon_lock" true || fail 'Stop the foreground logishell daemon, then run uninstall again.'
fi

rm -f -- "$HOME/.local/bin/logishell"
if (( ${#rules[@]} )); then
    sudo rm -- "${rules[@]}"
    sudo udevadm control --reload-rules
    printf 'Reconnect devices to refresh access. Reboot to clear existing virtual-input permissions.\n'
fi
printf 'logishell removed. Saved configuration, pairings, and hardware settings were kept.\n'
