use crate::{
    bluetooth, config, device,
    model::{Device, DeviceState, Inventory, Receiver, Transport},
    remap, runtime,
    terminal::clean,
};
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde_json::Value;
use std::time::Duration;

#[derive(Debug, Parser)]
#[command(
    name = "logishell",
    version,
    disable_help_flag = true,
    about = "Logitech device control from your terminal"
)]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Show devices and battery levels
    Status {
        #[arg(long)]
        watch: bool,
    },
    /// Configure devices and manage connections interactively
    Setup,
    /// Manage settings and bindings
    #[command(subcommand, disable_help_subcommand = true)]
    Config(ConfigCommand),
    /// Run button and key bindings
    Daemon {
        /// Limit remapping to one device (default: all configured devices)
        device: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Read all settings, or one setting, from a device
    Get {
        device: String,
        setting: Option<String>,
    },
    /// Change a supported setting; persist it unless --temporary is given
    Set {
        device: String,
        setting: String,
        value: String,
        /// Change hardware without saving; unavailable for daemon settings
        #[arg(long)]
        temporary: bool,
    },
    /// Reapply saved hardware settings to one or all detected configured devices
    Apply { device: Option<String> },
    /// Validate the file without opening devices or changing settings
    Check,
    /// Reload saved bindings in the running daemon
    Reload,
    /// Clear saved settings, nicknames, and bindings
    Reset,
    /// Assign a short, unique device alias
    Alias { device: String, alias: String },
    /// Save a button, key, or thumb-wheel action
    Bind {
        device: String,
        /// Control name, thumb-left/right, or cid:0xNNNN from `config get`
        button: String,
        /// Action such as mouse:middle, key:ctrl+c, or media:play-pause
        action: String,
    },
    /// Remove a saved binding
    Unbind { device: String, button: String },
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum PairVia {
    Bluetooth,
    Bolt,
    Unifying,
}

impl PairVia {
    fn transport(self) -> Transport {
        match self {
            Self::Bluetooth => Transport::Bluetooth,
            Self::Bolt => Transport::Bolt,
            Self::Unifying => Transport::Unifying,
        }
    }
}

fn connection_label(transport: Transport) -> &'static str {
    match transport {
        Transport::Bluetooth => "Bluetooth",
        Transport::Bolt => "Bolt",
        Transport::Unifying => "Unifying",
        Transport::Usb => "USB",
        Transport::Unknown => "Unknown",
    }
}

fn state_label(state: DeviceState) -> &'static str {
    match state {
        DeviceState::Online => "Connected",
        DeviceState::Offline => "Offline",
        DeviceState::Unknown => "Unknown",
    }
}

fn battery_label(device: &Device) -> String {
    let Some(battery) = &device.battery else {
        return "Unavailable".into();
    };
    let mut label = battery
        .percent
        .map(|p| format!("{p}%"))
        .or_else(|| battery.level.as_deref().map(clean))
        .unwrap_or_else(|| "Unknown".into());
    if battery.charging == Some(true) {
        label.push_str(" charging");
    }
    if battery.stale {
        label.push_str(" (stale)");
    }
    label
}

fn display_name(device: &Device, config: &config::Config) -> String {
    match config
        .devices
        .get(&device.id)
        .and_then(|saved| saved.alias.as_ref())
    {
        Some(alias) => format!("{} ({})", clean(alias), clean(&device.name)),
        None => clean(&device.name),
    }
}

fn compact(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    text.chars()
        .take(limit.saturating_sub(1))
        .chain(['…'])
        .collect()
}

fn inventory_output(inventory: &Inventory, config: &config::Config) -> String {
    let mut lines = Vec::new();
    if inventory.devices.is_empty() {
        lines.push("No Logitech mice or keyboards detected.".into());
    } else {
        lines.push(format!(
            "{:<36} {:<11} {:<10} BATTERY",
            "DEVICE", "CONNECTION", "STATUS"
        ));
        for device in &inventory.devices {
            lines.push(format!(
                "{:<36} {:<11} {:<10} {}",
                compact(&display_name(device, config), 36),
                connection_label(device.transport),
                state_label(device.state),
                battery_label(device),
            ));
            if inventory
                .devices
                .iter()
                .filter(|other| other.name.eq_ignore_ascii_case(&device.name))
                .count()
                > 1
            {
                lines.push(format!("  {}", clean(&device.id)));
            }
        }
    }
    if !inventory.receivers.is_empty() {
        lines.push(String::new());
        lines.push("Receivers".into());
        for receiver in &inventory.receivers {
            let count = inventory
                .devices
                .iter()
                .filter(|d| d.receiver_id.as_ref() == Some(&receiver.id))
                .count();
            let paired = if count == 0 {
                String::new()
            } else {
                format!(
                    " · {count} known device{}",
                    if count == 1 { "" } else { "s" }
                )
            };
            lines.push(format!("  {}{}", clean(&receiver.name), paired));
        }
    }
    lines.join("\n")
}

fn show_inventory(inventory: &Inventory, config: &config::Config) {
    println!("{}", inventory_output(inventory, config));
    for device in &inventory.devices {
        for warning in &device.warnings {
            eprintln!("{}: {}", display_name(device, config), clean(warning));
        }
    }
    for warning in &inventory.warnings {
        eprintln!("{}", clean(warning));
    }
}

fn setting_label(setting: &str) -> String {
    match setting {
        "dpi" => "Pointer speed (DPI)",
        "dpi-supported" => "Available DPI",
        "wheel-mode" => "Wheel mode",
        "smartshift" => "SmartShift",
        "smartshift-sensitivity" => "SmartShift sensitivity",
        "scroll-invert" => "Reverse scrolling",
        "thumb-wheel-invert" => "Reverse thumb wheel",
        "thumb-wheel-interval" => "Thumb-wheel repeat interval (ms)",
        "fn-lock" => "Fn lock (media keys)",
        "backlight" => "Backlight",
        "haptic-strength" => "Haptic strength (%)",
        other => return clean(other),
    }
    .into()
}

fn setting_value(value: &Value) -> String {
    match value {
        Value::Bool(true) => "On".into(),
        Value::Bool(false) => "Off".into(),
        Value::Null => "Unavailable".into(),
        Value::String(value) => match value.as_str() {
            "free-spin" => "Free spin".into(),
            "ratchet" => "Ratchet".into(),
            other => clean(other),
        },
        Value::Array(values) => {
            if let Some(numbers) = values.iter().map(Value::as_u64).collect::<Option<Vec<_>>>()
                && numbers.len() >= 3
                && let Some(step) = numbers[1].checked_sub(numbers[0]).filter(|step| *step > 0)
                && numbers
                    .windows(2)
                    .all(|pair| pair[1].checked_sub(pair[0]) == Some(step))
            {
                return format!(
                    "{}–{} (steps of {step})",
                    numbers[0],
                    numbers[numbers.len() - 1]
                );
            }
            values
                .iter()
                .map(setting_value)
                .collect::<Vec<_>>()
                .join(", ")
        }
        other => clean(&other.to_string()),
    }
}

fn settings_lines(value: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(settings) = value.get("settings").and_then(Value::as_object) {
        for (name, setting) in settings {
            lines.push(format!(
                "  {:<24} {}",
                setting_label(name),
                setting_value(setting)
            ));
        }
        lines.sort_by_cached_key(|line| line.to_lowercase());
        if settings.is_empty() {
            lines.push("  No configurable settings were reported.".into());
        }
    } else {
        lines.push(format!(
            "  {}",
            value
                .get("settings_error")
                .and_then(Value::as_str)
                .map(clean)
                .unwrap_or_else(|| "Settings are unavailable.".into())
        ));
    }
    lines
}

pub(crate) fn device_details(value: &Value, config: &config::Config) -> Result<String> {
    let device: Device =
        serde_json::from_value(value["device"].clone()).context("invalid device details")?;
    let mut lines = vec![clean(&device.name)];
    lines.push(format!(
        "  {:<24} {} · {}",
        "Connection",
        connection_label(device.transport),
        state_label(device.state)
    ));
    lines.push(format!("  {:<24} {}", "Battery", battery_label(&device)));
    lines.push(format!("  {:<24} {}", "Device ID", clean(&device.id)));
    if let Some(receiver) = &device.receiver_id {
        lines.push(format!("  {:<24} {}", "Receiver ID", clean(receiver)));
    }
    if let Some(slot) = device.slot {
        lines.push(format!("  {:<24} {slot}", "Receiver slot"));
    }
    if let Some(address) = &device.bluetooth_address {
        lines.push(format!("  {:<24} {}", "Bluetooth address", clean(address)));
    }
    if let Some(firmware) = &device.firmware {
        lines.push(format!("  {:<24} {}", "Firmware", clean(firmware)));
    }
    lines.push("\nSettings".into());
    lines.extend(settings_lines(value));
    let has_thumb_wheel = device.capabilities.iter().any(|cap| cap == "thumb-wheel");
    if value.get("controls").is_some() || has_thumb_wheel {
        let controls = value
            .get("controls")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let bindings = config.devices.get(&device.id).map(|saved| &saved.bindings);
        let mut sources: Vec<_> = controls
            .iter()
            .filter(|control| {
                control["divertible"] == true
                    && control["reprogrammable"] == true
                    && control["virtual_control"] == false
            })
            .filter_map(|control| control["source"].as_str())
            .collect();
        if has_thumb_wheel {
            sources.extend(["thumb-left", "thumb-right"]);
            let interval = config
                .devices
                .get(&device.id)
                .map_or(0, |saved| saved.thumb_wheel_interval_ms);
            lines.push(format!(
                "\nThumb-wheel repeat interval: {}",
                if interval == 0 {
                    "Unlimited".into()
                } else {
                    format!("{interval} ms")
                }
            ));
        }
        lines.push("\nRemappable controls (saved actions)".into());
        sources.sort_by_cached_key(|source| remap::control_label(source).to_lowercase());
        let wheel_active = bindings.is_some_and(|bindings| {
            ["thumb-left", "thumb-right"]
                .iter()
                .any(|source| remap::saved_binding(bindings, source).is_some())
        });
        for source in &sources {
            let label = remap::control_label(source);
            let name = if label == *source {
                clean(source)
            } else {
                format!("{} [{}]", clean(label), clean(source))
            };
            let action = bindings.and_then(|bindings| remap::saved_binding(bindings, source));
            lines.push(format!(
                "  {name} → {}",
                clean(remap::control_binding_label(source, action, wheel_active))
            ));
        }
        if sources.is_empty() {
            lines.push("  No controls support temporary remapping.".into());
        }
    }
    if let Some(error) = value.get("controls_error").and_then(Value::as_str) {
        lines.push(format!("\nRemapping: {}", clean(error)));
    }
    for warning in &device.warnings {
        lines.push(format!("\nNote: {}", clean(warning)));
    }
    Ok(lines.join("\n"))
}

fn config_output(value: &Value) -> Result<String> {
    if let Some(setting) = value.get("setting").and_then(Value::as_str) {
        let persistence = match value.get("saved").and_then(Value::as_bool) {
            Some(true) => " Saved.",
            Some(false) => " Temporary; your saved setting is unchanged.",
            None => "",
        };
        return Ok(format!(
            "{}: {}.{persistence}",
            setting_label(setting),
            setting_value(&value["value"])
        ));
    }
    if let Some(applied) = value.get("applied").and_then(Value::as_array) {
        return Ok(match applied.len() {
            0 => "No saved settings were applied.".into(),
            1 => "Applied saved settings to 1 device.".into(),
            count => format!("Applied saved settings to {count} devices."),
        });
    }
    if let Some(alias) = value.get("alias").and_then(Value::as_str) {
        return Ok(format!("Nickname saved: {}.", clean(alias)));
    }
    if let Some(button) = value.get("button").and_then(Value::as_str) {
        if value["removed"] == true {
            return Ok(format!("Binding removed: {}.", clean(button)));
        }
        if let Some(action) = value.get("action").and_then(Value::as_str) {
            return Ok(format!(
                "Binding saved: {} → {}.",
                clean(button),
                clean(action)
            ));
        }
    }
    bail!("unrecognized configuration result")
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ConnectionAction {
    Connect,
    Disconnect,
    Unpair,
}

pub(crate) async fn device_action(device: &Device, action: ConnectionAction) -> Result<()> {
    match action {
        ConnectionAction::Connect => bluetooth::connect(device).await,
        ConnectionAction::Disconnect => bluetooth::disconnect(device).await,
        ConnectionAction::Unpair if device.transport == Transport::Bluetooth => {
            bluetooth::unpair(device).await
        }
        ConnectionAction::Unpair => {
            let _lock = runtime::pairing_lock()?;
            let device = device.clone();
            tokio::task::spawn_blocking(move || crate::device::unpair(&device)).await?
        }
    }
}

pub async fn run(cli: Cli) -> Result<()> {
    let path = config::path()?;
    match cli.command.unwrap_or(Command::Status { watch: false }) {
        Command::Status { watch } => loop {
            let inventory = runtime::inventory().await?;
            let config = match config::load(&path) {
                Ok(config) => config,
                Err(error) => {
                    eprintln!("configuration: {}", clean(&format!("{error:#}")));
                    config::Config::default()
                }
            };
            show_inventory(&inventory, &config);
            if !watch {
                break;
            }
            tokio::select! {
                _=tokio::signal::ctrl_c()=>break,
                _=tokio::time::sleep(Duration::from_secs(5))=>{},
            }
            println!();
        },
        Command::Config(ConfigCommand::Get { device, setting }) => {
            if let Some(setting) = setting {
                let actual = runtime::get_setting(&path, &device, &setting).await?;
                println!("{}: {}", setting_label(&setting), setting_value(&actual));
            } else {
                let value = runtime::get(&path, &device).await?;
                println!("{}", device_details(&value, &config::load(&path)?)?);
            }
        }
        Command::Config(ConfigCommand::Set {
            device,
            setting,
            value,
            temporary,
        }) => {
            let result = runtime::set(&path, &device, &setting, &value, temporary).await?;
            if !temporary {
                runtime::reload_if_running(&path).await?;
            }
            println!("{}", config_output(&result)?);
        }
        Command::Config(ConfigCommand::Apply { device }) => {
            let result = runtime::apply(&path, device.as_deref()).await?;
            println!("{}", config_output(&result)?);
        }
        Command::Config(ConfigCommand::Bind {
            device,
            button,
            action,
        }) => {
            crate::remap::validate_binding(&button, &action)?;
            let result = runtime::bind(&path, &device, &button, &action).await?;
            runtime::reload_if_running(&path).await?;
            println!("{}", config_output(&result)?);
        }
        Command::Config(ConfigCommand::Unbind { device, button }) => {
            let result = runtime::unbind(&path, &device, &button).await?;
            runtime::reload_if_running(&path).await?;
            println!("{}", config_output(&result)?);
        }
        Command::Daemon { device } => {
            runtime::run_remapping(path, device).await?;
        }
        Command::Setup => {
            crate::wizard::run(&path).await?;
        }
        Command::Config(ConfigCommand::Check) => {
            config::load(&path)?;
            println!(
                "{}",
                if path.exists() {
                    "Configuration is valid."
                } else {
                    "No saved configuration."
                }
            );
        }
        Command::Config(ConfigCommand::Reload) => {
            runtime::reload(&path).await?;
            println!("Configuration reloaded.");
        }
        Command::Config(ConfigCommand::Reset) => {
            let lock = runtime::command_lock().await?;
            let backup = config::reset(&path)?;
            drop(lock);
            println!("Saved configuration reset.");
            if let Some(backup) = backup {
                println!("Backup: {}", backup.display());
            }
            runtime::reload_if_running(&path).await?;
        }
        Command::Config(ConfigCommand::Alias { device, alias }) => {
            let result = runtime::alias(&path, &device, &alias).await?;
            runtime::reload_if_running(&path).await?;
            println!("{}", config_output(&result)?);
        }
    }
    Ok(())
}

pub(crate) async fn pair_with_ui(
    via: PairVia,
    receiver: Option<String>,
    ui: crate::terminal::PairingUi,
) -> Result<()> {
    if matches!(via, PairVia::Bluetooth) {
        return bluetooth::pair_with_ui(60, ui).await;
    }
    // Discovery owns blocking HID work. Finish it before releasing its command
    // lock, even if setup requests cancellation while it is running.
    let inventory = runtime::inventory().await?;
    if ui.cancelled() {
        bail!("pairing cancelled");
    }
    let receiver = pairing_receiver(&inventory, via.transport(), receiver.as_deref())?;
    let _lock = runtime::pairing_lock()?;
    let cancelled = ui.cancel_flag();
    // Install both handlers before starting a pairing operation that changes
    // receiver state. Cancellation must close its lock and restore flags.
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let worker_ui = ui.clone();
    let worker = tokio::task::spawn_blocking(move || {
        device::pair_receiver_with_ui(&receiver, 60, worker_ui)
    });
    finish_receiver_pairing(worker, cancelled, async {
        tokio::select! {
            _ = interrupt.recv() => {},
            _ = terminate.recv() => {},
            _ = ui.cancelled_signal() => {},
        }
    })
    .await
}

async fn finish_receiver_pairing(
    mut worker: tokio::task::JoinHandle<Result<()>>,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    stop: impl std::future::Future<Output = ()>,
) -> Result<()> {
    tokio::select! {
        result = &mut worker => result?,
        _ = stop => {
            cancelled.store(true,std::sync::atomic::Ordering::Relaxed);
            // A dropped JoinHandle cannot stop blocking I/O. Keep ownership
            // until the pairing worker finishes restoring receiver state.
            worker.await?
        }
    }
}

fn pairing_receiver(
    inventory: &Inventory,
    transport: Transport,
    id: Option<&str>,
) -> Result<Receiver> {
    let candidates: Vec<_> = inventory
        .receivers
        .iter()
        .filter(|r| r.transport == transport && id.is_none_or(|id| r.id == id))
        .collect();
    match candidates.as_slice() {
        [receiver] => Ok((*receiver).clone()),
        [] => bail!(
            "no matching {transport} receiver found; run `logishell status` to check connected receivers"
        ),
        _ => bail!("multiple {transport} receivers found; select a receiver in setup"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use serde_json::json;

    #[test]
    fn commands_are_grouped_with_help() -> Result<()> {
        Cli::command().debug_assert();
        let mut command = Cli::command();
        command.build();
        let roots: Vec<_> = command.get_subcommands().map(|c| c.get_name()).collect();
        assert_eq!(roots, ["status", "setup", "config", "daemon", "help"]);
        for arguments in [
            vec!["logishell", "config", "get", "mouse"],
            vec![
                "logishell",
                "config",
                "set",
                "mouse",
                "dpi",
                "1600",
                "--temporary",
            ],
            vec!["logishell", "config", "bind", "mouse", "back", "key:ctrl+c"],
            vec!["logishell", "config", "unbind", "mouse", "back"],
            vec!["logishell", "config", "reload"],
            vec!["logishell", "config", "reset"],
            vec!["logishell", "daemon"],
            vec!["logishell", "daemon", "mouse"],
        ] {
            Cli::try_parse_from(arguments)?;
        }
        Ok(())
    }

    #[test]
    fn receiver_pairing_never_selects_the_wrong_transport_or_an_ambiguous_receiver() -> Result<()> {
        let receiver = |id: &str, transport| Receiver {
            id: id.into(),
            name: id.into(),
            transport,
            hid_path: "/not-opened".into(),
        };
        let mut inventory = Inventory {
            receivers: vec![
                receiver("bolt", Transport::Bolt),
                receiver("unifying", Transport::Unifying),
            ],
            ..Default::default()
        };
        assert_eq!(
            pairing_receiver(&inventory, Transport::Unifying, None)?.id,
            "unifying"
        );
        assert_eq!(
            pairing_receiver(&inventory, Transport::Bolt, None)?.id,
            "bolt"
        );
        assert!(pairing_receiver(&inventory, Transport::Unifying, Some("bolt")).is_err());
        inventory
            .receivers
            .push(receiver("second", Transport::Unifying));
        assert!(pairing_receiver(&inventory, Transport::Unifying, None).is_err());
        assert_eq!(
            pairing_receiver(&inventory, Transport::Unifying, Some("second"))?.id,
            "second"
        );
        Ok(())
    }

    #[tokio::test]
    async fn pairing_cancellation_waits_for_receiver_cleanup() -> Result<()> {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancelled.clone();
        let (signal, cancel_signal) = tokio::sync::oneshot::channel();
        let (observed, cancel_observed) = tokio::sync::oneshot::channel();
        let (cleaned, cleanup_finished) = tokio::sync::oneshot::channel();
        let worker = tokio::spawn(async move {
            while !worker_cancel.load(Ordering::Relaxed) {
                tokio::task::yield_now().await;
            }
            let _ = observed.send(());
            cleanup_finished.await?;
            bail!("pairing canceled after cleanup");
        });
        let finish = tokio::spawn(finish_receiver_pairing(worker, cancelled, async {
            let _ = cancel_signal.await;
        }));
        signal.send(()).expect("cancel handler remains active");
        tokio::time::timeout(Duration::from_secs(1), cancel_observed).await??;
        assert!(
            !finish.is_finished(),
            "pairing returned before receiver cleanup"
        );
        cleaned.send(()).expect("cleanup remains active");
        let result = finish.await?;
        assert!(
            result
                .expect_err("pairing canceled")
                .to_string()
                .contains("after cleanup")
        );
        Ok(())
    }

    fn display_fixture() -> Device {
        Device {
            id: "receiver:fixture:device:stable-serial".into(),
            name: "MX Master 4".into(),
            transport: Transport::Bolt,
            state: DeviceState::Online,
            battery: Some(crate::model::Battery {
                percent: Some(82),
                charging: Some(true),
                stale: true,
                level: None,
            }),
            receiver_id: Some("receiver:fixture".into()),
            slot: Some(1),
            hid_path: Some("/dev/test-only".into()),
            bluetooth_address: None,
            firmware: Some("test-firmware".into()),
            capabilities: vec!["dpi".into()],
            warnings: vec!["sample warning".into()],
        }
    }

    #[test]
    fn status_is_compact_and_preserves_battery_state() -> Result<()> {
        let device = display_fixture();
        let mut config = config::Config::default();
        config.devices.entry(device.id.clone()).or_default().alias = Some("mouse".into());
        let inventory = Inventory {
            devices: vec![device.clone()],
            receivers: vec![Receiver {
                id: "receiver:fixture".into(),
                name: "Logitech Bolt receiver".into(),
                transport: Transport::Bolt,
                hid_path: "/dev/test-only".into(),
            }],
            warnings: vec!["inventory warning".into()],
        };
        let human = inventory_output(&inventory, &config);
        assert!(human.contains("mouse (MX Master 4)"));
        assert!(human.contains("82% charging (stale)"));
        assert!(human.contains("Logitech Bolt receiver"));
        assert!(!human.contains("receiver:fixture"));
        assert!(!human.contains("/dev/test-only"));
        let mut unknown = device;
        unknown.battery = None;
        assert_eq!(battery_label(&unknown), "Unavailable");
        Ok(())
    }

    #[test]
    fn status_exposes_ids_when_device_names_are_ambiguous() {
        let first = display_fixture();
        let mut second = first.clone();
        second.id = "receiver:fixture:device:other-serial".into();
        let inventory = Inventory {
            devices: vec![first, second],
            ..Default::default()
        };
        let output = inventory_output(&inventory, &config::Config::default());
        for device in inventory.devices {
            assert!(output.contains(&device.id));
        }
    }

    #[test]
    fn device_details_include_ids_settings_and_eligible_controls() -> Result<()> {
        let mut device = display_fixture();
        device.capabilities.push("thumb-wheel".into());
        let value = json!({
            "device": device,
            "settings": { "dpi": 1600, "dpi-supported": [800, 1600, 2400], "fn-lock": true },
            "controls": [
                { "source": "back", "divertible": true, "reprogrammable": true, "virtual_control": false },
                { "source": "cid:0x01a0", "divertible": true, "reprogrammable": true, "virtual_control": false },
                { "source": "cid:0x00d4", "divertible": true, "reprogrammable": true, "virtual_control": false },
                { "source": "cid:0x00e2", "divertible": true, "reprogrammable": true, "virtual_control": false },
                { "source": "cid:0xabcd", "divertible": true, "reprogrammable": true, "virtual_control": false },
                { "source": "blocked-control", "divertible": false, "reprogrammable": true, "virtual_control": false }
            ]
        });
        let mut config = config::Config::default();
        config
            .devices
            .entry(display_fixture().id)
            .or_default()
            .bindings = std::collections::BTreeMap::from([
            ("cid:83".into(), "key:ctrl+c".into()),
            ("haptic".into(), "key:super".into()),
            ("cid:0x00d4".into(), "key:ctrl+f".into()),
            ("thumb-left".into(), "media:volume-down".into()),
        ]);
        config
            .devices
            .entry("other-device".into())
            .or_default()
            .bindings
            .insert("back".into(), "key:ctrl+v".into());
        config
            .devices
            .entry(display_fixture().id)
            .or_default()
            .thumb_wheel_interval_ms = 250;
        let info = device_details(&value, &config)?;
        assert!(info.contains("Thumb-wheel repeat interval: 250 ms"));
        assert!(info.contains("receiver:fixture:device:stable-serial"));
        assert!(info.contains("Pointer speed (DPI)"));
        assert!(info.contains("800–2400 (steps of 800)"));
        assert!(info.contains("Remappable controls (saved actions)"));
        assert!(info.contains("Back button [back] → Copy (Ctrl+C)"));
        assert!(info.contains("Haptic thumb pad [cid:0x01a0] → Overview (Super)"));
        assert!(info.contains("Search [cid:0x00d4] → key:ctrl+f"));
        assert!(info.contains("[cid:0x00e2] → Default (device behavior)"));
        assert!(info.contains("cid:0xabcd → Default (device behavior)"));
        assert!(info.contains("Thumb wheel left [thumb-left] → Volume down"));
        assert!(info.contains("Thumb wheel right [thumb-right] → No action"));
        assert!(!info.contains("blocked-control"));
        assert!(!info.contains("Paste (Ctrl+V)"));
        let default = device_details(&value, &config::Config::default())?;
        assert!(default.contains("Thumb-wheel repeat interval: Unlimited"));
        assert!(default.contains("Back button [back] → Default (device behavior)"));
        assert!(default.contains("Thumb wheel right [thumb-right] → Default (device behavior)"));
        assert!(!default.contains("Copy (Ctrl+C)"));
        let settings = settings_lines(&value);
        assert!(
            settings
                .windows(2)
                .all(|pair| pair[0].to_lowercase() <= pair[1].to_lowercase())
        );
        let controls: Vec<_> = info
            .split("Remappable controls (saved actions)\n")
            .nth(1)
            .expect("control section")
            .lines()
            .take_while(|line| line.starts_with("  "))
            .collect();
        assert!(
            controls
                .windows(2)
                .all(|pair| pair[0].to_lowercase() <= pair[1].to_lowercase())
        );
        assert_eq!(setting_value(&json!([800, 1200, 2400])), "800, 1200, 2400");
        Ok(())
    }
}
