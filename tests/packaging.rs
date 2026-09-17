//! Exercise copied installation scripts with temporary files and mocked host commands.
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Output},
};

const UID: &str = "1001";
const GROUP: &str = "logishell-1001:x:991:fixture\n";
const CURRENT: [&str; 2] = ["72-logishell.rules", "72-logishell-remap.rules"];
const LEGACY: [&str; 2] = ["70-logishell.rules", "70-logishell-remap.rules"];
// These are the released bytes, including the remap rule's final blank line.
// Fixtures must not silently follow accidental edits to the migration references.
const LEGACY_CONTENTS: [&str; 2] = [
    r#"# Logitech USB devices and receivers (Bolt, Unifying, and other HID interfaces).
# uaccess grants the active local seat user access to Logitech hidraw interfaces.
# Feature discovery in logishell determines which operations each device supports.
SUBSYSTEM=="hidraw", ATTRS{idVendor}=="046d", TAG+="uaccess"

# Logitech Bluetooth HID devices, without a product/model allowlist.
SUBSYSTEM=="hidraw", KERNELS=="0005:046D:*", TAG+="uaccess"
"#,
    r#"# Optional: allow the active local seat user to create virtual input for button bindings.
# This grants input injection, not just Logitech access. Install only if remapping is wanted.
SUBSYSTEM=="misc", KERNEL=="uinput", TAG+="uaccess", OPTIONS+="static_node=uinput"

"#,
];

// PATH contains only this dispatcher. Privileged commands never reach the host;
// ordinary file changes are allowed only beneath this test's temporary root.
const COMMANDS: &str = r#"#!/usr/bin/bash
set -euo pipefail
name=${0##*/}
printf '%s' "$name" >> "$LOGISHELL_TEST_LOG"
printf '\t%s' "$@" >> "$LOGISHELL_TEST_LOG"
printf '\n' >> "$LOGISHELL_TEST_LOG"
fail() { printf 'Unexpected test command: %s\n' "$*" >&2; exit 97; }
case $name in
    sudo)
        case ${1:-} in install|rm|udevadm|modprobe|groupadd|gpasswd|groupdel) ;; *) fail "$@" ;; esac
        exec "$LOGISHELL_TEST_BIN/$1" "${@:2}"
        ;;
    id)
        [[ $* == -un ]] || fail "$@"
        printf 'fixture\n'
        ;;
    getent)
        case $* in
            'group logishell-1001')
                [[ ! -e $LOGISHELL_TEST_ROOT/getent-error ]] || exit 1
                [[ -e $LOGISHELL_TEST_ROOT/group ]] || exit 2
                exec /usr/bin/cat "$LOGISHELL_TEST_ROOT/group"
                ;;
            group)
                printf 'root:x:0:\n'
                if [[ -e $LOGISHELL_TEST_ROOT/group ]]; then /usr/bin/cat "$LOGISHELL_TEST_ROOT/group"; fi
                if [[ -e $LOGISHELL_TEST_ROOT/group-alias ]]; then /usr/bin/cat "$LOGISHELL_TEST_ROOT/group-alias"; fi
                ;;
            passwd) exec /usr/bin/cat "$LOGISHELL_TEST_ROOT/passwd" ;;
            *) fail "$@" ;;
        esac
        ;;
    groupadd)
        [[ $* == '--system logishell-1001' && ! -e $LOGISHELL_TEST_ROOT/group ]] || fail "$@"
        printf 'logishell-1001:x:991:\n' > "$LOGISHELL_TEST_ROOT/group"
        ;;
    gpasswd)
        [[ $* == '--add fixture logishell-1001' ]] || fail "$@"
        IFS=: read -r group password gid members < "$LOGISHELL_TEST_ROOT/group"
        if [[ ,$members, != *,fixture,* ]]; then members=${members:+$members,}fixture; fi
        printf '%s:%s:%s:%s\n' "$group" "$password" "$gid" "$members" > "$LOGISHELL_TEST_ROOT/group"
        ;;
    groupdel)
        [[ $* == logishell-1001 ]] || fail "$@"
        exec /usr/bin/rm -- "$LOGISHELL_TEST_ROOT/group"
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
    install|rm|mv|cp)
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
            "gpasswd",
            "groupdel",
            "install",
            "rm",
            "mv",
            "cp",
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
        Ok(if CURRENT.contains(&rule) {
            fs::read_to_string(self.repo.join("packaging").join(format!("{rule}.in")))?
                .replace("@LOGISHELL_UID@", uid)
        } else {
            LEGACY_CONTENTS[LEGACY
                .iter()
                .position(|name| *name == rule)
                .context("unknown legacy rule")?]
            .to_owned()
        })
    }

    fn commands(&self) -> Result<String> {
        Ok(fs::read_to_string(&self.log)?)
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
        assert!(rendered.contains("GROUP:=\"logishell-1001\""));
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
    assert!(commands.contains("sudo\tgroupdel\tlogishell-1001"));
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
fn exact_legacy_rules_are_migrated_or_removed() -> Result<()> {
    for script in ["setup.sh", "uninstall.sh"] {
        let installation = Installation::new()?;
        for rule in LEGACY {
            assert_eq!(
                fs::read_to_string(installation.repo.join("packaging/legacy").join(rule))?,
                installation.expected(rule, UID)?,
                "legacy reference changed: {rule}"
            );
            fs::write(
                installation.rules.join(rule),
                installation.expected(rule, UID)?,
            )?;
        }
        successful(&installation.run(script)?)?;
        for rule in LEGACY {
            assert!(!installation.rules.join(rule).exists());
        }
        for rule in CURRENT {
            if script == "setup.sh" {
                assert_eq!(
                    fs::read_to_string(installation.rules.join(rule))?,
                    installation.expected(rule, UID)?
                );
            } else {
                assert!(!installation.rules.join(rule).exists());
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
    for rule in CURRENT.into_iter().chain(LEGACY) {
        for alteration in ["foreign", "modified", "symlink"] {
            if alteration == "foreign" && LEGACY.contains(&rule) {
                continue;
            }
            let installation = Installation::new()?;
            fs::write(installation.temporary.path().join("group"), GROUP)?;
            let target = installation.rules.join(rule);
            let mut contents =
                installation.expected(rule, if alteration == "foreign" { "1002" } else { UID })?;
            if alteration == "modified" {
                contents.push_str("# locally modified\n");
            }
            let referent = installation.temporary.path().join("symlink-target");
            if alteration == "symlink" {
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
            if alteration == "symlink" {
                assert!(fs::symlink_metadata(&target)?.file_type().is_symlink());
                assert_eq!(fs::read_to_string(&referent)?, contents);
            }
        }
    }
    Ok(())
}

#[test]
fn unrecognized_or_unsafe_groups_are_never_used_or_deleted() -> Result<()> {
    for state in [
        "foreign",
        "shared",
        "primary",
        "alias",
        "lookup-failure",
        "unrecognized",
    ] {
        let installation = Installation::new()?;
        let root = installation.temporary.path();
        let record = match state {
            "foreign" => "logishell-1001:x:991:other\n",
            "shared" => "logishell-1001:x:991:fixture,other\n",
            _ => GROUP,
        };
        fs::write(root.join("group"), record)?;
        if state == "primary" {
            fs::write(
                root.join("passwd"),
                "other:x:1002:991::/home/other:/bin/bash\n",
            )?;
        } else if state == "lookup-failure" {
            fs::write(root.join("getent-error"), "")?;
        } else if state == "alias" {
            fs::write(root.join("group-alias"), "other:x:991:other\n")?;
        }
        if state != "unrecognized" {
            for rule in CURRENT {
                fs::write(
                    installation.rules.join(rule),
                    installation.expected(rule, UID)?,
                )?;
            }
        }
        let output = installation.run("setup.sh")?;
        assert!(!output.status.success(), "accepted {state} group");
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
        assert_eq!(fs::read_to_string(root.join("group"))?, record);
        assert!(!installation.commands()?.contains("groupdel\t"));
    }
    Ok(())
}
