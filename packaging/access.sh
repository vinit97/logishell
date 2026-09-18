#!/usr/bin/env bash
# Shared checks for the single-account raw-device access group.
# Source after setup/uninstall has checked EUID and defined fail().

access_group=logitech
legacy_access_group=logishell-$EUID
access_migration=/etc/udev/rules.d/.logishell-group-migration
access_rules=(72-logishell.rules 72-logishell-remap.rules)
access_user=$(id -un) || fail 'Cannot determine the installing account.'
[[ -n $access_user && $access_user != *[:,[:space:]]* ]] ||
    fail 'The installing account has an unsupported name.'

inspect_access_group() {
    local access_group=$1 record result group_name group_gid members groups group found accounts account primary_gid
    access_group_present=false
    access_group_gid=
    if record=$(getent group "$access_group"); then
        :
    else
        result=$?
        if (( result == 2 )); then
            return 0
        fi
        printf 'logishell: Cannot inspect access group %s.\n' "$access_group" >&2
        return 1
    fi
    if [[ $record == *$'\n'* || ! $record =~ ^([^:]+):([^:]*):([0-9]+):([^:]*)$ ]]; then
        printf 'logishell: Invalid access group record for %s.\n' "$access_group" >&2
        return 1
    fi
    group_name=${BASH_REMATCH[1]}
    group_gid=${BASH_REMATCH[3]}
    members=${BASH_REMATCH[4]}
    if [[ $group_name != "$access_group" || ! $group_gid =~ ^[1-9][0-9]{0,9}$ ]] ||
        (( 10#$group_gid >= 4294967295 )); then
        printf 'logishell: Unsafe access group identity for %s.\n' "$access_group" >&2
        return 1
    fi
    if [[ -n $members && $members != "$access_user" ]]; then
        printf 'logishell: Access group %s has other members.\n' "$access_group" >&2
        return 1
    fi
    if ! groups=$(getent group) || [[ -z $groups ]]; then
        printf 'logishell: Cannot inspect group aliases for %s.\n' "$access_group" >&2
        return 1
    fi
    found=false
    while IFS= read -r group; do
        if [[ ! $group =~ ^([^:]+):([^:]*):([0-9]+):([^:]*)$ ]]; then
            printf 'logishell: Invalid group record while inspecting %s.\n' "$access_group" >&2
            return 1
        fi
        if [[ ${BASH_REMATCH[3]} == "$group_gid" ]]; then
            if [[ $group != "$record" ]]; then
                printf 'logishell: Access group %s shares its numeric GID or changed during inspection.\n' "$access_group" >&2
                return 1
            fi
            found=true
        fi
    done <<< "$groups"
    if [[ $found == false ]]; then
        printf 'logishell: Access group %s was absent from the group listing.\n' "$access_group" >&2
        return 1
    fi
    if ! accounts=$(getent passwd) || [[ -z $accounts ]]; then
        printf 'logishell: Cannot inspect primary groups for %s.\n' "$access_group" >&2
        return 1
    fi
    while IFS= read -r account; do
        if [[ ! $account =~ ^[^:]+:[^:]*:[0-9]+:([0-9]+):[^:]*:[^:]*:[^:]*$ ]]; then
            printf 'logishell: Invalid account record while inspecting %s.\n' "$access_group" >&2
            return 1
        fi
        primary_gid=${BASH_REMATCH[1]}
        if [[ $primary_gid == "$group_gid" ]]; then
            printf 'logishell: Access group %s is an account primary group.\n' "$access_group" >&2
            return 1
        fi
    done <<< "$accounts"
    access_group_present=true
    access_group_gid=$group_gid
}

prepare_access_rules() {
    local rule
    current_access_rules=false
    legacy_access_rules=false
    for rule in "${access_rules[@]}"; do
        sed "s/@LOGISHELL_UID@/$EUID/g" "$repo/packaging/$rule.in" > "$staging/$rule"
        # The previous templates differed only in their group and owner comment.
        sed -e '/^# logishell access owner UID: @LOGISHELL_UID@$/d' \
            -e "s/GROUP:=\"logitech\"/GROUP:=\"$legacy_access_group\"/g" \
            "$repo/packaging/$rule.in" > "$staging/legacy-$rule"
    done
}

inspect_access_rule() {
    local rule=$1 target=/etc/udev/rules.d/$1
    access_rule_kind=absent
    if [[ -e $target || -L $target ]]; then
        access_rule_kind=unrecognized
        if [[ -f $target && ! -L $target ]]; then
            if cmp -s "$staging/$rule" "$target"; then
                access_rule_kind=current
                current_access_rules=true
            elif cmp -s "$staging/legacy-$rule" "$target"; then
                access_rule_kind=legacy
                legacy_access_rules=true
            fi
        fi
    fi
}

write_access_migration() {
    printf '# logishell group migration\nUID=%s\nGID=%s\n' "$EUID" "${1:-$installed_access_gid}"
}

# Rule comments retain account ownership after the group loses its UID suffix.
# A short-lived record binds an interrupted rename to the original numeric GID;
# old rules alone must never authorize adoption of an existing logitech group.
inspect_access_installation() {
    local current_present current_gid legacy_present legacy_gid migration_gid
    installed_access_group=
    installed_access_gid=
    access_migration_present=false
    inspect_access_group "$access_group" || return 1
    current_present=$access_group_present current_gid=$access_group_gid
    inspect_access_group "$legacy_access_group" || return 1
    legacy_present=$access_group_present legacy_gid=$access_group_gid
    if [[ $current_present == true && $legacy_present == true ]]; then
        printf 'logishell: Both %s and %s exist; refusing an ambiguous installation.\n' "$access_group" "$legacy_access_group" >&2
        return 1
    fi
    if [[ $current_present == true ]]; then
        installed_access_group=$access_group installed_access_gid=$current_gid
    elif [[ $legacy_present == true ]]; then
        installed_access_group=$legacy_access_group installed_access_gid=$legacy_gid
    fi
    if [[ -e $access_migration || -L $access_migration ]]; then
        if [[ -f $access_migration && ! -L $access_migration ]]; then
            migration_gid=$(sed -n 's/^GID=//p' "$access_migration") || return 1
            if [[ $migration_gid =~ ^[1-9][0-9]{0,9}$ ]] &&
                (( 10#$migration_gid < 4294967295 )) &&
                [[ -z $installed_access_gid || $installed_access_gid == "$migration_gid" ]] &&
                cmp -s <(write_access_migration "$migration_gid") "$access_migration"; then
                # An absent group is safe too: uninstall may have stopped after
                # groupdel, leaving only this record to clean up.
                access_migration_present=true
                return 0
            fi
        fi
        printf 'logishell: Unrecognized group migration record: %s\n' "$access_migration" >&2
        return 1
    fi
    if [[ $current_access_rules == true && $legacy_access_rules == true ]] ||
        [[ $current_present == true && ( $current_access_rules == false || $legacy_access_rules == true ) ]] ||
        [[ $legacy_present == true && ( $legacy_access_rules == false || $current_access_rules == true ) ]]; then
        printf 'logishell: Refusing access groups without matching rules for this account.\n' >&2
        return 1
    fi
}
