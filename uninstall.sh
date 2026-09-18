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
need id
source "$repo/packaging/access.sh"
prepare_access_rules
rules=()
own_access_rules=false
preserved_access_rules=false
for rule in "${access_rules[@]}"; do
    inspect_access_rule "$rule"
    target=/etc/udev/rules.d/$rule
    if [[ $access_rule_kind != absent ]]; then
        if [[ $access_rule_kind != unrecognized ]]; then
            rules+=("$target")
            own_access_rules=true
        else
            printf 'Keeping modified, unrecognized, or other-account rule: %s\n' "$target" >&2
            preserved_access_rules=true
        fi
    fi
done
if (( ${#rules[@]} )); then
    need sudo
    need udevadm
fi
remove_access_group=false
access_migration_present=false
if [[ $preserved_access_rules == false &&
    ( $own_access_rules == true || -e $access_migration || -L $access_migration ) ]]; then
    need getent
    if inspect_access_installation; then
        if [[ $access_migration_present == true ]]; then
            need sudo
        fi
        if [[ -n $installed_access_group ]]; then
            need sudo
            need groupdel
            remove_access_group=true
        fi
    else
        printf 'Keeping access groups and migration record because ownership could not be verified.\n' >&2
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
fi
if [[ ( $remove_access_group == true || $access_migration_present == true ) &&
    ! -e /etc/udev/rules.d/72-logishell.rules && ! -L /etc/udev/rules.d/72-logishell.rules &&
    ! -e /etc/udev/rules.d/72-logishell-remap.rules && ! -L /etc/udev/rules.d/72-logishell-remap.rules ]]; then
    if [[ $remove_access_group == true ]]; then
        sudo groupdel "$installed_access_group"
    fi
    if [[ $access_migration_present == true ]]; then
        sudo rm -- "$access_migration"
    fi
fi
if (( ${#rules[@]} )) || [[ $remove_access_group == true ]]; then
    printf 'Reboot to remove remaining device ownership, permissions, and open handles.\n'
fi
printf 'logishell removed. Saved configuration, pairings, and hardware settings were kept.\n'
