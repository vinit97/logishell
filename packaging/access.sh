#!/usr/bin/env bash
# Shared checks for the single-account raw-device access group.
# Source after setup/uninstall has checked EUID and defined fail().

access_group=logishell-$EUID
access_user=$(id -un) || fail 'Cannot determine the installing account.'
[[ -n $access_user && $access_user != *[:,[:space:]]* ]] ||
    fail 'The installing account has an unsupported name.'

inspect_access_group() {
    local record result group_name group_gid members groups group found accounts account primary_gid
    access_group_present=false
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
}
