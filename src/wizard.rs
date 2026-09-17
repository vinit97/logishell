// Interactive menus use the same validated operations as ordinary commands.
use crate::{
    cli::{self, ConnectionAction, PairVia},
    config,
    model::{Device, DeviceState, Inventory, Settings, Transport},
    remap::{
        self, BINDINGS, ControlInfo, binding_label, control_binding_label, control_label,
        saved_binding,
    },
    runtime,
    terminal::{self, Choice, Event as PairingEvent, PairingUi, Terminal},
};
use anyhow::{Context, Result};
use serde_json::Value;
use std::{collections::BTreeMap, path::Path};

struct Setting {
    key: &'static str,
    title: &'static str,
}

const SETTINGS: &[Setting] = &[
    Setting {
        key: "dpi",
        title: "Pointer sensitivity (DPI)",
    },
    Setting {
        key: "wheel-mode",
        title: "Scroll wheel feel",
    },
    Setting {
        key: "smartshift",
        title: "Automatic wheel switching (SmartShift)",
    },
    Setting {
        key: "smartshift-sensitivity",
        title: "Automatic switching sensitivity",
    },
    Setting {
        key: "scroll-invert",
        title: "Reverse wheel direction",
    },
    Setting {
        key: "thumb-wheel-invert",
        title: "Reverse thumb wheel direction",
    },
    Setting {
        key: "fn-lock",
        title: "Top-row key behavior",
    },
    Setting {
        key: "backlight",
        title: "Keyboard backlight",
    },
    Setting {
        key: "haptic-strength",
        title: "Haptic strength",
    },
];

fn choice(label: impl Into<String>, detail: impl Into<String>) -> Choice {
    Choice {
        label: label.into(),
        detail: detail.into(),
    }
}

fn display_value(key: &str, value: &Value) -> String {
    match (key, value) {
        ("wheel-mode", Value::String(mode)) if mode == "ratchet" => "Stepped (ratchet)".into(),
        ("wheel-mode", Value::String(mode)) if mode == "free-spin" => "Smooth (free-spin)".into(),
        ("fn-lock", Value::Bool(true)) => "Media and action keys".into(),
        ("fn-lock", Value::Bool(false)) => "Standard F1-F12 keys".into(),
        ("smartshift-sensitivity", Value::Number(n)) if n.as_u64() == Some(255) => "Off".into(),
        (_, Value::String(value)) if value == "custom" => "Choose another value...".into(),
        ("dpi", Value::Number(n)) => format!("{n} DPI"),
        ("haptic-strength", Value::Number(n)) => format!("{n}%"),
        (_, Value::Bool(true)) => "On".into(),
        (_, Value::Bool(false)) => "Off".into(),
        (_, Value::String(value)) => value.clone(),
        _ => value.to_string(),
    }
}

fn raw_value(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Bool(value) => if *value { "on" } else { "off" }.into(),
        _ => value.to_string(),
    }
}

fn device_description(device: &Device) -> String {
    let state = match device.state {
        DeviceState::Online => "Connected",
        DeviceState::Offline => "Offline - wake or reconnect the device",
        DeviceState::Unknown => "Connection not confirmed",
    };
    let battery = device.battery.as_ref().map(|battery| {
        let value = battery
            .percent
            .map(|percent| format!("{percent}% battery"))
            .or_else(|| battery.level.clone())
            .unwrap_or_else(|| "battery unknown".into());
        format!(
            "{value}{}{}",
            if battery.charging == Some(true) {
                ", charging"
            } else {
                ""
            },
            if battery.stale { " (last known)" } else { "" }
        )
    });
    format!(
        "{state} | {}{}",
        device.transport,
        battery.map(|b| format!(" | {b}")).unwrap_or_default()
    )
}

#[derive(Default)]
struct Draft {
    devices: BTreeMap<String, PendingDevice>,
    applied: usize,
    attempts: usize,
    reload_pending: bool,
}

#[derive(Default)]
struct PendingDevice {
    name: String,
    edits: config::DeviceConfig,
    before: Settings,
    previous_alias: String,
    bindings: BTreeMap<String, Option<String>>,
    previous_bindings: BTreeMap<String, Option<String>>,
}

#[derive(Default)]
struct DeviceSnapshot {
    value: Value,
    settings: Settings,
    controls: Vec<String>,
    error: String,
}

impl DeviceSnapshot {
    async fn read(path: &Path, device: &Device) -> Self {
        let result: Result<Self> = async {
            let value = runtime::inspect_selected(path, device).await?;
            let settings = serde_json::from_value(value["settings"].clone())
                .context("could not read settings")?;
            let controls: Vec<ControlInfo> = value
                .get("controls")
                .map(|value| serde_json::from_value(value.clone()))
                .transpose()
                .context("could not read remappable controls")?
                .unwrap_or_default();
            let mut controls: Vec<_> = controls
                .into_iter()
                .filter(|control| {
                    control.reprogrammable && control.divertible && !control.virtual_control
                })
                .map(|control| control.source)
                .collect();
            if value["device"]["capabilities"]
                .as_array()
                .is_some_and(|caps| caps.iter().any(|cap| cap == "thumb-wheel"))
            {
                controls.extend(["thumb-left".into(), "thumb-right".into()]);
            }
            controls.sort_by_cached_key(|source| control_label(source).to_lowercase());
            let error = value["controls_error"]
                .as_str()
                .map(|error| format!("Remappable controls are unavailable: {error}"))
                .unwrap_or_default();
            Ok(Self {
                value,
                settings,
                controls,
                error,
            })
        }
        .await;
        result.unwrap_or_else(|error| {
            let error = format!("Settings are unavailable: {error:#}");
            Self {
                value: serde_json::json!({"device":device, "settings":null, "settings_error":error}),
                error,
                ..Self::default()
            }
        })
    }
}

impl Draft {
    fn count(&self) -> usize {
        self.devices
            .values()
            .map(|device| {
                device.edits.settings.len()
                    + device.bindings.len()
                    + usize::from(device.edits.alias.is_some())
            })
            .sum::<usize>()
            + usize::from(self.reload_pending)
    }

    fn stage_setting(&mut self, device: &Device, key: &str, value: &str, actual: &Settings) {
        let pending = self.devices.entry(device.id.clone()).or_default();
        pending.name = device.name.clone();
        if let Some(before) = actual.get(key) {
            pending.before.insert(key.into(), before.clone());
        }
        config::record_setting(&mut pending.edits, key, value);
        // Enabling SmartShift forces ratchet mode, so an explicit wheel choice
        // must survive even when it matches the device's original mode.
        let wheel_override = key == "wheel-mode"
            && pending
                .edits
                .settings
                .get("smartshift")
                .is_some_and(|value| matches!(value.as_str(), "on" | "true"));
        if actual
            .get(key)
            .is_some_and(|before| raw_value(before) == value)
            && !wheel_override
        {
            pending.edits.settings.remove(key);
        }
    }

    fn stage_alias(&mut self, device: &Device, alias: String, previous: &str) {
        let pending = self.devices.entry(device.id.clone()).or_default();
        pending.name = device.name.clone();
        pending.previous_alias = previous.into();
        pending.edits.alias = (alias != previous).then_some(alias);
    }

    fn stage_binding(
        &mut self,
        device: &Device,
        source: &str,
        action: Option<String>,
        previous: Option<&str>,
    ) {
        let pending = self.devices.entry(device.id.clone()).or_default();
        pending.name = device.name.clone();
        if action.as_deref() == previous {
            pending.bindings.remove(source);
            pending.previous_bindings.remove(source);
        } else {
            pending
                .previous_bindings
                .insert(source.into(), previous.map(str::to_owned));
            pending.bindings.insert(source.into(), action);
        }
    }

    fn preview(&self, device: &Device, actual: &Settings) -> Settings {
        let mut result = actual.clone();
        if let Some(pending) = self.devices.get(&device.id) {
            for (key, value) in &pending.edits.settings {
                result.insert(key.clone(), setting_value(key, value));
            }
            if let Some(enabled) = pending.edits.settings.get("smartshift") {
                let enabled = matches!(enabled.as_str(), "on" | "true");
                if enabled && !pending.edits.settings.contains_key("wheel-mode") {
                    result.insert("wheel-mode".into(), Value::String("ratchet".into()));
                }
                if let Some(threshold) = result.get_mut("smartshift-sensitivity") {
                    if !enabled {
                        *threshold = Value::from(255);
                    } else if !threshold.as_u64().is_some_and(|n| (1..255).contains(&n)) {
                        *threshold = Value::String("Device default".into());
                    }
                }
            }
            if let Some(value) = pending.edits.settings.get("smartshift-sensitivity") {
                result.insert("smartshift".into(), Value::Bool(value != "255"));
            }
        }
        result
    }

    fn validate(&self, path: &Path) -> Result<()> {
        let mut prospective = config::load(path)?;
        for (id, pending) in &self.devices {
            let saved = prospective.devices.entry(id.clone()).or_default();
            for (key, value) in config::ordered_settings(&pending.edits.settings) {
                config::record_setting(saved, &key, &value);
            }
            if let Some(alias) = &pending.edits.alias {
                saved.alias = Some(alias.clone());
            }
            saved.bindings = remap::updated_bindings(&saved.bindings, &pending.bindings)?;
        }
        config::validate(&prospective)
    }

    fn review(&self) -> Vec<Choice> {
        let mut lines = Vec::new();
        for device in self.devices.values() {
            for (key, value) in config::ordered_settings(&device.edits.settings) {
                let title = SETTINGS
                    .iter()
                    .find(|s| s.key == key)
                    .map_or(key.as_str(), |s| s.title);
                let previous = device
                    .before
                    .get(&key)
                    .map(|value| display_value(&key, value))
                    .unwrap_or_else(|| "unknown".into());
                lines.push(choice(
                    format!("{}: {title}", device.name),
                    format!(
                        "{previous} -> {}",
                        display_value(&key, &setting_value(&key, &value))
                    ),
                ));
            }
            if let Some(alias) = &device.edits.alias {
                let previous = if device.previous_alias.is_empty() {
                    "not set"
                } else {
                    &device.previous_alias
                };
                lines.push(choice(
                    format!("{}: Nickname", device.name),
                    format!("{previous} -> {alias}"),
                ));
            }
            for (source, action) in &device.bindings {
                let previous = device
                    .previous_bindings
                    .get(source)
                    .and_then(Option::as_deref);
                let unset = if matches!(source.as_str(), "thumb-left" | "thumb-right") {
                    "No saved action"
                } else {
                    binding_label(None)
                };
                lines.push(choice(
                    format!("{}: {}", device.name, control_label(source)),
                    format!(
                        "{} -> {}",
                        previous
                            .map(|action| binding_label(Some(action)))
                            .unwrap_or(unset),
                        action
                            .as_deref()
                            .map(|action| binding_label(Some(action)))
                            .unwrap_or(unset)
                    ),
                ));
            }
        }
        if self.reload_pending {
            lines.push(choice("Reload saved configuration", ""));
        }
        lines.sort_by_cached_key(|choice| choice.label.to_lowercase());
        lines
    }
}

fn setting_value(key: &str, value: &str) -> Value {
    match key {
        "dpi" | "smartshift-sensitivity" | "haptic-strength" => value
            .parse::<u64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(value.into())),
        "wheel-mode" => Value::String(value.into()),
        _ => Value::Bool(matches!(value, "on" | "true")),
    }
}

pub async fn run(path: &Path) -> Result<()> {
    let mut terminal = Terminal::enter()?;
    let mut draft = Draft::default();
    let mut notice = String::new();
    let mut selected = 0;
    let mut inventory = None;
    let mut snapshots = BTreeMap::new();
    let mut completion =
        "Setup closed. No settings, nicknames, or bindings were changed.".to_owned();
    loop {
        if inventory.is_none() {
            terminal.message("Setup", "Looking for your devices...")?;
            match runtime::inventory().await {
                Ok(devices) => inventory = Some(devices),
                Err(error) => {
                    let action = terminal
                        .select(
                            "Cannot read devices",
                            &format!("{error:#}"),
                            &[choice("Try again", ""), choice("Exit setup", "")],
                            0,
                        )
                        .await?;
                    if action == Some(0) {
                        continue;
                    }
                    break;
                }
            }
        }
        let devices = inventory.as_ref().context("missing device inventory")?;
        let saved = config::load(path)?;
        let mut choices: Vec<_> = devices
            .devices
            .iter()
            .enumerate()
            .map(|(index, device)| {
                let alias = draft
                    .devices
                    .get(&device.id)
                    .and_then(|d| d.edits.alias.as_deref())
                    .or_else(|| {
                        saved
                            .devices
                            .get(&device.id)
                            .and_then(|saved| saved.alias.as_deref())
                    });
                let mut label = match alias {
                    Some(alias) => format!("{} ({alias})", device.name),
                    None => device.name.clone(),
                };
                if devices
                    .devices
                    .iter()
                    .filter(|other| other.name == device.name)
                    .count()
                    > 1
                {
                    label.push_str(&format!(" [device {}]", index + 1));
                }
                choice(label, device_description(device))
            })
            .collect();
        let device_count = choices.len();
        choices.extend([
            choice("Pair a new device", ""),
            choice("Refresh devices", ""),
            if draft.count() == 0 {
                choice("Done", "")
            } else {
                choice(format!("Review changes ({})", draft.count()), "")
            },
        ]);
        let description = if notice.is_empty() {
            if device_count == 0 {
                "No devices found.".into()
            } else {
                String::new()
            }
        } else {
            std::mem::take(&mut notice)
        };
        let selection = terminal
            .select("Setup", &description, &choices, selected)
            .await?;
        if selection.is_none() || selection.is_some_and(|index| index > device_count + 1) {
            if draft.count() == 0 {
                break;
            }
            let previous_attempts = draft.attempts;
            if let Some(message) = review(&mut terminal, path, &mut draft).await? {
                completion = message;
                break;
            }
            if draft.attempts != previous_attempts {
                snapshots.clear();
            }
            continue;
        }
        let index = selection.context("missing menu selection")?;
        selected = index;
        if index < device_count {
            if let Some(message) = customize(
                &mut terminal,
                path,
                &devices.devices[index],
                &mut draft,
                &mut snapshots,
            )
            .await?
            {
                notice = message;
                inventory = None;
                snapshots.clear();
            }
        } else if index == device_count {
            if let Some((message, changed)) = pair(&mut terminal, devices).await? {
                notice = message;
                if changed {
                    inventory = None;
                    snapshots.clear();
                }
            }
        } else {
            inventory = None;
            snapshots.clear();
        }
    }
    drop(terminal);
    if draft.applied > 0 && completion.starts_with("Setup closed.") {
        completion = "Setup closed. Changes already applied remain saved.".into();
    } else if draft.attempts > 0 && completion.starts_with("Setup closed.") {
        completion = "Setup closed. An earlier apply attempt may have changed device settings; check current settings to confirm their state.".into();
    }
    println!("{completion}");
    Ok(())
}

async fn review(terminal: &mut Terminal, path: &Path, draft: &mut Draft) -> Result<Option<String>> {
    let mut error = String::new();
    loop {
        let summary = if error.is_empty() {
            format!("{} pending changes", draft.count())
        } else {
            error.clone()
        };
        let mut choices = vec![
            choice("Apply changes", ""),
            choice("Keep editing", ""),
            choice("Discard", ""),
        ];
        choices.extend(draft.review());
        match terminal
            .select("Review changes", &summary, &choices, 1)
            .await?
        {
            Some(0) => {
                if let Err(problem) = draft.validate(path) {
                    error = format!(
                        "Please fix the draft before applying: {problem:#}. Nothing was changed."
                    );
                    continue;
                }
                terminal.message(
                    "Applying changes",
                    "Updating devices and saving your choices...",
                )?;
                draft.attempts += 1;
                match apply_draft(path, draft).await {
                    Ok(()) => return Ok(Some("Changes applied and saved.".into())),
                    Err(problem) => {
                        error = format!(
                            "Application stopped: {problem:#}. Earlier successful changes remain applied; the remaining draft is shown below."
                        )
                    }
                }
            }
            Some(2) => {
                return Ok(Some(
                    "Remaining draft discarded. No further settings, nicknames, or bindings were changed."
                        .into(),
                ));
            }
            Some(index) if index >= 3 => {
                let change = &choices[index];
                terminal
                    .select_back(&change.label, &change.detail, &[], 0)
                    .await?;
            }
            _ => return Ok(None),
        }
    }
}

async fn apply_draft(path: &Path, draft: &mut Draft) -> Result<()> {
    let result: Result<()> = async {
        if !draft.devices.values().any(|pending| {
            !pending.edits.settings.is_empty()
                || pending.edits.alias.is_some()
                || !pending.bindings.is_empty()
        }) {
            return Ok(());
        }
        let command = runtime::CommandContext::open(path).await?;
        for (id, pending) in &mut draft.devices {
            for (key, value) in config::ordered_settings(&pending.edits.settings) {
                command
                    .set(path, id, &key, &value, false)
                    .await
                    .with_context(|| format!("{}: {key}", pending.name))?;
                pending.edits.settings.remove(&key);
                draft.applied += 1;
                draft.reload_pending = true;
            }
        }
        // Nicknames may move between devices or swap, so validate and save the
        // complete assignment before clearing any of its pending changes.
        let aliases: BTreeMap<_, _> = draft
            .devices
            .iter()
            .filter_map(|(id, pending)| {
                pending.edits.alias.clone().map(|alias| (id.clone(), alias))
            })
            .collect();
        if !aliases.is_empty() {
            command
                .aliases(path, &aliases)
                .context("device nicknames")?;
            for pending in draft.devices.values_mut() {
                pending.edits.alias = None;
            }
            draft.applied += aliases.len();
            draft.reload_pending = true;
        }
        for (id, pending) in &mut draft.devices {
            if !pending.bindings.is_empty() {
                command
                    .update_bindings(path, id, &pending.bindings)
                    .await
                    .with_context(|| format!("{}: bindings", pending.name))?;
                draft.applied += pending.bindings.len();
                pending.bindings.clear();
                pending.previous_bindings.clear();
                draft.reload_pending = true;
            }
        }
        Ok(())
    }
    .await;
    if draft.reload_pending {
        match runtime::reload_if_running(path).await {
            Ok(()) => draft.reload_pending = false,
            Err(reload) => {
                return match result {
                    Ok(()) => Err(reload),
                    Err(error) => Err(error.context(format!("{reload:#}"))),
                };
            }
        }
    }
    result
}

async fn customize(
    terminal: &mut Terminal,
    path: &Path,
    device: &Device,
    draft: &mut Draft,
    snapshots: &mut BTreeMap<String, DeviceSnapshot>,
) -> Result<Option<String>> {
    let mut selected = 0;
    let mut notice = String::new();
    let mut refresh = !snapshots.contains_key(&device.id);
    loop {
        if refresh {
            terminal.message(&device.name, "Reading current settings...")?;
            snapshots.insert(device.id.clone(), DeviceSnapshot::read(path, device).await);
            refresh = false;
        }
        let snapshot = snapshots
            .get(&device.id)
            .context("missing device settings")?;
        let actual = &snapshot.settings;
        let controls = &snapshot.controls;
        let preview = draft.preview(device, actual);
        let mut available: Vec<_> = SETTINGS
            .iter()
            .filter(|setting| actual.contains_key(setting.key))
            .collect();
        available.sort_by_cached_key(|setting| setting.title.to_lowercase());
        let mut choices: Vec<_> = available
            .iter()
            .map(|setting| {
                let pending = draft
                    .devices
                    .get(&device.id)
                    .is_some_and(|d| d.edits.settings.contains_key(setting.key))
                    || preview.get(setting.key) != actual.get(setting.key);
                choice(
                    format!(
                        "{}: {}{}",
                        setting.title,
                        display_value(setting.key, &preview[setting.key]),
                        if pending { " (draft)" } else { "" }
                    ),
                    "",
                )
            })
            .collect();
        let count = choices.len();
        let bindings_index = (!controls.is_empty()).then_some(choices.len());
        if bindings_index.is_some() {
            choices.push(choice("Remappable controls", ""));
        }
        let nickname_index = choices.len();
        let saved = config::load(path)?;
        let previous_alias = saved
            .devices
            .get(&device.id)
            .and_then(|saved| saved.alias.as_deref())
            .unwrap_or("");
        let alias = draft
            .devices
            .get(&device.id)
            .and_then(|d| d.edits.alias.as_deref())
            .unwrap_or(previous_alias);
        choices.extend([
            choice(
                "Device nickname",
                if alias.is_empty() {
                    String::new()
                } else {
                    format!("Nickname: {alias}")
                },
            ),
            choice("Refresh settings", ""),
        ]);
        if !connection_actions(device).is_empty() {
            choices.push(choice("Connection", ""));
        }
        let details_index = choices.len();
        choices.push(choice("Device details", ""));
        let description = if !notice.is_empty() {
            std::mem::take(&mut notice)
        } else if !snapshot.error.is_empty() {
            snapshot.error.clone()
        } else if available.is_empty() && controls.is_empty() {
            "No configurable settings available.".into()
        } else {
            String::new()
        };
        let Some(index) = terminal
            .select_back(&device.name, &description, &choices, selected)
            .await?
        else {
            break;
        };
        selected = index;
        if index < count {
            let setting = available[index];
            if let Some(value) = select_value(terminal, setting, &preview).await? {
                draft.stage_setting(device, setting.key, &value, actual);
            }
        } else if Some(index) == bindings_index {
            configure_bindings(terminal, path, device, controls, draft).await?;
        } else if index == nickname_index {
            if let Some(value) = terminal
                .input(
                    "Device nickname",
                    "Letters, numbers, hyphens, or underscores.",
                    alias,
                )
                .await?
            {
                if value.is_empty() && !previous_alias.is_empty() {
                    continue;
                }
                if !value
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
                {
                    notice = "Use only letters, numbers, hyphens, or underscores for the nickname."
                        .into();
                    continue;
                }
                draft.stage_alias(device, value, previous_alias);
            }
        } else if index == nickname_index + 1 {
            refresh = true;
        } else if index == details_index {
            match cli::device_details(&snapshot.value, &saved) {
                Ok(details) => terminal.details("Device details", &details).await?,
                Err(error) => notice = format!("Device details are unavailable: {error:#}"),
            }
        } else if index == nickname_index + 2 {
            if let Some(message) = connection(terminal, device, draft).await? {
                return Ok(Some(message));
            }
        } else {
            break;
        }
    }
    Ok(None)
}

async fn configure_bindings(
    terminal: &mut Terminal,
    path: &Path,
    device: &Device,
    controls: &[String],
    draft: &mut Draft,
) -> Result<()> {
    let mut selected = 0;
    loop {
        let saved = config::load(path)?;
        let saved_bindings = saved
            .devices
            .get(&device.id)
            .map(|saved| saved.bindings.clone())
            .unwrap_or_default();
        let pending = draft.devices.get(&device.id);
        let effective = |source: &str| {
            pending
                .and_then(|pending| pending.bindings.get(source))
                .map_or_else(|| saved_binding(&saved_bindings, source), Option::as_deref)
        };
        let wheel_active = ["thumb-left", "thumb-right"]
            .iter()
            .any(|source| effective(source).is_some());
        let saved_wheel_active = ["thumb-left", "thumb-right"]
            .iter()
            .any(|source| saved_binding(&saved_bindings, source).is_some());
        let choices: Vec<_> = controls
            .iter()
            .map(|source| {
                let action = effective(source);
                let label = control_binding_label(source, action, wheel_active);
                let changed = label
                    != control_binding_label(
                        source,
                        saved_binding(&saved_bindings, source),
                        saved_wheel_active,
                    );
                choice(
                    format!(
                        "{}: {}{}",
                        control_label(source),
                        label,
                        if changed { " (draft)" } else { "" }
                    ),
                    "",
                )
            })
            .collect();
        let Some(index) = terminal
            .select_back("Remappable controls", "", &choices, selected)
            .await?
        else {
            return Ok(());
        };
        selected = index;
        let source = &controls[index];
        let previous = saved_binding(&saved_bindings, source);
        let current = effective(source);
        let other_wheel_active = ["thumb-left", "thumb-right"]
            .iter()
            .any(|other| *other != source && effective(other).is_some());
        if let Some(action) = select_binding(terminal, source, current, other_wheel_active).await? {
            draft.stage_binding(device, source, action, previous);
        }
    }
}

async fn select_binding(
    terminal: &mut Terminal,
    source: &str,
    current: Option<&str>,
    other_wheel_active: bool,
) -> Result<Option<Option<String>>> {
    let custom_index = BINDINGS.len() + 1;
    let mut selected = current.map_or(0, |current| {
        BINDINGS
            .iter()
            .position(|(_, action)| *action == current)
            .map_or(custom_index, |index| index + 1)
    });
    let mut choices = vec![choice(
        control_binding_label(source, None, other_wheel_active),
        "",
    )];
    choices.extend(BINDINGS.iter().map(|(label, _)| choice(*label, "")));
    choices.push(choice("Custom action...", current.unwrap_or("")));
    choices[selected].label.push_str(" (selected)");
    loop {
        let Some(index) = terminal
            .select_back(control_label(source), "", &choices, selected)
            .await?
        else {
            return Ok(None);
        };
        selected = index;
        if index == 0 {
            return Ok(Some(None));
        }
        if index < custom_index {
            return Ok(Some(Some(BINDINGS[index - 1].1.into())));
        }
        let mut text = current.unwrap_or("").to_owned();
        let mut description =
            "Shortcut or action, such as key:ctrl+c, mouse:middle, or media:mute.".to_owned();
        while let Some(value) = terminal.input("Custom action", &description, &text).await? {
            match remap::validate_binding(source, &value) {
                Ok(()) => return Ok(Some(Some(value))),
                Err(error) => {
                    description = format!("{error:#}");
                    text = value;
                }
            }
        }
    }
}

fn connection_actions(device: &Device) -> Vec<(ConnectionAction, Choice)> {
    let mut actions = Vec::new();
    if device.transport == Transport::Bluetooth {
        actions.push(if device.state == DeviceState::Online {
            (ConnectionAction::Disconnect, choice("Disconnect", ""))
        } else {
            (ConnectionAction::Connect, choice("Connect", ""))
        });
    }
    if matches!(
        device.transport,
        Transport::Bluetooth | Transport::Bolt | Transport::Unifying
    ) {
        actions.push((ConnectionAction::Unpair, choice("Unpair device", "")));
    }
    actions
}

async fn connection(
    terminal: &mut Terminal,
    device: &Device,
    draft: &mut Draft,
) -> Result<Option<String>> {
    let mut notice = device_description(device);
    loop {
        let (mut actions, choices): (Vec<_>, Vec<_>) =
            connection_actions(device).into_iter().unzip();
        let Some(index) = terminal
            .select_back("Connection", &notice, &choices, 0)
            .await?
        else {
            return Ok(None);
        };
        let action = actions.remove(index);
        let unpair = matches!(action, ConnectionAction::Unpair);
        if unpair && terminal.select(
            "Unpair device?",
            &format!("{} will be unpaired. Its pending changes will be discarded. Saved configuration is kept.", device.name),
            &[choice("Cancel", ""), choice("Unpair", "")],
            0,
        ).await? != Some(1) {
            continue;
        }
        let completed = match &action {
            ConnectionAction::Connect => "Device connected.",
            ConnectionAction::Disconnect => "Device disconnected.",
            ConnectionAction::Unpair => "Device unpaired.",
        };
        terminal.message("Connection", "Updating connection...")?;
        match cli::device_action(device, action).await {
            Ok(()) => {
                if unpair {
                    draft.devices.remove(&device.id);
                }
                return Ok(Some(completed.into()));
            }
            Err(error) => {
                notice = format!(
                    "Connection change failed: {error:#}. Refresh devices to check its state."
                )
            }
        }
    }
}

fn value_options(setting: &Setting, settings: &Settings) -> Vec<(String, Choice)> {
    let values: Vec<String> = match setting.key {
        "wheel-mode" => vec!["ratchet".into(), "free-spin".into()],
        "dpi" => settings
            .get("dpi-supported")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_u64)
            .filter(|value| (1..=65535).contains(value))
            .map(|value| value.to_string())
            .collect(),
        "smartshift-sensitivity" | "haptic-strength" => {
            let haptic = setting.key == "haptic-strength";
            let range = if haptic { 0..=100 } else { 1..=254 };
            let mut values = if haptic {
                vec![0, 25, 50, 75, 100]
            } else {
                vec![10, 20, 30, 50, 75, 100, 150, 200, 254]
            };
            if let Some(current) = settings
                .get(setting.key)
                .and_then(Value::as_u64)
                .filter(|value| range.contains(value))
            {
                values.push(current);
            }
            values.sort_unstable();
            values.dedup();
            let mut options: Vec<_> = values.into_iter().map(|value| value.to_string()).collect();
            if !haptic {
                options.push("255".into());
            }
            options.push("custom".into());
            options
        }
        _ => vec!["on".into(), "off".into()],
    };
    values
        .into_iter()
        .map(|value| {
            let label = display_value(setting.key, &setting_value(setting.key, &value));
            (value, choice(label, ""))
        })
        .collect()
}

async fn select_value(
    terminal: &mut Terminal,
    setting: &Setting,
    settings: &Settings,
) -> Result<Option<String>> {
    let current = raw_value(&settings[setting.key]);
    let options = value_options(setting, settings);
    let mut selected = options
        .iter()
        .position(|(value, _)| *value == current)
        .unwrap_or(0);
    let (values, mut choices): (Vec<_>, Vec<_>) = options.into_iter().unzip();
    if values.get(selected) == Some(&current) {
        choices[selected].label.push_str(" (selected)");
    }
    let description = if values.is_empty() {
        "The device did not report any supported values. Go back and refresh settings."
    } else {
        ""
    };
    loop {
        let Some(index) = terminal
            .select_back(setting.title, description, &choices, selected)
            .await?
        else {
            return Ok(None);
        };
        selected = index;
        let value = &values[index];
        if value != "custom" {
            return Ok(Some(value.clone()));
        }
        let range = if setting.key == "haptic-strength" {
            0..=100
        } else {
            1..=254
        };
        let hint = format!("Whole number from {} to {}.", range.start(), range.end());
        let mut explanation = hint.clone();
        let mut text = if current.parse::<u8>().is_ok_and(|n| range.contains(&n)) {
            current.clone()
        } else {
            String::new()
        };
        while let Some(value) = terminal.input(setting.title, &explanation, &text).await? {
            if let Ok(number) = value.trim().parse::<u8>()
                && range.contains(&number)
            {
                return Ok(Some(number.to_string()));
            }
            explanation = format!("Invalid value. {hint}");
            text = value;
        }
    }
}

async fn pair(terminal: &mut Terminal, inventory: &Inventory) -> Result<Option<(String, bool)>> {
    let choices = [
        choice("Bluetooth", ""),
        choice("Logi Bolt receiver", ""),
        choice("Unifying receiver", ""),
    ];
    let Some(index) = terminal
        .select_back("Pair a new device", "", &choices, 0)
        .await?
    else {
        return Ok(None);
    };
    let (via, transport) = match index {
        0 => (PairVia::Bluetooth, Transport::Bluetooth),
        1 => (PairVia::Bolt, Transport::Bolt),
        2 => (PairVia::Unifying, Transport::Unifying),
        _ => return Ok(None),
    };
    let receiver = if transport == Transport::Bluetooth {
        None
    } else {
        let receivers: Vec<_> = inventory
            .receivers
            .iter()
            .filter(|receiver| receiver.transport == transport)
            .collect();
        if receivers.is_empty() {
            return Ok(Some((
                format!(
                    "No {transport} receiver was found. Plug it in and refresh the device list."
                ),
                false,
            )));
        }
        if receivers.len() == 1 {
            Some(receivers[0].id.clone())
        } else {
            let choices: Vec<_> = receivers
                .iter()
                .enumerate()
                .map(|(i, receiver)| choice(format!("{} {}", receiver.name, i + 1), &receiver.id))
                .collect();
            let Some(index) = terminal
                .select_back("Choose a receiver", "", &choices, 0)
                .await?
            else {
                return Ok(None);
            };
            Some(receivers[index].id.clone())
        }
    };
    let (ui, events) = terminal::channel();
    let result = pairing_screens(
        terminal,
        ui.clone(),
        events,
        cli::pair_with_ui(via, receiver, ui),
    )
    .await?;
    let changed = result.is_ok();
    Ok(Some((
        match result {
            Ok(()) => "Device paired.".into(),
            Err(error) => {
                format!("Pairing did not complete: {error:#}. Refresh devices to check its state.")
            }
        },
        changed,
    )))
}

/// Own the reply until the screen answers or a new backend event replaces it.
async fn pairing_screen(terminal: &mut Terminal, screen: PairingEvent) -> Result<bool> {
    match screen {
        PairingEvent::Status { title, message } => {
            terminal.pairing_wait(&title, &message).await?;
            Ok(true)
        }
        PairingEvent::Choose {
            title,
            message,
            choices,
            reply,
        } => {
            let answer = terminal.select_back(&title, &message, &choices, 0).await?;
            Ok(pairing_reply(reply, answer))
        }
        PairingEvent::Input {
            title,
            message,
            reply,
        } => {
            let answer = terminal.input(&title, &message, "").await?;
            Ok(pairing_reply(reply, answer))
        }
        PairingEvent::Dismiss => unreachable!("dismiss events do not create a screen"),
    }
}

fn pairing_reply<T>(reply: tokio::sync::oneshot::Sender<Option<T>>, answer: Option<T>) -> bool {
    let cancelled = answer.is_none();
    let _ = reply.send(answer);
    cancelled
}

/// The pairing future owns its cleanup. Never drop it to dismiss a screen.
async fn pairing_screens(
    terminal: &mut Terminal,
    ui: PairingUi,
    mut events: tokio::sync::mpsc::UnboundedReceiver<PairingEvent>,
    operation: impl std::future::Future<Output = Result<()>>,
) -> Result<Result<()>> {
    tokio::pin!(operation);
    let waiting = || PairingEvent::Status {
        title: "Pairing".into(),
        message: "Waiting for your device. Each pairing phase can take up to 60 seconds.".into(),
    };
    let mut screen = waiting();
    loop {
        tokio::select! {
            biased;
            result = &mut operation => return Ok(result),
            Some(event) = events.recv() => {
                screen = if matches!(event, PairingEvent::Dismiss) { waiting() } else { event };
            },
            answer = pairing_screen(terminal, std::mem::replace(&mut screen, waiting())) => {
                if matches!(answer, Ok(false)) { continue; }
                ui.cancel();
                let drawn = answer.and_then(|_| terminal.message("Cancelling pairing", "Closing the pairing session. Your pending settings are unchanged."));
                // Drain receiver/BlueZ cleanup even when drawing or input fails.
                let result = operation.await;
                drawn?;
                return Ok(result);
            },
        }
    }
}
