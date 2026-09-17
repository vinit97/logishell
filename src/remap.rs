//! Per-device HID++ control remapping with uinput actions.
//!
//! Configured buttons and a bound thumb wheel are temporarily diverted. Either
//! thumb-wheel binding diverts both directions; an unbound direction does nothing.
//! Pointer motion, vertical scrolling, and unconfigured buttons remain native.
//! Unit tests use simulated reports and never create virtual input devices.

use crate::model::Device;
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io,
    os::fd::AsRawFd,
    sync::{Arc, atomic::AtomicU16},
};
use tokio::sync::watch;

#[path = "remap_diversion.rs"]
mod diversion;
pub use diversion::{ControlInfo, check_bindings, control_id, controls};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BindingSource {
    Control(u16),
    ThumbLeft,
    ThumbRight,
}

pub fn binding_source(source: &str) -> Result<BindingSource> {
    match source {
        "thumb-left" => Ok(BindingSource::ThumbLeft),
        "thumb-right" => Ok(BindingSource::ThumbRight),
        _ => control_id(source).map(BindingSource::Control),
    }
}

pub const BINDINGS: &[(&str, &str)] = &[
    ("Middle click", "mouse:middle"),
    ("Back", "mouse:back"),
    ("Forward", "mouse:forward"),
    ("Copy (Ctrl+C)", "key:ctrl+c"),
    ("Paste (Ctrl+V)", "key:ctrl+v"),
    ("Cut (Ctrl+X)", "key:ctrl+x"),
    ("Undo (Ctrl+Z)", "key:ctrl+z"),
    ("Redo (Ctrl+Shift+Z)", "key:ctrl+shift+z"),
    ("Switch applications (Alt+Tab)", "key:alt+tab"),
    ("Overview (Super)", "key:super"),
    ("Play / pause", "media:play-pause"),
    ("Next track", "media:next"),
    ("Previous track", "media:previous"),
    ("Volume up", "media:volume-up"),
    ("Volume down", "media:volume-down"),
    ("Mute", "media:mute"),
];

pub fn control_label(source: &str) -> &str {
    match source {
        "thumb-left" => return "Thumb wheel left",
        "thumb-right" => return "Thumb wheel right",
        _ => {}
    }
    match control_id(source).ok() {
        Some(0x0050) => "Left button",
        Some(0x0051) => "Right button",
        Some(0x0052) => "Wheel click",
        Some(0x0053) => "Back button",
        Some(0x0056) => "Forward button",
        Some(0x00c3) => "Gesture button",
        Some(0x00c4) => "Wheel mode button",
        Some(0x00d4) => "Search",
        Some(0x00e2) => "Keyboard backlight down",
        Some(0x00e3) => "Keyboard backlight up",
        Some(0x00e7) => "Mute sound",
        Some(0x00e8) => "Volume down",
        Some(0x00e9) => "Volume up",
        Some(0x0103) => "Dictation",
        Some(0x0108) => "Emoji",
        Some(0x010a) => "Screenshot",
        Some(0x010b) => "Grave accent",
        Some(0x010c) => "Tab",
        Some(0x010d) => "Caps Lock",
        Some(0x010e) => "Left Shift",
        Some(0x010f) => "Left Ctrl",
        Some(0x0110) => "Left Super / Option",
        Some(0x0111) => "Left Alt / Command",
        Some(0x0112) => "Right Alt / Command",
        Some(0x0115) => "Right Shift",
        Some(0x0117) => "Delete",
        Some(0x0118) => "Home",
        Some(0x0119) => "End",
        Some(0x011a) => "Page Up",
        Some(0x011b) => "Page Down",
        Some(0x011c) => "Mute microphone",
        Some(0x011e) => "Backslash",
        Some(0x013c) => "Right Super / Option",
        Some(0x0141) => "Play / pause",
        Some(0x01a0) => "Haptic thumb pad",
        _ => source,
    }
}

pub fn binding_label(action: Option<&str>) -> &str {
    match action {
        None => "Default (device behavior)",
        Some(action) => BINDINGS
            .iter()
            .find(|(_, value)| *value == action)
            .map_or(action, |(label, _)| *label),
    }
}

pub fn control_binding_label<'a>(
    source: &str,
    action: Option<&'a str>,
    wheel_active: bool,
) -> &'a str {
    if action.is_none() && wheel_active && matches!(source, "thumb-left" | "thumb-right") {
        "No action"
    } else {
        binding_label(action)
    }
}

pub fn saved_binding<'a>(bindings: &'a BTreeMap<String, String>, source: &str) -> Option<&'a str> {
    let identity = binding_source(source).ok()?;
    bindings.iter().find_map(|(source, action)| {
        (binding_source(source).ok() == Some(identity)).then_some(action.as_str())
    })
}

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const SYN_REPORT: u16 = 0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Mouse(u16),
    Keys(Vec<u16>),
}

impl Action {
    pub fn parse(value: &str) -> Result<Self> {
        let (kind, value) = value
            .split_once(':')
            .context("action must be mouse:middle, key:ctrl+c, or media:play-pause")?;
        match kind {
            "mouse" => Ok(Self::Mouse(parse_button(value)?)),
            "key" => {
                let names: Vec<_> = value.split('+').collect();
                ensure!(
                    names.len() <= 5,
                    "a shortcut supports at most four modifiers and a key"
                );
                let mut keys = Vec::new();
                for (index, name) in names.iter().enumerate() {
                    let key = parse_key(name).with_context(|| format!("unknown key {name:?}"))?;
                    ensure!(!keys.contains(&key), "duplicate shortcut key {name:?}");
                    ensure!(
                        index + 1 == names.len() || is_modifier(key),
                        "only modifiers may precede the final shortcut key"
                    );
                    keys.push(key);
                }
                Ok(Self::Keys(keys))
            }
            "media" => Ok(Self::Keys(vec![match value {
                "play-pause" => 164,
                "play" => 207,
                "pause" => 119,
                "stop" => 166,
                "next" => 163,
                "previous" => 165,
                "volume-up" => 115,
                "volume-down" => 114,
                "mute" => 113,
                _ => bail!("unknown media action {value:?}"),
            }])),
            _ => bail!("unknown action type {kind:?}; use mouse:, key:, or media:"),
        }
    }

    fn keys(&self) -> &[u16] {
        match self {
            Self::Mouse(key) => std::slice::from_ref(key),
            Self::Keys(keys) => keys,
        }
    }
}

/// Numeric buttons are Linux BTN_* event codes, not desktop button numbers.
pub fn parse_button(button: &str) -> Result<u16> {
    let code = match button {
        "left" => 0x110,
        "right" => 0x111,
        "middle" => 0x112,
        "back" | "side" => 0x113,
        "forward" | "extra" => 0x114,
        "task" => 0x117,
        value => {
            if let Some(hex) = value.strip_prefix("0x") {
                u16::from_str_radix(hex, 16).context("invalid hexadecimal button code")?
            } else {
                value.parse().context("unknown button; use left, right, middle, back, forward, task, or a Linux button code")?
            }
        }
    };
    ensure!(
        (0x110..=0x11f).contains(&code),
        "button must be a Linux mouse button code (272..287)"
    );
    Ok(code)
}

pub fn validate_binding(button: &str, action: &str) -> Result<()> {
    binding_source(button)?;
    Action::parse(action)?;
    Ok(())
}

/// Validate a complete map as well, catching aliases for the same physical key.
pub fn validate_bindings(bindings: &BTreeMap<String, String>) -> Result<()> {
    diversion::parse_bindings(bindings).map(|_| ())
}

pub fn updated_bindings(
    current: &BTreeMap<String, String>,
    edits: &BTreeMap<String, Option<String>>,
) -> Result<BTreeMap<String, String>> {
    let mut changed = BTreeSet::new();
    for (source, action) in edits {
        let identity = binding_source(source)?;
        ensure!(
            changed.insert(identity),
            "multiple edits refer to the same control {source}"
        );
        if let Some(action) = action {
            validate_binding(source, action)?;
        }
    }
    let mut updated = BTreeMap::new();
    for (source, action) in current {
        if !changed.contains(&binding_source(source)?) {
            updated.insert(source.clone(), action.clone());
        }
    }
    for (source, action) in edits {
        if let Some(action) = action {
            updated.insert(source.clone(), action.clone());
        }
    }
    validate_bindings(&updated)?;
    Ok(updated)
}

fn is_modifier(key: u16) -> bool {
    matches!(key, 29 | 42 | 54 | 56 | 97 | 100 | 125 | 126)
}

fn parse_key(name: &str) -> Option<u16> {
    let code = match name {
        "ctrl" | "control" => 29,
        "shift" => 42,
        "alt" => 56,
        "super" | "meta" => 125,
        "rightctrl" => 97,
        "rightshift" => 54,
        "rightalt" => 100,
        "rightsuper" => 126,
        "esc" | "escape" => 1,
        "enter" | "return" => 28,
        "space" => 57,
        "tab" => 15,
        "backspace" => 14,
        "delete" => 111,
        "insert" => 110,
        "home" => 102,
        "end" => 107,
        "pageup" => 104,
        "pagedown" => 109,
        "up" => 103,
        "down" => 108,
        "left" => 105,
        "right" => 106,
        "minus" => 12,
        "equal" => 13,
        "leftbrace" => 26,
        "rightbrace" => 27,
        "semicolon" => 39,
        "apostrophe" => 40,
        "grave" => 41,
        "backslash" => 43,
        "comma" => 51,
        "dot" => 52,
        "slash" => 53,
        "menu" => 139,
        "f11" => 87,
        "f12" => 88,
        _ if name.len() > 1 && name.starts_with('f') => {
            let number: u16 = name[1..].parse().ok()?;
            match number {
                1..=10 => 58 + number,
                13..=24 => 170 + number,
                _ => return None,
            }
        }
        _ if name.len() == 1 => {
            let byte = name.as_bytes()[0];
            if byte.is_ascii_lowercase() {
                const LETTERS: [u16; 26] = [
                    30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20,
                    22, 47, 17, 45, 21, 44,
                ];
                LETTERS[(byte - b'a') as usize]
            } else if byte.is_ascii_digit() {
                if byte == b'0' {
                    11
                } else {
                    u16::from(byte - b'1') + 2
                }
            } else {
                return None;
            }
        }
        _ => return None,
    };
    Some(code)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Event {
    kind: u16,
    code: u16,
    value: i32,
}

impl Event {
    fn key(code: u16, value: i32) -> Self {
        Self {
            kind: EV_KEY,
            code,
            value,
        }
    }
    fn sync() -> Self {
        Self {
            kind: EV_SYN,
            code: SYN_REPORT,
            value: 0,
        }
    }
}

/// Shared output keys stay held until their last physical owner releases them.
#[derive(Default)]
struct Mapper {
    bindings: BTreeMap<u16, Action>,
    down: BTreeSet<u16>,
    output_holds: BTreeSet<u16>,
    pulse_holds: Vec<u16>,
}

impl Mapper {
    fn update(&mut self, pressed: &[u16]) -> Vec<Event> {
        if self.down.iter().all(|cid| pressed.contains(cid))
            && pressed
                .iter()
                .all(|cid| !self.bindings.contains_key(cid) || self.down.contains(cid))
        {
            return Vec::new();
        }
        let holds: BTreeSet<_> = pressed
            .iter()
            .filter_map(|cid| self.bindings.get(cid))
            .flat_map(|action| action.keys().iter().copied())
            .collect();
        let mut events = Vec::new();
        for cid in &self.down {
            for &code in self.bindings[cid].keys().iter().rev() {
                let retained = holds.contains(&code)
                    && (self.down.iter().any(|cid| {
                        pressed.contains(cid) && self.bindings[cid].keys().contains(&code)
                    }) || self.bindings[cid].keys().last() != Some(&code)
                        && pressed
                            .iter()
                            .filter_map(|cid| self.bindings.get(cid))
                            .all(|action| action.keys().last() != Some(&code)));
                if !retained && !events.iter().any(|event: &Event| event.code == code) {
                    events.push(Event::key(code, 0));
                }
            }
        }
        // Finish old shortcuts before starting new ones, keeping only modifiers
        // shared with the new snapshot. A final key is a target even when Super.
        events.sort_by_key(|event| is_modifier(event.code));
        let released = events.len();
        for action in pressed.iter().filter_map(|cid| self.bindings.get(cid)) {
            for &code in action.keys() {
                if (!self.output_holds.contains(&code)
                    || events[..released].iter().any(|event| event.code == code))
                    && !events[released..].iter().any(|event| event.code == code)
                {
                    events.push(Event::key(code, 1));
                }
            }
        }
        self.down.retain(|cid| pressed.contains(cid));
        self.down.extend(
            pressed
                .iter()
                .filter(|cid| self.bindings.contains_key(cid))
                .copied(),
        );
        self.output_holds = holds;
        if !events.is_empty() {
            events.push(Event::sync());
        }
        events
    }

    fn pulse(&mut self, action: &Action) -> Vec<Event> {
        let keys = action.keys();
        if keys
            .last()
            .is_some_and(|key| self.output_holds.contains(key))
        {
            return Vec::new();
        }
        let keys: Vec<_> = keys
            .iter()
            .copied()
            .filter(|key| !self.output_holds.contains(key))
            .collect();
        for &key in &keys {
            if !self.pulse_holds.contains(&key) {
                self.pulse_holds.push(key);
            }
        }
        keys.iter()
            .map(|&key| Event::key(key, 1))
            .chain([Event::sync()])
            .chain(keys.iter().rev().map(|&key| Event::key(key, 0)))
            .chain([Event::sync()])
            .collect()
    }

    fn release_all(&mut self) -> Vec<Event> {
        let mut events: Vec<_> = std::mem::take(&mut self.pulse_holds)
            .into_iter()
            .rev()
            .map(|key| Event::key(key, 0))
            .collect();
        // Finish an interrupted pulse before releasing button-owned modifiers.
        for event in self.update(&[]) {
            if event.kind != EV_SYN && !events.iter().any(|released| released.code == event.code) {
                events.push(event);
            }
        }
        if !events.is_empty() {
            events.push(Event::sync());
        }
        events
    }
}

fn ioctl_value(file: &File, request: libc::Ioctl, value: libc::c_int) -> io::Result<()> {
    // SAFETY: these ioctl requests take an integer value, never a pointer.
    let result = unsafe { libc::ioctl(file.as_raw_fd(), request, value) };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn set_capability(file: &File, number: u32, value: u16) -> io::Result<()> {
    ioctl_value(
        file,
        libc::_IOW::<libc::c_int>(u32::from(b'U'), number),
        i32::from(value),
    )
}

struct VirtualDevice(File);

impl VirtualDevice {
    fn emit(&self, event: Event) -> io::Result<()> {
        // SAFETY: input_event has only integer fields; timestamps are ignored by uinput.
        let mut raw: libc::input_event = unsafe { std::mem::zeroed() };
        raw.type_ = event.kind;
        raw.code = event.code;
        raw.value = event.value;
        loop {
            // SAFETY: raw points to a complete initialized input_event for the syscall.
            let result = unsafe {
                libc::write(
                    self.0.as_raw_fd(),
                    (&raw as *const libc::input_event).cast(),
                    std::mem::size_of_val(&raw),
                )
            };
            if result == std::mem::size_of_val(&raw) as isize {
                return Ok(());
            }
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "incomplete uinput event write",
            ));
        }
    }
}

impl Drop for VirtualDevice {
    fn drop(&mut self) {
        let _ = ioctl_value(&self.0, libc::_IO(u32::from(b'U'), 2), 0);
    }
}

/// Run configured actions for a device's reported divertible HID++ controls.
/// Native input remains available without grabbing any /dev/input nodes.
pub async fn run(
    device: Device,
    bindings: BTreeMap<String, String>,
    thumb_wheel_interval_ms: Arc<AtomicU16>,
    shutdown: watch::Receiver<bool>,
) -> Result<()> {
    crate::model::require_remappable(&device)?;
    diversion::run(device, bindings, thumb_wheel_interval_ms, shutdown).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_labels_resolve_identity_aliases_and_preserve_unknown_sources() {
        for source in ["haptic", "cid:0x01a0", "cid:416"] {
            assert_eq!(control_label(source), "Haptic thumb pad");
        }
        for (source, label) in [
            ("side", "Back button"),
            ("cid:0x00e2", "Keyboard backlight down"),
            ("cid:0x0110", "Left Super / Option"),
            ("cid:0x013c", "Right Super / Option"),
            ("cid:0x0141", "Play / pause"),
        ] {
            assert_eq!(control_label(source), label);
        }
        for source in ["cid:0x7fff", "future-control", "cid:invalid"] {
            assert_eq!(control_label(source), source);
        }
    }

    #[test]
    fn binding_edits_replace_and_remove_control_aliases() -> Result<()> {
        let saved = BTreeMap::from([
            ("cid:0x01a0".into(), "key:ctrl+c".into()),
            ("side".into(), "mouse:back".into()),
            ("forward".into(), "mouse:forward".into()),
        ]);
        let updated = updated_bindings(
            &saved,
            &BTreeMap::from([
                ("haptic".into(), Some("media:play-pause".into())),
                ("back".into(), None),
            ]),
        )?;
        assert_eq!(
            updated,
            BTreeMap::from([
                ("haptic".into(), "media:play-pause".into()),
                ("forward".into(), "mouse:forward".into()),
            ])
        );
        let removed = updated_bindings(&updated, &BTreeMap::from([("cid:416".into(), None)]))?;
        assert_eq!(removed.len(), 1);
        assert!(removed.contains_key("forward"));
        assert_eq!(saved.len(), 3);
        Ok(())
    }

    #[test]
    fn binding_batches_reject_invalid_or_conflicting_edits_without_partial_changes() {
        let saved = BTreeMap::from([("haptic".into(), "key:ctrl+c".into())]);
        for edits in [
            BTreeMap::from([
                ("haptic".into(), None),
                ("back".into(), Some("exec:sh".into())),
            ]),
            BTreeMap::from([
                ("haptic".into(), None),
                ("cid:0x01a0".into(), Some("mouse:middle".into())),
            ]),
            BTreeMap::from([("cid:0".into(), None)]),
        ] {
            assert!(updated_bindings(&saved, &edits).is_err());
            assert_eq!(saved["haptic"], "key:ctrl+c");
        }
    }

    #[test]
    fn parses_actions_and_rejects_unsafe_or_ambiguous_syntax() -> Result<()> {
        for (input, expected) in [
            ("key:ctrl+c", Action::Keys(vec![29, 46])),
            ("key:f", Action::Keys(vec![33])),
            ("key:ctrl+f", Action::Keys(vec![29, 33])),
            ("key:f1", Action::Keys(vec![59])),
            ("key:ctrl+f1", Action::Keys(vec![29, 59])),
            ("key:f13", Action::Keys(vec![183])),
            ("key:ctrl+f24", Action::Keys(vec![29, 194])),
            ("media:play-pause", Action::Keys(vec![164])),
            ("mouse:middle", Action::Mouse(274)),
        ] {
            assert_eq!(Action::parse(input)?, expected, "{input}");
        }
        for letter in 'a'..='z' {
            Action::parse(&format!("key:{letter}"))?;
            Action::parse(&format!("key:ctrl+{letter}"))?;
        }
        assert_eq!(parse_button("0x113")?, parse_button("back")?);
        assert!(validate_binding("back", "exec:sh").is_err());
        assert!(validate_binding("1", "key:a").is_err());
        for invalid in [
            "key:",
            "key:c+ctrl",
            "key:ctrl+control+c",
            "key:ctrl++c",
            "key:f25",
            "key:f13+a",
            "media:nope",
            "mouse:46",
        ] {
            assert!(Action::parse(invalid).is_err(), "accepted {invalid}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn remapping_admission_depends_on_capabilities_not_model_names() -> Result<()> {
        let mut device = Device {
            id: "test-device".into(),
            name: "M720 Triathlon".into(),
            transport: crate::model::Transport::Bolt,
            state: crate::model::DeviceState::Online,
            receiver_id: Some("test receiver".into()),
            slot: Some(1),
            hid_path: Some("/not-a-real-hidraw-node".into()),
            capabilities: vec!["button-diversion".into()],
            ..Default::default()
        };
        let (_sender, receiver) = watch::channel(false);
        for name in ["M720 Triathlon", "MX Mechanical Mini", "K780"] {
            device.name = name.into();
            // An empty mapping is a no-op after capability admission, so this
            // test cannot open HID or uinput even when the model is accepted.
            run(
                device.clone(),
                BTreeMap::new(),
                Arc::new(AtomicU16::new(0)),
                receiver.clone(),
            )
            .await?;
        }
        device.capabilities = vec!["thumb-wheel".into()];
        run(
            device.clone(),
            BTreeMap::new(),
            Arc::new(AtomicU16::new(0)),
            receiver.clone(),
        )
        .await?;
        assert!(controls(&device)?.is_empty());
        device.capabilities.clear();
        assert!(
            run(
                device.clone(),
                BTreeMap::new(),
                Arc::new(AtomicU16::new(0)),
                receiver.clone()
            )
            .await
            .is_err()
        );
        assert!(controls(&device).is_err());
        assert!(
            check_bindings(
                &device,
                &BTreeMap::from([("cid:0x0199".into(), "key:f13".into())])
            )
            .is_err()
        );
        device.capabilities.push("button-diversion".into());
        device.hid_path = None;
        assert!(
            run(
                device,
                BTreeMap::new(),
                Arc::new(AtomicU16::new(0)),
                receiver
            )
            .await
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn wheel_sources_stay_distinct_from_cids_and_preserve_saved_action_labels() -> Result<()> {
        let saved = BTreeMap::from([
            ("thumb-left".into(), "key:left".into()),
            ("cid:0x00c3".into(), "key:super".into()),
        ]);
        assert_eq!(binding_source("thumb-left")?, BindingSource::ThumbLeft);
        assert_eq!(binding_source("thumb-right")?, BindingSource::ThumbRight);
        assert!(control_id("thumb-left").is_err());
        assert_eq!(saved_binding(&saved, "gesture"), Some("key:super"));
        assert_eq!(saved_binding(&saved, "thumb-left"), Some("key:left"));
        assert_eq!(saved_binding(&saved, "thumb-right"), None);
        assert_eq!(control_label("gesture"), "Gesture button");
        assert_eq!(control_label("thumb-left"), "Thumb wheel left");
        assert_eq!(control_label("thumb-right"), "Thumb wheel right");
        assert_eq!(
            control_binding_label("thumb-right", None, true),
            "No action"
        );
        assert_eq!(
            control_binding_label("thumb-right", None, false),
            binding_label(None)
        );
        assert_eq!(
            control_binding_label("gesture", None, true),
            binding_label(None)
        );
        let updated = updated_bindings(
            &saved,
            &BTreeMap::from([
                ("thumb-left".into(), None),
                ("thumb-right".into(), Some("key:right".into())),
            ]),
        )?;
        assert_eq!(saved_binding(&updated, "thumb-left"), None);
        assert_eq!(saved_binding(&updated, "thumb-right"), Some("key:right"));
        assert_eq!(saved_binding(&updated, "gesture"), Some("key:super"));
        Ok(())
    }
}
