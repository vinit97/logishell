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
staging=$(mktemp -d)
trap 'rm -rf -- "$staging"' EXIT
service=$HOME/.local/share/systemd/user/logishell.service
rules=()
own_access_rules=false
preserved_access_rules=false
for rule in 72-logishell.rules 72-logishell-remap.rules 70-logishell.rules 70-logishell-remap.rules; do
    case $rule in
        72-*) sed "s/@LOGISHELL_UID@/$EUID/g" "$repo/packaging/$rule.in" > "$staging/$rule" ;;
        70-*) cp -- "$repo/packaging/legacy/$rule" "$staging/$rule" ;;
    esac
    target=/etc/udev/rules.d/$rule
    if [[ -e $target || -L $target ]]; then
        if [[ -f $target && ! -L $target ]] && cmp -s "$staging/$rule" "$target"; then
            rules+=("$target")
            [[ $rule != 72-* ]] || own_access_rules=true
        else
            printf 'Keeping modified, unrecognized, or other-account rule: %s\n' "$target" >&2
            [[ $rule != 72-* ]] || preserved_access_rules=true
        fi
    fi
done
if (( ${#rules[@]} )); then
    need sudo
    need udevadm
fi
remove_access_group=false
if [[ $own_access_rules == true && $preserved_access_rules == false ]]; then
    need getent
    need id
    . "$repo/packaging/access.sh"
    if inspect_access_group; then
        if [[ $access_group_present == true ]]; then
            need groupdel
            remove_access_group=true
        fi
    else
        printf 'Keeping access group %s because its ownership could not be verified.\n' "$access_group" >&2
    fi
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
    if [[ $remove_access_group == true &&
        ! -e /etc/udev/rules.d/72-logishell.rules && ! -L /etc/udev/rules.d/72-logishell.rules &&
        ! -e /etc/udev/rules.d/72-logishell-remap.rules && ! -L /etc/udev/rules.d/72-logishell-remap.rules ]]; then
        sudo groupdel "$access_group"
    fi
    printf 'Reboot to remove remaining device ownership, permissions, and open handles.\n'
fi
printf 'logishell removed. Saved configuration, pairings, and hardware settings were kept.\n'
