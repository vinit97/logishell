//! CLI behavior and the real setup wizard with simulated devices.
//! Mock writes stay in temporary files shared with the PTY child.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    fs::{self, File},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

// Compile the real menu flow against a test-only backend, never physical hardware.
use logishell::{config, model, remap, terminal};
#[path = "support/wizard.rs"]
mod wizard;

mod cli {
    use crate::model::Device;
    use crate::terminal::{Choice, PairingUi};
    use anyhow::{Context, Result, bail, ensure};
    use serde_json::{Value, json};
    use std::{fs::OpenOptions, io::Write, path::PathBuf, time::Duration};

    pub enum PairVia {
        Bluetooth,
        Bolt,
        Unifying,
    }

    pub enum ConnectionAction {
        Connect,
        Disconnect,
        Unpair,
    }

    pub fn device_details(value: &Value, config: &crate::config::Config) -> Result<String> {
        if let Some(error) = value["settings_error"].as_str() {
            return Ok(format!(
                "{}\nDevice ID: {}\n{error}",
                value["device"]["name"], value["device"]["id"]
            ));
        }
        let controls = value["details"]
            .as_array()
            .context("fixture controls")?
            .iter()
            .map(|control| control.as_str().context("fixture control"))
            .collect::<Result<Vec<_>>>()?
            .join("\n");
        let action = config
            .devices
            .get(super::DEVICE)
            .and_then(|saved| crate::remap::saved_binding(&saved.bindings, "haptic"));
        Ok(format!(
            "{}\nWheel mode: {}\nSaved haptic: {}\n{controls}\nEnd of device details",
            value["device"]["name"].as_str().context("fixture name")?,
            value["settings"]["wheel-mode"]
                .as_str()
                .context("fixture wheel mode")?,
            action.unwrap_or("Default"),
        ))
    }

    pub async fn device_action(device: &Device, action: ConnectionAction) -> Result<()> {
        ensure!(device.id == super::DEVICE, "unexpected connection target");
        let path =
            PathBuf::from(std::env::var_os("LOGISHELL_TEST_CONFIG").context("fixture config")?);
        let operation = match action {
            ConnectionAction::Connect => "connect",
            ConnectionAction::Disconnect => "disconnect",
            ConnectionAction::Unpair => "unpair",
        };
        let success = std::env::var("LOGISHELL_TEST_PAIR_SCENARIO")? != "connection-error";
        writeln!(
            OpenOptions::new()
                .append(true)
                .open(path.with_file_name("connection-events.jsonl"))?,
            "{}",
            json!({"operation":operation, "device":device.id, "success":success})
        )?;
        ensure!(success, "simulated connection failure");
        Ok(())
    }
    pub async fn pair_with_ui(via: PairVia, receiver: Option<String>, ui: PairingUi) -> Result<()> {
        let scenario = std::env::var("LOGISHELL_TEST_PAIR_SCENARIO")?;
        let path =
            PathBuf::from(std::env::var_os("LOGISHELL_TEST_CONFIG").context("fixture config")?);
        let mut events = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path.with_file_name("pairing-events.jsonl"))?;
        let transport = match via {
            PairVia::Bluetooth => "bluetooth",
            PairVia::Bolt => "bolt",
            PairVia::Unifying => "unifying",
        };
        writeln!(
            events,
            "{}",
            json!({"stage":"start", "transport":transport, "receiver":receiver})
        )?;
        let result = async {
            ui.status(
                "Finding pairing devices",
                "Looking for simulated devices only.",
            )?;
            let selected = ui
                .choose(
                    "Choose pairing device",
                    "Select the simulated device to pair.",
                    vec![
                        Choice {
                            label: "Simulated mouse".into(),
                            detail: "First pairing candidate".into(),
                        },
                        Choice {
                            label: "Simulated keyboard".into(),
                            detail: "Second pairing candidate".into(),
                        },
                    ],
                )
                .await?
                .context("simulated pairing cancelled during selection")?;
            writeln!(events, "{}", json!({"stage":"selected", "device":selected}))?;
            ui.status(
                "Pairing authentication",
                "Type 123456 on the simulated keyboard. Esc cancels pairing.",
            )?;
            if scenario == "cancel-auth" {
                ui.cancelled_signal().await;
                ensure!(
                    ui.cancelled(),
                    "pairing UI cancellation signal was not recorded"
                );
                bail!("simulated authentication cancelled");
            }
            let confirmation = ui
                .input(
                    "Confirm simulated authentication",
                    "Enter 123456 to finish this test-only pairing.",
                )
                .await?
                .context("simulated authentication input cancelled")?;
            ensure!(
                confirmation == "123456",
                "wrong simulated authentication code"
            );
            writeln!(events, "{}", json!({"stage":"authenticated"}))?;
            if scenario == "error" {
                bail!("simulated receiver rejected pairing");
            }
            ui.status(
                "Finishing pairing",
                "Checking the simulated pairing record.",
            )?;
            Ok(())
        }
        .await;
        // Model asynchronous receiver cleanup. The setup screen must wait for
        // this marker before accepting another action or leaving its screen.
        tokio::time::sleep(Duration::from_millis(30)).await;
        writeln!(
            events,
            "{}",
            json!({"stage":"cleanup", "success":result.is_ok()})
        )?;
        events.sync_all()?;
        result
    }
}

mod runtime {
    use super::{DEVICE, SECOND_DEVICE, read_events, writes};
    use anyhow::{Context, Result, bail, ensure};
    use logishell::{
        config,
        model::{Device, Inventory},
    };
    use serde_json::{Value, json};
    use std::{
        collections::BTreeMap,
        fs::OpenOptions,
        io::Write,
        path::{Path, PathBuf},
    };

    fn check_target(path: &Path, selector: &str) -> Result<()> {
        ensure!(
            Some(path)
                == std::env::var_os("LOGISHELL_TEST_CONFIG")
                    .map(PathBuf::from)
                    .as_deref(),
            "wizard used a different config"
        );
        ensure!(
            selector == DEVICE
                || (selector == SECOND_DEVICE
                    && std::env::var("LOGISHELL_TEST_PAIR_SCENARIO")? == "aliases"),
            "unexpected device target"
        );
        Ok(())
    }

    fn fail_once(path: &Path, operation: &str) -> Result<()> {
        let marker = path.with_file_name(format!("{operation}-failed"));
        if std::env::var("LOGISHELL_TEST_PAIR_SCENARIO")? == format!("{operation}-fails-once")
            && !marker.exists()
        {
            std::fs::write(marker, "failed")?;
            bail!("simulated {operation} failure");
        }
        Ok(())
    }

    fn record_read(path: &Path, operation: &str) -> Result<()> {
        writeln!(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(path.with_file_name("device-reads.jsonl"))?,
            "{}",
            json!({"operation":operation})
        )?;
        Ok(())
    }

    pub async fn inventory() -> Result<Inventory> {
        let path =
            PathBuf::from(std::env::var_os("LOGISHELL_TEST_CONFIG").context("fixture config")?);
        record_read(&path, "inventory")?;
        let events = read_events(&path, "connection-events.jsonl")?;
        let mut inventory: Inventory = serde_json::from_value(json!({
            "devices":[{"id":DEVICE, "name":"Test mouse", "transport":"bolt", "state":"online",
                "receiver_id":"test-only-receiver", "slot":1, "capabilities":["wheel-mode"], "warnings":[]}],
            "receivers":[
                {"id":"test-bolt-one", "name":"Test Bolt receiver", "transport":"bolt", "hid_path":"test-only-first"},
                {"id":"test-bolt-two", "name":"Test Bolt receiver", "transport":"bolt", "hid_path":"test-only-second"}
            ],
            "warnings":[]
        }))?;
        if events
            .iter()
            .any(|event| event["operation"] == "unpair" && event["success"] == true)
        {
            inventory.devices.clear();
        } else if std::env::var("LOGISHELL_TEST_PAIR_SCENARIO")? == "bluetooth" {
            let device = &mut inventory.devices[0];
            device.transport = logishell::model::Transport::Bluetooth;
            device.receiver_id = None;
            device.slot = None;
            device.bluetooth_address = Some("AA:BB:CC:DD:EE:FF".into());
            device.state = if events
                .last()
                .is_some_and(|event| event["operation"] == "disconnect")
            {
                logishell::model::DeviceState::Offline
            } else {
                logishell::model::DeviceState::Online
            };
        }
        if std::env::var("LOGISHELL_TEST_PAIR_SCENARIO")? == "aliases" {
            let mut second = inventory.devices[0].clone();
            second.id = SECOND_DEVICE.into();
            second.name = "Test keyboard".into();
            second.slot = Some(2);
            inventory.devices.push(second);
        }
        Ok(inventory)
    }

    pub async fn inspect_selected(path: &Path, device: &Device) -> Result<Value> {
        check_target(path, &device.id)?;
        record_read(path, "settings")?;
        fail_once(path, "settings")?;
        ensure!(
            !read_events(path, "connection-events.jsonl")?.last()
                .is_some_and(|event| event["operation"] == "disconnect" && event["success"] == true),
            "simulated device is offline"
        );
        // Simulated hardware follows writes, not edits to the config file.
        let events = writes(path)?;
        let wheel = events
            .iter()
            .rev()
            .find(|event| event["operation"] == "set" && event["key"] == "wheel-mode")
            .map(|event| event["value"].clone())
            .unwrap_or(json!("ratchet"));
        let details: Vec<_> = (0..60)
            .map(|index| format!("Test control {index:02}"))
            .collect();
        let mut result = json!({"device":device, "settings":{"wheel-mode":wheel}, "controls":[], "details":details});
        if std::env::var("LOGISHELL_TEST_PAIR_SCENARIO")?.starts_with("bindings") {
            let strength = events
                .into_iter()
                .rev()
                .find(|event| event["operation"] == "set" && event["key"] == "haptic-strength")
                .and_then(|event| event["value"].as_str()?.parse::<u8>().ok())
                .unwrap_or(50);
            result["settings"]["haptic-strength"] = json!(strength);
            result["controls"] = json!([
                {"cid":416,"source":"haptic","task_id":416,"reprogrammable":true,"divertible":true,"virtual_control":false},
                {"cid":83,"source":"back","task_id":83,"reprogrammable":true,"divertible":true,"virtual_control":false},
                {"cid":80,"source":"left","task_id":80,"reprogrammable":false,"divertible":false,"virtual_control":false}
            ]);
            if let Err(error) = fail_once(path, "bindings-controls") {
                result["controls"] = json!([]);
                result["controls_error"] = json!(error.to_string());
            }
        }
        if std::env::var("LOGISHELL_TEST_PAIR_SCENARIO")? == "thumb" {
            result["device"]["capabilities"] = json!(["thumb-wheel"]);
            result["settings"]["thumb-wheel-invert"] = json!(false);
        }
        Ok(result)
    }

    fn update(
        path: &Path,
        selector: &str,
        edit: impl FnOnce(&mut config::DeviceConfig),
        event: Value,
    ) -> Result<Value> {
        check_target(path, selector)?;
        let mut saved = config::load(path)?;
        edit(saved.devices.entry(selector.into()).or_default());
        config::save(path, &saved)?;
        writeln!(
            OpenOptions::new()
                .append(true)
                .open(path.with_file_name("device-writes.jsonl"))?,
            "{event}"
        )?;
        Ok(Value::Null)
    }

    pub struct CommandContext;

    impl CommandContext {
        pub async fn open(path: &Path) -> Result<Self> {
            check_target(path, DEVICE)?;
            inventory().await?;
            Ok(Self)
        }

        pub async fn set(
            &self,
            path: &Path,
            selector: &str,
            key: &str,
            value: &str,
            temporary: bool,
        ) -> Result<Value> {
            ensure!(
                matches!(key, "wheel-mode" | "haptic-strength" | "thumb-wheel-invert")
                    && !temporary,
                "unexpected setting write"
            );
            update(
                path,
                selector,
                |device| config::record_setting(device, key, value),
                json!({"operation":"set", "selector":selector, "key":key, "value":value, "temporary":temporary}),
            )
        }

        pub fn aliases(&self, path: &Path, aliases: &BTreeMap<String, String>) -> Result<()> {
            if aliases.is_empty() {
                return Ok(());
            }
            fail_once(path, "alias")?;
            let mut saved = config::load(path)?;
            for (selector, alias) in aliases {
                check_target(path, selector)?;
                saved.devices.entry(selector.clone()).or_default().alias = Some(alias.clone());
            }
            config::save(path, &saved)?;
            let mut events = OpenOptions::new()
                .append(true)
                .open(path.with_file_name("device-writes.jsonl"))?;
            for (selector, alias) in aliases {
                writeln!(
                    events,
                    "{}",
                    json!({"operation":"alias", "selector":selector, "alias":alias})
                )?;
            }
            Ok(())
        }

        pub async fn update_bindings(
            &self,
            path: &Path,
            selector: &str,
            changes: &std::collections::BTreeMap<String, Option<String>>,
        ) -> Result<Value> {
            fail_once(path, "bindings-update")?;
            update(
                path,
                selector,
                |device| {
                    device.bindings = logishell::remap::updated_bindings(&device.bindings, changes)
                        .expect("valid simulated binding changes");
                },
                json!({"operation":"bindings", "selector":selector, "changes":changes}),
            )
        }
    }

    pub async fn reload_if_running(path: &Path) -> Result<()> {
        check_target(path, DEVICE)?;
        writeln!(
            OpenOptions::new()
                .append(true)
                .open(path.with_file_name("reload-events.jsonl"))?,
            "{}",
            json!({"writes":writes(path)?.len()})
        )?;
        if std::env::var("LOGISHELL_TEST_PAIR_SCENARIO")? == "bindings-reload-fails-once" {
            fail_once(path, "bindings-reload")
        } else {
            fail_once(path, "reload")
        }
    }
}

#[test]
#[ignore = "child process entry point used only by the PTY tests"]
fn wizard_fixture_entry() -> Result<()> {
    let config = PathBuf::from(
        std::env::var_os("LOGISHELL_TEST_CONFIG").context("missing fixture configuration")?,
    );
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(wizard::run(&config))
}

const DEVICE: &str = "test-only-wizard-mouse";
const SECOND_DEVICE: &str = "test-only-wizard-second";

fn writes(config: &Path) -> Result<Vec<Value>> {
    read_events(config, "device-writes.jsonl")
}

fn read_counts(config: &Path) -> Result<(usize, usize)> {
    let events = read_events(config, "device-reads.jsonl")?;
    Ok((
        events
            .iter()
            .filter(|event| event["operation"] == "inventory")
            .count(),
        events
            .iter()
            .filter(|event| event["operation"] == "settings")
            .count(),
    ))
}

fn read_events(config: &Path, file: &str) -> Result<Vec<Value>> {
    fs::read_to_string(config.with_file_name(file))?
        .lines()
        .map(|line| serde_json::from_str(line).map_err(Into::into))
        .collect()
}

fn assert_draft_unchanged(config: &Path) -> Result<()> {
    ensure!(
        writes(config)?.is_empty(),
        "setup applied pending device changes"
    );
    ensure!(
        fs::read_to_string(config)? == "schema_version = 1\n",
        "setup saved its pending draft"
    );
    ensure!(
        read_events(config, "reload-events.jsonl")?.is_empty(),
        "setup reloaded the daemon before final confirmation"
    );
    Ok(())
}

struct Pty {
    master: File,
    slave: File,
    child: Child,
    original: libc::termios,
    output: String,
}

impl Pty {
    fn start(config: &Path, pairing_scenario: &str) -> Result<Self> {
        let (mut master, mut slave) = (-1, -1);
        let size = libc::winsize {
            ws_row: 32,
            ws_col: 110,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: all pointers refer to valid outputs or an initialized winsize.
        ensure!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    &size,
                )
            } == 0,
            "openpty: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: openpty transferred two distinct, owned file descriptors.
        let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
        let original = Self::attributes(&slave)?;
        let mut command = Command::new(std::env::current_exe()?);
        command
            .env("LOGISHELL_TEST_CONFIG", config)
            .env("LOGISHELL_TEST_PAIR_SCENARIO", pairing_scenario)
            .env("TERM", "xterm-256color")
            .args([
                "--exact",
                "wizard_fixture_entry",
                "--ignored",
                "--nocapture",
            ])
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave.try_clone()?));
        // SAFETY: this child-only hook uses only async-signal-safe syscalls.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn()?;
        Ok(Self {
            master,
            slave,
            child,
            original,
            output: String::new(),
        })
    }

    fn attributes(file: &File) -> Result<libc::termios> {
        let mut attributes = std::mem::MaybeUninit::uninit();
        // SAFETY: tcgetattr initializes the provided termios on success.
        ensure!(
            unsafe { libc::tcgetattr(file.as_raw_fd(), attributes.as_mut_ptr()) } == 0,
            "tcgetattr failed"
        );
        // SAFETY: successful tcgetattr initialized this value.
        Ok(unsafe { attributes.assume_init() })
    }

    fn collect(&mut self) -> Result<()> {
        let mut descriptor = libc::pollfd {
            fd: self.master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one initialized descriptor is valid for the duration of poll.
        let ready = unsafe { libc::poll(&mut descriptor, 1, 20) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                return Ok(());
            }
            return Err(error.into());
        }
        if ready > 0 && descriptor.revents & libc::POLLIN != 0 {
            let mut buffer = [0; 16384];
            let length = self.master.read(&mut buffer)?;
            self.output
                .push_str(&String::from_utf8_lossy(&buffer[..length]));
        }
        Ok(())
    }

    fn until(&mut self, from: usize, text: &str) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.output[from..].contains(text) {
            self.collect()?;
            ensure!(
                Instant::now() < deadline,
                "menu did not show {text:?}; output: {}",
                self.output
            );
            ensure!(
                self.child.try_wait()?.is_none(),
                "setup exited before {text:?}; output: {}",
                self.output
            );
        }
        Ok(())
    }

    fn press(&mut self, keys: &[u8], expected: &str) -> Result<()> {
        let mark = self.output.len();
        self.master.write_all(keys)?;
        self.until(mark, expected)
    }

    fn resize(&mut self, rows: u16, columns: u16, expected: &str) -> Result<()> {
        let mark = self.output.len();
        let size = libc::winsize {
            ws_row: rows,
            ws_col: columns,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: this ioctl receives our owned test PTY and an initialized winsize.
        ensure!(
            unsafe { libc::ioctl(self.slave.as_raw_fd(), libc::TIOCSWINSZ, &size) } == 0,
            "resize test terminal failed"
        );
        self.until(mark, expected)
    }

    fn finish(&mut self, keys: &[u8]) -> Result<ExitStatus> {
        self.master.write_all(keys)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            self.collect()?;
            if let Some(status) = self.child.try_wait()? {
                break status;
            }
            ensure!(
                Instant::now() < deadline,
                "setup did not exit; output: {}",
                self.output
            );
        };
        self.collect()?;
        let restored = Self::attributes(&self.slave)?;
        ensure!(
            restored.c_iflag == self.original.c_iflag
                && restored.c_oflag == self.original.c_oflag
                && restored.c_cflag == self.original.c_cflag
                && restored.c_lflag == self.original.c_lflag
                && restored.c_cc == self.original.c_cc,
            "setup left terminal modes changed"
        );
        ensure!(
            self.output.contains("\x1b[?1049l") && self.output.contains("\x1b[?25h"),
            "setup did not restore screen and cursor"
        );
        Ok(status)
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn stage_wheel_and_nickname(pty: &mut Pty) -> Result<()> {
    pty.until(0, "Test mouse")?;
    pty.press(b"\r", "Scroll wheel feel")?;
    pty.press(b"\r", "Smooth (free-spin)")?;
    pty.press(b"\x1b[B\r", "Device nickname")?;
    pty.press(b"\x1b[B\r", "Letters, numbers")?;
    pty.press(b"desk-mouse\r", "Scroll wheel feel")?;
    Ok(())
}

fn fixture() -> Result<(tempfile::TempDir, PathBuf, Pty)> {
    fixture_with_pairing("success")
}

fn fixture_with_pairing(scenario: &str) -> Result<(tempfile::TempDir, PathBuf, Pty)> {
    let directory = tempfile::Builder::new()
        .prefix("logishell-wizard-test-")
        .tempdir_in("/tmp")?;
    let config = directory.path().join("config.toml");
    fs::write(&config, "schema_version = 1\n")?;
    if scenario == "bindings-existing" {
        let mut saved = config::Config::default();
        saved
            .devices
            .entry(DEVICE.into())
            .or_default()
            .bindings
            .insert("cid:0x01a0".into(), "key:ctrl+c".into());
        config::save(&config, &saved)?;
    }
    if scenario == "aliases" {
        let mut saved = config::Config::default();
        for (id, alias) in [(DEVICE, "first"), (SECOND_DEVICE, "second")] {
            saved.devices.entry(id.into()).or_default().alias = Some(alias.into());
        }
        config::save(&config, &saved)?;
    }
    fs::write(config.with_file_name("device-writes.jsonl"), "")?;
    fs::write(config.with_file_name("connection-events.jsonl"), "")?;
    fs::write(config.with_file_name("reload-events.jsonl"), "")?;
    let pty = Pty::start(&config, scenario)?;
    Ok((directory, config, pty))
}

fn stage_binding_and_strength(pty: &mut Pty) -> Result<()> {
    pty.until(0, "Test mouse")?;
    pty.press(b"\r", "Haptic strength")?;
    pty.press(b"\r", "75%")?;
    pty.press(b"\x1b[B\r", "75% (draft)")?;
    pty.press(b"\x1b[B\x1b[B\r", "Haptic thumb pad")?;
    pty.press(b"\x1b[B\r", "Copy (Ctrl+C)")?;
    pty.press(b"\x1b[H\x1b[B\x1b[B\x1b[B\x1b[B\r", "Copy (Ctrl+C)")?;
    pty.press(b"\x1b", "Device nickname")?;
    pty.press(b"\x1b", "Review changes (2)")
}

#[test]
fn wizard_navigation_caches_reads_until_an_explicit_refresh() -> Result<()> {
    let (_directory, path, mut pty) = fixture()?;
    stage_wheel_and_nickname(&mut pty)?;
    ensure!(read_counts(&path)? == (1, 1));
    pty.press(b"\x1b", "Review changes (2)")?;
    pty.press(b"\r", "Smooth (free-spin) (draft)")?;
    ensure!(
        read_counts(&path)? == (1, 1),
        "revisiting scanned devices or reread settings"
    );
    pty.press(b"\x1b[F\x1b[A\r", "Wheel mode: ratchet")?;
    pty.press(b"\r", "Scroll wheel feel")?;
    ensure!(
        read_counts(&path)? == (1, 1),
        "details reread cached device information"
    );
    pty.press(b"\x1b", "Review changes (2)")?;
    pty.press(b"\x1b[F\r", "Apply changes")?;
    pty.press(b"\x1b", "Review changes (2)")?;
    pty.press(b"\x1b[H\r", "Smooth (free-spin) (draft)")?;
    ensure!(
        read_counts(&path)? == (1, 1),
        "back from review invalidated the draft cache"
    );

    let mark = pty.output.len();
    pty.press(b"\x1b[F\x1b[A\x1b[A\x1b[A\r", "Reading current settings...")?;
    pty.until(mark, "Scroll wheel feel")?;
    pty.press(b"\x1b", "Review changes (2)")?;
    ensure!(
        read_counts(&path)? == (1, 2),
        "settings refresh performed a full inventory scan"
    );
    let mark = pty.output.len();
    pty.press(b"\x1b[F\x1b[A\r", "Looking for your devices...")?;
    pty.until(mark, "Test mouse")?;
    pty.press(b"\x1b[H\r", "Smooth (free-spin) (draft)")?;
    ensure!(
        read_counts(&path)? == (2, 3),
        "device refresh failed to invalidate cached settings"
    );
    ensure!(
        !pty.output.contains("Stepped scrolling gives") && !pty.output.contains("Feel each step")
    );
    assert_draft_unchanged(&path)?;
    let _ = pty.finish(b"\x03")?;
    Ok(())
}

#[test]
fn wizard_cached_read_failures_wait_for_explicit_settings_refresh() -> Result<()> {
    for (scenario, error) in [
        ("settings-fails-once", "simulated settings failure"),
        (
            "bindings-controls-fails-once",
            "Remappable controls are unavailable",
        ),
    ] {
        let (_directory, path, mut pty) = fixture_with_pairing(scenario)?;
        pty.until(0, "Test mouse")?;
        pty.press(b"\r", error)?;
        if scenario.starts_with("bindings") {
            pty.until(0, "Haptic strength: 50%")?;
        }
        pty.press(b"\x1b", "Pair a new device")?;
        pty.press(b"\r", error)?;
        ensure!(read_counts(&path)? == (1, 1));
        pty.press(
            b"\x1b[F\x1b[A\r",
            if scenario.starts_with("bindings") {
                "Wheel mode: ratchet"
            } else {
                "Device ID:"
            },
        )?;
        pty.press(b"\r", "Refresh settings")?;
        ensure!(
            read_counts(&path)? == (1, 1),
            "failure details triggered another read"
        );
        pty.press(b"\x1b[F\x1b[A\x1b[A\x1b[A\r", "Scroll wheel feel")?;
        if scenario.starts_with("bindings") {
            pty.press(b"\x1b[H\x1b[B\x1b[B\r", "Haptic thumb pad")?;
        }
        ensure!(read_counts(&path)? == (1, 2));
        assert_draft_unchanged(&path)?;
        let _ = pty.finish(b"\x03")?;
    }
    Ok(())
}

#[test]
fn wizard_custom_strength_validates_and_avoids_staging_an_unchanged_value() -> Result<()> {
    let (_directory, path, mut pty) = fixture_with_pairing("bindings")?;
    pty.until(0, "Test mouse")?;
    pty.press(b"\r", "Haptic strength")?;
    pty.press(b"\r", "75%")?;
    pty.press(b"\x1b[F\x1b[A\r", "Whole number from 0 to 100.")?;
    pty.press(b"\x15101\r", "Invalid value.")?;
    let mark = pty.output.len();
    pty.press(b"\x15 +050 \r", "Haptic strength: 50%")?;
    ensure!(!pty.output[mark..].contains("(draft)"));
    pty.press(b"\x1b", "Done")?;
    ensure!(pty.finish(b"\x1b[F\r")?.success());
    assert_draft_unchanged(&path)
}

#[test]
fn wizard_binding_and_strength_changes_require_final_apply() -> Result<()> {
    for apply in [false, true] {
        let (_directory, path, mut pty) = fixture_with_pairing("bindings")?;
        stage_binding_and_strength(&mut pty)?;
        assert_draft_unchanged(&path)?;
        pty.press(b"\x1b[F\r", "Apply changes")?;
        if !apply {
            ensure!(pty.finish(b"\x1b[B\r")?.success());
            assert_draft_unchanged(&path)?;
            continue;
        }
        pty.press(b"\x1b", "Review changes (2)")?;
        assert_draft_unchanged(&path)?;
        pty.press(b"\x1b[F\r", "Apply changes")?;
        ensure!(pty.finish(b"\x1b[A\r")?.success());
        let saved = config::load(&path)?;
        ensure!(saved.devices[DEVICE].settings["haptic-strength"] == "75");
        ensure!(saved.devices[DEVICE].bindings["haptic"] == "key:ctrl+c");
        let reloads = read_events(&path, "reload-events.jsonl")?;
        ensure!(writes(&path)?.len() == 2 && reloads.len() == 1 && reloads[0]["writes"] == 2);
        ensure!(
            read_counts(&path)? == (2, 1),
            "Apply rescanned for each edit"
        );
    }
    Ok(())
}

#[test]
fn wizard_default_action_removes_an_existing_numeric_binding_only_on_apply() -> Result<()> {
    let (_directory, path, mut pty) = fixture_with_pairing("bindings-existing")?;
    let original = fs::read_to_string(&path)?;
    pty.until(0, "Test mouse")?;
    pty.press(b"\r", "Remappable controls")?;
    pty.press(b"\x1b[B\x1b[B\r", "Haptic thumb pad: Copy (Ctrl+C)")?;
    pty.press(b"\x1b[B\r", "Default (device behavior)")?;
    pty.press(b"\x1b[H\r", "Haptic thumb pad: Default")?;
    ensure!(fs::read_to_string(&path)? == original && writes(&path)?.is_empty());
    ensure!(read_events(&path, "reload-events.jsonl")?.is_empty());
    pty.press(b"\x1b", "Device nickname")?;
    pty.press(b"\x1b[F\x1b[A\r", "Saved haptic: key:ctrl+c")?;
    pty.press(b"\x1b", "Device nickname")?;
    pty.press(b"\x1b", "Review changes (1)")?;
    pty.press(b"\x1b[F\r", "Apply changes")?;
    ensure!(pty.finish(b"\x1b[A\r")?.success());
    ensure!(config::load(&path)?.devices[DEVICE].bindings.is_empty());
    ensure!(writes(&path)?.len() == 1);
    ensure!(read_events(&path, "reload-events.jsonl")?.len() == 1);
    Ok(())
}

#[test]
fn wizard_thumb_wheel_settings_and_actions_stay_draft_until_apply() -> Result<()> {
    for apply in [false, true] {
        let (_directory, path, mut pty) = fixture_with_pairing("thumb")?;
        pty.until(0, "Test mouse")?;
        let mark = pty.output.len();
        pty.press(b"\r", "Scroll wheel feel")?;
        let screen = &pty.output[mark..];
        ensure!(
            screen
                .find("Reverse thumb wheel direction")
                .context("thumb wheel setting")?
                < screen
                    .find("Scroll wheel feel")
                    .context("scroll wheel setting")?
        );
        pty.press(b"\r", "Off (selected)")?;
        pty.press(b"\x1b[H\r", "Reverse thumb wheel direction: On (draft)")?;
        pty.press(b"\x1b[B\x1b[B\r", "Thumb wheel right: Default")?;
        pty.press(b"\r", "Copy (Ctrl+C)")?;
        pty.press(
            b"\x1b[H\x1b[B\x1b[B\x1b[B\x1b[B\r",
            "Thumb wheel left: Copy (Ctrl+C) (draft)",
        )?;
        pty.until(0, "Thumb wheel right: No action (draft)")?;
        pty.press(b"\r", "Copy (Ctrl+C) (selected)")?;
        pty.press(b"\x1b[H\r", "Thumb wheel right: Default")?;
        assert_draft_unchanged(&path)?;
        pty.press(b"\r", "Copy (Ctrl+C)")?;
        pty.press(
            b"\x1b[H\x1b[B\x1b[B\x1b[B\x1b[B\r",
            "Thumb wheel right: No action (draft)",
        )?;
        assert_draft_unchanged(&path)?;
        pty.press(b"\x1b", "Device nickname")?;
        pty.press(b"\x1b", "Review changes (2)")?;
        pty.press(b"\x1b[F\r", "Apply changes")?;
        ensure!(
            pty.finish(if apply { b"\x1b[A\r" } else { b"\x1b[B\r" })?
                .success()
        );
        if apply {
            let saved = config::load(&path)?;
            ensure!(saved.devices[DEVICE].settings["thumb-wheel-invert"] == "on");
            ensure!(saved.devices[DEVICE].bindings["thumb-left"] == "key:ctrl+c");
            ensure!(writes(&path)?.len() == 2);
            ensure!(read_events(&path, "reload-events.jsonl")?.len() == 1);
        } else {
            assert_draft_unchanged(&path)?;
        }
    }
    Ok(())
}

#[test]
fn wizard_binding_batch_and_reload_failures_keep_only_unapplied_changes() -> Result<()> {
    for (scenario, error, first_writes) in [
        (
            "bindings-update-fails-once",
            "simulated bindings-update failure",
            1,
        ),
        (
            "bindings-reload-fails-once",
            "simulated bindings-reload failure",
            2,
        ),
    ] {
        let (_directory, path, mut pty) = fixture_with_pairing(scenario)?;
        stage_binding_and_strength(&mut pty)?;
        pty.press(b"\x1b[F\r", "Apply changes")?;
        pty.press(b"\x1b[A\r", error)?;
        ensure!(writes(&path)?.len() == first_writes);
        ensure!(pty.finish(b"\x1b[A\r")?.success());
        let saved = config::load(&path)?;
        ensure!(saved.devices[DEVICE].settings["haptic-strength"] == "75");
        ensure!(saved.devices[DEVICE].bindings["haptic"] == "key:ctrl+c");
        ensure!(writes(&path)?.len() == 2);
        ensure!(read_events(&path, "reload-events.jsonl")?.len() == 2);
        ensure!(read_counts(&path)? == (if first_writes == 1 { 3 } else { 2 }, 1));
    }
    Ok(())
}

#[test]
fn wizard_applies_staged_changes_only_after_explicit_final_confirmation() -> Result<()> {
    let (_directory, config, mut pty) = fixture()?;
    stage_wheel_and_nickname(&mut pty)?;
    pty.resize(28, 95, "Scroll wheel feel")?;
    assert_draft_unchanged(&config)?;
    pty.press(b"\x1b", "Pair a new device")?;
    pty.press(b"\x1b[F\r", "Apply changes")?;
    // Review defaults to Keep editing. Esc must return to the draft, without writes.
    pty.press(b"\x1b", "Pair a new device")?;
    assert_draft_unchanged(&config)?;
    pty.press(b"\x1b[F\r", "Apply changes")?;
    ensure!(pty.finish(b"\x1b[A\r")?.success(), "confirmed setup failed");
    let writes = writes(&config)?;
    ensure!(
        writes.len() == 2,
        "expected one setting and one nickname write: {writes:?}"
    );
    ensure!(
        writes
            .iter()
            .any(|w| w["operation"] == "set" && w["value"] == "free-spin"),
        "wrong setting applied"
    );
    ensure!(
        writes
            .iter()
            .any(|w| w["operation"] == "alias" && w["alias"] == "desk-mouse"),
        "nickname was not applied"
    );
    let saved = logishell::config::load(&config)?;
    ensure!(
        saved.devices[DEVICE].settings["wheel-mode"] == "free-spin",
        "final setting was not saved"
    );
    let reloads = read_events(&config, "reload-events.jsonl")?;
    ensure!(
        reloads.len() == 1 && reloads[0]["writes"] == 2,
        "setup must reload once after saving all changes: {reloads:?}"
    );
    Ok(())
}

#[test]
fn wizard_clears_an_unsaved_nickname_without_discarding_other_changes() -> Result<()> {
    let (_directory, path, mut pty) = fixture()?;
    stage_wheel_and_nickname(&mut pty)?;
    pty.press(b"\r", "Letters, numbers")?;
    pty.press(b"\x15\r", "Scroll wheel feel: Smooth (free-spin) (draft)")?;
    assert_draft_unchanged(&path)?;
    pty.press(b"\x1b", "Review changes (1)")?;
    pty.press(b"\x1b[F\r", "Apply changes")?;
    ensure!(pty.finish(b"\x1b[A\r")?.success());
    let saved = config::load(&path)?;
    ensure!(saved.devices[DEVICE].alias.is_none());
    ensure!(saved.devices[DEVICE].settings["wheel-mode"] == "free-spin");
    let events = writes(&path)?;
    ensure!(
        events.len() == 1 && events[0]["operation"] == "set",
        "cleared nickname was applied: {events:?}"
    );
    let reloads = read_events(&path, "reload-events.jsonl")?;
    ensure!(reloads.len() == 1 && reloads[0]["writes"] == 1);
    Ok(())
}

#[test]
fn wizard_applies_nickname_transfers_and_swaps_as_one_batch() -> Result<()> {
    for second_alias in ["third", "first"] {
        let (_directory, path, mut pty) = fixture_with_pairing("aliases")?;
        let original = fs::read(&path)?;
        pty.until(0, "Test mouse (first)")?;
        pty.press(b"\r", "Device nickname")?;
        pty.press(b"\x1b[H\x1b[B\r", "Letters, numbers")?;
        pty.press(b"\x15second\r", "Nickname: second")?;
        pty.press(b"\x1b", "Review changes (1)")?;
        pty.press(b"\x1b[B\r", "Device nickname")?;
        pty.press(b"\x1b[H\x1b[B\r", "Letters, numbers")?;
        pty.press(
            format!("\x15{second_alias}\r").as_bytes(),
            &format!("Nickname: {second_alias}"),
        )?;
        pty.press(b"\x1b", "Review changes (2)")?;
        pty.press(b"\x1b[F\r", "Apply changes")?;
        ensure!(fs::read(&path)? == original);
        ensure!(writes(&path)?.is_empty());
        ensure!(read_events(&path, "reload-events.jsonl")?.is_empty());
        ensure!(pty.finish(b"\x1b[A\r")?.success());
        let saved = config::load(&path)?;
        ensure!(saved.devices[DEVICE].alias.as_deref() == Some("second"));
        ensure!(saved.devices[SECOND_DEVICE].alias.as_deref() == Some(second_alias));
        let events = writes(&path)?;
        ensure!(events.len() == 2 && events.iter().all(|event| event["operation"] == "alias"));
        let reloads = read_events(&path, "reload-events.jsonl")?;
        ensure!(reloads.len() == 1 && reloads[0]["writes"] == 2);
    }
    Ok(())
}

#[test]
fn wizard_discard_and_control_c_preserve_configuration_and_restore_terminal() -> Result<()> {
    for discard in [true, false] {
        let (_directory, config, mut pty) = fixture()?;
        stage_wheel_and_nickname(&mut pty)?;
        if discard {
            pty.press(b"\x1b", "Pair a new device")?;
            pty.press(b"\x1b[F\r", "Apply changes")?;
            ensure!(pty.finish(b"\x1b[B\r")?.success(), "discard failed");
        } else {
            let _ = pty.finish(b"\x03")?;
        }
        assert_draft_unchanged(&config)?;
    }
    Ok(())
}

#[test]
fn wizard_retries_partial_apply_and_reload_without_repeating_saved_changes() -> Result<()> {
    for (scenario, first_writes, error) in [
        ("alias-fails-once", 1, "simulated alias failure"),
        ("reload-fails-once", 2, "simulated reload failure"),
    ] {
        let (_directory, config, mut pty) = fixture_with_pairing(scenario)?;
        stage_wheel_and_nickname(&mut pty)?;
        assert_draft_unchanged(&config)?;
        pty.press(b"\x1b", "Pair a new device")?;
        pty.press(b"\x1b[F\r", "Apply changes")?;
        pty.press(b"\x1b[A\r", error)?;
        let reloads = read_events(&config, "reload-events.jsonl")?;
        ensure!(
            writes(&config)?.len() == first_writes
                && reloads.len() == 1
                && reloads[0]["writes"] == first_writes,
            "partial changes were not reloaded for {scenario}: {reloads:?}"
        );
        if scenario == "reload-fails-once" {
            pty.press(b"\x1b", "Review changes (1)")?;
            pty.press(b"\x1b[F\r", "Reload saved configuration")?;
        } else {
            pty.press(b"\x1b", "Review changes (1)")?;
            let mark = pty.output.len();
            pty.press(b"\x1b[H\r", "Scroll wheel feel: Smooth (free-spin)")?;
            ensure!(!pty.output[mark..].contains("Smooth (free-spin) (draft)"));
            ensure!(
                read_counts(&config)? == (2, 2),
                "partial apply repeated discovery"
            );
            pty.press(b"\x1b", "Review changes (1)")?;
            pty.press(b"\x1b[F\r", "Apply changes")?;
        }
        ensure!(pty.finish(b"\x1b[A\r")?.success(), "retry failed");
        let reloads = read_events(&config, "reload-events.jsonl")?;
        ensure!(
            writes(&config)?.len() == 2 && reloads.len() == 2 && reloads[1]["writes"] == 2,
            "retry repeated saved changes or skipped reload for {scenario}: {reloads:?}"
        );
        let saved = config::load(&config)?;
        ensure!(saved.devices[DEVICE].settings["wheel-mode"] == "free-spin");
        ensure!(saved.devices[DEVICE].alias.as_deref() == Some("desk-mouse"));
    }
    Ok(())
}

fn begin_simulated_bolt_pairing(pty: &mut Pty) -> Result<()> {
    stage_wheel_and_nickname(pty)?;
    pty.press(b"\x1b", "Review changes (2)")?;
    pty.press(b"\x1b[B\r", "Logi Bolt receiver")?;
    pty.press(b"\x1b[B\r", "Choose a receiver")?;
    pty.press(b"\x1b[B\r", "Choose pairing device")
}

fn pairing_events(config: &Path) -> Result<Vec<Value>> {
    read_events(config, "pairing-events.jsonl")
}

fn assert_pairing_kept_draft_and_screen(config: &Path, pty: &Pty) -> Result<Vec<Value>> {
    assert_draft_unchanged(config)?;
    ensure!(
        pty.output.matches("\x1b[?1049h").count() == 1,
        "pairing re-entered the alternate screen"
    );
    ensure!(
        !pty.output.contains("\x1b[?1049l"),
        "pairing left the setup screen"
    );
    let events = pairing_events(config)?;
    ensure!(
        events.first().is_some_and(
            |event| event["transport"] == "bolt" && event["receiver"] == "test-bolt-two"
        ),
        "pairing did not use the selected transport and receiver: {events:?}"
    );
    ensure!(
        events
            .last()
            .is_some_and(|event| event["stage"] == "cleanup"),
        "setup resumed before pairing cleanup finished: {events:?}"
    );
    Ok(events)
}

#[test]
fn wizard_pairing_escape_and_back_cancel_selection_without_leaving_setup() -> Result<()> {
    for cancel in [b"\x1b".as_slice(), b"\x1b[F\r".as_slice()] {
        let (_directory, config, mut pty) = fixture_with_pairing("success")?;
        begin_simulated_bolt_pairing(&mut pty)?;
        pty.press(cancel, "Review changes (2)")?;
        let events = assert_pairing_kept_draft_and_screen(&config, &pty)?;
        ensure!(
            read_counts(&config)? == (1, 1),
            "cancelled pairing scanned devices"
        );
        ensure!(
            !events.iter().any(|event| event["stage"] == "selected"),
            "canceling device selection chose a candidate"
        );
        ensure!(
            events.last().context("cleanup event")?["success"] == false,
            "canceled pairing was reported as successful"
        );
        let _ = pty.finish(b"\x03")?;
        assert_draft_unchanged(&config)?;
    }
    Ok(())
}

#[test]
fn wizard_pairing_escape_during_authentication_waits_for_cleanup_and_preserves_draft() -> Result<()>
{
    let (_directory, config, mut pty) = fixture_with_pairing("cancel-auth")?;
    begin_simulated_bolt_pairing(&mut pty)?;
    pty.press(b"\x1b[B\r", "Pairing authentication")?;
    pty.press(b"\x1b", "Review changes (2)")?;
    let events = assert_pairing_kept_draft_and_screen(&config, &pty)?;
    ensure!(
        read_counts(&config)? == (1, 1),
        "cancelled authentication scanned devices"
    );
    ensure!(
        events
            .iter()
            .any(|event| event["stage"] == "selected" && event["device"] == 1),
        "authentication did not use the selected keyboard"
    );
    ensure!(
        events.last().context("cleanup event")?["success"] == false,
        "canceled authentication succeeded"
    );
    let _ = pty.finish(b"\x03")?;
    Ok(())
}

#[test]
fn wizard_pairing_control_c_finishes_cleanup_before_exiting() -> Result<()> {
    let (_directory, config, mut pty) = fixture_with_pairing("cancel-auth")?;
    begin_simulated_bolt_pairing(&mut pty)?;
    pty.press(b"\x1b[B\r", "Pairing authentication")?;
    let _ = pty.finish(b"\x03")?;
    let events = pairing_events(&config)?;
    ensure!(
        events
            .last()
            .is_some_and(|event| event["stage"] == "cleanup" && event["success"] == false),
        "Ctrl-C exited before cancelling and cleaning up pairing"
    );
    assert_draft_unchanged(&config)?;
    ensure!(
        pty.output.matches("\x1b[?1049h").count() == 1,
        "pairing re-entered the setup screen"
    );
    Ok(())
}

#[test]
fn wizard_pairing_success_and_error_keep_one_screen_and_never_apply_the_draft() -> Result<()> {
    for scenario in ["success", "error"] {
        let (_directory, config, mut pty) = fixture_with_pairing(scenario)?;
        begin_simulated_bolt_pairing(&mut pty)?;
        pty.press(b"\x1b[B\r", "Confirm simulated authentication")?;
        pty.press(b"123456\r", "Review changes (2)")?;
        let events = assert_pairing_kept_draft_and_screen(&config, &pty)?;
        ensure!(read_counts(&config)? == (if scenario == "success" { 2 } else { 1 }, 1));
        ensure!(
            events
                .iter()
                .any(|event| event["stage"] == "selected" && event["device"] == 1),
            "pairing used the wrong device"
        );
        ensure!(
            events.iter().any(|event| event["stage"] == "authenticated"),
            "pairing never received authentication input"
        );
        ensure!(
            events.last().context("cleanup event")?["success"] == (scenario == "success"),
            "incorrect pairing completion result"
        );
        let expected = if scenario == "success" {
            "Device paired."
        } else {
            "simulated receiver rejected pairing"
        };
        ensure!(
            pty.output.contains(expected),
            "setup omitted pairing result {expected:?}"
        );
        let _ = pty.finish(b"\x03")?;
        assert_draft_unchanged(&config)?;
    }
    Ok(())
}

fn open_connection(pty: &mut Pty) -> Result<()> {
    pty.press(b"\x1b[F\x1b[A\x1b[A\r", "Unpair device")
}

#[test]
fn wizard_device_details_scroll_and_back_preserve_pending_changes() -> Result<()> {
    let (_directory, config, mut pty) = fixture()?;
    stage_wheel_and_nickname(&mut pty)?;
    let mark = pty.output.len();
    pty.press(b"\x1b[F\x1b[A\r", "Wheel mode: ratchet")?;
    ensure!(
        !pty.output[mark..].contains("End of device details"),
        "long device details were not paged"
    );
    pty.press(b"\x1b[F", "End of device details")?;
    pty.press(b"\x1b[H", "Wheel mode: ratchet")?;
    pty.press(b"\r", "Scroll wheel feel: Smooth (free-spin) (draft)")?;
    pty.press(b"\x1b", "Review changes (2)")?;
    assert_draft_unchanged(&config)?;
    ensure!(
        read_events(&config, "connection-events.jsonl")?.is_empty(),
        "viewing details changed a device connection"
    );
    ensure!(
        !pty.output.contains("\x1b[?1049l"),
        "device details left the setup screen"
    );
    let _ = pty.finish(b"\x03")?;
    assert_draft_unchanged(&config)?;
    Ok(())
}

#[test]
fn wizard_unpair_requires_confirmation_then_discards_only_the_pending_draft() -> Result<()> {
    let (_directory, config, mut pty) = fixture()?;
    stage_wheel_and_nickname(&mut pty)?;
    open_connection(&mut pty)?;
    pty.press(b"\r", "Unpair device?")?;
    ensure!(
        pty.output.contains("pending changes will be discarded"),
        "unpair did not explain draft removal"
    );
    // Enter must select Cancel, preserving both the physical device and draft.
    pty.press(b"\r", "Connection")?;
    ensure!(
        read_events(&config, "connection-events.jsonl")?.is_empty(),
        "default confirmation unpaired a device"
    );
    pty.press(b"\x1b", "Scroll wheel feel")?;
    pty.press(b"\x1b", "Review changes (2)")?;
    assert_draft_unchanged(&config)?;
    ensure!(
        read_counts(&config)? == (1, 1),
        "cancelled unpair invalidated cache"
    );
    pty.press(b"\r", "Scroll wheel feel")?;
    open_connection(&mut pty)?;
    pty.press(b"\r", "Unpair device?")?;
    let mark = pty.output.len();
    pty.press(b"\x1b[B\r", "Device unpaired.")?;
    pty.until(mark, "Done")?;
    ensure!(
        read_counts(&config)? == (2, 1),
        "unpair did not refresh device inventory"
    );
    let events = read_events(&config, "connection-events.jsonl")?;
    ensure!(
        events.len() == 1 && events[0]["operation"] == "unpair",
        "wrong connection changes: {events:?}"
    );
    ensure!(
        !pty.output.contains("\x1b[?1049l"),
        "unpair left the setup screen"
    );
    assert_draft_unchanged(&config)?;
    ensure!(
        pty.finish(b"\x1b[F\r")?.success(),
        "setup did not close after unpair"
    );
    assert_draft_unchanged(&config)?;
    Ok(())
}

#[test]
fn wizard_failed_unpair_preserves_pending_settings_and_nickname() -> Result<()> {
    let (_directory, config, mut pty) = fixture_with_pairing("connection-error")?;
    stage_wheel_and_nickname(&mut pty)?;
    open_connection(&mut pty)?;
    pty.press(b"\r", "Unpair device?")?;
    pty.press(b"\x1b[B\r", "simulated connection failure")?;
    pty.press(b"\x1b", "Scroll wheel feel")?;
    pty.press(b"\x1b", "Review changes (2)")?;
    ensure!(read_counts(&config)? == (1, 1));
    assert_draft_unchanged(&config)?;
    let events = read_events(&config, "connection-events.jsonl")?;
    ensure!(
        events.len() == 1 && events[0]["success"] == false,
        "wrong failure events"
    );
    let _ = pty.finish(b"\x03")?;
    assert_draft_unchanged(&config)?;
    Ok(())
}

#[test]
fn wizard_bluetooth_connection_changes_preserve_the_pending_draft() -> Result<()> {
    let (_directory, config, mut pty) = fixture_with_pairing("bluetooth")?;
    stage_wheel_and_nickname(&mut pty)?;
    open_connection(&mut pty)?;
    let mark = pty.output.len();
    pty.press(b"\r", "Device disconnected.")?;
    pty.until(mark, "Review changes (2)")?;
    ensure!(read_counts(&config)? == (2, 1));
    pty.press(b"\r", "Settings are unavailable")?;
    open_connection(&mut pty)?;
    let mark = pty.output.len();
    pty.press(b"\r", "Device connected.")?;
    pty.until(mark, "Review changes (2)")?;
    ensure!(read_counts(&config)? == (3, 2));
    assert_draft_unchanged(&config)?;
    let events = read_events(&config, "connection-events.jsonl")?;
    ensure!(
        events.len() == 2
            && events[0]["operation"] == "disconnect"
            && events[1]["operation"] == "connect",
        "wrong Bluetooth actions: {events:?}"
    );
    let _ = pty.finish(b"\x03")?;
    assert_draft_unchanged(&config)?;
    Ok(())
}

fn command() -> Command {
    Command::new(env!("CARGO_BIN_EXE_logishell"))
}

fn command_at(home: &Path) -> Command {
    let mut command = command();
    command.env("HOME", home).env_remove("XDG_CONFIG_HOME");
    command
}

#[test]
fn help_needs_no_hardware_or_config() -> Result<()> {
    let output = command()
        .args(["help"])
        .env_remove("HOME")
        .env_remove("XDG_CONFIG_HOME")
        .output()?;
    ensure!(output.status.success(), "help failed");
    let help = String::from_utf8(output.stdout)?;
    for text in ["status", "setup", "config", "daemon"] {
        ensure!(help.contains(text), "missing command {text}");
    }
    ensure!(
        !help.contains("--json"),
        "removed JSON option is advertised"
    );
    let output = command()
        .args(["help", "config", "set"])
        .env_remove("HOME")
        .env_remove("XDG_CONFIG_HOME")
        .output()?;
    ensure!(output.status.success(), "nested command help failed");
    ensure!(
        String::from_utf8(output.stdout)?.contains("--temporary"),
        "nested help did not show config set options"
    );
    Ok(())
}

#[test]
fn removed_commands_and_options_are_rejected_before_any_runtime_work() -> Result<()> {
    for arguments in [
        vec!["device", "info", "mouse"],
        vec!["remap", "run"],
        vec!["system", "doctor"],
        vec!["doctor"],
        vec!["completions", "bash"],
        vec!["-h"],
        vec!["--help"],
        vec!["config", "set", "--help"],
        vec!["config", "help"],
        vec!["config", "path"],
        vec!["--json"],
        vec!["status", "--json"],
        vec!["setup", "--json"],
        vec!["daemon", "--json"],
        vec!["config", "check", "--json"],
        vec!["--config", "unused.toml", "config", "check"],
        vec!["status", "--watch", "--interval", "1"],
    ] {
        let output = command()
            .args(&arguments)
            .env_remove("HOME")
            .env_remove("XDG_CONFIG_HOME")
            .output()?;
        ensure!(
            output.status.code() == Some(2),
            "removed command accepted: {arguments:?}"
        );
        ensure!(
            output.stdout.is_empty(),
            "removed command produced output: {arguments:?}"
        );
    }
    Ok(())
}

#[test]
fn configuration_validation_reports_errors_and_keeps_files_intact() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let path = temporary.path().join(".config/logishell/config.toml");
    let output = command_at(temporary.path())
        .args(["config", "check"])
        .output()?;
    ensure!(
        output.status.success(),
        "missing configuration check failed"
    );
    ensure!(String::from_utf8(output.stdout)? == "No saved configuration.\n");
    ensure!(!path.exists(), "checking created configuration");
    fs::create_dir_all(path.parent().context("config directory")?)?;
    let example = include_str!("../docs/automation.md")
        .split_once("```toml\n")
        .context("documented configuration example")?
        .1
        .split_once("```")
        .context("configuration example closing fence")?
        .0;
    fs::write(&path, example)?;
    let output = command_at(temporary.path())
        .args(["config", "check"])
        .output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(String::from_utf8(output.stdout)? == "Configuration is valid.\n");
    ensure!(
        fs::read_to_string(&path)? == example,
        "valid config overwritten"
    );
    for invalid in [
        "schema_version = 700\n",
        "schema_version = 1\n[devices.mouse.settings]\n\"\\u001b]52;c;payload\\u0007\\u202e\" = \"bad\"\n",
    ] {
        fs::write(&path, invalid)?;
        let output = command_at(temporary.path())
            .args(["config", "check"])
            .output()?;
        ensure!(output.status.code() == Some(1), "invalid config succeeded");
        ensure!(output.stdout.is_empty(), "error contaminated stdout");
        let error = String::from_utf8(output.stderr)?;
        ensure!(
            !error.contains(['\x1b', '\x07', '\u{202e}']),
            "error injected terminal controls"
        );
        ensure!(
            fs::read_to_string(&path)? == invalid,
            "invalid config overwritten"
        );
    }
    Ok(())
}

#[test]
fn configuration_uses_home_even_when_xdg_config_home_is_set() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let path = temporary.path().join(".config/logishell/config.toml");
    let xdg = temporary.path().join("elsewhere");
    let ignored = xdg.join("logishell/config.toml");
    fs::create_dir_all(path.parent().context("config directory")?)?;
    fs::create_dir_all(ignored.parent().context("ignored config directory")?)?;
    fs::write(&path, "schema_version = 1\n")?;
    fs::write(&ignored, "schema_version = 700\n")?;

    let output = command_at(temporary.path())
        .env("XDG_CONFIG_HOME", &xdg)
        .args(["config", "check"])
        .output()?;
    ensure!(
        output.status.success(),
        "home configuration was ignored: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(String::from_utf8(output.stdout)? == "Configuration is valid.\n");
    ensure!(fs::read_to_string(&path)? == "schema_version = 1\n");
    ensure!(fs::read_to_string(&ignored)? == "schema_version = 700\n");
    Ok(())
}

#[test]
fn config_reset_and_reload_work_with_an_isolated_daemon() -> Result<()> {
    struct TestDaemon(Child);
    impl Drop for TestDaemon {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let directory = tempfile::tempdir()?;
    let home = directory.path();
    let path = home.join(".config/logishell/config.toml");
    fs::create_dir_all(path.parent().context("config directory")?)?;
    fs::write(&path, b"invalid config\xff")?;
    let run = |args: &[&str]| {
        command_at(home)
            .env("XDG_RUNTIME_DIR", home)
            .args(args)
            .stdin(Stdio::null())
            .output()
    };
    let reset = run(&["config", "reset"])?;
    ensure!(
        reset.status.success(),
        "{}",
        String::from_utf8_lossy(&reset.stderr)
    );
    let reset = String::from_utf8(reset.stdout)?;
    let mut lines = reset.lines();
    ensure!(lines.next() == Some("Saved configuration reset."));
    let backup = lines
        .next()
        .and_then(|line| line.strip_prefix("Backup: "))
        .context("reset backup path")?;
    ensure!(fs::read(backup)? == b"invalid config\xff");
    ensure!(config::load(&path)? == config::Config::default());

    let missing = run(&["config", "reload"])?;
    ensure!(missing.status.code() == Some(1));
    ensure!(missing.stdout.is_empty());
    ensure!(String::from_utf8_lossy(&missing.stderr).contains("not running"));

    let mut daemon = TestDaemon(
        command_at(home)
            .env("XDG_RUNTIME_DIR", home)
            .arg("daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?,
    );
    // SAFETY: geteuid takes no arguments and has no preconditions.
    let socket = home.join(format!("logishell-{}/daemon.sock", unsafe {
        libc::geteuid()
    }));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        ensure!(
            daemon.0.try_wait()?.is_none(),
            "isolated daemon exited during startup"
        );
        ensure!(Instant::now() < deadline, "isolated daemon did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    // Aliases without bindings never trigger device discovery or uinput access.
    fs::write(
        &path,
        "schema_version = 1\n[devices.test]\nalias = 'mouse'\n",
    )?;
    let reload = run(&["config", "reload"])?;
    ensure!(
        reload.status.success(),
        "{}",
        String::from_utf8_lossy(&reload.stderr)
    );
    ensure!(String::from_utf8(reload.stdout)? == "Configuration reloaded.\n");
    for _ in 0..2 {
        fs::write(&path, "schema_version = 700\n")?;
        let invalid = run(&["config", "reload"])?;
        ensure!(invalid.status.code() == Some(1) && invalid.stdout.is_empty());
        ensure!(String::from_utf8_lossy(&invalid.stderr).contains("700"));
        ensure!(
            daemon.0.try_wait()?.is_none(),
            "invalid config stopped the daemon"
        );
    }
    ensure!(run(&["config", "reset"])?.status.success());
    ensure!(run(&["config", "reload"])?.status.success());
    // SAFETY: this is the still-running child process owned by this fixture.
    ensure!(unsafe { libc::kill(daemon.0.id() as i32, libc::SIGTERM) } == 0);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = daemon.0.try_wait()? {
            ensure!(status.success(), "daemon shutdown failed");
            break;
        }
        ensure!(Instant::now() < deadline, "daemon did not stop");
        std::thread::sleep(Duration::from_millis(10));
    }
    ensure!(!socket.exists(), "daemon left its socket behind");
    Ok(())
}

#[test]
fn rejects_invalid_settings_before_device_access() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let path = temporary.path().join(".config/logishell/config.toml");
    let cases: &[(&[&str], i32, &str)] = &[
        (
            &["config", "bind", "mouse", "back", "key:invalid-key"],
            1,
            "invalid-key",
        ),
        (&["config", "set", "mouse", "dpi", "0"], 1, "dpi"),
        (
            &["config", "set", "mouse", "thumb-wheel-interval", "5001"],
            1,
            "thumb-wheel-interval",
        ),
        (
            &[
                "config",
                "set",
                "mouse",
                "thumb-wheel-interval",
                "250",
                "--temporary",
            ],
            1,
            "--temporary",
        ),
        (&["setup"], 1, "interactive terminal"),
    ];
    for (arguments, code, message) in cases {
        let output = command_at(temporary.path())
            .args(*arguments)
            .stdin(Stdio::null())
            .output()?;
        ensure!(
            output.status.code() == Some(*code),
            "unexpected exit for {arguments:?}"
        );
        ensure!(
            output.stdout.is_empty(),
            "error contaminated stdout for {arguments:?}"
        );
        ensure!(
            String::from_utf8_lossy(&output.stderr).contains(message),
            "missing {message:?} for {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    ensure!(!path.exists(), "rejected commands created configuration");
    Ok(())
}
