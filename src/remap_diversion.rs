//! Per-device button and thumb-wheel diversion. Protocol references:
//! https://lekensteyn.nl/files/logitech/x1b04_specialkeysmsebuttons.html
//! https://openlogi.org/hidpp/features/x2150-thumbwheel
//! Temporary diversion preserves native remaps and thumb-wheel inversion.

use super::{
    Action, BindingSource, Device, EV_KEY, EV_REL, Event, Mapper, VirtualDevice, binding_source,
    ioctl_value, parse_button, set_capability,
};
use crate::hidpp::{Client, Hidraw, Packet, Transport};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io,
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU16, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::watch;

const FEATURE: u16 = 0x1b04;
const SOFTWARE_ID: u8 = 0x0c;
static CONFIGURE: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlInfo {
    pub cid: u16,
    pub source: String,
    pub task_id: u16,
    pub reprogrammable: bool,
    pub divertible: bool,
    pub virtual_control: bool,
}

fn source_name(cid: u16) -> String {
    match cid {
        0x0050 => "left".into(),
        0x0051 => "right".into(),
        0x0052 => "middle".into(),
        0x0053 => "back".into(),
        0x0056 => "forward".into(),
        0x00c3 => "gesture".into(),
        0x00c4 => "smartshift".into(),
        0x01a0 => "haptic".into(),
        _ => format!("cid:0x{cid:04x}"),
    }
}

pub fn control_id(button: &str) -> Result<u16> {
    if let Some(value) = button.strip_prefix("cid:") {
        let cid = if let Some(hex) = value.strip_prefix("0x") {
            u16::from_str_radix(hex, 16).context("invalid hexadecimal HID++ control ID")?
        } else {
            value.parse().context("invalid decimal HID++ control ID")?
        };
        ensure!(cid != 0, "HID++ control ID zero is reserved");
        return Ok(cid);
    }
    match button {
        "gesture" => Ok(0x00c3),
        "smartshift" => Ok(0x00c4),
        "haptic" => Ok(0x01a0),
        _ => match parse_button(button)? {
            0x110 => Ok(0x0050),
            0x111 => Ok(0x0051),
            0x112 => Ok(0x0052),
            0x113 => Ok(0x0053),
            0x114 => Ok(0x0056),
            0x117 => Ok(0x00c3),
            _ => bail!(
                "this Linux button has no standard HID++ identity; use cid:0xNNNN from the device's control table"
            ),
        },
    }
}

pub(super) fn parse_bindings(
    bindings: &BTreeMap<String, String>,
) -> Result<BTreeMap<BindingSource, Action>> {
    ensure!(
        bindings.len() <= 32,
        "at most 32 control bindings are supported per device"
    );
    let mut parsed = BTreeMap::new();
    for (button, action) in bindings {
        let source = binding_source(button).with_context(|| format!("control {button}"))?;
        let action = Action::parse(action).with_context(|| format!("button {button}"))?;
        ensure!(
            parsed.insert(source, action).is_none(),
            "multiple bindings refer to the same control {button}; remove its alias or numeric duplicate"
        );
    }
    Ok(parsed)
}

fn split_bindings(
    bindings: BTreeMap<BindingSource, Action>,
) -> (BTreeMap<u16, Action>, [Option<Action>; 2]) {
    let mut controls = BTreeMap::new();
    let mut wheel = [None, None];
    for (source, action) in bindings {
        match source {
            BindingSource::Control(cid) => {
                controls.insert(cid, action);
            }
            BindingSource::ThumbLeft => wheel[0] = Some(action),
            BindingSource::ThumbRight => wheel[1] = Some(action),
        }
    }
    (controls, wheel)
}

fn thumb_status<T: Transport>(client: &mut Client<T>, slot: u8, feature: u8) -> Result<(u8, bool)> {
    let reply = client.feature(slot, feature, 1, &[])?;
    ensure!(
        reply.len() >= 2 && reply[0] <= 1,
        "invalid thumb-wheel reporting status"
    );
    Ok((reply[0], reply[1] & 1 != 0))
}

struct ThumbWheel {
    feature: u8,
    native_resolution: i64,
    diverted_resolution: i64,
    positive_left: bool,
    actions: [Option<Action>; 2],
    remainder: i64,
    repeat_interval_ms: Arc<AtomicU16>,
    last_action: Option<Instant>,
    attempted: bool,
}

impl ThumbWheel {
    fn set_diverted<T: Transport>(
        &mut self,
        client: &mut Client<T>,
        slot: u8,
        enabled: bool,
    ) -> Result<()> {
        let (current_mode, inverted) = thumb_status(client, slot, self.feature)?;
        ensure!(
            !enabled || current_mode == 0,
            "thumb wheel was diverted by another controller before activation"
        );
        let mode = u8::from(enabled);
        if enabled {
            self.attempted = true;
        }
        client.feature(slot, self.feature, 2, &[mode, u8::from(inverted), 0])?;
        ensure!(
            thumb_status(client, slot, self.feature)? == (mode, inverted),
            "device did not apply thumb-wheel diversion"
        );
        self.attempted = enabled;
        Ok(())
    }

    fn read<T: Transport>(
        client: &mut Client<T>,
        slot: u8,
        actions: [Option<Action>; 2],
        exclusive: bool,
    ) -> Result<Self> {
        let (feature, version) = client
            .root_feature(slot, 0x2150)?
            .context("device does not expose a thumb wheel")?;
        ensure!(
            version == 0,
            "unsupported thumb-wheel feature version {version}"
        );
        let info = client.feature(slot, feature, 0, &[])?;
        ensure!(info.len() >= 8, "truncated thumb-wheel information");
        let native_resolution = i64::from(u16::from_be_bytes([info[0], info[1]]));
        let diverted_resolution = i64::from(u16::from_be_bytes([info[2], info[3]]));
        ensure!(
            native_resolution != 0 && diverted_resolution != 0,
            "invalid thumb-wheel resolution"
        );
        let (mode, _) = thumb_status(client, slot, feature)?;
        ensure!(
            !exclusive || mode == 0,
            "thumb wheel is already diverted by another controller; stop that controller or reconnect the device before remapping"
        );
        Ok(Self {
            feature,
            native_resolution,
            diverted_resolution,
            positive_left: info[4] & 1 == 0,
            actions,
            remainder: 0,
            repeat_interval_ms: Arc::new(AtomicU16::new(0)),
            last_action: None,
            attempted: false,
        })
    }

    fn snapshot(&mut self, packet: &Packet, slot: u8, mapper: &mut Mapper) -> Result<Vec<Event>> {
        self.snapshot_at(packet, slot, mapper, Instant::now())
    }

    fn snapshot_at(
        &mut self,
        packet: &Packet,
        slot: u8,
        mapper: &mut Mapper,
        now: Instant,
    ) -> Result<Vec<Event>> {
        if packet.device != slot || packet.command != self.feature || packet.address != 0 {
            return Ok(Vec::new());
        }
        ensure!(packet.data.len() >= 6, "truncated thumb-wheel notification");
        let status = packet.data[4];
        ensure!(status <= 3, "invalid thumb-wheel rotation status");
        let delta = i64::from(i16::from_be_bytes([packet.data[0], packet.data[1]]));
        if status == 1 || delta != 0 && self.remainder.signum() != delta.signum() {
            self.remainder = 0;
        }
        self.remainder += delta * self.native_resolution;
        let pulses = self.remainder / self.diverted_resolution;
        self.remainder %= self.diverted_resolution;
        if status == 0 || status == 3 {
            self.remainder = 0;
        }
        if pulses == 0 {
            return Ok(Vec::new());
        }
        let left = (pulses > 0) == self.positive_left;
        let Some(action) = self.actions[usize::from(!left)].as_ref() else {
            return Ok(Vec::new());
        };
        let repeat_interval =
            Duration::from_millis(u64::from(self.repeat_interval_ms.load(Ordering::Relaxed)));
        if !repeat_interval.is_zero() {
            if self
                .last_action
                .is_some_and(|last| now.saturating_duration_since(last) < repeat_interval)
            {
                return Ok(Vec::new());
            }
            // Drop excess movement rather than replaying queued shortcuts after
            // the user stops turning. Gesture changes do not bypass the limit.
            let events = mapper.pulse(action);
            if !events.is_empty() {
                self.last_action = Some(now);
            }
            return Ok(events);
        }
        let mut events = Vec::new();
        // Discard implausibly large bursts instead of queuing an input flood.
        for _ in 0..pulses.abs().min(64) {
            events.extend(mapper.pulse(action));
        }
        Ok(events)
    }
}

fn get_reporting<T: Transport>(
    client: &mut Client<T>,
    slot: u8,
    feature: u8,
    cid: u16,
) -> Result<u8> {
    let reply = client
        .feature(slot, feature, 2, &cid.to_be_bytes())
        .with_context(|| format!("read reporting for control 0x{cid:04x}"))?;
    ensure!(
        reply.len() >= 5 && reply[..2] == cid.to_be_bytes(),
        "invalid HID++ control reporting response"
    );
    Ok(reply[2])
}

fn check_heartbeat<T: Transport>(
    client: &mut Client<T>,
    slot: u8,
    control: Option<(u8, u16)>,
    wheel: Option<u8>,
    command_path: &Path,
    mapper: &mut Mapper,
) -> Result<Vec<Event>> {
    // Skip a busy controller so event processing never waits for another command.
    let Some(_command) = crate::runtime::try_lock_file(command_path)? else {
        return Ok(Vec::new());
    };
    let _guard = CONFIGURE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let check = (|| -> Result<()> {
        if let Some((feature, cid)) = control {
            ensure!(
                get_reporting(client, slot, feature, cid)? & 1 != 0,
                "device reset its control diversion; remapping will restart after discovery"
            );
        }
        if let Some(feature) = wheel {
            ensure!(
                thumb_status(client, slot, feature)?.0 == 1,
                "device reset its thumb-wheel diversion; remapping will restart after discovery"
            );
        }
        Ok(())
    })();
    match check {
        Err(error) if error.is::<crate::hidpp::RequestTimeout>() => Ok(mapper.release_all()),
        result => result.map(|_| Vec::new()),
    }
}

fn set_diverted<T: Transport>(
    client: &mut Client<T>,
    slot: u8,
    feature: u8,
    cid: u16,
    enabled: bool,
) -> Result<()> {
    let [hi, lo] = cid.to_be_bytes();
    // DVALID updates only temporary diversion. Remap zero preserves the prior
    // native action; no persistent or raw movement valid bits are sent.
    let request = [hi, lo, 0x02 | u8::from(enabled), 0, 0];
    let reply = client
        .feature(slot, feature, 3, &request)
        .with_context(|| format!("set diversion for control 0x{cid:04x}"))?;
    ensure!(
        reply.starts_with(&request),
        "device did not acknowledge its exact diversion request"
    );
    let reporting =
        get_reporting(client, slot, feature, cid).context("verify updated diversion")?;
    ensure!(
        (reporting & 1 != 0) == enabled,
        "device did not apply temporary button diversion"
    );
    Ok(())
}

fn read_controls<T: Transport>(client: &mut Client<T>, slot: u8) -> Result<(u8, Vec<ControlInfo>)> {
    let (feature, _) = client
        .root_feature(slot, FEATURE)?
        .context("device does not expose reprogrammable control feature 0x1b04")?;
    let count = client
        .feature(slot, feature, 0, &[])?
        .first()
        .copied()
        .context("missing HID++ control count")?;
    ensure!(
        count > 0 && count <= 128,
        "unsupported HID++ control table size {count}"
    );
    let mut controls = BTreeMap::new();
    for index in 0..count {
        let row = client.feature(slot, feature, 1, &[index])?;
        ensure!(row.len() >= 5, "truncated HID++ control table entry");
        let cid = u16::from_be_bytes([row[0], row[1]]);
        let flags = row[4];
        let info = ControlInfo {
            cid,
            source: source_name(cid),
            task_id: u16::from_be_bytes([row[2], row[3]]),
            reprogrammable: flags & 0x10 != 0,
            divertible: flags & 0x20 != 0,
            virtual_control: flags & 0x80 != 0,
        };
        ensure!(
            controls.insert(cid, info).is_none(),
            "duplicate HID++ control table entry"
        );
    }
    Ok((feature, controls.into_values().collect()))
}

fn validate_controls(controls: &[ControlInfo], bindings: &BTreeMap<u16, Action>) -> Result<()> {
    // Complete all validation before changing any physical controls.
    for cid in bindings.keys() {
        let control = controls
            .iter()
            .find(|control| control.cid == *cid)
            .with_context(|| format!("device does not expose requested control 0x{cid:04x}"))?;
        ensure!(
            !control.virtual_control && control.reprogrammable && control.divertible,
            "control 0x{cid:04x} cannot be reprogrammed and temporarily diverted on this device"
        );
    }
    Ok(())
}

fn prepare<T: Transport>(
    client: &mut Client<T>,
    slot: u8,
    bindings: &BTreeMap<u16, Action>,
) -> Result<u8> {
    let (feature, controls) = read_controls(client, slot).context("read remapping controls")?;
    validate_controls(&controls, bindings)?;
    for cid in bindings.keys() {
        let reporting =
            get_reporting(client, slot, feature, *cid).context("check existing diversion")?;
        ensure!(
            reporting & 0x15 == 0,
            "control 0x{cid:04x} is already diverted by another controller; stop that controller or reconnect the device before remapping"
        );
    }
    Ok(feature)
}

fn prepare_bindings<T: Transport>(
    client: &mut Client<T>,
    slot: u8,
    controls: &BTreeMap<u16, Action>,
    wheel_actions: [Option<Action>; 2],
) -> Result<(Option<u8>, Option<ThumbWheel>)> {
    let feature = if controls.is_empty() {
        None
    } else {
        Some(prepare(client, slot, controls)?)
    };
    let wheel = if wheel_actions.iter().any(Option::is_some) {
        Some(ThumbWheel::read(client, slot, wheel_actions, true)?)
    } else {
        None
    };
    Ok((feature, wheel))
}

fn wake<T: Transport>(client: &mut Client<T>, slot: u8, long: bool) -> Result<()> {
    let protocol = client
        .ping(slot, long)
        .context("wake device before reading its remapping controls")?;
    ensure!(
        protocol.0 >= 2,
        "control diversion requires HID++ 2.0 or newer"
    );
    Ok(())
}

fn open(device: &Device, remapping: bool) -> Result<(Client<Hidraw>, u8)> {
    crate::model::require_remappable(device)?;
    ensure!(
        device.receiver_id.is_none() || matches!(device.slot, Some(1..=6)),
        "receiver remapping requires an explicit device slot"
    );
    // Short-lived command reads share the normal command lock and client ID.
    // The live remapper uses its own ID so concurrent reads cannot consume its
    // acknowledgements or heartbeat replies.
    let mut client = if remapping {
        crate::device::open_route_with_client_id(device, SOFTWARE_ID)?
    } else {
        crate::device::open_route(device)?
    };
    let slot = device.slot.unwrap_or(0xff);
    // The peripheral can sleep after inventory. Give its first probe the
    // wireless wake deadline before sending ordinary feature requests.
    wake(&mut client, slot, crate::device::long_ping(device))?;
    Ok((client, slot))
}

/// Read-only capability enumeration, suitable for `info` and binding preflight.
pub fn controls(device: &Device) -> Result<Vec<ControlInfo>> {
    let _guard = CONFIGURE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    crate::model::require_remappable(device)?;
    if !device
        .capabilities
        .iter()
        .any(|capability| matches!(capability.as_str(), "button-diversion" | "feature:1b04"))
    {
        return Ok(Vec::new());
    }
    let (mut client, slot) = open(device, false)?;
    read_controls(&mut client, slot).map(|(_, controls)| controls)
}

/// Validate device support without changing reporting or creating virtual input.
/// A running remapper may own diversion while the user updates saved bindings.
pub fn check_bindings(device: &Device, bindings: &BTreeMap<String, String>) -> Result<()> {
    crate::model::require_remappable(device)?;
    let bindings = parse_bindings(bindings)?;
    if bindings.is_empty() {
        return Ok(());
    }
    let (controls, wheel) = split_bindings(bindings);
    let _guard = CONFIGURE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (mut client, slot) = open(device, false)?;
    if !controls.is_empty() {
        validate_controls(&read_controls(&mut client, slot)?.1, &controls)?;
    }
    if wheel.iter().any(Option::is_some) {
        ThumbWheel::read(&mut client, slot, wheel, false)?;
    }
    Ok(())
}

fn snapshot(packet: &Packet, slot: u8, feature: u8, mapper: &mut Mapper) -> Result<Vec<Event>> {
    // Low software ID zero distinguishes notifications from request responses.
    if packet.device != slot || packet.command != feature || packet.address != 0 {
        return Ok(Vec::new());
    }
    ensure!(
        packet.data.len() >= 8,
        "truncated diverted-button notification"
    );
    let mut pressed = [0; 4];
    let mut count = 0;
    let mut ended = false;
    for &bytes in packet.data[..8].as_chunks::<2>().0 {
        let cid = u16::from_be_bytes(bytes);
        if cid == 0 {
            ended = true;
            continue;
        }
        ensure!(
            !ended && !pressed[..count].contains(&cid),
            "invalid diverted-button notification"
        );
        pressed[count] = cid;
        count += 1;
    }
    Ok(mapper.update(&pressed[..count]))
}

impl VirtualDevice {
    fn actions(bindings: &BTreeMap<BindingSource, Action>) -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open("/dev/uinput")
            .context("opening /dev/uinput; button actions require virtual-input access")?;
        set_capability(&file, 100, EV_KEY)?;
        let has_keyboard = bindings
            .values()
            .any(|action| !matches!(action, Action::Mouse(_)));
        let has_mouse = bindings
            .values()
            .any(|action| matches!(action, Action::Mouse(_)));
        if has_keyboard {
            for key in 1..=88 {
                set_capability(&file, 101, key)?;
            }
        }
        if has_mouse {
            // Relative axes identify a mouse to desktop input stacks; actual
            // motion continues through the untouched physical input device.
            set_capability(&file, 100, EV_REL)?;
            for axis in [0, 1] {
                set_capability(&file, 102, axis)?;
            }
            for button in 0x110..=0x112 {
                set_capability(&file, 101, button)?;
            }
        }
        for action in bindings.values() {
            for &key in action.keys() {
                set_capability(&file, 101, key)?;
            }
        }
        // SAFETY: uinput_setup is composed entirely of integer fields/arrays.
        let mut setup: libc::uinput_setup = unsafe { std::mem::zeroed() };
        setup.id.bustype = 0x06; // BUS_VIRTUAL
        for (dst, src) in setup.name.iter_mut().zip(b"logishell actions") {
            *dst = *src as libc::c_char;
        }
        // SAFETY: the ioctl reads exactly this initialized uinput_setup.
        let result = unsafe {
            libc::ioctl(
                file.as_raw_fd(),
                libc::_IOW::<libc::uinput_setup>(u32::from(b'U'), 3),
                &setup,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error().into());
        }
        ioctl_value(&file, libc::_IO(u32::from(b'U'), 1), 0)
            .context("creating virtual action device")?;
        Ok(Self(file))
    }
}

struct Session<T: Transport> {
    device: Device,
    client: Client<T>,
    slot: u8,
    feature: Option<u8>,
    wheel: Option<ThumbWheel>,
    attempted: Vec<u16>,
    output: VirtualDevice,
    mapper: Mapper,
}

fn restore_diversions<T: Transport>(
    client: &mut Client<T>,
    device: &Device,
    slot: u8,
    feature: u8,
    attempted: &[u16],
) {
    for cid in attempted.iter().rev() {
        if let Err(error) = crate::device::verify_route(client, device) {
            tracing::warn!(
                "skipped remapping cleanup because device identity could not be verified: {error:#}; reconnect the original device if a control remains inactive"
            );
            break;
        }
        if let Err(error) = set_diverted(client, slot, feature, *cid, false) {
            tracing::warn!(
                control = format_args!("0x{cid:04x}"),
                "could not restore temporary control diversion: {error:#}; reconnect the device if the control remains inactive"
            );
        }
    }
}

fn restore_thumb_diversion<T: Transport>(
    client: &mut Client<T>,
    device: &Device,
    slot: u8,
    wheel: &mut ThumbWheel,
) -> Result<()> {
    crate::device::verify_route(client, device)?;
    wheel.set_diverted(client, slot, false)
}

impl<T: Transport> Drop for Session<T> {
    fn drop(&mut self) {
        for event in self.mapper.release_all() {
            let _ = self.output.emit(event);
        }
        if self.attempted.is_empty() && !self.wheel.as_ref().is_some_and(|wheel| wheel.attempted) {
            return;
        }
        let _command = match crate::runtime::mapping_command_lock() {
            Ok(lock) => lock,
            Err(error) => {
                tracing::warn!(
                    "could not lock remapping cleanup: {error:#}; reconnect the device if a control remains inactive"
                );
                return;
            }
        };
        let _guard = CONFIGURE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(wheel) = self.wheel.as_mut().filter(|wheel| wheel.attempted) {
            let restored =
                restore_thumb_diversion(&mut self.client, &self.device, self.slot, wheel);
            if let Err(error) = restored {
                tracing::warn!(
                    "could not restore native thumb-wheel reporting: {error:#}; reconnect the device if scrolling remains inactive"
                );
            }
        }
        if let Some(feature) = self.feature {
            restore_diversions(
                &mut self.client,
                &self.device,
                self.slot,
                feature,
                &self.attempted,
            );
        }
    }
}

struct CancelOnDrop(Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn stopped(cancel: &AtomicBool, shutdown: &watch::Receiver<bool>) -> bool {
    cancel.load(Ordering::Acquire) || *shutdown.borrow() || shutdown.has_changed().is_err()
}

pub(super) async fn run(
    device: Device,
    bindings: BTreeMap<String, String>,
    thumb_wheel_interval_ms: Arc<AtomicU16>,
    shutdown: watch::Receiver<bool>,
) -> Result<()> {
    if bindings.is_empty() || *shutdown.borrow() || shutdown.has_changed().is_err() {
        return Ok(());
    }
    let parsed = parse_bindings(&bindings)?;
    let cancel = Arc::new(AtomicBool::new(false));
    let _cancel_on_drop = CancelOnDrop(cancel.clone());
    tokio::task::spawn_blocking(move || {
        let command_path = crate::runtime::runtime_dir()?.join("controller.lock");
        let (controls, wheel_actions) = split_bindings(parsed.clone());
        let (client, slot, feature, wheel) = {
            let _command = crate::runtime::mapping_command_lock()?;
            let _guard = CONFIGURE
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let (mut client, slot) = open(&device, true).context("open remapping device")?;
            let (feature, mut wheel) =
                prepare_bindings(&mut client, slot, &controls, wheel_actions)?;
            if let Some(wheel) = &mut wheel {
                wheel.repeat_interval_ms = thumb_wheel_interval_ms;
            }
            (client, slot, feature, wheel)
        };
        if stopped(&cancel, &shutdown) {
            return Ok(());
        }
        let output = VirtualDevice::actions(&parsed)?;
        let mut session = Session {
            device: device.clone(),
            client,
            slot,
            feature,
            wheel,
            attempted: Vec::new(),
            output,
            mapper: Mapper {
                bindings: controls,
                ..Mapper::default()
            },
        };
        // Leave the physical buttons in native mode while udev sees the output.
        std::thread::sleep(Duration::from_millis(250));
        if stopped(&cancel, &shutdown) {
            return Ok(());
        }
        {
            // Keep the guard in this inner scope: Session::drop must acquire it
            // after a failed request, including a lost acknowledgement.
            let _command = crate::runtime::mapping_command_lock()?;
            let _guard = CONFIGURE
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let keys: Vec<_> = session.mapper.bindings.keys().copied().collect();
            for cid in keys {
                if stopped(&cancel, &shutdown) {
                    return Ok(());
                }
                crate::device::verify_route(&mut session.client, &session.device).with_context(
                    || format!("verify device before diverting control 0x{cid:04x}"),
                )?;
                session.attempted.push(cid);
                set_diverted(
                    &mut session.client,
                    slot,
                    feature.context("missing control feature")?,
                    cid,
                    true,
                )?;
            }
            if let Some(wheel) = &mut session.wheel {
                if stopped(&cancel, &shutdown) {
                    return Ok(());
                }
                crate::device::verify_route(&mut session.client, &session.device)?;
                wheel.set_diverted(&mut session.client, slot, true)?;
            }
        }
        tracing::info!(device = %device.id, "HID++ control remapping active");
        let mut heartbeat = Instant::now();
        while !stopped(&cancel, &shutdown) {
            if let Some(packet) = session.client.next_event(Duration::from_millis(100))? {
                let mut events = if let Some(feature) = feature {
                    snapshot(&packet, slot, feature, &mut session.mapper)?
                } else {
                    Vec::new()
                };
                if let Some(wheel) = &mut session.wheel {
                    events.extend(wheel.snapshot(&packet, slot, &mut session.mapper)?);
                }
                for event in events {
                    session
                        .output
                        .emit(event)
                        .context("emitting mapped control action")?;
                }
                session.mapper.pulse_holds.clear();
            }
            if heartbeat.elapsed() >= Duration::from_secs(2) {
                // A missed wireless reply releases held keys without disabling
                // bindings. A confirmed diversion reset still restarts the worker.
                let control = feature.zip(session.attempted.first().copied());
                let wheel = session.wheel.as_ref().map(|wheel| wheel.feature);
                for event in check_heartbeat(
                    &mut session.client,
                    slot,
                    control,
                    wheel,
                    &command_path,
                    &mut session.mapper,
                )? {
                    session
                        .output
                        .emit(event)
                        .context("releasing mapped keys after a missed device reply")?;
                }
                heartbeat = Instant::now();
            }
        }
        Ok(())
    })
    .await
    .context("button diversion worker failed")?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hidpp::testing::Scripted;

    fn report(feature: u8, function: u8, data: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0; 20];
        bytes[..4].copy_from_slice(&[0x11, 2, feature, function << 4 | SOFTWARE_ID]);
        bytes[4..4 + data.len()].copy_from_slice(data);
        bytes
    }
    fn packet(cids: &[u16]) -> Packet {
        let mut data = vec![0; 16];
        for (index, cid) in cids.iter().take(8).enumerate() {
            data[index * 2..index * 2 + 2].copy_from_slice(&cid.to_be_bytes());
        }
        Packet {
            device: 2,
            command: 9,
            address: 0,
            data,
        }
    }
    fn mapper() -> Result<Mapper> {
        Ok(Mapper {
            bindings: BTreeMap::from([
                (0x53, Action::parse("key:ctrl+c")?),
                (0x56, Action::parse("key:ctrl+v")?),
            ]),
            ..Mapper::default()
        })
    }

    #[test]
    fn cleanup_never_writes_to_a_receiver_slot_replaced_by_another_device() -> Result<()> {
        let serial = [1; 16];
        let receiver_id = format!("receiver:c548:{}", "01".repeat(16));
        let device = Device {
            id: format!("{receiver_id}:device:1234:01020304"),
            name: "original peripheral".into(),
            transport: crate::model::Transport::Bolt,
            state: crate::model::DeviceState::Online,
            receiver_id: Some(receiver_id),
            slot: Some(2),
            hid_path: Some("/not-opened".into()),
            capabilities: vec!["button-diversion".into()],
            ..Default::default()
        };
        let register = |long: bool, address: u8, data: &[u8]| {
            let mut bytes = vec![0; if long { 20 } else { 7 }];
            bytes[..4].copy_from_slice(&[if long { 0x11 } else { 0x10 }, 0xff, 0x83, address]);
            bytes[4..4 + data.len()].copy_from_slice(data);
            bytes
        };
        for wheel in [false, true] {
            let mut client = Client::with_client_id(
                // Any restoration write after these identity reads fails the
                // mock immediately. The new occupant must remain untouched.
                Scripted::new(
                    [register(false, 0xfb, &[]), register(false, 0xb5, &[0x52])]
                        .into_iter()
                        .zip([
                            register(true, 0xfb, &serial),
                            register(true, 0xb5, &[0, 0, 0x12, 0x34, 5, 6, 7, 8]),
                        ]),
                ),
                SOFTWARE_ID,
            )?;
            if wheel {
                assert!(restore_thumb_diversion(&mut client, &device, 2, &mut thumb()?).is_err());
            } else {
                restore_diversions(&mut client, &device, 2, 9, &[0x53]);
            }
            assert!(client.into_transport().exchanges.is_empty());
        }
        Ok(())
    }

    #[test]
    fn names_and_explicit_cids_cannot_create_duplicate_bindings() -> Result<()> {
        assert_eq!(control_id("back")?, 0x0053);
        assert_eq!(control_id("cid:0x0199")?, 0x0199);
        assert!(control_id("cid:0").is_err());
        let bindings = BTreeMap::from([
            ("back".into(), "key:a".into()),
            ("cid:0x0053".into(), "key:b".into()),
        ]);
        assert!(parse_bindings(&bindings).is_err());
        Ok(())
    }

    #[test]
    fn fresh_remap_sessions_probe_with_private_id_before_querying_controls() -> Result<()> {
        for (slot, long) in [(2, false), (0xff, true), (0xff, false)] {
            let request = |feature, function, data: &[u8]| {
                let mut bytes = report(feature, function, data);
                bytes[1] = slot;
                bytes
            };
            let mut ping = request(0, 1, &[0, 0, 0x5a]);
            if !long {
                ping[0] = 0x10;
                ping.truncate(7);
            }
            let mut client = Client::with_client_id(
                Scripted::new(
                    [
                        ping,
                        request(0, 0, &[0x1b, 0x04]),
                        request(9, 0, &[]),
                        request(9, 1, &[0]),
                    ]
                    .into_iter()
                    .zip([
                        request(0, 1, &[4, 5, 0x5a]),
                        request(0, 0, &[9, 0, 2]),
                        request(9, 0, &[1]),
                        request(9, 1, &[0x01, 0x99, 0, 0, 0x30]),
                    ]),
                ),
                SOFTWARE_ID,
            )?;
            wake(&mut client, slot, long)?;
            let (_, controls) = read_controls(&mut client, slot)?;
            assert_eq!(controls[0].cid, 0x0199);
            assert!(controls[0].divertible && controls[0].reprogrammable);
        }
        Ok(())
    }

    #[test]
    fn keyboard_cids_map_only_when_the_reported_control_is_divertible() -> Result<()> {
        let (bindings, _) = split_bindings(parse_bindings(&BTreeMap::from([(
            "cid:0x0199".into(),
            "key:f13".into(),
        )]))?);
        let mut client = Client::with_client_id(
            Scripted::new(
                [
                    report(0, 0, &[0x1b, 0x04]),
                    report(9, 0, &[]),
                    report(9, 1, &[0]),
                    report(9, 2, &[0x01, 0x99]),
                ]
                .into_iter()
                .zip([
                    report(0, 0, &[9, 0, 2]),
                    report(9, 0, &[1]),
                    report(9, 1, &[0x01, 0x99, 0, 0, 0x30]),
                    report(9, 2, &[0x01, 0x99, 0, 0, 0]),
                ]),
            ),
            SOFTWARE_ID,
        )?;
        assert_eq!(prepare(&mut client, 2, &bindings)?, 9);
        let mut mapper = Mapper {
            bindings,
            ..Mapper::default()
        };
        assert!(snapshot(&packet(&[0x0053]), 2, 9, &mut mapper)?.is_empty());
        assert_eq!(
            snapshot(&packet(&[0x0199]), 2, 9, &mut mapper)?,
            vec![Event::key(183, 1), Event::sync()]
        );
        assert_eq!(
            snapshot(&packet(&[]), 2, 9, &mut mapper)?,
            vec![Event::key(183, 0), Event::sync()]
        );
        Ok(())
    }

    #[test]
    fn legacy_protocol_is_rejected_before_feature_queries() -> Result<()> {
        let mut client = Client::with_client_id(
            Scripted::new(
                [vec![0x10, 2, 0, 0x1c, 0, 0, 0x5a]]
                    .into_iter()
                    .zip([report(0, 1, &[1, 0, 0x5a])]),
            ),
            SOFTWARE_ID,
        )?;
        assert!(wake(&mut client, 2, false).is_err());
        Ok(())
    }

    #[test]
    fn snapshots_track_control_identity_not_position() -> Result<()> {
        let mut mapper = mapper()?;
        assert_eq!(
            snapshot(&packet(&[0x53]), 2, 9, &mut mapper)?,
            vec![Event::key(29, 1), Event::key(46, 1), Event::sync()]
        );
        assert_eq!(
            snapshot(&packet(&[0x53, 0x56]), 2, 9, &mut mapper)?,
            vec![Event::key(47, 1), Event::sync()]
        );
        assert_eq!(
            snapshot(&packet(&[0x56]), 2, 9, &mut mapper)?,
            vec![Event::key(46, 0), Event::sync()]
        );
        assert_eq!(
            snapshot(&packet(&[]), 2, 9, &mut mapper)?,
            vec![Event::key(47, 0), Event::key(29, 0), Event::sync()]
        );
        Ok(())
    }

    #[test]
    fn repeated_snapshots_and_shared_output_keys_release_once() -> Result<()> {
        for action in ["mouse:left", "key:super", "key:ctrl+super"] {
            let action = Action::parse(action)?;
            let mut mapper = Mapper {
                bindings: BTreeMap::from([(0x53, action.clone()), (0x56, action.clone())]),
                ..Mapper::default()
            };
            let events = |pressed: bool| -> Vec<_> {
                let mut keys = action.keys().to_vec();
                if !pressed {
                    keys.reverse();
                }
                keys.into_iter()
                    .map(|code| Event::key(code, i32::from(pressed)))
                    .chain([Event::sync()])
                    .collect()
            };
            assert_eq!(snapshot(&packet(&[0x53]), 2, 9, &mut mapper)?, events(true));
            assert!(snapshot(&packet(&[0x53]), 2, 9, &mut mapper)?.is_empty());
            assert!(snapshot(&packet(&[0x53, 0x56]), 2, 9, &mut mapper)?.is_empty());
            assert!(snapshot(&packet(&[0x56]), 2, 9, &mut mapper)?.is_empty());
            assert_eq!(snapshot(&packet(&[]), 2, 9, &mut mapper)?, events(false));
            assert!(mapper.release_all().is_empty());
        }
        Ok(())
    }

    #[test]
    fn cleanup_releases_shortcut_targets_before_shared_modifiers_once() -> Result<()> {
        let mut mapper = mapper()?;
        snapshot(&packet(&[0x53, 0x56]), 2, 9, &mut mapper)?;
        assert_eq!(
            mapper.release_all(),
            vec![
                Event::key(46, 0),
                Event::key(47, 0),
                Event::key(29, 0),
                Event::sync()
            ]
        );
        assert!(mapper.release_all().is_empty());
        assert!(snapshot(&packet(&[]), 2, 9, &mut mapper)?.is_empty());
        Ok(())
    }

    #[test]
    fn other_devices_replies_and_unbound_controls_never_emit_actions() -> Result<()> {
        let mut mapper = mapper()?;
        assert!(snapshot(&packet(&[0xc4]), 2, 9, &mut mapper)?.is_empty());
        assert!(snapshot(&packet(&[0x53]), 3, 9, &mut mapper)?.is_empty());
        let mut reply = packet(&[0x53]);
        reply.address = SOFTWARE_ID;
        assert!(snapshot(&reply, 2, 9, &mut mapper)?.is_empty());
        assert!(snapshot(&packet(&[0x53, 0x53]), 2, 9, &mut mapper).is_err());
        Ok(())
    }

    #[test]
    fn heartbeat_skips_busy_commands_and_resumes_without_losing_diversion_checks() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let command_path = directory.path().join("controller.lock");
        let command = crate::runtime::try_lock_file(&command_path)?.expect("fresh command lock");
        let mut client = Client::with_client_id(
            Scripted::new([
                (report(9, 2, &[0, 0xc3]), report(9, 2, &[0, 0xc3, 1, 0, 0])),
                (report(9, 2, &[0, 0xc3]), report(9, 2, &[0, 0xc3, 0, 0, 0])),
            ]),
            SOFTWARE_ID,
        )?;
        let mut mapper = Mapper::default();
        let started = Instant::now();
        check_heartbeat(
            &mut client,
            2,
            Some((9, 0xc3)),
            None,
            &command_path,
            &mut mapper,
        )?;
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(command);
        check_heartbeat(
            &mut client,
            2,
            Some((9, 0xc3)),
            None,
            &command_path,
            &mut mapper,
        )?;
        let error = check_heartbeat(
            &mut client,
            2,
            Some((9, 0xc3)),
            None,
            &command_path,
            &mut mapper,
        )
        .expect_err("cleared diversion must stop the mapping");
        assert!(
            error
                .to_string()
                .contains("device reset its control diversion")
        );
        assert!(client.into_transport().exchanges.is_empty());
        assert!(crate::runtime::try_lock_file(&command_path)?.is_some());
        Ok(())
    }

    #[test]
    fn missed_heartbeat_releases_shortcuts_and_keeps_bindings_usable() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let command_path = directory.path().join("controller.lock");
        let mut client = Client::with_client_id(
            Scripted::new([
                (report(9, 2, &[0, 0x53]), Vec::new()),
                (report(9, 2, &[0, 0x53]), report(9, 2, &[0, 0x53, 1, 0, 0])),
            ]),
            SOFTWARE_ID,
        )?;
        let mut mapper = mapper()?;
        snapshot(&packet(&[0x53]), 2, 9, &mut mapper)?;
        let release = vec![Event::key(46, 0), Event::key(29, 0), Event::sync()];
        assert_eq!(
            check_heartbeat(
                &mut client,
                2,
                Some((9, 0x53)),
                None,
                &command_path,
                &mut mapper
            )?,
            release
        );
        assert!(mapper.down.is_empty() && mapper.output_holds.is_empty());
        assert!(snapshot(&packet(&[]), 2, 9, &mut mapper)?.is_empty());
        assert!(
            check_heartbeat(
                &mut client,
                2,
                Some((9, 0x53)),
                None,
                &command_path,
                &mut mapper
            )?
            .is_empty()
        );
        assert_eq!(
            snapshot(&packet(&[0x53]), 2, 9, &mut mapper)?,
            vec![Event::key(29, 1), Event::key(46, 1), Event::sync()]
        );
        assert_eq!(snapshot(&packet(&[]), 2, 9, &mut mapper)?, release);
        assert!(client.into_transport().exchanges.is_empty());
        assert!(crate::runtime::try_lock_file(&command_path)?.is_some());
        Ok(())
    }

    #[test]
    fn heartbeat_does_not_hide_malformed_replies_or_protocol_errors() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let command_path = directory.path().join("controller.lock");
        let mut mapper = Mapper::default();
        let protocol_error =
            crate::hidpp::testing::packet(2, 0xff, 9, &[0x20 | SOFTWARE_ID, 2], true);
        for (response, expected) in [
            (
                report(9, 2, &[0, 0x56, 1, 0, 0]),
                "invalid HID++ control reporting response",
            ),
            (protocol_error, "HID++ 2 error"),
        ] {
            let mut client = Client::with_client_id(
                Scripted::new([(report(9, 2, &[0, 0x53]), response)]),
                SOFTWARE_ID,
            )?;
            let error = check_heartbeat(
                &mut client,
                2,
                Some((9, 0x53)),
                None,
                &command_path,
                &mut mapper,
            )
            .expect_err("only request timeouts may be recovered");
            assert!(format!("{error:#}").contains(expected));
        }
        Ok(())
    }

    #[test]
    fn heartbeat_transport_disconnect_remains_fatal() -> Result<()> {
        struct Disconnected;
        impl Transport for Disconnected {
            fn send(&mut self, _: &[u8]) -> Result<()> {
                Ok(())
            }

            fn receive(&mut self, _: Duration) -> Result<Option<Vec<u8>>> {
                Err(io::Error::from(io::ErrorKind::NotConnected).into())
            }
        }
        let directory = tempfile::tempdir()?;
        let mut client = Client::with_client_id(Disconnected, SOFTWARE_ID)?;
        let error = check_heartbeat(
            &mut client,
            2,
            Some((9, 0x53)),
            None,
            &directory.path().join("controller.lock"),
            &mut Mapper::default(),
        )
        .expect_err("a transport disconnect must stop the mapping");
        assert_eq!(
            error
                .downcast_ref::<io::Error>()
                .expect("transport error must be preserved")
                .kind(),
            io::ErrorKind::NotConnected
        );
        Ok(())
    }

    #[test]
    fn temporary_diversion_preserves_native_remaps_and_checks_both_write_directions() -> Result<()>
    {
        for (enabled, flags, remap) in [
            (true, 1, 0),
            (false, 0x14, 0x52),
            (true, 0, 0),
            (false, 1, 0x52),
        ] {
            let set = report(9, 3, &[0, 0x53, 2 | u8::from(enabled), 0, 0]);
            let mut client = Client::with_client_id(
                Scripted::new([
                    (set.clone(), set),
                    (
                        report(9, 2, &[0, 0x53]),
                        report(9, 2, &[0, 0x53, flags, 0, remap]),
                    ),
                ]),
                SOFTWARE_ID,
            )?;
            assert_eq!(
                set_diverted(&mut client, 2, 9, 0x53, enabled).is_ok(),
                (flags & 1 != 0) == enabled,
                "enabled={enabled}, observed flags={flags:#x}"
            );
            assert!(client.into_transport().exchanges.is_empty());
        }
        Ok(())
    }

    #[test]
    fn shortcut_handoff_does_not_release_its_shared_modifier() -> Result<()> {
        let mut mapper = mapper()?;
        snapshot(&packet(&[0x53]), 2, 9, &mut mapper)?;
        assert_eq!(
            snapshot(&packet(&[0x56]), 2, 9, &mut mapper)?,
            vec![Event::key(46, 0), Event::key(47, 1), Event::sync()]
        );
        Ok(())
    }

    #[test]
    fn handoffs_release_old_shortcuts_before_new_actions_and_retrigger_new_owners() -> Result<()> {
        for (old, new, expected) in [
            ("key:ctrl+c", "key:v", vec![(46, 0), (29, 0), (47, 1)]),
            (
                "key:ctrl+c",
                "key:alt+v",
                vec![(46, 0), (29, 0), (56, 1), (47, 1)],
            ),
            ("key:ctrl+c", "key:ctrl+c", vec![(46, 0), (46, 1)]),
            ("key:super", "key:super", vec![(125, 0), (125, 1)]),
            (
                "key:super+a",
                "key:super",
                vec![(30, 0), (125, 0), (125, 1)],
            ),
            ("key:ctrl+super", "key:ctrl+super", vec![(125, 0), (125, 1)]),
            (
                "key:super",
                "key:ctrl+super",
                vec![(125, 0), (29, 1), (125, 1)],
            ),
            (
                "key:ctrl+super",
                "key:super",
                vec![(125, 0), (29, 0), (125, 1)],
            ),
            ("mouse:left", "mouse:left", vec![(0x110, 0), (0x110, 1)]),
            ("key:ctrl+super", "key:a", vec![(125, 0), (29, 0), (30, 1)]),
        ] {
            let mut mapper = Mapper {
                bindings: BTreeMap::from([
                    (0x53, Action::parse(old)?),
                    (0x56, Action::parse(new)?),
                ]),
                ..Mapper::default()
            };
            snapshot(&packet(&[0x53]), 2, 9, &mut mapper)?;
            let expected: Vec<_> = expected
                .into_iter()
                .map(|(code, value)| Event::key(code, value))
                .chain([Event::sync()])
                .collect();
            assert_eq!(
                snapshot(&packet(&[0x56]), 2, 9, &mut mapper)?,
                expected,
                "{old} -> {new}"
            );
            assert!(snapshot(&packet(&[0x56]), 2, 9, &mut mapper)?.is_empty());
            assert!(!mapper.release_all().is_empty());
            assert!(mapper.down.is_empty() && mapper.output_holds.is_empty());
        }
        Ok(())
    }

    #[test]
    fn refuses_unavailable_or_already_owned_controls_before_diversion() -> Result<()> {
        for (flags, reporting) in [
            (0x31, Some(1)),
            (0x31, Some(4)),
            (0x31, Some(16)),
            (1, None),
            (0x10, None),
            (0x20, None),
            (0xb0, None),
        ] {
            let mut exchanges = vec![
                (report(0, 0, &[0x1b, 0x04]), report(0, 0, &[9, 0, 2])),
                (report(9, 0, &[]), report(9, 0, &[1])),
                (report(9, 1, &[0]), report(9, 1, &[0, 0x53, 0, 0, flags])),
            ];
            if let Some(reporting) = reporting {
                exchanges.push((
                    report(9, 2, &[0, 0x53]),
                    report(9, 2, &[0, 0x53, reporting, 0, 0]),
                ));
            }
            let mut client = Client::with_client_id(Scripted::new(exchanges), SOFTWARE_ID)?;
            let bindings = BTreeMap::from([(0x53, Action::parse("key:a")?)]);
            assert!(prepare(&mut client, 2, &bindings).is_err());
            assert!(client.into_transport().exchanges.is_empty());
        }
        Ok(())
    }

    fn thumb() -> Result<ThumbWheel> {
        Ok(ThumbWheel {
            feature: 19,
            native_resolution: 20,
            diverted_resolution: 120,
            positive_left: true,
            actions: [
                Some(Action::parse("key:left")?),
                Some(Action::parse("key:right")?),
            ],
            remainder: 0,
            repeat_interval_ms: Arc::new(AtomicU16::new(0)),
            last_action: None,
            attempted: false,
        })
    }

    fn wheel_packet(delta: i16, status: u8) -> Packet {
        let mut packet = packet(&[]);
        packet.command = 19;
        packet.data[..2].copy_from_slice(&delta.to_be_bytes());
        packet.data[4] = status;
        packet
    }

    #[test]
    fn thumb_rotation_accumulates_native_steps_and_resets_between_gestures() -> Result<()> {
        for (deltas, expected) in [
            (vec![(2, 1), (2, 2), (2, 2)], [1, 0]),
            (vec![(5, 1), (-1, 2), (-5, 2)], [0, 1]),
            (vec![(3, 1), (0, 2), (3, 2)], [1, 0]),
            (vec![(5, 1), (0, 3), (1, 1)], [0, 0]),
            (vec![(5, 1), (5, 1)], [0, 0]),
            (vec![(5, 1), (1, 3)], [1, 0]),
            (vec![(5, 2), (0, 0), (1, 2)], [0, 0]),
            (vec![(6, 2), (-6, 2)], [1, 1]),
        ] {
            let mut wheel = thumb()?;
            let mut mapper = Mapper::default();
            let mut pulses = [0; 2];
            for (delta, status) in deltas {
                for event in wheel.snapshot(&wheel_packet(delta, status), 2, &mut mapper)? {
                    if event.kind == EV_KEY && event.value == 1 {
                        pulses[usize::from(event.code == 106)] += 1;
                    }
                }
                mapper.pulse_holds.clear();
            }
            assert_eq!(pulses, expected);
        }
        let mut wheel = thumb()?;
        wheel.native_resolution = 18;
        let mut mapper = Mapper::default();
        let pulses: usize = (0..6)
            .map(|_| {
                wheel
                    .snapshot(&wheel_packet(20, 2), 2, &mut mapper)
                    .expect("valid rotation")
                    .len()
                    / 4
            })
            .sum();
        assert_eq!(
            pulses, 18,
            "fractional resolution ratios must retain their remainder"
        );
        Ok(())
    }

    #[test]
    fn thumb_repeat_interval_limits_bursts_without_delaying_or_queueing_actions() -> Result<()> {
        let mut wheel = thumb()?;
        wheel.repeat_interval_ms.store(250, Ordering::Relaxed);
        wheel.actions = [
            Some(Action::parse("key:ctrl+pageup")?),
            Some(Action::parse("key:ctrl+pagedown")?),
        ];
        let start = Instant::now();
        let mut mapper = Mapper::default();
        for (delta, status, millis, expected) in [
            (1, 1, 0, None),
            (5, 2, 10, Some(104)),
            (600, 2, 20, None),
            (6, 2, 259, None),
            (600, 2, 260, Some(104)),
            (0, 3, 500, None),
            (-6, 1, 505, None),
            (-600, 2, 510, Some(109)),
            (0, 0, 1010, None),
            (6, 1, 1011, Some(104)),
        ] {
            let events = wheel.snapshot_at(
                &wheel_packet(delta, status),
                2,
                &mut mapper,
                start + Duration::from_millis(millis),
            )?;
            let expected = expected.map_or_else(Vec::new, |key| {
                vec![
                    Event::key(29, 1),
                    Event::key(key, 1),
                    Event::sync(),
                    Event::key(key, 0),
                    Event::key(29, 0),
                    Event::sync(),
                ]
            });
            assert_eq!(events, expected, "at {millis} ms");
            mapper.pulse_holds.clear();
        }
        // Reuse the same wheel and last-action timestamp when a reload changes
        // the shared interval, including returning to unrestricted scrolling.
        let interval = wheel.repeat_interval_ms.clone();
        for (limit, millis, emits) in [
            (150, 1160, false),
            (150, 1161, true),
            (300, 1311, false),
            (300, 1461, true),
            (0, 1462, true),
        ] {
            interval.store(limit, Ordering::Relaxed);
            let events = wheel.snapshot_at(
                &wheel_packet(6, 2),
                2,
                &mut mapper,
                start + Duration::from_millis(millis),
            )?;
            assert_eq!(
                !events.is_empty(),
                emits,
                "{limit} ms interval at {millis} ms"
            );
            mapper.pulse_holds.clear();
        }
        Ok(())
    }

    #[test]
    fn thumb_repeat_interval_starts_only_when_an_action_is_emitted() -> Result<()> {
        let mut wheel = thumb()?;
        wheel.repeat_interval_ms.store(250, Ordering::Relaxed);
        wheel.actions[0] = None;
        let now = Instant::now();
        let mut mapper = Mapper {
            bindings: BTreeMap::from([(0x53, Action::parse("key:right")?)]),
            ..Mapper::default()
        };
        assert!(
            wheel
                .snapshot_at(&wheel_packet(6, 2), 2, &mut mapper, now)?
                .is_empty()
        );
        mapper.update(&[0x53]);
        assert!(
            wheel
                .snapshot_at(&wheel_packet(-6, 2), 2, &mut mapper, now)?
                .is_empty()
        );
        mapper.update(&[]);
        let events = wheel.snapshot_at(&wheel_packet(-6, 2), 2, &mut mapper, now)?;
        assert_eq!(
            events,
            vec![
                Event::key(106, 1),
                Event::sync(),
                Event::key(106, 0),
                Event::sync()
            ]
        );
        Ok(())
    }

    #[test]
    fn thumb_direction_filtering_unbound_actions_and_burst_limits_are_explicit() -> Result<()> {
        for positive_left in [true, false] {
            let mut wheel = thumb()?;
            wheel.positive_left = positive_left;
            let mut mapper = Mapper::default();
            let events = wheel.snapshot(&wheel_packet(6, 2), 2, &mut mapper)?;
            assert_eq!(
                events[0],
                Event::key(if positive_left { 105 } else { 106 }, 1)
            );
            wheel.actions[usize::from(!positive_left)] = None;
            assert!(
                wheel
                    .snapshot(&wheel_packet(6, 2), 2, &mut mapper)?
                    .is_empty()
            );
            assert!(
                !wheel
                    .snapshot(&wheel_packet(-6, 2), 2, &mut mapper)?
                    .is_empty()
            );
        }
        let mut wheel = thumb()?;
        wheel.remainder = 40;
        let mut mapper = Mapper::default();
        for (slot, feature, address) in
            [(3, 19, 0), (2, 18, 0), (2, 19, SOFTWARE_ID), (2, 19, 0x10)]
        {
            let mut packet = wheel_packet(6, 2);
            packet.command = feature;
            packet.address = address;
            assert!(wheel.snapshot(&packet, slot, &mut mapper)?.is_empty());
            assert_eq!(wheel.remainder, 40);
        }
        for mut packet in [wheel_packet(6, 4), wheel_packet(6, 2)] {
            if packet.data[4] == 2 {
                packet.data.truncate(5);
            }
            assert!(wheel.snapshot(&packet, 2, &mut mapper).is_err());
            assert_eq!(wheel.remainder, 40);
        }
        wheel.native_resolution = 65_535;
        wheel.diverted_resolution = 1;
        assert_eq!(
            wheel
                .snapshot(&wheel_packet(i16::MIN, 2), 2, &mut mapper)?
                .len(),
            64 * 4
        );
        assert_eq!(wheel.remainder, 0);
        Ok(())
    }

    #[test]
    fn wheel_pulses_preserve_button_holds_and_cleanup_uncertain_output() -> Result<()> {
        for held in [false, true] {
            let mut mapper = mapper()?;
            if held {
                mapper.update(&[0x53]);
            }
            mapper.pulse(&Action::parse("key:ctrl+super")?);
            let released = mapper.release_all();
            let expected = if held {
                vec![
                    Event::key(125, 0),
                    Event::key(46, 0),
                    Event::key(29, 0),
                    Event::sync(),
                ]
            } else {
                vec![Event::key(125, 0), Event::key(29, 0), Event::sync()]
            };
            assert_eq!(
                released, expected,
                "a modifier target releases before its shortcut modifiers"
            );
            assert!(mapper.release_all().is_empty());
        }

        let mut mapper = mapper()?;
        mapper.update(&[0x53]);
        assert_eq!(
            mapper.pulse(&Action::parse("key:ctrl+v")?),
            vec![
                Event::key(47, 1),
                Event::sync(),
                Event::key(47, 0),
                Event::sync()
            ]
        );
        mapper.pulse_holds.clear();
        assert_eq!(
            mapper.output_holds,
            BTreeMap::from([(29, ()), (46, ())]).into_keys().collect()
        );
        assert!(
            mapper.pulse(&Action::parse("key:alt+c")?).is_empty(),
            "never release or retrigger a button-owned target"
        );
        assert!(
            mapper.pulse_holds.is_empty(),
            "a suppressed target must not press stray modifiers"
        );
        mapper.pulse(&Action::parse("key:alt+x")?);
        let released = mapper.release_all();
        for code in [29, 46, 56, 45] {
            assert_eq!(
                released
                    .iter()
                    .filter(|event| **event == Event::key(code, 0))
                    .count(),
                1
            );
        }
        assert!(
            mapper.down.is_empty()
                && mapper.output_holds.is_empty()
                && mapper.pulse_holds.is_empty()
        );
        assert_eq!(released.last(), Some(&Event::sync()));
        assert!(mapper.release_all().is_empty());
        Ok(())
    }

    #[test]
    fn thumb_only_preflight_uses_no_button_feature_and_rejects_invalid_info_or_ownership()
    -> Result<()> {
        for (version, native, diverted, mode, accepted) in [
            (0, 20, 120, 0, true),
            (1, 20, 120, 0, false),
            (0, 0, 120, 0, false),
            (0, 20, 0, 0, false),
            (0, 20, 120, 1, false),
        ] {
            let mut exchanges =
                vec![(report(0, 0, &[0x21, 0x50]), report(0, 0, &[19, 0, version]))];
            if version == 0 {
                exchanges.push((
                    report(19, 0, &[]),
                    report(19, 0, &[0, native, 0, diverted, 0, 3, 3, 232]),
                ));
                if native != 0 && diverted != 0 {
                    exchanges.push((report(19, 1, &[]), report(19, 1, &[mode, 7])));
                }
            }
            let mut client = Client::with_client_id(Scripted::new(exchanges), SOFTWARE_ID)?;
            let prepared = prepare_bindings(&mut client, 2, &BTreeMap::new(), thumb()?.actions);
            assert_eq!(prepared.is_ok(), accepted);
            if let Ok((buttons, wheel)) = prepared {
                assert!(buttons.is_none() && wheel.is_some());
            }
            assert!(client.into_transport().exchanges.is_empty());
        }
        let mut client = Client::with_client_id(
            Scripted::new([
                (report(0, 0, &[0x1b, 4]), report(0, 0, &[9, 0, 2])),
                (report(9, 0, &[]), report(9, 0, &[1])),
                (report(9, 1, &[0]), report(9, 1, &[0, 0x53, 0, 0, 0x30])),
                (report(9, 2, &[0, 0x53]), report(9, 2, &[0, 0x53, 0, 0, 0])),
                (report(0, 0, &[0x21, 0x50]), report(0, 0, &[0, 0, 0])),
            ]),
            SOFTWARE_ID,
        )?;
        assert!(
            prepare_bindings(
                &mut client,
                2,
                &BTreeMap::from([(0x53, Action::parse("key:a")?)]),
                thumb()?.actions
            )
            .is_err()
        );
        assert!(
            client.into_transport().exchanges.is_empty(),
            "all preflight reads precede diversion writes"
        );
        Ok(())
    }

    #[test]
    fn thumb_diversion_preserves_current_inversion_and_recovers_uncertain_writes() -> Result<()> {
        for uncertain in [false, true] {
            let set = report(19, 2, &[1, 1, 0]);
            let mut exchanges = vec![
                (report(19, 1, &[]), report(19, 1, &[0, 7])),
                (set.clone(), if uncertain { Vec::new() } else { set }),
            ];
            if !uncertain {
                exchanges.push((report(19, 1, &[]), report(19, 1, &[1, 7])));
            }
            // A user changes inversion while the worker is active; cleanup preserves it.
            let restore = report(19, 2, &[0, 0, 0]);
            exchanges.extend([
                (report(19, 1, &[]), report(19, 1, &[1, 6])),
                (restore.clone(), restore),
                (report(19, 1, &[]), report(19, 1, &[0, 6])),
            ]);
            let mut client = Client::with_client_id(Scripted::new(exchanges), SOFTWARE_ID)?;
            let mut wheel = thumb()?;
            assert_eq!(wheel.set_diverted(&mut client, 2, true).is_ok(), !uncertain);
            assert!(
                wheel.attempted,
                "cleanup is necessary even when the enable acknowledgement is lost"
            );
            wheel.set_diverted(&mut client, 2, false)?;
            assert!(!wheel.attempted);
            assert!(client.into_transport().exchanges.is_empty());
        }
        for flags in [0, 1] {
            let set = report(19, 2, &[1, 0, 0]);
            let mut client = Client::with_client_id(
                Scripted::new([
                    (report(19, 1, &[]), report(19, 1, &[0, 0])),
                    (set.clone(), set),
                    (report(19, 1, &[]), report(19, 1, &[flags, 1])),
                ]),
                SOFTWARE_ID,
            )?;
            let mut wheel = thumb()?;
            assert!(
                wheel.set_diverted(&mut client, 2, true).is_err(),
                "both mode and inversion must match readback"
            );
            assert!(wheel.attempted);
        }
        let mut client = Client::with_client_id(
            Scripted::new([(report(19, 1, &[]), report(19, 1, &[1, 0]))]),
            SOFTWARE_ID,
        )?;
        let mut wheel = thumb()?;
        assert!(wheel.set_diverted(&mut client, 2, true).is_err());
        assert!(
            !wheel.attempted,
            "a new external owner must never become our cleanup responsibility"
        );
        assert!(client.into_transport().exchanges.is_empty());
        Ok(())
    }

    #[test]
    fn thumb_only_heartbeat_checks_ownership_and_releases_keys_on_timeout() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut client = Client::with_client_id(
            Scripted::new([
                (report(19, 1, &[]), report(19, 1, &[1, 7])),
                (report(19, 1, &[]), Vec::new()),
                (report(19, 1, &[]), report(19, 1, &[0, 7])),
            ]),
            SOFTWARE_ID,
        )?;
        let mut mapper = Mapper::default();
        let path = directory.path().join("controller.lock");
        assert!(check_heartbeat(&mut client, 2, None, Some(19), &path, &mut mapper)?.is_empty());
        mapper.pulse(&Action::parse("key:super")?);
        assert_eq!(
            check_heartbeat(&mut client, 2, None, Some(19), &path, &mut mapper)?,
            vec![Event::key(125, 0), Event::sync()]
        );
        assert!(check_heartbeat(&mut client, 2, None, Some(19), &path, &mut mapper).is_err());
        assert!(client.into_transport().exchanges.is_empty());
        Ok(())
    }
}
