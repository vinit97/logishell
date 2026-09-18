//! Exercise copied installation scripts with temporary files and mocked host commands.
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Output},
};

const UID: &str = "1001";
const GROUP: &str = "logitech:x:991:fixture\n";
const CURRENT: [&str; 2] = ["72-logishell.rules", "72-logishell-remap.rules"];
// Freeze the previously shipped rules so migration must recognize their exact
// contents, independently of how the installer renders its compatibility copy.
const LEGACY: [&str; 2] = [
    r#"# setup.sh grants raw Logitech access to one explicitly trusted account.
# Run after 70-uaccess.rules and before 73-seat-late.rules applies seat ACLs.
SUBSYSTEM=="hidraw", ATTRS{idVendor}=="046d", TAG-="uaccess", OWNER:="0", GROUP:="logishell-@LOGISHELL_UID@", MODE:="0660"
SUBSYSTEM=="hidraw", KERNELS=="0005:046D:*", TAG-="uaccess", OWNER:="0", GROUP:="logishell-@LOGISHELL_UID@", MODE:="0660"
"#,
    r#"# setup.sh grants machine-wide input injection to one explicitly trusted account.
# Keep TAG removal separate: udev's static-node pass treats TAG-= as a tag to add.
SUBSYSTEM=="misc", KERNEL=="uinput", TAG-="uaccess"
SUBSYSTEM=="misc", KERNEL=="uinput", OWNER:="0", GROUP:="logishell-@LOGISHELL_UID@", MODE:="0660", OPTIONS+="static_node=uinput"
"#,
];
const MIGRATION: &str = ".logishell-group-migration";

// PATH contains only this dispatcher. Privileged commands never reach the host;
// ordinary file changes are allowed only beneath this test's temporary root.
const COMMANDS: &str = r#"#!/usr/bin/bash
set -euo pipefail
name=${0##*/}
printf '%s' "$name" >> "$LOGISHELL_TEST_LOG"
printf '\t%s' "$@" >> "$LOGISHELL_TEST_LOG"
printf '\n' >> "$LOGISHELL_TEST_LOG"
fail() { printf 'Unexpected test command: %s\n' "$*" >&2; exit 97; }
if [[ -f $LOGISHELL_TEST_ROOT/fail-command ]]; then
    read -r failure < "$LOGISHELL_TEST_ROOT/fail-command"
    [[ "$name ${*: -1}" != "$failure" ]] || exit 96
fi
case $name in
    sudo)
        case ${1:-} in install|rm|udevadm|modprobe|groupadd|groupmod|gpasswd|groupdel) ;; *) fail "$@" ;; esac
        exec "$LOGISHELL_TEST_BIN/$1" "${@:2}"
        ;;
    id)
        [[ $* == -un ]] || fail "$@"
        printf 'fixture\n'
        ;;
    getent)
        case $* in
            'group logitech'|'group logishell-1001')
                [[ ! -e $LOGISHELL_TEST_ROOT/getent-error ]] || exit 1
                file=group
                if [[ $2 == logishell-1001 ]]; then file=legacy-group; fi
                [[ -e $LOGISHELL_TEST_ROOT/$file ]] || exit 2
                exec /usr/bin/cat "$LOGISHELL_TEST_ROOT/$file"
                ;;
            group)
                printf 'root:x:0:\n'
                if [[ -e $LOGISHELL_TEST_ROOT/group ]]; then /usr/bin/cat "$LOGISHELL_TEST_ROOT/group"; fi
                if [[ -e $LOGISHELL_TEST_ROOT/legacy-group ]]; then /usr/bin/cat "$LOGISHELL_TEST_ROOT/legacy-group"; fi
                if [[ -e $LOGISHELL_TEST_ROOT/group-alias ]]; then /usr/bin/cat "$LOGISHELL_TEST_ROOT/group-alias"; fi
                ;;
            passwd) exec /usr/bin/cat "$LOGISHELL_TEST_ROOT/passwd" ;;
            *) fail "$@" ;;
        esac
        ;;
    groupadd)
        [[ $* == '--system logitech' && ! -e $LOGISHELL_TEST_ROOT/group ]] || fail "$@"
        printf 'logitech:x:991:\n' > "$LOGISHELL_TEST_ROOT/group"
        ;;
    groupmod)
        [[ $* == '--new-name logitech logishell-1001' && ! -e $LOGISHELL_TEST_ROOT/group ]] || fail "$@"
        IFS=: read -r group password gid members < "$LOGISHELL_TEST_ROOT/legacy-group"
        [[ $group == logishell-1001 ]] || fail "$@"
        printf 'logitech:%s:%s:%s\n' "$password" "$gid" "$members" > "$LOGISHELL_TEST_ROOT/group"
        exec /usr/bin/rm -- "$LOGISHELL_TEST_ROOT/legacy-group"
        ;;
    gpasswd)
        [[ $* == '--add fixture logitech' ]] || fail "$@"
        IFS=: read -r group password gid members < "$LOGISHELL_TEST_ROOT/group"
        if [[ ,$members, != *,fixture,* ]]; then members=${members:+$members,}fixture; fi
        printf '%s:%s:%s:%s\n' "$group" "$password" "$gid" "$members" > "$LOGISHELL_TEST_ROOT/group"
        ;;
    groupdel)
        case $* in
            logitech) exec /usr/bin/rm -- "$LOGISHELL_TEST_ROOT/group" ;;
            logishell-1001) exec /usr/bin/rm -- "$LOGISHELL_TEST_ROOT/legacy-group" ;;
            *) fail "$@" ;;
        esac
        ;;
    cargo)
        [[ ${1:-} == build && ${2:-} == --locked ]] || fail "$@"
        ;;
    systemctl)
        [[ ${1:-} == --user ]] || fail "$@"
        ;;
    udevadm)
        if [[ $* == --version ]]; then
            printf '%s\n' "$LOGISHELL_TEST_UDEV_VERSION"
        else
            [[ $* == 'control --reload-rules' ||
                $* == 'trigger --subsystem-match=misc --sysname-match=uinput' ]] || fail "$@"
        fi
        ;;
    modprobe)
        [[ $* == uinput ]] || fail "$@"
        ;;
    install|rm|mv)
        for argument in "$@"; do
            case $name:$argument in
                install:-d|install:-m|install:-Dm|rm:-f|rm:-rf|mv:-fT|*:--) ;;
                *:"$LOGISHELL_TEST_ROOT"/*)
                    resolved=$(/usr/bin/readlink -m -- "$argument")
                    [[ $resolved == "$LOGISHELL_TEST_ROOT"/* ]] || fail "$name $*"
                    ;;
                *) [[ $name == install && $argument =~ ^0?[0-7]{3}$ ]] || fail "$name $*" ;;
            esac
        done
        exec "/usr/bin/$name" "$@"
        ;;
    mktemp)
        for argument in "$@"; do
            [[ $argument == -d || $argument == "$LOGISHELL_TEST_ROOT"/* ]] || fail "$@"
        done
        exec /usr/bin/mktemp "$@"
        ;;
    dirname|uname|cmp|sed|cat) exec "/usr/bin/$name" "$@" ;;
    *) fail "$name $*" ;;
esac
"#;

struct Installation {
    temporary: tempfile::TempDir,
    repo: PathBuf,
    home: PathBuf,
    rules: PathBuf,
    log: PathBuf,
}

impl Installation {
    fn new() -> Result<Self> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path();
        let repo = root.join("repo");
        let home = root.join("home");
        let rules = root.join("rules.d");
        let bin = root.join("bin");
        for path in [&repo, &home, &rules, &bin, &root.join("runtime")] {
            fs::create_dir_all(path)?;
        }
        let source = Path::new(env!("CARGO_MANIFEST_DIR"));
        copy_directory(&source.join("packaging"), &repo.join("packaging"))?;
        for script in ["setup.sh", "uninstall.sh", "packaging/access.sh"] {
            // Only the copies use temporary system paths and a simulated caller.
            // This also permits the same tests to run safely under a root runner.
            let contents = fs::read_to_string(source.join(script))?
                .replace("/etc/udev/rules.d", &rules.to_string_lossy())
                .replace("$EUID", "$LOGISHELL_TEST_UID");
            fs::write(repo.join(script), contents)?;
        }
        fs::copy(source.join("Cargo.toml"), repo.join("Cargo.toml"))?;
        fs::write(
            root.join("passwd"),
            "fixture:x:1001:1001::/home/fixture:/bin/bash\n",
        )?;
        fs::create_dir_all(repo.join("target/release"))?;
        fs::write(repo.join("target/release/logishell"), "mock executable\n")?;
        let dispatcher = bin.join("dispatch");
        fs::write(&dispatcher, COMMANDS)?;
        fs::set_permissions(&dispatcher, fs::Permissions::from_mode(0o755))?;
        for name in [
            "sudo",
            "cargo",
            "systemctl",
            "udevadm",
            "modprobe",
            "id",
            "getent",
            "groupadd",
            "groupmod",
            "gpasswd",
            "groupdel",
            "install",
            "rm",
            "mv",
            "mktemp",
            "dirname",
            "uname",
            "cmp",
            "sed",
            "cat",
        ] {
            symlink(&dispatcher, bin.join(name))?;
        }
        let log = root.join("commands");
        fs::write(&log, "")?;
        Ok(Self {
            temporary,
            repo,
            home,
            rules,
            log,
        })
    }

    fn run(&self, script: &str) -> Result<Output> {
        self.run_with_udev_version(script, "259")
    }

    fn run_with_udev_version(&self, script: &str, version: &str) -> Result<Output> {
        let root = self.temporary.path();
        Command::new("/usr/bin/bash")
            .arg(self.repo.join(script))
            .current_dir(root)
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", root.join("bin"))
            .env("TMPDIR", root)
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("LOGISHELL_TEST_ROOT", root)
            .env("LOGISHELL_TEST_BIN", root.join("bin"))
            .env("LOGISHELL_TEST_LOG", &self.log)
            .env("LOGISHELL_TEST_UID", UID)
            .env("LOGISHELL_TEST_UDEV_VERSION", version)
            .output()
            .with_context(|| format!("run copied {script}"))
    }

    fn expected(&self, rule: &str, uid: &str) -> Result<String> {
        Ok(
            fs::read_to_string(self.repo.join("packaging").join(format!("{rule}.in")))?
                .replace("@LOGISHELL_UID@", uid),
        )
    }

    fn commands(&self) -> Result<String> {
        Ok(fs::read_to_string(&self.log)?)
    }

    fn seed_rules(&self, legacy: bool) -> Result<()> {
        for (index, rule) in CURRENT.into_iter().enumerate() {
            let contents = if legacy {
                LEGACY[index].replace("@LOGISHELL_UID@", UID)
            } else {
                self.expected(rule, UID)?
            };
            fs::write(self.rules.join(rule), contents)?;
        }
        Ok(())
    }
}

fn copy_directory(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_directory(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn successful(output: &Output) -> Result<()> {
    ensure!(
        output.status.success(),
        "script failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[test]
fn installation_scopes_rules_to_one_uid_and_uninstalls_idempotently() -> Result<()> {
    let installation = Installation::new()?;
    let output = installation.run("setup.sh")?;
    successful(&output)?;
    assert!(String::from_utf8_lossy(&output.stdout).contains("Reboot"));
    for rule in CURRENT {
        let rendered = fs::read_to_string(installation.rules.join(rule))?;
        assert_eq!(rendered, installation.expected(rule, UID)?);
        assert!(rendered.contains("OWNER:=\"0\""));
        assert!(rendered.contains("GROUP:=\"logitech\""));
        assert!(rendered.contains("# logishell access owner UID: 1001\n"));
        assert!(rendered.contains("MODE:=\"0660\""));
        assert!(rendered.contains("TAG-=\"uaccess\""));
        assert!(!rendered.contains("TAG+=\"uaccess\""));
        assert!(!rendered.contains("@LOGISHELL_UID@"));
    }
    let uinput = fs::read_to_string(installation.rules.join(CURRENT[1]))?;
    let static_rule = uinput
        .lines()
        .find(|line| !line.starts_with('#') && line.contains("static_node=uinput"))
        .context("missing static uinput rule")?;
    // The static-node pass interprets TAG-= as a tag addition, unlike device events.
    assert!(!static_rule.contains("TAG"));
    assert!(uinput.lines().any(|line| {
        !line.starts_with('#') && line.contains("TAG-=\"uaccess\"") && line != static_rule
    }));
    let binary = installation.home.join(".local/bin/logishell");
    let service = installation
        .home
        .join(".local/share/systemd/user/logishell.service");
    assert_eq!(fs::read_to_string(&binary)?, "mock executable\n");
    assert_eq!(fs::metadata(&binary)?.permissions().mode() & 0o777, 0o755);
    assert!(service.is_file());
    let group = installation.temporary.path().join("group");
    assert_eq!(fs::read_to_string(&group)?, GROUP);
    assert!(!installation.commands()?.contains("\t--user\trestart\t"));
    // Updating an existing installation reuses its exclusive group and stops
    // the old service before changing access; fresh groups need a new login.
    fs::write(&installation.log, "")?;
    successful(&installation.run("setup.sh")?)?;
    assert_eq!(fs::read_to_string(&group)?, GROUP);
    let commands = installation.commands()?;
    assert!(!commands.contains("sudo\tgroupadd\t"));
    let stop = commands
        .find("systemctl\t--user\tstop\tlogishell.service")
        .context("update must stop the existing service")?;
    let permissions = commands
        .find("sudo\tinstall\t")
        .context("update must install rules")?;
    assert!(stop < permissions);
    assert!(!commands.contains("\t--user\trestart\t"));
    assert!(!commands.contains("\t--user\tis-active\t"));
    let config = installation.home.join(".config/logishell/config.toml");
    fs::create_dir_all(config.parent().context("configuration parent")?)?;
    fs::write(&config, "saved configuration\n")?;

    let output = installation.run("uninstall.sh")?;
    successful(&output)?;
    assert!(String::from_utf8_lossy(&output.stdout).contains("Reboot"));
    assert!(!binary.exists());
    assert!(!service.exists());
    assert!(!group.exists());
    for rule in CURRENT {
        assert!(!installation.rules.join(rule).exists());
    }
    assert_eq!(fs::read_to_string(config)?, "saved configuration\n");
    // Permissions and existing handles are cleared by the requested reboot;
    // uninstall must not reset the live device node while other sessions use it.
    let commands = installation.commands()?;
    assert!(!commands.contains("/dev/uinput"));
    assert!(commands.contains("systemctl\t--user\tdisable\t--now\tlogishell.service"));
    assert!(commands.contains("sudo\tgroupdel\tlogitech"));
    fs::write(&installation.log, "")?;
    successful(&installation.run("uninstall.sh")?)?;
    assert!(
        !installation
            .commands()?
            .lines()
            .any(|line| { line.starts_with("sudo\t") || line.starts_with("systemctl\t") })
    );
    Ok(())
}

#[test]
fn obsolete_rules_block_setup_and_are_preserved_by_uninstall() -> Result<()> {
    for rule in ["70-logishell.rules", "70-logishell-remap.rules"] {
        for dangling in [false, true] {
            let installation = Installation::new()?;
            let target = installation.rules.join(rule);
            let missing = installation.temporary.path().join("missing");
            if dangling {
                symlink(&missing, &target)?;
            } else {
                fs::write(&target, "# obsolete rule\n")?;
            }
            let output = installation.run("setup.sh")?;
            assert!(!output.status.success());
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("Remove the old device-access rule")
            );
            for rule in CURRENT {
                assert!(!installation.rules.join(rule).exists());
            }
            assert!(!installation.home.join(".local/bin/logishell").exists());
            successful(&installation.run("uninstall.sh")?)?;
            assert!(
                !installation
                    .commands()?
                    .lines()
                    .any(|line| { line.starts_with("sudo\t") || line.starts_with("cargo\t") })
            );
            if dangling {
                assert_eq!(fs::read_link(&target)?, missing);
            } else {
                assert_eq!(fs::read_to_string(&target)?, "# obsolete rule\n");
            }
        }
    }
    Ok(())
}

#[test]
fn unsupported_udev_stops_before_build_or_installation() -> Result<()> {
    for version in ["246", "unknown"] {
        let installation = Installation::new()?;
        let output = installation.run_with_udev_version("setup.sh", version)?;
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("247 or newer"));
        assert!(fs::read_dir(&installation.rules)?.next().is_none());
        assert!(
            !installation
                .commands()?
                .lines()
                .any(|line| { line.starts_with("sudo\t") || line.starts_with("cargo\t") })
        );
    }
    Ok(())
}

#[test]
fn foreign_modified_and_symlinked_rules_are_never_replaced_or_removed() -> Result<()> {
    for (index, rule) in CURRENT.into_iter().enumerate() {
        for alteration in [
            "foreign",
            "missing-owner",
            "modified",
            "symlink",
            "legacy-foreign",
            "legacy-modified",
            "legacy-symlink",
        ] {
            let installation = Installation::new()?;
            fs::write(installation.temporary.path().join("group"), GROUP)?;
            let target = installation.rules.join(rule);
            let uid = if alteration.ends_with("foreign") {
                "1002"
            } else {
                UID
            };
            let mut contents = if alteration.starts_with("legacy-") {
                LEGACY[index].replace("@LOGISHELL_UID@", uid)
            } else {
                installation.expected(rule, uid)?
            };
            if alteration == "missing-owner" {
                contents = contents.replace("# logishell access owner UID: 1001\n", "");
            }
            if alteration.ends_with("modified") {
                contents.push_str("# locally modified\n");
            }
            let referent = installation.temporary.path().join("symlink-target");
            if alteration.ends_with("symlink") {
                fs::write(&referent, &contents)?;
                symlink(&referent, &target)?;
            } else {
                fs::write(&target, &contents)?;
            }

            let output = installation.run("setup.sh")?;
            assert!(!output.status.success(), "accepted {alteration} {rule}");
            assert!(String::from_utf8_lossy(&output.stderr).contains("Refusing"));
            assert!(
                !installation
                    .commands()?
                    .lines()
                    .any(|line| { line.starts_with("sudo\t") || line.starts_with("cargo\t") })
            );
            assert!(!installation.home.join(".local/bin/logishell").exists());
            assert_eq!(fs::read_to_string(&target)?, contents);
            assert_eq!(
                fs::read_to_string(installation.temporary.path().join("group"))?,
                GROUP
            );

            fs::write(&installation.log, "")?;
            let output = installation.run("uninstall.sh")?;
            successful(&output)?;
            assert!(String::from_utf8_lossy(&output.stderr).contains("Keeping"));
            assert!(
                !installation
                    .commands()?
                    .lines()
                    .any(|line| line.starts_with("sudo\t"))
            );
            assert_eq!(fs::read_to_string(&target)?, contents);
            assert_eq!(
                fs::read_to_string(installation.temporary.path().join("group"))?,
                GROUP
            );
            if alteration.ends_with("symlink") {
                assert!(fs::symlink_metadata(&target)?.file_type().is_symlink());
                assert_eq!(fs::read_to_string(&referent)?, contents);
            }
        }
    }
    Ok(())
}

#[test]
fn unrecognized_or_unsafe_groups_are_never_used_or_deleted() -> Result<()> {
    for legacy in [false, true] {
        for state in [
            "foreign",
            "shared",
            "primary",
            "own-primary",
            "alias",
            "lookup-failure",
            "unrecognized",
            "zero-gid",
            "wrong-name",
        ] {
            let installation = Installation::new()?;
            let root = installation.temporary.path();
            let record = match state {
                "foreign" => "logitech:x:991:other\n",
                "shared" => "logitech:x:991:fixture,other\n",
                "zero-gid" => "logitech:x:0:fixture\n",
                "wrong-name" => "other:x:991:fixture\n",
                _ => GROUP,
            };
            let record = if legacy {
                record.replace("logitech", "logishell-1001")
            } else {
                record.to_owned()
            };
            let group = root.join(if legacy { "legacy-group" } else { "group" });
            fs::write(&group, &record)?;
            if state == "primary" || state == "own-primary" {
                fs::write(
                    root.join("passwd"),
                    if state == "primary" {
                        "other:x:1002:991::/home/other:/bin/bash\n"
                    } else {
                        "fixture:x:1001:991::/home/fixture:/bin/bash\n"
                    },
                )?;
            } else if state == "lookup-failure" {
                fs::write(root.join("getent-error"), "")?;
            } else if state == "alias" {
                fs::write(root.join("group-alias"), "other:x:991:other\n")?;
            }
            if state != "unrecognized" {
                installation.seed_rules(legacy)?;
            }
            let output = installation.run("setup.sh")?;
            assert!(
                !output.status.success(),
                "accepted {state} group (legacy={legacy})"
            );
            assert!(
                !installation
                    .commands()?
                    .lines()
                    .any(|line| { line.starts_with("sudo\t") || line.starts_with("cargo\t") })
            );
            // Own rules can be removed, but an unrecognized group or one that
            // changed owners/members must survive even when no rules remain.
            fs::write(&installation.log, "")?;
            successful(&installation.run("uninstall.sh")?)?;
            assert_eq!(fs::read_to_string(group)?, record);
            assert!(!installation.commands()?.contains("groupdel\t"));
        }
    }
    Ok(())
}

#[test]
fn migration_preserves_gid_and_can_resume_or_uninstall_after_interruption() -> Result<()> {
    for failure in [
        "none", "record", "rename", "hid", "uinput", "member", "cleanup",
    ] {
        for finish in ["setup.sh", "uninstall.sh"] {
            let installation = Installation::new()?;
            let root = installation.temporary.path();
            let legacy_group = root.join("legacy-group");
            let group = root.join("group");
            let marker = installation.rules.join(MIGRATION);
            fs::write(&legacy_group, "logishell-1001:x:963:fixture\n")?;
            installation.seed_rules(true)?;
            let service = installation
                .home
                .join(".local/share/systemd/user/logishell.service");
            fs::create_dir_all(service.parent().context("service parent")?)?;
            fs::copy(
                installation.repo.join("packaging/logishell.service"),
                &service,
            )?;
            let command = match failure {
                "record" => format!("install {}", marker.display()),
                "rename" => "groupmod logishell-1001".into(),
                "hid" => format!("install {}", installation.rules.join(CURRENT[0]).display()),
                "uinput" => format!("install {}", installation.rules.join(CURRENT[1]).display()),
                "member" => "gpasswd logitech".into(),
                "cleanup" => format!("rm {}", marker.display()),
                _ => String::new(),
            };
            if failure != "none" {
                fs::write(root.join("fail-command"), format!("{command}\n"))?;
                let output = installation.run("setup.sh")?;
                assert_eq!(output.status.code(), Some(96), "{failure}: {output:?}");
                fs::remove_file(root.join("fail-command"))?;
                let commands = installation.commands()?;
                let stop = commands
                    .find("systemctl\t--user\tstop\tlogishell.service")
                    .context("service stop")?;
                let change = commands.find("sudo\t").context("access change")?;
                assert!(stop < change);
                assert_eq!(marker.exists(), failure != "record");
                if marker.exists() {
                    assert_eq!(
                        fs::read_to_string(&marker)?,
                        "# logishell group migration\nUID=1001\nGID=963\n"
                    );
                }
            }
            fs::write(&installation.log, "")?;
            if finish == "uninstall.sh" && matches!(failure, "member" | "cleanup") {
                // Recovery must also work if uninstall removed the rules, or
                // both rules and group, before its final cleanup was interrupted.
                let command = if failure == "member" {
                    "groupdel logitech".to_owned()
                } else {
                    format!("rm {}", marker.display())
                };
                fs::write(root.join("fail-command"), format!("{command}\n"))?;
                let output = installation.run(finish)?;
                assert_eq!(output.status.code(), Some(96), "{failure}: {output:?}");
                assert!(marker.exists());
                assert_eq!(group.exists(), failure == "member");
                for rule in CURRENT {
                    assert!(!installation.rules.join(rule).exists());
                }
                fs::remove_file(root.join("fail-command"))?;
            }
            successful(&installation.run(finish)?)?;
            if finish == "setup.sh" {
                assert_eq!(fs::read_to_string(&group)?, "logitech:x:963:fixture\n");
                assert!(!legacy_group.exists());
                for rule in CURRENT {
                    assert_eq!(
                        fs::read_to_string(installation.rules.join(rule))?,
                        installation.expected(rule, UID)?
                    );
                }
                assert!(!installation.commands()?.contains("groupadd\t"));
                assert!(!installation.commands()?.contains("groupdel\t"));
                fs::write(&installation.log, "")?;
                successful(&installation.run("setup.sh")?)?;
                assert!(!installation.commands()?.contains("groupmod\t"));
                assert_eq!(fs::read_to_string(&group)?, "logitech:x:963:fixture\n");
                successful(&installation.run("uninstall.sh")?)?;
            }
            assert!(!marker.exists());
            assert!(!legacy_group.exists());
            assert!(!group.exists());
            for rule in CURRENT {
                assert!(!installation.rules.join(rule).exists());
            }
        }
    }
    Ok(())
}

#[test]
fn interrupted_fresh_installation_can_finish_creating_exclusive_group() -> Result<()> {
    for failure in ["groupadd logitech", "gpasswd logitech"] {
        let installation = Installation::new()?;
        let root = installation.temporary.path();
        fs::write(root.join("fail-command"), format!("{failure}\n"))?;
        let output = installation.run("setup.sh")?;
        assert_eq!(output.status.code(), Some(96), "{output:?}");
        fs::remove_file(root.join("fail-command"))?;
        successful(&installation.run("setup.sh")?)?;
        assert_eq!(fs::read_to_string(root.join("group"))?, GROUP);
        successful(&installation.run("uninstall.sh")?)?;
    }
    Ok(())
}

#[test]
fn collisions_and_unrecognized_migration_records_preserve_groups() -> Result<()> {
    for state in [
        "both",
        "legacy-rules",
        "current-rules",
        "mixed-rules",
        "foreign-marker",
        "wrong-gid",
        "symlink-marker",
    ] {
        let installation = Installation::new()?;
        let root = installation.temporary.path();
        let group = root.join("group");
        let legacy_group = root.join("legacy-group");
        let legacy_record = "logishell-1001:x:963:fixture\n";
        if state == "current-rules" {
            fs::write(&legacy_group, legacy_record)?;
        } else {
            fs::write(&group, GROUP)?;
        }
        if state == "both" {
            fs::write(&legacy_group, legacy_record)?;
        }
        installation.seed_rules(state == "legacy-rules" || state == "both")?;
        if state == "mixed-rules" {
            fs::write(
                installation.rules.join(CURRENT[0]),
                LEGACY[0].replace("@LOGISHELL_UID@", UID),
            )?;
        }
        let marker = installation.rules.join(MIGRATION);
        if state.ends_with("marker") || state == "wrong-gid" {
            let contents = format!(
                "# logishell group migration\nUID={}\nGID={}\n",
                if state == "foreign-marker" {
                    "1002"
                } else {
                    UID
                },
                if state == "wrong-gid" { "963" } else { "991" }
            );
            if state == "symlink-marker" {
                let referent = root.join("marker-target");
                fs::write(&referent, contents)?;
                symlink(referent, &marker)?;
            } else {
                fs::write(&marker, contents)?;
            }
        }
        let output = installation.run("setup.sh")?;
        assert!(!output.status.success(), "accepted {state}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("Refusing"));
        assert!(
            !installation
                .commands()?
                .lines()
                .any(|line| { line.starts_with("sudo\t") || line.starts_with("cargo\t") })
        );
        successful(&installation.run("uninstall.sh")?)?;
        assert!(!installation.commands()?.contains("groupdel\t"));
        if state != "current-rules" {
            assert_eq!(fs::read_to_string(group)?, GROUP);
        }
        if state == "both" || state == "current-rules" {
            assert_eq!(fs::read_to_string(legacy_group)?, legacy_record);
        }
        if state.ends_with("marker") || state == "wrong-gid" {
            assert!(marker.exists());
        }
    }
    Ok(())
}
