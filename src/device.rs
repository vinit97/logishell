//! Linux device inventory and capability-driven Logitech feature adapters.
//!
//! Status uses read commands only. Receiver pairing is a separate, explicit
//! workflow. See docs/devices.md for protocol references and support limits.

use crate::{
    hidpp::{Client, Hidraw, ProtocolError, Transport as HidTransport, legacy_error},
    model::{Battery, Device, DeviceState, Inventory, Receiver, Settings, Transport},
    terminal::{Choice, PairingUi},
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

mod bolt;
mod unifying;

trait ReceiverUi {
    fn status(&mut self, title: &str, message: &str) -> Result<()>;
    fn select(&mut self, title: &str, message: &str, choices: Vec<Choice>)
    -> Result<Option<usize>>;
    fn cancelled(&mut self) -> Result<bool>;

    fn confirm(&mut self, title: &str, message: &str) -> Result<bool> {
        Ok(self.select(
            title,
            message,
            vec![
                Choice {
                    label: "Go back".into(),
                    detail: "Keep this receiver closed.".into(),
                },
                Choice {
                    label: "Start pairing".into(),
                    detail: "Allow the next compatible device to pair.".into(),
                },
            ],
        )? == Some(1))
    }
}

struct ReceiverSetup {
    ui: PairingUi,
    timeout: Duration,
}

impl ReceiverUi for ReceiverSetup {
    fn status(&mut self, title: &str, message: &str) -> Result<()> {
        self.ui.status(title, message)
    }

    fn select(
        &mut self,
        title: &str,
        message: &str,
        choices: Vec<Choice>,
    ) -> Result<Option<usize>> {
        self.ui
            .choose_blocking(title, message, choices, self.timeout)
    }

    fn cancelled(&mut self) -> Result<bool> {
        Ok(self.ui.cancelled())
    }
}

fn finish_pairing(
    mut result: Result<()>,
    cleanup: impl IntoIterator<Item = (&'static str, Result<()>)>,
) -> Result<()> {
    // Callers perform every cleanup request before aggregating its outcome.
    // Preserve both the original failure and any failed receiver restoration.
    for (message, outcome) in cleanup {
        if let Err(error) = outcome {
            result = Err(match result {
                Ok(()) => error.context(format!("paired, but {message}")),
                Err(original) => original.context(format!("{message}: {error:#}")),
            });
        }
    }
    result
}

const FEATURES: &[u16] = &[
    0x0003, 0x0005, 0x1000, 0x1001, 0x1004, 0x1982, 0x19b0, 0x1b04, 0x2110, 0x2111, 0x2121, 0x2150,
    0x2201, 0x40a0, 0x40a2, 0x40a3,
];

#[derive(Debug)]
struct Node {
    path: String,
    sys: PathBuf,
    name: String,
    product: u16,
    bus: u16,
    unique: String,
    physical: String,
    usb: Option<PathBuf>,
    has_hidpp: bool,
    short_reports: bool,
    long_reports: bool,
    kernel_slot: Option<u8>,
}

fn read_trim(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path)
        .unwrap_or_default()
        .trim()
        .to_owned()
}

fn property<'a>(properties: &'a str, key: &str) -> Option<&'a str> {
    properties.lines().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        (name == key).then_some(value)
    })
}

fn receiver_kind(product: u16) -> Option<Transport> {
    match product {
        0xc548 => Some(Transport::Bolt),
        0xc52b | 0xc532 => Some(Transport::Unifying),
        _ => None,
    }
}

fn receiver_identity<T: HidTransport>(client: &mut Client<T>, product: u16) -> Result<String> {
    let serial = match receiver_kind(product) {
        Some(Transport::Bolt) => {
            let data = client.register(0xff, 0xfb, &[], true)?;
            ensure!(data.len() == 16, "truncated Bolt receiver serial");
            data
        }
        Some(Transport::Unifying) => {
            let data = client.register(0xff, 0xb5, &[3], true)?;
            data.get(1..5)
                .context("truncated Unifying receiver serial")?
                .to_vec()
        }
        _ => bail!("HID path is no longer a supported receiver"),
    };
    ensure!(
        serial.iter().any(|byte| *byte != 0),
        "receiver has no verifiable serial"
    );
    Ok(format!("receiver:{product:04x}:{}", hex(&serial)))
}

fn bolt_identity(receiver_id: &str, pairing: &[u8]) -> Result<String> {
    ensure!(pairing.len() >= 8, "truncated Bolt pairing identity");
    ensure!(
        pairing[2..4].iter().any(|byte| *byte != 0) && pairing[4..8].iter().any(|byte| *byte != 0),
        "Bolt pairing has no verifiable device identity"
    );
    Ok(format!(
        "{receiver_id}:device:{}:{}",
        hex(&pairing[2..4]),
        hex(&pairing[4..8])
    ))
}

fn verify_receiver<T: HidTransport>(client: &mut Client<T>, id: &str, product: u16) -> Result<()> {
    ensure!(
        receiver_identity(client, product)? == id,
        "receiver identity changed; refresh device status before continuing"
    );
    Ok(())
}

fn receiver_product(product: u16) -> bool {
    // Logitech reserves this USB product family for receivers. Unknown Nano,
    // Lightspeed and legacy receivers must not become direct-device routes.
    product & 0xff00 == 0xc500
}

fn kernel_receiver_slot(physical: &str, parent_physical: &str, driver: &str) -> Option<u8> {
    // hid-logitech-dj creates children with their parent's physical path plus
    // ":<slot>" and pins raw requests to that slot. Incoming reports retain it.
    // Check the actual parent driver rather than guessing from a USB product.
    if driver != "logitech-djreceiver" || parent_physical.is_empty() {
        return None;
    }
    let slot = physical.strip_prefix(parent_physical)?.strip_prefix(':')?;
    if slot.len() != 1 {
        return None;
    }
    slot.parse::<u8>()
        .ok()
        .filter(|slot| (1..=7).contains(slot))
}

// Parse HID short/global items instead of looking for the bytes 0x85,0x11
// anywhere in the descriptor (where they could also be literal usage data).
fn descriptor_has_report(descriptor: &[u8], report_id: u8) -> bool {
    let mut offset = 0;
    while offset < descriptor.len() {
        let tag = descriptor[offset];
        if tag == 0xfe {
            let Some(&length) = descriptor.get(offset + 1) else {
                return false;
            };
            offset += 3 + usize::from(length);
            continue;
        }
        let length = match tag & 3 {
            3 => 4,
            n => usize::from(n),
        };
        if offset + 1 + length > descriptor.len() {
            return false;
        }
        if tag == 0x85 && descriptor[offset + 1] == report_id {
            return true;
        }
        offset += 1 + length;
    }
    false
}

fn hidpp_descriptor(descriptor: &[u8]) -> bool {
    descriptor_has_report(descriptor, 0x10) || descriptor_has_report(descriptor, 0x11)
}

fn nodes() -> Result<Vec<Node>> {
    let entries = match fs::read_dir("/sys/class/hidraw") {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("reading Linux HID inventory"),
    };
    let mut nodes = Vec::new();
    for entry in entries {
        let entry = entry?;
        let sys = match fs::canonicalize(entry.path().join("device")) {
            Ok(path) => path,
            Err(_) => continue,
        };
        let properties = read_trim(sys.join("uevent"));
        let Some(identifier) = property(&properties, "HID_ID") else {
            continue;
        };
        let fields: Vec<_> = identifier.split(':').collect();
        if fields.len() != 3 {
            continue;
        }
        if u32::from_str_radix(fields[1], 16).ok() != Some(0x046d) {
            continue;
        }
        let product = u16::from_str_radix(fields[2].trim_start_matches('0'), 16).unwrap_or(0);
        let bus = u16::from_str_radix(fields[0], 16).unwrap_or(0);
        let usb = if bus == 3 {
            sys.ancestors()
                .find(|path| path.join("idVendor").is_file())
                .map(Path::to_path_buf)
        } else {
            None
        };
        let physical = property(&properties, "HID_PHYS").unwrap_or_default();
        let mut kernel_slot = None;
        // Known receivers use our pairing-table route. For other receivers,
        // retain only child routes whose addressing is owned by the DJ driver.
        if let Some(usb) = &usb {
            let usb_product =
                u16::from_str_radix(&read_trim(usb.join("idProduct")), 16).unwrap_or(0);
            if receiver_product(usb_product) && product != usb_product {
                if receiver_kind(usb_product).is_some() {
                    continue;
                }
                if let Some(parent) = sys.parent() {
                    let parent_properties = read_trim(parent.join("uevent"));
                    let driver = fs::read_link(parent.join("driver")).unwrap_or_default();
                    kernel_slot = kernel_receiver_slot(
                        physical,
                        property(&parent_properties, "HID_PHYS").unwrap_or_default(),
                        driver
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or_default(),
                    );
                }
                if kernel_slot.is_none() {
                    continue;
                }
            }
        }
        let descriptor = fs::read(sys.join("report_descriptor")).unwrap_or_default();
        nodes.push(Node {
            path: format!("/dev/{}", entry.file_name().to_string_lossy()),
            name: property(&properties, "HID_NAME")
                .unwrap_or("Logitech device")
                .to_owned(),
            unique: property(&properties, "HID_UNIQ")
                .unwrap_or_default()
                .to_owned(),
            physical: physical.to_owned(),
            has_hidpp: hidpp_descriptor(&descriptor),
            short_reports: descriptor_has_report(&descriptor, 0x10),
            long_reports: descriptor_has_report(&descriptor, 0x11),
            kernel_slot,
            product,
            bus,
            usb,
            sys,
        });
    }
    nodes.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(nodes)
}

fn node_group(node: &Node) -> String {
    if node.kernel_slot.is_some() {
        return node.sys.display().to_string();
    }
    if let Some(usb) = &node.usb {
        return usb.display().to_string();
    }
    if !node.unique.is_empty() {
        return format!("{}:{}", node.bus, node.unique);
    }
    if !node.physical.is_empty() {
        return node
            .physical
            .split("/input")
            .next()
            .unwrap_or(&node.physical)
            .to_owned();
    }
    node.sys.display().to_string()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn component(text: &str) -> String {
    text.bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
                (byte as char).to_string()
            } else {
                format!("%{byte:02x}")
            }
        })
        .collect()
}

fn address(text: &str) -> Option<String> {
    let fields: Vec<_> = text.split(':').collect();
    (fields.len() == 6
        && fields
            .iter()
            .all(|part| part.len() == 2 && u8::from_str_radix(part, 16).is_ok()))
    .then(|| text.to_uppercase())
}

fn fallback_identity(node: &Node, receiver: bool) -> (String, bool) {
    let prefix = if receiver { "receiver" } else { "usb" };
    if node.bus == 5
        && let Some(mac) = address(&node.unique)
    {
        return (format!("bt:{mac}"), false);
    }
    let serial = node
        .usb
        .as_ref()
        .filter(|_| node.kernel_slot.is_none())
        .map(|usb| read_trim(usb.join("serial")))
        .unwrap_or_default();
    let serial = if serial.is_empty() {
        &node.unique
    } else {
        &serial
    };
    if !serial.is_empty() {
        return (
            format!("{prefix}:{:04x}:{}", node.product, component(serial)),
            false,
        );
    }
    // USB port topology survives hidraw renumbering but not moving ports.
    // Never treat a hidraw number as a persistent identity.
    let location = if node.kernel_slot.is_some() {
        node.physical.clone()
    } else if let Some(usb) = &node.usb {
        usb.strip_prefix("/sys/devices")
            .unwrap_or(usb)
            .display()
            .to_string()
    } else {
        node.physical.clone()
    };
    (
        format!(
            "{prefix}:{:04x}:port:{}",
            node.product,
            component(&location)
        ),
        true,
    )
}

fn base_device(node: &Node) -> Device {
    let (id, topology) = fallback_identity(node, false);
    Device {
        id,
        name: node.name.clone(),
        transport: if node.bus == 5 {
            Transport::Bluetooth
        } else {
            Transport::Usb
        },
        battery: kernel_battery(&node.sys),
        slot: node.kernel_slot,
        hid_path: node.has_hidpp.then(|| node.path.clone()),
        bluetooth_address: if node.bus == 5 {
            address(&node.unique)
        } else {
            None
        },
        capabilities: [
            (node.short_reports, "hidpp-short-reports"),
            (node.long_reports, "hidpp-long-reports"),
            (node.kernel_slot.is_some(), "kernel-receiver-route"),
        ]
        .into_iter()
        .filter(|(present, _)| *present)
        .map(|(_, name)| name.to_owned())
        .collect(),
        warnings: [
            (topology, "Device has no readable serial; its ID is tied to its USB port."),
            (node.kernel_slot.is_some(), "Wireless device uses a kernel-managed USB receiver route; receiver pairing is unsupported and configuration requires a verified device unit identity."),
        ]
        .into_iter()
        .filter(|(present, _)| *present)
        .map(|(_, warning)| warning.to_owned())
        .collect(),
            ..Default::default()
        }
}

fn kernel_battery(sys: &Path) -> Option<Battery> {
    let entries = fs::read_dir("/sys/class/power_supply").ok()?;
    for entry in entries.flatten() {
        // Another power supply may disappear between enumeration and lookup
        // (for example an unplugged USB device). Keep looking for this HID's
        // battery instead of discarding the entire inventory on that race.
        let Ok(path) = fs::canonicalize(entry.path()) else {
            continue;
        };
        if !path.starts_with(sys) {
            continue;
        }
        let percent = read_trim(path.join("capacity"))
            .parse::<u8>()
            .ok()
            .filter(|n| *n <= 100);
        let level = read_trim(path.join("capacity_level"));
        let status = read_trim(path.join("status"));
        return Some(Battery {
            percent,
            level: (!level.is_empty() && level != "Unknown").then(|| level.to_lowercase()),
            charging: match status.as_str() {
                "Charging" => Some(true),
                "Discharging" | "Full" | "Not charging" => Some(false),
                _ => None,
            },
            stale: read_trim(path.join("online")) == "0",
        });
    }
    None
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Discovery {
    Status,
    Control,
    Details,
}

pub fn discover(detail: Discovery) -> Result<Inventory> {
    let mut groups: BTreeMap<String, Vec<Node>> = BTreeMap::new();
    for node in nodes()? {
        groups.entry(node_group(&node)).or_default().push(node);
    }
    let mut inventory = Inventory::default();
    for nodes in groups.values() {
        let node = nodes
            .iter()
            .find(|node| node.long_reports)
            .or_else(|| nodes.iter().find(|node| node.has_hidpp))
            .unwrap_or(&nodes[0]);
        if let Some(kind) = receiver_kind(node.product) {
            let (id, topology) = fallback_identity(node, true);
            let mut receiver = Receiver {
                id,
                name: format!("Logitech {kind} receiver"),
                transport: kind,
                hid_path: node.path.clone(),
            };
            if !node.has_hidpp {
                inventory.warnings.push(format!(
                    "{}: HID++ interface is unavailable; cannot enumerate paired devices",
                    receiver.name
                ));
            } else {
                match Hidraw::open(&node.path) {
                    Ok(transport) => {
                        let mut client = Client::new(transport);
                        if let Ok(id) = receiver_identity(&mut client, node.product) {
                            receiver.id = id;
                        } else if topology {
                            inventory.warnings.push(format!(
                                "{} has no readable serial; its ID is tied to its USB port",
                                receiver.name
                            ));
                        }
                        discover_receiver(&mut client, &receiver, &mut inventory, detail);
                    }
                    Err(error) => inventory
                        .warnings
                        .push(format!("{}: {error:#}", receiver.name)),
                }
            }
            inventory.receivers.push(receiver);
            continue;
        }
        if node.bus == 3 && receiver_product(node.product) {
            let (id, _) = fallback_identity(node, true);
            inventory.receivers.push(Receiver {
                id,
                name: node.name.clone(),
                transport: Transport::Unknown,
                hid_path: node.path.clone(),
            });
            inventory.warnings.push(format!("{} (USB {:04x}) has unsupported receiver pairing; device features are available only through any kernel-managed HID++ child routes", node.name, node.product));
            continue;
        }
        let mut device = base_device(node);
        if node.has_hidpp {
            match Hidraw::open(&node.path) {
                Ok(transport) => {
                    let mut client = Client::new(transport);
                    if node.kernel_slot.is_none() {
                        client = client.with_direct_addressing();
                    }
                    if let Err(error) = enrich(
                        &mut client,
                        node.kernel_slot.unwrap_or(0xff),
                        &mut device,
                        detail,
                    ) {
                        device
                            .warnings
                            .push(format!("HID++ information unavailable: {error:#}"));
                    }
                }
                Err(error) => device.warnings.push(format!("{error:#}")),
            }
        } else {
            device.state = DeviceState::Online;
            device.warnings.push(
                "Device is present but exposes no supported HID++ configuration interface.".into(),
            );
        }
        inventory.devices.push(device);
    }
    deduplicate_devices(&mut inventory.devices);
    inventory.receivers.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(inventory)
}

fn deduplicate_devices(devices: &mut Vec<Device>) {
    let route_priority = |state| match state {
        DeviceState::Online => 0,
        DeviceState::Unknown => 1,
        DeviceState::Offline => 2,
    };
    // Multiple Easy-Switch channels can pair the same physical device to
    // separate slots on one receiver. Keep its persistent identity, but use
    // the reachable slot together with its battery and feature information.
    // Stable sorting preserves the original route order for equal states.
    devices.sort_by(|a, b| {
        a.id.cmp(&b.id)
            .then_with(|| route_priority(a.state).cmp(&route_priority(b.state)))
    });
    devices.dedup_by(|a, b| a.id == b.id);
}

fn discover_receiver<T: HidTransport>(
    client: &mut Client<T>,
    receiver: &Receiver,
    inventory: &mut Inventory,
    detail: Discovery,
) {
    for slot in 1..=6 {
        let subregister = if receiver.transport == Transport::Bolt {
            0x50 + slot
        } else {
            0x20 + slot - 1
        };
        let pairing = match client.register(0xff, 0xb5, &[subregister], true) {
            Ok(data) => data,
            Err(error) => {
                tracing::debug!(
                    receiver = %receiver.id,
                    slot,
                    error = %error,
                    "receiver slot pairing record unavailable"
                );
                if !error
                    .downcast_ref::<ProtocolError>()
                    .is_some_and(|error| matches!(error.code, 2 | 3 | 8))
                {
                    inventory
                        .warnings
                        .push(format!("{} slot {slot}: {error:#}", receiver.name));
                }
                continue;
            }
        };
        if pairing.len() < 8 || pairing[1..].iter().all(|byte| *byte == 0) {
            tracing::debug!(receiver = %receiver.id, slot, "receiver slot is empty");
            continue;
        }
        let mut device = Device {
            id: format!("{}:slot:{slot}", receiver.id),
            name: format!("Logitech device (slot {slot})"),
            transport: receiver.transport,
            receiver_id: Some(receiver.id.clone()),
            slot: Some(slot),
            hid_path: Some(receiver.hid_path.clone()),
            ..Default::default()
        };
        // Keep the receiver/slot identity constant online and asleep. Its
        // paired serial prevents a replacement device inheriting settings.
        if receiver.transport == Transport::Unifying {
            if let Ok(serial) = client.register(0xff, 0xb5, &[0x30 + slot - 1], true)
                && serial.len() >= 5
                && serial[1..5].iter().any(|byte| *byte != 0)
            {
                device.id = format!("{}:device:{}", receiver.id, hex(&serial[1..5]));
            }
            if let Ok(name) = client.register(0xff, 0xb5, &[0x40 + slot - 1], true)
                && name.len() >= 2
            {
                let length = usize::from(name[1]).min(name.len() - 2);
                if length > 0 {
                    device.name = clean_name(&name[2..2 + length]);
                }
            }
        } else {
            // The Bolt pairing record includes its per-pairing address.
            // Include the opaque record identity so reusing a slot cannot
            // silently inherit the previous occupant's configuration.
            match bolt_identity(&receiver.id, &pairing) {
                Ok(id) => device.id = id,
                Err(error) => device
                    .warnings
                    .push(format!("{error:#}; configuration unavailable")),
            }
            if let Ok(name) = client.register(0xff, 0xb5, &[0x60 + slot, 1], true)
                && name.len() >= 4
            {
                let length = usize::from(name[2]).min(name.len() - 3);
                if length > 0 {
                    device.name = clean_name(&name[3..3 + length]);
                }
            }
        }
        if inventory
            .devices
            .iter()
            .any(|known| known.id == device.id && known.state == DeviceState::Online)
        {
            continue;
        }
        if let Err(error) = enrich(client, slot, &mut device, detail) {
            if legacy_error(&error, &[4, 8]) {
                device.state = DeviceState::Offline;
            }
            device
                .warnings
                .push(format!("Device may be asleep or unavailable: {error:#}"));
        }
        tracing::debug!(
            receiver = %receiver.id,
            slot,
            device_id = %device.id,
            name = %device.name,
            state = ?device.state,
            warnings = ?device.warnings,
            "receiver slot discovered before inventory deduplication"
        );
        inventory.devices.push(device);
    }
}

fn clean_name(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .filter(|character| !character.is_control())
        .collect::<String>()
        .trim()
        .to_owned()
}

pub(crate) fn long_ping(device: &Device) -> bool {
    device.receiver_id.is_none()
        && !device
            .capabilities
            .iter()
            .any(|cap| cap == "hidpp-short-reports")
}

fn protocol_version<T: HidTransport>(
    client: &mut Client<T>,
    slot: u8,
    long: bool,
) -> Result<(u8, u8)> {
    match client.ping(slot, long) {
        Ok(version) if version.0 >= 1 => Ok(version),
        Ok(_) => bail!("unrecognized HID++ protocol response"),
        Err(error) if legacy_error(&error, &[1]) => Ok((1, 0)),
        Err(error) => Err(error),
    }
}

fn optional_register<T: HidTransport>(
    client: &mut Client<T>,
    slot: u8,
    register: u8,
) -> Result<Option<Vec<u8>>> {
    match client.register(slot, register, &[], false) {
        Ok(data) => Ok(Some(data)),
        Err(error) if legacy_error(&error, &[1, 2]) => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("read HID++ 1.0 register 0x{register:02x}"))
        }
    }
}

fn legacy_battery(register: u8, data: &[u8]) -> Result<Battery> {
    ensure!(data.len() >= 3, "truncated HID++ 1.0 battery response");
    match register {
        0x0d => Ok(Battery {
            percent: (data[0] <= 100).then_some(data[0]),
            level: None,
            charging: match data[2] & 0xf0 {
                0x30 | 0x90 => Some(false),
                0x50 => Some(true),
                _ => None,
            },
            stale: false,
        }),
        0x07 => Ok(Battery {
            percent: None,
            level: match data[0] {
                1 => Some("critical"),
                3 => Some("low"),
                5 => Some("good"),
                7 => Some("full"),
                _ => None,
            }
            .map(str::to_owned),
            charging: if data[1] == 0 || data[1] & 0x22 == 0x22 {
                Some(false)
            } else if data[1] & 0x21 == 0x21 {
                Some(true)
            } else {
                None
            },
            stale: false,
        }),
        _ => bail!("unsupported HID++ 1.0 battery register"),
    }
}

fn enrich_legacy<T: HidTransport>(
    client: &mut Client<T>,
    slot: u8,
    device: &mut Device,
    detail: Discovery,
) -> Result<()> {
    if detail == Discovery::Control {
        return Ok(());
    }
    for register in [0x0d, 0x07] {
        match optional_register(client, slot, register) {
            Ok(Some(data)) => {
                device.battery = Some(legacy_battery(register, &data)?);
                device.capabilities.push(format!("register:{register:04x}"));
                break;
            }
            Ok(None) => {}
            Err(error) => {
                device
                    .warnings
                    .push(format!("Battery unavailable: {error:#}"));
                break;
            }
        }
    }
    if detail == Discovery::Status {
        return Ok(());
    }
    match optional_register(client, slot, 0x09) {
        Ok(Some(_)) => device
            .capabilities
            .extend(["register:0009".into(), "fn-lock".into()]),
        Ok(None) => {}
        Err(error) => device
            .warnings
            .push(format!("Legacy Fn setting unavailable: {error:#}")),
    }
    Ok(())
}

struct Session<'a, T: HidTransport> {
    client: &'a mut Client<T>,
    slot: u8,
    features: BTreeMap<u16, Option<(u8, u8)>>,
    scroll_inversion_supported: Option<bool>,
    protocol: (u8, u8),
}

impl<'a, T: HidTransport> Session<'a, T> {
    fn new(client: &'a mut Client<T>, slot: u8) -> Self {
        Self {
            client,
            slot,
            features: BTreeMap::new(),
            scroll_inversion_supported: None,
            protocol: (2, 0),
        }
    }
    fn awake(client: &'a mut Client<T>, slot: u8, long: bool) -> Result<Self> {
        // A fresh settings route can reach a sleeping peripheral even when
        // inventory saw it online. Use the four-second protocol probe before
        // issuing feature requests with the ordinary 700 ms deadline.
        let protocol = protocol_version(client, slot, long)
            .with_context(|| format!("wake device slot {slot} before reading its settings"))?;
        let mut session = Self::new(client, slot);
        session.protocol = protocol;
        Ok(session)
    }
    fn feature(&mut self, id: u16) -> Result<Option<(u8, u8)>> {
        ensure!(
            self.protocol.0 >= 2,
            "HID++ 2.0 features are unavailable on this HID++ 1.0 device"
        );
        if let Some(feature) = self.features.get(&id) {
            return Ok(*feature);
        }
        let feature = self.client.root_feature(self.slot, id).with_context(|| {
            format!(
                "discover HID++ feature 0x{id:04x} on device slot {}",
                self.slot
            )
        })?;
        self.features.insert(id, feature);
        Ok(feature)
    }
    fn supports(&mut self, id: u16) -> Result<bool> {
        Ok(self.feature(id)?.is_some())
    }
    fn supports_scroll_inversion(&mut self) -> Result<bool> {
        if let Some(supported) = self.scroll_inversion_supported {
            return Ok(supported);
        }
        let supported = self.supports(0x2121)? && self.call(0x2121, 0, &[])?[1] & 8 != 0;
        self.scroll_inversion_supported = Some(supported);
        Ok(supported)
    }
    fn supports_thumb_wheel(&mut self) -> Result<bool> {
        Ok(self
            .feature(0x2150)?
            .is_some_and(|(_, version)| version == 0))
    }
    fn thumb_wheel_status(&mut self) -> Result<(u8, bool)> {
        ensure!(
            self.supports_thumb_wheel()?,
            "thumb wheel requires supported HID++ 0x2150 version 0"
        );
        let data = self.call(0x2150, 1, &[])?;
        ensure!(data.len() >= 2, "truncated thumb wheel status");
        ensure!(
            data[0] <= 1,
            "unknown thumb wheel reporting mode {}",
            data[0]
        );
        Ok((data[0], data[1] & 1 != 0))
    }
    fn supports_backlight(&mut self) -> Result<bool> {
        Ok(self
            .feature(0x1982)?
            .is_some_and(|(_, version)| matches!(version, 2 | 3)))
    }
    fn call(&mut self, id: u16, function: u8, data: &[u8]) -> Result<Vec<u8>> {
        let (index, _) = self
            .feature(id)?
            .with_context(|| format!("device does not support HID++ feature 0x{id:04x}"))?;
        self.client
            .feature(self.slot, index, function, data)
            .with_context(|| {
                format!(
                    "HID++ feature 0x{id:04x}, function {function}, index 0x{index:02x}, device slot {}",
                    self.slot
                )
            })
    }
    fn smartshift(&mut self) -> Result<Option<(u16, u8)>> {
        if self.supports(0x2111)? {
            Ok(Some((0x2111, 1)))
        } else if self.supports(0x2110)? {
            Ok(Some((0x2110, 0)))
        } else {
            Ok(None)
        }
    }
    fn fn_feature(&mut self) -> Result<Option<u16>> {
        for id in [0x40a3, 0x40a2, 0x40a0] {
            if self.supports(id)? {
                return Ok(Some(id));
            }
        }
        Ok(None)
    }
    fn fn_state(&mut self, id: u16) -> Result<bool> {
        let data = self.call(id, 0, if id == 0x40a3 { &[0xff] } else { &[] })?;
        let state = data[usize::from(id == 0x40a3)];
        ensure!(state <= 1, "unknown Fn inversion state {state}");
        Ok(state != 0)
    }
    fn haptic_configuration(&mut self) -> Result<(u8, u8)> {
        let data = self.call(0x19b0, 1, &[0, 0, 0])?;
        ensure!(data.len() >= 3, "truncated haptic configuration");
        ensure!(data[0] <= 1, "unknown haptic enable state {}", data[0]);
        ensure!(data[1] <= 100, "invalid haptic strength {}", data[1]);
        Ok((data[0], data[1]))
    }
    fn preview_haptic_strength(&mut self) -> Result<()> {
        let capabilities = self.call(0x19b0, 0, &[0, 0, 0])?;
        ensure!(capabilities.len() >= 8, "truncated haptic capabilities");
        let waveforms = u32::from_be_bytes(capabilities[4..8].try_into()?);
        if let Some(waveform) = [1u8, 4]
            .into_iter()
            .find(|waveform| waveforms & (1 << waveform) != 0)
        {
            self.call(0x19b0, 4, &[waveform, 0, 0])?;
        }
        Ok(())
    }
}

fn enrich<T: HidTransport>(
    client: &mut Client<T>,
    slot: u8,
    device: &mut Device,
    detail: Discovery,
) -> Result<()> {
    enrich_session(
        &mut Session::awake(client, slot, long_ping(device))?,
        device,
        detail,
    )
}

fn enrich_session<T: HidTransport>(
    session: &mut Session<'_, T>,
    device: &mut Device,
    detail: Discovery,
) -> Result<()> {
    device.state = DeviceState::Online;
    device.capabilities.retain(|capability| {
        matches!(
            capability.as_str(),
            "hidpp-short-reports" | "hidpp-long-reports" | "kernel-receiver-route"
        )
    });
    device.capabilities.push(format!(
        "hidpp-{}.{}",
        session.protocol.0, session.protocol.1
    ));
    if session.protocol.0 == 1 {
        return enrich_legacy(session.client, session.slot, device, detail);
    }
    if detail == Discovery::Details {
        for &feature in FEATURES {
            if session.feature(feature)?.is_some() {
                device.capabilities.push(format!("feature:{feature:04x}"));
            }
        }
    }
    if session.supports(0x0005)? {
        let name = (|| -> Result<String> {
            let length = usize::from(session.call(0x0005, 0, &[])?[0]);
            ensure!(length > 0, "empty device name");
            let mut name = Vec::with_capacity(length);
            while name.len() < length {
                let data = session.call(0x0005, 1, &[name.len() as u8])?;
                name.extend_from_slice(&data[..data.len().min(length - name.len())]);
            }
            Ok(clean_name(&name))
        })();
        if let Ok(name) = name {
            device.name = name;
        }
    }
    if (detail == Discovery::Details
        || device.receiver_id.is_none() && device.transport != Transport::Bluetooth)
        && let Some((_, version)) = session.feature(0x0003)?
        && let Ok(info) = session.call(0x0003, 0, &[])
        && info.len() >= 13
    {
        if version >= 1
            && info[1..5].iter().any(|byte| *byte != 0)
            && device.receiver_id.is_none()
            && device.transport != Transport::Bluetooth
        {
            device.id = unit_identity(&info).expect("identity length and serial checked");
            device
                .warnings
                .retain(|warning| !warning.contains("tied to its USB port"));
        }
        if detail == Discovery::Details {
            for entity in 0..info[0].min(16) {
                if let Ok(firmware) = session.call(0x0003, 1, &[entity])
                    && firmware.len() >= 8
                    && firmware[0] == 0
                {
                    device.firmware = Some(format!(
                        "{} {:02x}.{:02x}.{:02x}{:02x}",
                        clean_name(&firmware[1..4]),
                        firmware[4],
                        firmware[5],
                        firmware[6],
                        firmware[7]
                    ));
                    break;
                }
            }
        }
    }
    if detail == Discovery::Control {
        if session.supports(0x1b04)? {
            device.capabilities.push("button-diversion".into());
        }
        if session.supports_thumb_wheel()? {
            device.capabilities.push("thumb-wheel".into());
        }
        return Ok(());
    }
    match read_battery(session) {
        Ok(Some(battery)) => device.battery = Some(battery),
        Err(error) => device
            .warnings
            .push(format!("Battery unavailable: {error:#}")),
        _ => {}
    }
    if detail == Discovery::Status {
        return Ok(());
    }
    for (feature, capability) in [
        (0x2201, "dpi"),
        (0x1b04, "button-diversion"),
        (0x19b0, "haptic-strength"),
    ] {
        if session.supports(feature)? {
            device.capabilities.push(capability.into());
        }
    }
    if session.supports_thumb_wheel()? {
        device.capabilities.push("thumb-wheel".into());
    }
    match session.supports_scroll_inversion() {
        Ok(true) => device.capabilities.push("scroll-invert".into()),
        Err(error) => device
            .warnings
            .push(format!("Scroll inversion availability unknown: {error:#}")),
        _ => {}
    }
    if session.smartshift()?.is_some() {
        device
            .capabilities
            .extend(["wheel-mode", "smartshift", "smartshift-sensitivity"].map(str::to_owned));
    }
    if session.fn_feature()?.is_some() {
        device.capabilities.push("fn-lock".into());
    }
    if session.supports_backlight()? {
        device.capabilities.push("backlight".into());
    }
    Ok(())
}

fn read_battery<T: HidTransport>(session: &mut Session<'_, T>) -> Result<Option<Battery>> {
    if session.supports(0x1004)? {
        let caps = session.call(0x1004, 0, &[])?;
        let data = session.call(0x1004, 1, &[])?;
        return Ok(Some(unified_battery(&caps, &data)?));
    }
    if session.supports(0x1000)? {
        let data = session.call(0x1000, 0, &[])?;
        // 0 means unknown in this feature; don't invent a drained battery.
        return Ok(Some(Battery {
            percent: (data[0] > 0 && data[0] <= 100).then_some(data[0]),
            level: None,
            charging: match data[2] {
                0 | 3 => Some(false),
                1 | 2 | 4 => Some(true),
                _ => None,
            },
            stale: false,
        }));
    }
    if session.supports(0x1001)? {
        return Ok(Some(voltage_battery(&session.call(0x1001, 0, &[])?)?));
    }
    Ok(None)
}

fn voltage_battery(data: &[u8]) -> Result<Battery> {
    ensure!(data.len() >= 3, "truncated battery voltage response");
    let flags = data[2];
    Ok(Battery {
        // A voltage-to-percent curve depends on battery chemistry. Keep the
        // reading unknown rather than applying one mouse's curve to all models.
        percent: None,
        level: if flags & 0x20 != 0 {
            Some("critical")
        } else if flags & 0x87 == 0x81 {
            Some("full")
        } else {
            None
        }
        .map(str::to_owned),
        charging: if flags & 0x80 == 0 {
            Some(false)
        } else {
            match flags & 7 {
                0 => Some(true),
                1 | 2 => Some(false),
                _ => None,
            }
        },
        stale: false,
    })
}

fn unified_battery(caps: &[u8], data: &[u8]) -> Result<Battery> {
    ensure!(
        caps.len() >= 2 && data.len() >= 3,
        "truncated unified battery response"
    );
    // Levels are capability bits, not an enum. Ignore levels the device does
    // not advertise and use the highest supported bit when several are set.
    let levels = caps[0] & data[1] & 0x0f;
    Ok(Battery {
        percent: (caps[1] & 2 != 0 && data[0] <= 100).then_some(data[0]),
        level: match levels {
            1 => Some("critical"),
            2..=3 => Some("low"),
            4..=7 => Some("good"),
            8..=15 => Some("full"),
            _ => None,
        }
        .map(str::to_owned),
        charging: match data[2] {
            0 | 3 => Some(false),
            1 | 2 => Some(true),
            _ => None,
        },
        stale: false,
    })
}

fn open_device(device: &Device) -> Result<Client<Hidraw>> {
    ensure!(
        device.state != DeviceState::Offline,
        "device is offline; wake or connect it first"
    );
    open_route(device)
}

fn unit_identity(info: &[u8]) -> Option<String> {
    (info.len() >= 13 && info[1..5].iter().any(|byte| *byte != 0))
        .then(|| format!("unit:{}:{}", hex(&info[7..13]), hex(&info[1..5])))
}

fn current_node(path: &str) -> Result<Node> {
    nodes()?
        .into_iter()
        .find(|node| node.path == path)
        .context("HID device identity is no longer available; refresh device status")
}

fn verify_unifying_route<T: HidTransport>(
    client: &mut Client<T>,
    device: &Device,
    product: u16,
) -> Result<()> {
    ensure!(
        receiver_kind(product) == Some(Transport::Unifying),
        "HID path is no longer a Unifying receiver"
    );
    let receiver_id = device
        .receiver_id
        .as_deref()
        .context("missing receiver identity")?;
    verify_receiver(client, receiver_id, product)?;
    let slot = device.slot.context("missing Unifying pairing slot")?;
    ensure!((1..=6).contains(&slot), "invalid Unifying pairing slot");
    let pairing = client.register(0xff, 0xb5, &[0x20 + slot - 1], true)?;
    ensure!(
        pairing.len() >= 8 && pairing[1..].iter().any(|byte| *byte != 0),
        "Unifying pairing slot is empty"
    );
    let serial = client.register(0xff, 0xb5, &[0x30 + slot - 1], true)?;
    ensure!(
        serial.len() >= 5 && serial[1..5].iter().any(|byte| *byte != 0),
        "cannot verify paired device serial; refusing to change an unidentified slot"
    );
    ensure!(
        device.id == format!("{receiver_id}:device:{}", hex(&serial[1..5])),
        "Unifying slot identity changed; refresh device status"
    );
    Ok(())
}

fn verify_usb_route<T: HidTransport>(
    client: &mut Client<T>,
    device: &Device,
    node: &Node,
) -> Result<()> {
    ensure!(
        node.bus == 3 && !receiver_product(node.product),
        "HID path is no longer a USB peripheral"
    );
    ensure!(
        node.kernel_slot == device.slot,
        "USB device route changed; refresh device status"
    );
    if node.kernel_slot.is_some() {
        ensure!(
            device.id.starts_with("unit:"),
            "kernel-managed receiver device has no verified unit identity; inventory only"
        );
    }
    let current = if device.id.starts_with("unit:") {
        let mut session = Session::awake(
            client,
            node.kernel_slot.unwrap_or(0xff),
            !node.short_reports,
        )?;
        let (_, version) = session
            .feature(0x0003)?
            .context("USB device no longer exposes its firmware identity")?;
        ensure!(version >= 1, "USB firmware identity is unavailable");
        unit_identity(&session.call(0x0003, 0, &[])?).context("USB firmware identity is empty")?
    } else {
        let (id, topology) = fallback_identity(node, false);
        ensure!(
            !topology,
            "USB device has no stable serial; its port identity supports inventory only"
        );
        id
    };
    ensure!(
        current == device.id,
        "USB device identity changed; refresh device status"
    );
    Ok(())
}

/// Open the current route only if its physical identity still matches the
/// inventory selection. A reused hidraw number or receiver slot must never
/// redirect a pending setting change to another device.
pub fn open_route(device: &Device) -> Result<Client<Hidraw>> {
    open_route_with_client_id(device, 0x0b)
}

pub fn open_route_with_client_id(device: &Device, client_id: u8) -> Result<Client<Hidraw>> {
    let path = device
        .hid_path
        .as_deref()
        .context("device has no accessible HID++ interface; check Logitech device-access permissions (see packaging/README.md)")?;
    let mut client = Client::with_client_id(Hidraw::open(path)?, client_id)?;
    if device.transport == Transport::Bluetooth
        || device.transport == Transport::Usb && device.slot.is_none()
    {
        client = client.with_direct_addressing();
    }
    verify_route(&mut client, device)?;
    Ok(client)
}

/// Revalidate an already-open route before another device write. The client
/// retains its software ID and direct-addressing mode from open_route.
pub(crate) fn verify_route<T: HidTransport>(client: &mut Client<T>, device: &Device) -> Result<()> {
    let path = device
        .hid_path
        .as_deref()
        .context("missing HID device route")?;
    if device.transport == Transport::Bolt {
        let receiver_id = device
            .receiver_id
            .as_deref()
            .context("missing receiver identity")?;
        verify_receiver(client, receiver_id, 0xc548)?;
        let slot = device.slot.context("missing Bolt pairing slot")?;
        ensure!((1..=6).contains(&slot), "invalid Bolt pairing slot");
        let pairing = client.register(0xff, 0xb5, &[0x50 + slot], true)?;
        let current_id = bolt_identity(receiver_id, &pairing)?;
        ensure!(
            current_id == device.id,
            "receiver slot now belongs to another device; refresh device status"
        );
    } else if device.transport == Transport::Unifying {
        let node = current_node(path)?;
        verify_unifying_route(client, device, node.product)?;
    } else if device.transport == Transport::Bluetooth {
        let node = Path::new(path)
            .file_name()
            .context("invalid HID device path")?;
        let properties = read_trim(
            Path::new("/sys/class/hidraw")
                .join(node)
                .join("device/uevent"),
        );
        let actual = property(&properties, "HID_UNIQ")
            .and_then(address)
            .context("cannot verify Bluetooth HID identity")?;
        ensure!(
            device.bluetooth_address.as_deref() == Some(&actual),
            "Bluetooth HID identity changed; refresh device status"
        );
    } else if device.transport == Transport::Usb {
        let node = current_node(path)?;
        verify_usb_route(client, device, &node)?;
    } else {
        bail!("device has no supported HID++ route");
    }
    Ok(())
}

pub fn settings(device: &mut Device) -> Result<Settings> {
    crate::model::require_supported(device)?;
    let mut client = open_device(device)?;
    let mut session = Session::awake(&mut client, device.slot.unwrap_or(0xff), long_ping(device))?;
    if let Err(error) = enrich_session(&mut session, device, Discovery::Details) {
        device
            .warnings
            .push(format!("HID++ information unavailable: {error:#}"));
    }
    read_settings(&mut session)
}

pub fn setting(device: &Device, key: &str) -> Result<Value> {
    ensure!(
        matches!(
            key,
            "dpi"
                | "dpi-supported"
                | "wheel-mode"
                | "smartshift"
                | "smartshift-sensitivity"
                | "scroll-invert"
                | "thumb-wheel-invert"
                | "fn-lock"
                | "backlight"
                | "haptic-strength"
        ),
        "unknown setting {key}"
    );
    crate::model::require_supported(device)?;
    let mut client = open_device(device)?;
    let mut session = Session::awake(&mut client, device.slot.unwrap_or(0xff), long_ping(device))?;
    ensure!(
        session.protocol.0 >= 2 || key == "fn-lock",
        "{key} is unavailable on this HID++ 1.0 device"
    );
    if key == "scroll-invert" {
        ensure!(
            session.supports_scroll_inversion()?,
            "device wheel does not support inversion"
        );
    }
    read_one(&mut session, key)
}

fn read_settings<T: HidTransport>(session: &mut Session<'_, T>) -> Result<Settings> {
    let mut settings = Settings::new();
    if session.protocol.0 == 1 {
        if let Some(data) = optional_register(session.client, session.slot, 0x09)? {
            settings.insert("fn-lock".into(), json!(data[1] & 1 != 0));
        }
        return Ok(settings);
    }
    if session.supports(0x2201)? {
        for key in ["dpi", "dpi-supported"] {
            settings.insert(key.into(), read_one(session, key)?);
        }
    }
    if let Some((id, get)) = session.smartshift()? {
        let data = session.call(id, get, &[])?;
        for key in ["wheel-mode", "smartshift", "smartshift-sensitivity"] {
            settings.insert(key.into(), wheel_value(key, &data)?);
        }
    }
    for (key, supported) in [
        ("scroll-invert", session.supports_scroll_inversion()?),
        ("thumb-wheel-invert", session.supports_thumb_wheel()?),
        ("fn-lock", session.fn_feature()?.is_some()),
        ("backlight", session.supports_backlight()?),
        ("haptic-strength", session.supports(0x19b0)?),
    ] {
        if supported {
            settings.insert(key.into(), read_one(session, key)?);
        }
    }
    Ok(settings)
}

fn dpi_values(data: &[u8]) -> Result<Vec<u16>> {
    ensure!(
        data.len() >= 3 && data[0] == 0,
        "invalid DPI list response for sensor 0"
    );
    let entries: Vec<_> = data[1..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|bytes| u16::from_be_bytes([bytes[0], bytes[1]]))
        .take_while(|value| *value != 0)
        .collect();
    let mut supported = BTreeSet::new();
    let mut index = 0;
    while index < entries.len() {
        let value = entries[index];
        ensure!(
            value & 0xe000 != 0xe000,
            "DPI list starts with an invalid range marker"
        );
        if index + 1 < entries.len() && entries[index + 1] & 0xe000 == 0xe000 {
            let step = entries[index + 1] & 0x1fff;
            let end = *entries.get(index + 2).context("truncated DPI range")?;
            ensure!(
                step > 0 && end >= value && end & 0xe000 != 0xe000,
                "invalid DPI range"
            );
            for dpi in (u32::from(value)..=u32::from(end)).step_by(usize::from(step)) {
                supported.insert(dpi as u16);
            }
            supported.insert(end);
            index += 3;
        } else {
            supported.insert(value);
            index += 1;
        }
    }
    ensure!(!supported.is_empty(), "device returned an empty DPI list");
    Ok(supported.into_iter().collect())
}

fn boolean(value: &str) -> Result<bool> {
    match value {
        "on" | "true" => Ok(true),
        "off" | "false" => Ok(false),
        _ => bail!("expected on/off or true/false"),
    }
}

pub fn set_setting(device: &Device, key: &str, value: &str) -> Result<Value> {
    crate::model::validate_setting(device, key, value)?;
    let mut client = open_device(device)?;
    let mut session = Session::awake(&mut client, device.slot.unwrap_or(0xff), long_ping(device))?;
    set_with_session(&mut session, key, value)
}

fn set_with_session<T: HidTransport>(
    session: &mut Session<'_, T>,
    key: &str,
    value: &str,
) -> Result<Value> {
    if session.protocol.0 == 1 {
        ensure!(
            key == "fn-lock",
            "{key} is unavailable on this HID++ 1.0 device"
        );
        let enabled = boolean(value)?;
        let mut current = optional_register(session.client, session.slot, 0x09)?
            .context("device does not support the Fn inversion register")?;
        current[1] = (current[1] & !1) | u8::from(enabled);
        session
            .client
            .write_register(session.slot, 0x09, &current[..3])?;
        let observed = optional_register(session.client, session.slot, 0x09)?
            .context("Fn inversion write was acknowledged but readback failed")?;
        ensure!(
            observed[1] & 1 == u8::from(enabled),
            "Fn inversion write was not verified"
        );
        return Ok(json!(enabled));
    }
    let expected = match key {
        "dpi" => {
            let dpi = value
                .parse::<u16>()
                .context("DPI must be an integer from 1 to 65535")?;
            ensure!(dpi > 0, "DPI must be greater than zero");
            let list = dpi_values(&session.call(0x2201, 1, &[0])?)?;
            ensure!(
                list.contains(&dpi),
                "DPI {dpi} is not supported; use `logishell config get <device> dpi-supported`"
            );
            session.call(0x2201, 3, &[0, (dpi >> 8) as u8, dpi as u8])?;
            json!(dpi)
        }
        "wheel-mode" | "smartshift" | "smartshift-sensitivity" => {
            let (id, get) = session
                .smartshift()?
                .context("device does not support SmartShift wheel configuration")?;
            let current = session.call(id, get, &[])?;
            let mut update = [0u8; 3];
            let expected = match key {
                "wheel-mode" => {
                    update[0] = match value {
                        "free-spin" => 1,
                        "ratchet" => 2,
                        _ => bail!("wheel-mode must be ratchet or free-spin"),
                    };
                    json!(value)
                }
                "smartshift-sensitivity" => {
                    update[1] = value
                        .parse::<u8>()
                        .context("smartshift-sensitivity must be 1..255")?;
                    ensure!(update[1] > 0, "smartshift-sensitivity must be 1..255");
                    json!(update[1])
                }
                _ => {
                    let enabled = boolean(value)?;
                    update[1] = if !enabled {
                        255
                    } else if (1..255).contains(&current[1]) {
                        current[1]
                    } else {
                        let default = if id == 0x2111 {
                            session.call(id, 0, &[])?[1]
                        } else {
                            current[2]
                        };
                        ensure!(
                            (1..255).contains(&default),
                            "device has no usable SmartShift default; set smartshift-sensitivity first"
                        );
                        default
                    };
                    if enabled {
                        update[0] = 2;
                    }
                    json!(enabled)
                }
            };
            session.call(id, get + 1, &update)?;
            expected
        }
        "scroll-invert" => {
            let enabled = boolean(value)?;
            let mode = session.call(0x2121, 1, &[])?[0];
            let caps = session.call(0x2121, 0, &[])?;
            ensure!(caps[1] & 8 != 0, "device wheel does not support inversion");
            session.call(0x2121, 2, &[if enabled { mode | 4 } else { mode & !4 }])?;
            json!(enabled)
        }
        "thumb-wheel-invert" => {
            let enabled = boolean(value)?;
            let (mode, _) = session.thumb_wheel_status()?;
            session.call(0x2150, 2, &[mode, u8::from(enabled), 0])?;
            let observed = session
                .thumb_wheel_status()
                .context("thumb-wheel-invert write was acknowledged but verification failed")?;
            ensure!(
                observed == (mode, enabled),
                "device acknowledged thumb-wheel-invert but readback was mode={}, inverted={} (expected mode={mode}, inverted={enabled}); change was not verified",
                observed.0,
                observed.1
            );
            return Ok(json!(enabled));
        }
        "fn-lock" => {
            let enabled = boolean(value)?;
            let id = session
                .fn_feature()?
                .context("device does not support Fn inversion")?;
            let data = if id == 0x40a3 {
                vec![0xff, u8::from(enabled)]
            } else {
                vec![u8::from(enabled)]
            };
            session.call(id, 1, &data)?;
            json!(enabled)
        }
        "backlight" => {
            let enabled = boolean(value)?;
            ensure!(
                session.supports_backlight()?,
                "backlight requires supported HID++ 0x1982 version 2 or 3"
            );
            let current = session.call(0x1982, 0, &[])?;
            ensure!(current.len() >= 12, "truncated backlight configuration");
            let mut update = [0u8; 16];
            update[0] = u8::from(enabled);
            update[1] = current[1] & 0x1f;
            update[2] = 0xff; // Preserve current effect.
            update[3] = if current[1] & 0x18 == 0x18 {
                current[5]
            } else {
                0
            };
            update[4..10].copy_from_slice(&current[6..12]);
            session.call(0x1982, 1, &update)?;
            json!(enabled)
        }
        "haptic-strength" => {
            crate::config::validate_setting(key, value)?;
            let strength = value.parse::<u8>()?;
            let (enabled, _) = session.haptic_configuration()?;
            session.call(0x19b0, 2, &[enabled, strength, 0])?;
            let observed = session
                .haptic_configuration()
                .context("haptic-strength write was acknowledged but verification failed")?;
            ensure!(
                observed == (enabled, strength),
                "device acknowledged haptic-strength but readback was enabled={}, strength={} (expected enabled={enabled}, strength={strength}); change was not verified",
                observed.0,
                observed.1
            );
            if enabled != 0
                && strength != 0
                && let Err(error) = session.preview_haptic_strength()
            {
                tracing::debug!("haptic strength applied; optional preview failed: {error:#}");
            }
            return Ok(json!(observed.1));
        }
        _ => bail!("unknown or read-only setting {key}"),
    };
    let observed = read_one(session, key)
        .with_context(|| format!("{key} write was acknowledged but verification failed"))?;
    ensure!(
        observed == expected,
        "device acknowledged {key} but readback was {observed} (expected {expected}); change was not verified"
    );
    Ok(observed)
}

fn read_one<T: HidTransport>(session: &mut Session<'_, T>, key: &str) -> Result<Value> {
    match key {
        "dpi" => {
            let data = session.call(0x2201, 2, &[0])?;
            ensure!(data[0] == 0, "invalid DPI response for sensor 0");
            Ok(json!(u16::from_be_bytes([data[1], data[2]])))
        }
        "dpi-supported" => Ok(json!(dpi_values(&session.call(0x2201, 1, &[0])?)?)),
        "scroll-invert" => Ok(json!(session.call(0x2121, 1, &[])?[0] & 4 != 0)),
        "thumb-wheel-invert" => Ok(json!(session.thumb_wheel_status()?.1)),
        "fn-lock" if session.protocol.0 == 1 => {
            let data = optional_register(session.client, session.slot, 0x09)?
                .context("device does not support the Fn inversion register")?;
            Ok(json!(data[1] & 1 != 0))
        }
        "fn-lock" => {
            let id = session.fn_feature()?.context("Fn inversion unavailable")?;
            Ok(json!(session.fn_state(id)?))
        }
        "backlight" => {
            ensure!(
                session.supports_backlight()?,
                "backlight requires supported HID++ 0x1982 version 2 or 3"
            );
            Ok(json!(session.call(0x1982, 0, &[])?[0] & 1 != 0))
        }
        "haptic-strength" => Ok(json!(session.haptic_configuration()?.1)),
        _ => {
            let (id, get) = session.smartshift()?.context("SmartShift unavailable")?;
            let data = session.call(id, get, &[])?;
            wheel_value(key, &data)
        }
    }
}

fn wheel_value(key: &str, data: &[u8]) -> Result<Value> {
    match key {
        "smartshift" => Ok(json!(data[1] != 255 && data[1] != 0)),
        "smartshift-sensitivity" => Ok(json!(data[1])),
        "wheel-mode" => match data[0] {
            1 => Ok(json!("free-spin")),
            2 => Ok(json!("ratchet")),
            mode => bail!("unknown wheel mode {mode}"),
        },
        _ => bail!("unknown setting {key}"),
    }
}

pub fn pair_receiver_with_ui(receiver: &Receiver, timeout_secs: u64, ui: PairingUi) -> Result<()> {
    let maximum = match receiver.transport {
        Transport::Bolt => 60,
        Transport::Unifying => 255,
        _ => bail!(
            "receiver pairing supports Bolt and Unifying; use Bluetooth pairing for a Bluetooth device"
        ),
    };
    ensure!(
        (1..=maximum).contains(&timeout_secs),
        "{} pairing timeout must be 1..{maximum} seconds",
        receiver.transport
    );
    ensure!(!ui.cancelled(), "pairing cancelled");
    let mut client = Client::new(Hidraw::open(&receiver.hid_path)?);
    let node = current_node(&receiver.hid_path)?;
    ensure!(
        receiver_kind(node.product) == Some(receiver.transport),
        "HID path is no longer the selected receiver type"
    );
    verify_receiver(&mut client, &receiver.id, node.product)?;
    let timeout = Duration::from_secs(timeout_secs);
    let mut interaction = ReceiverSetup { ui, timeout };
    match receiver.transport {
        Transport::Bolt => bolt::run(&mut client, &mut interaction, timeout_secs as u8),
        Transport::Unifying => unifying::run(&mut client, &mut interaction, timeout_secs as u8),
        _ => unreachable!("receiver type checked before opening it"),
    }
}

pub fn unpair(device: &Device) -> Result<()> {
    crate::model::require_supported(device)?;
    match device.transport {
        Transport::Bolt => bolt::unpair(device),
        Transport::Unifying => unifying::unpair(device),
        _ => bail!(
            "receiver unpairing supports Bolt and Unifying; remove Bluetooth pairings through Bluetooth management"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hidpp::testing::{Scripted, packet};
    use std::collections::VecDeque;

    fn wire(feature: u8, function: u8, payload: &[u8]) -> Vec<u8> {
        packet(1, feature, function << 4 | 0x0b, payload, true)
    }

    fn test_device() -> Device {
        Device {
            id: "receiver:c52b:11223344:device:aabbccdd".into(),
            name: "Logitech keyboard".into(),
            transport: Transport::Unifying,
            receiver_id: Some("receiver:c52b:11223344".into()),
            slot: Some(1),
            hid_path: Some("/dev/example".into()),
            ..Default::default()
        }
    }

    #[test]
    fn receiver_setup_uses_screens_for_selection_authentication_consent_and_cancel() {
        use crate::terminal::{self, Event};
        let (ui, mut events) = terminal::channel();
        let screen = ui.clone();
        let worker = std::thread::spawn(move || {
            let mut setup = ReceiverSetup {
                ui,
                timeout: Duration::from_secs(2),
            };
            setup
                .status("Finding", "Searching for your device.")
                .expect("send discovery status");
            let choice = Choice {
                label: "MX Master 4".into(),
                detail: "Mouse · 010203040506".into(),
            };
            assert_eq!(
                setup
                    .select("Choose a device", "Select your mouse", vec![choice])
                    .expect("receive device selection"),
                Some(0)
            );
            setup
                .status("Confirm on your device", "Click left, then right.")
                .expect("send authentication instructions");
            assert!(
                !setup
                    .confirm(
                        "Open this receiver for pairing?",
                        "Unifying pairs the next compatible device."
                    )
                    .expect("receive receiver consent")
            );
            setup
        });
        assert!(matches!(events.blocking_recv(), Some(Event::Status { .. })));
        let Some(Event::Choose { choices, reply, .. }) = events.blocking_recv() else {
            panic!("expected device selection")
        };
        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0].label, "MX Master 4");
        assert!(choices[0].detail.contains("010203040506"));
        reply.send(Some(0)).expect("select the displayed device");
        let Some(Event::Status { title, message }) = events.blocking_recv() else {
            panic!("expected authentication screen")
        };
        assert_eq!(title, "Confirm on your device");
        assert_eq!(message, "Click left, then right.");
        let Some(Event::Choose {
            message,
            choices,
            reply,
            ..
        }) = events.blocking_recv()
        else {
            panic!("expected consent screen")
        };
        assert!(message.contains("next compatible device"));
        assert_eq!(choices[0].label, "Go back");
        assert_eq!(choices[1].label, "Start pairing");
        reply.send(Some(0)).expect("decline receiver opening");
        screen.cancel();
        assert!(
            worker
                .join()
                .expect("setup worker completed")
                .cancelled()
                .expect("check setup cancellation")
        );
    }

    #[test]
    fn legacy_discovery_reads_battery_but_probes_fn_only_for_details() -> Result<()> {
        for detail in [Discovery::Status, Discovery::Control, Discovery::Details] {
            let mut exchanges = VecDeque::from([(
                vec![0x10, 1, 0, 0x1b, 0, 0, 0x5a],
                vec![0x10, 1, 0x8f, 0, 0x1b, 1, 0],
            )]);
            if detail != Discovery::Control {
                exchanges.extend([
                    (
                        vec![0x10, 1, 0x81, 0x0d, 0, 0, 0],
                        vec![0x10, 1, 0x8f, 0x81, 0x0d, 2, 0],
                    ),
                    (
                        vec![0x10, 1, 0x81, 0x07, 0, 0, 0],
                        vec![0x10, 1, 0x81, 0x07, 5, 0, 0],
                    ),
                ]);
            }
            if detail == Discovery::Details {
                exchanges.push_back((
                    vec![0x10, 1, 0x81, 0x09, 0, 0, 0],
                    vec![0x10, 1, 0x81, 0x09, 0, 1, 0],
                ));
            }
            let mut client = Client::new(Scripted::new(exchanges));
            let mut device = test_device();
            enrich(&mut client, 1, &mut device, detail)?;
            assert_eq!(device.state, DeviceState::Online);
            assert!(device.capabilities.contains(&"hidpp-1.0".into()));
            assert_eq!(
                device.capabilities.contains(&"fn-lock".into()),
                detail == Discovery::Details
            );
            assert_eq!(device.battery.is_some(), detail != Discovery::Control);
            if let Some(battery) = device.battery {
                assert_eq!(battery.percent, None);
                assert_eq!(battery.level.as_deref(), Some("good"));
            }
            assert!(client.into_transport().exchanges.is_empty());
        }
        Ok(())
    }

    #[test]
    fn lean_discovery_preserves_full_name_with_only_requested_capabilities() -> Result<()> {
        let name = b"MX Mechanical Mini";
        for detail in [Discovery::Status, Discovery::Control] {
            let mut exchanges = vec![
                (
                    vec![0x10, 1, 0, 0x1b, 0, 0, 0x5a],
                    wire(0, 1, &[4, 5, 0x5a]),
                ),
                (wire(0, 0, &[0, 5]), wire(0, 0, &[2, 0, 0])),
                (wire(2, 0, &[]), wire(2, 0, &[name.len() as u8])),
                (wire(2, 1, &[0]), wire(2, 1, &name[..16])),
                (wire(2, 1, &[16]), wire(2, 1, &name[16..])),
            ];
            if detail == Discovery::Control {
                exchanges.extend([
                    (wire(0, 0, &[0x1b, 4]), wire(0, 0, &[9, 0, 6])),
                    (wire(0, 0, &[0x21, 0x50]), wire(0, 0, &[19, 0, 0])),
                ]);
            } else {
                exchanges.extend([
                    (wire(0, 0, &[0x10, 4]), wire(0, 0, &[3, 0, 0])),
                    (wire(3, 0, &[]), wire(3, 0, &[15, 3])),
                    (wire(3, 1, &[]), wire(3, 1, &[65, 4, 1])),
                ]);
            }
            let mut client = Client::new(Scripted::new(exchanges));
            let mut device = test_device();
            device.name = "MX MCHNCL M".into();
            let original_id = device.id.clone();
            enrich(&mut client, 1, &mut device, detail)?;
            assert_eq!(device.id, original_id);
            assert_eq!(device.name, "MX Mechanical Mini");
            assert_eq!(device.state, DeviceState::Online);
            assert!(device.capabilities.contains(&"hidpp-4.5".into()));
            assert_eq!(
                device.capabilities.contains(&"button-diversion".into()),
                detail == Discovery::Control
            );
            assert_eq!(
                device.capabilities.contains(&"thumb-wheel".into()),
                detail == Discovery::Control
            );
            assert!(device.firmware.is_none());
            assert_eq!(device.battery.is_some(), detail == Discovery::Status);
            if let Some(battery) = device.battery {
                assert_eq!(battery.percent, Some(65));
                assert_eq!(battery.level.as_deref(), Some("good"));
                assert_eq!(battery.charging, Some(true));
                assert!(!battery.stale);
            }
            assert!(device.warnings.is_empty());
            assert!(client.into_transport().exchanges.is_empty());
        }
        Ok(())
    }

    #[test]
    fn all_discovery_modes_preserve_the_same_direct_usb_unit_identity() -> Result<()> {
        let wire = |feature: u8, function: u8, payload: &[u8]| {
            packet(0xff, feature, function << 4 | 0x0b, payload, true)
        };
        for detail in [Discovery::Status, Discovery::Control, Discovery::Details] {
            let mut exchanges = VecDeque::from([(
                vec![0x10, 0xff, 0, 0x1b, 0, 0, 0x5a],
                wire(0, 1, &[4, 5, 0x5a]),
            )]);
            if detail == Discovery::Details {
                for &feature in FEATURES {
                    let response = match feature {
                        0x0003 => [3, 0, 1],
                        0x1000 => [4, 0, 0],
                        _ => [0, 0, 0],
                    };
                    exchanges
                        .push_back((wire(0, 0, &feature.to_be_bytes()), wire(0, 0, &response)));
                }
            } else {
                exchanges.extend([
                    (wire(0, 0, &[0, 5]), wire(0, 0, &[0, 0, 0])),
                    (wire(0, 0, &[0, 3]), wire(0, 0, &[3, 0, 1])),
                ]);
            }
            exchanges.push_back((
                wire(3, 0, &[]),
                wire(3, 0, &[1, 1, 2, 3, 4, 0, 0, 0xc0, 0x7d, 0, 0, 0, 0]),
            ));
            if detail == Discovery::Details {
                exchanges.push_back((
                    wire(3, 1, &[0]),
                    wire(3, 1, &[0, b'M', b'O', b'U', 0x12, 0x34, 0x56, 0x78]),
                ));
            } else if detail == Discovery::Status {
                exchanges.extend([
                    (wire(0, 0, &[0x10, 4]), wire(0, 0, &[0, 0, 0])),
                    (wire(0, 0, &[0x10, 0]), wire(0, 0, &[4, 0, 0])),
                ]);
            }
            if detail == Discovery::Control {
                exchanges.extend([
                    (wire(0, 0, &[0x1b, 4]), wire(0, 0, &[0, 0, 0])),
                    (wire(0, 0, &[0x21, 0x50]), wire(0, 0, &[0, 0, 0])),
                ]);
            } else {
                exchanges.push_back((wire(4, 0, &[]), wire(4, 0, &[73, 0, 0])));
            }
            let mut client = Client::new(Scripted::new(exchanges)).with_direct_addressing();
            let mut device = test_device();
            device.id = "usb:c07d:port".into();
            device.transport = Transport::Usb;
            device.receiver_id = None;
            device.slot = None;
            device.capabilities = vec!["hidpp-short-reports".into()];
            device.warnings =
                vec!["Device has no readable serial; its ID is tied to its USB port.".into()];
            enrich(&mut client, 0xff, &mut device, detail)?;
            assert_eq!(device.id, "unit:c07d00000000:01020304");
            assert_eq!(device.state, DeviceState::Online);
            assert_eq!(
                device.battery.and_then(|battery| battery.percent),
                (detail != Discovery::Control).then_some(73)
            );
            assert_eq!(
                device.firmware.as_deref(),
                (detail == Discovery::Details).then_some("MOU 12.34.5678")
            );
            assert!(device.warnings.is_empty());
            assert!(client.into_transport().exchanges.is_empty());
        }
        Ok(())
    }

    #[test]
    fn legacy_battery_preserves_unknown_levels_and_charging_information() -> Result<()> {
        let coarse = legacy_battery(0x07, &[0, 0x21, 0])?;
        assert_eq!(coarse.percent, None);
        assert_eq!(coarse.level, None);
        assert_eq!(coarse.charging, Some(true));
        assert_eq!(legacy_battery(0x0d, &[63, 0, 0x50])?.percent, Some(63));
        assert_eq!(legacy_battery(0x0d, &[255, 0, 0x90])?.percent, None);
        assert!(legacy_battery(0x0d, &[42]).is_err());
        Ok(())
    }

    #[test]
    fn voltage_battery_never_turns_voltage_into_an_unverified_percentage() -> Result<()> {
        let charging = voltage_battery(&[0x0f, 0xa0, 0x80])?;
        assert_eq!(charging.percent, None);
        assert_eq!(charging.level, None);
        assert_eq!(charging.charging, Some(true));
        let full = voltage_battery(&[0x10, 0x5a, 0x81])?;
        assert_eq!(full.level.as_deref(), Some("full"));
        assert_eq!(full.percent, None);
        assert_eq!(full.charging, Some(false));
        assert_eq!(
            voltage_battery(&[0x0d, 0xac, 0x20])?.level.as_deref(),
            Some("critical")
        );
        assert_eq!(voltage_battery(&[0x0f, 0xa0, 0x87])?.charging, None);
        Ok(())
    }

    #[test]
    fn legacy_fn_toggle_preserves_other_register_bits_and_verifies_readback() -> Result<()> {
        let mut client = Client::new(Scripted::new(VecDeque::from([
            (
                vec![0x10, 1, 0x81, 0x09, 0, 0, 0],
                vec![0x10, 1, 0x81, 0x09, 0xaa, 0xb4, 0xcc],
            ),
            (
                vec![0x10, 1, 0x80, 0x09, 0xaa, 0xb5, 0xcc],
                vec![0x10, 1, 0x80, 0x09, 0, 0, 0],
            ),
            (
                vec![0x10, 1, 0x81, 0x09, 0, 0, 0],
                vec![0x10, 1, 0x81, 0x09, 0xaa, 0xb5, 0xcc],
            ),
        ])));
        let mut session = Session::new(&mut client, 1);
        session.protocol = (1, 0);
        assert_eq!(
            set_with_session(&mut session, "fn-lock", "on")?,
            json!(true)
        );
        assert!(set_with_session(&mut session, "dpi", "800").is_err());
        assert!(client.into_transport().exchanges.is_empty());
        Ok(())
    }

    #[test]
    fn legacy_fn_toggle_rejects_unsupported_register_before_any_write() {
        let mut client = Client::new(Scripted::new(VecDeque::from([(
            vec![0x10, 1, 0x81, 0x09, 0, 0, 0],
            vec![0x10, 1, 0x8f, 0x81, 0x09, 2, 0],
        )])));
        let mut session = Session::new(&mut client, 1);
        session.protocol = (1, 0);
        assert!(set_with_session(&mut session, "fn-lock", "on").is_err());
        assert!(client.into_transport().exchanges.is_empty());
    }

    #[test]
    fn unifying_route_verifies_receiver_and_paired_device_serial() -> Result<()> {
        let exchange = |subregister: u8, data: &[u8]| {
            let request = vec![0x10, 0xff, 0x83, 0xb5, subregister, 0, 0];
            let mut response = vec![0; 20];
            response[..4].copy_from_slice(&[0x11, 0xff, 0x83, 0xb5]);
            response[4..4 + data.len()].copy_from_slice(data);
            (request, response)
        };
        for (serial, expected_success) in [([0xaa, 0xbb, 0xcc, 0xdd], true), ([1, 2, 3, 4], false)]
        {
            let mut client = Client::new(Scripted::new(VecDeque::from([
                exchange(3, &[3, 0x11, 0x22, 0x33, 0x44]),
                exchange(0x20, &[0x20, 1, 0, 0x20, 0x08, 0, 0, 1]),
                exchange(0x30, &[0x30, serial[0], serial[1], serial[2], serial[3]]),
            ])));
            assert_eq!(
                verify_unifying_route(&mut client, &test_device(), 0xc52b).is_ok(),
                expected_success
            );
            assert!(client.into_transport().exchanges.is_empty());
        }
        Ok(())
    }

    #[test]
    fn receiver_products_cannot_be_misclassified_as_direct_usb_peripherals() {
        assert!(receiver_product(0xc534)); // Nano
        assert!(receiver_product(0xc539)); // Lightspeed
        assert!(receiver_product(0xc548)); // Bolt
        assert!(!receiver_product(0xc07d)); // Direct HID++ mouse
        assert!(!receiver_product(0xc32b)); // Direct HID++ keyboard
        assert_eq!(receiver_kind(0xc534), None);
        assert_eq!(receiver_kind(0xc539), None);
    }

    fn kernel_node(slot: u8) -> Node {
        Node {
            path: format!("/dev/test-hidraw{slot}"),
            sys: format!("/sys/devices/test-receiver/child-{slot}").into(),
            name: "Logitech test mouse".into(),
            product: 0x407f,
            bus: 3,
            unique: String::new(),
            physical: format!("usb-test/input2:{slot}"),
            usb: Some("/sys/devices/test-receiver".into()),
            has_hidpp: true,
            short_reports: true,
            long_reports: true,
            kernel_slot: Some(slot),
        }
    }

    #[test]
    fn kernel_child_requires_exact_parent_and_driver_owned_slot() {
        for slot in 1..=7 {
            assert_eq!(
                kernel_receiver_slot(
                    &format!("usb-test/input2:{slot}"),
                    "usb-test/input2",
                    "logitech-djreceiver",
                ),
                Some(slot)
            );
        }
        for physical in [
            "usb-test/input2",
            "usb-test/input2:0",
            "usb-test/input2:8",
            "usb-test/input2:01",
            "usb-test/input22:1",
        ] {
            assert_eq!(
                kernel_receiver_slot(physical, "usb-test/input2", "logitech-djreceiver"),
                None
            );
        }
        assert_eq!(
            kernel_receiver_slot("usb-test/input2:1", "usb-test/input2", "hid-generic"),
            None
        );
        assert_eq!(kernel_receiver_slot(":1", "", "logitech-djreceiver"), None);
    }

    #[test]
    fn kernel_children_keep_separate_inventory_and_ignore_receiver_serial() -> Result<()> {
        let receiver = tempfile::tempdir()?;
        fs::write(receiver.path().join("serial"), "receiver-only-serial")?;
        let mut first = kernel_node(1);
        first.usb = Some(receiver.path().into());
        let mut second = kernel_node(2);
        second.usb = Some(receiver.path().into());
        assert_ne!(node_group(&first), node_group(&second));
        assert_ne!(node_group(&first), receiver.path().display().to_string());
        let (first_id, topology) = fallback_identity(&first, false);
        assert!(topology);
        assert!(!first_id.contains("receiver-only-serial"));
        assert_ne!(first_id, fallback_identity(&second, false).0);
        first.unique = "child-serial".into();
        assert_eq!(
            fallback_identity(&first, false),
            ("usb:407f:child-serial".into(), false)
        );
        Ok(())
    }

    #[test]
    fn kernel_route_matches_actual_slot_and_checks_live_unit_identity() -> Result<()> {
        for (slot, serial, success) in [(3, 4, true), (7, 4, true), (3, 9, false)] {
            let packet = |feature, function, data: &[u8]| {
                let mut packet = wire(feature, function, data);
                packet[1] = slot;
                packet
            };
            let mut client = Client::new(Scripted::new(VecDeque::from([
                (
                    vec![0x10, slot, 0, 0x1b, 0, 0, 0x5a],
                    packet(0, 1, &[4, 5, 0x5a]),
                ),
                (packet(0, 0, &[0, 3]), packet(0, 0, &[7, 0, 1])),
                (
                    packet(7, 0, &[]),
                    packet(7, 0, &[1, 1, 2, 3, serial, 0, 0, 0xc0, 0x7d, 0, 0, 0, 0]),
                ),
            ])));
            let mut device = test_device();
            device.id = "unit:c07d00000000:01020304".into();
            device.transport = Transport::Usb;
            device.receiver_id = None;
            device.slot = Some(slot);
            assert_eq!(
                verify_usb_route(&mut client, &device, &kernel_node(slot)).is_ok(),
                success
            );
            assert!(client.into_transport().exchanges.is_empty());
        }
        Ok(())
    }

    #[test]
    fn kernel_route_without_unit_identity_or_with_changed_slot_stops_before_requests() {
        let mut client = Client::new(Scripted::new(VecDeque::new()));
        let mut device = test_device();
        device.id = "usb:407f:child-serial".into();
        device.transport = Transport::Usb;
        device.receiver_id = None;
        device.slot = Some(1);
        assert!(verify_usb_route(&mut client, &device, &kernel_node(1)).is_err());
        device.id = "unit:c07d00000000:01020304".into();
        assert!(verify_usb_route(&mut client, &device, &kernel_node(2)).is_err());
        assert!(client.into_transport().exchanges.is_empty());
    }

    #[test]
    fn usb_unit_identity_rejects_missing_or_zero_serials() {
        assert_eq!(unit_identity(&[0; 13]), None);
        assert_eq!(unit_identity(&[1, 2, 3]), None);
        assert_eq!(
            unit_identity(&[1, 1, 2, 3, 4, 0, 0, 0xc0, 0x7d, 0, 0, 0, 0]).as_deref(),
            Some("unit:c07d00000000:01020304")
        );
    }

    #[test]
    fn bolt_routes_reject_unverifiable_pairing_identities() -> Result<()> {
        let receiver = format!("receiver:c548:{}", "01".repeat(16));
        for identity in [[0, 2, 0x42, 0xb0, 0, 0, 0, 0], [0, 2, 0, 0, 1, 2, 3, 4]] {
            let mut pairing = identity;
            pairing[0] = 0x51;
            let mut client = Client::new(Scripted::new([
                (
                    packet(0xff, 0x83, 0xfb, &[], false),
                    packet(0xff, 0x83, 0xfb, &[1; 16], true),
                ),
                (
                    packet(0xff, 0x83, 0xb5, &[0x51], false),
                    packet(0xff, 0x83, 0xb5, &pairing, true),
                ),
            ]));
            let device = Device {
                id: format!(
                    "{receiver}:device:{}:{}",
                    hex(&pairing[2..4]),
                    hex(&pairing[4..8])
                ),
                receiver_id: Some(receiver.clone()),
                transport: Transport::Bolt,
                ..test_device()
            };
            assert!(
                verify_route(&mut client, &device)
                    .expect_err("missing pairing identity")
                    .to_string()
                    .contains("no verifiable device identity")
            );
            assert!(client.into_transport().exchanges.is_empty());
        }
        assert!(bolt_identity(&receiver, &[0; 3]).is_err());
        assert_eq!(
            bolt_identity(&receiver, &[0x51, 2, 0x42, 0xb0, 1, 2, 3, 4])?,
            format!("{receiver}:device:42b0:01020304")
        );
        Ok(())
    }

    #[test]
    fn settings_session_probes_connection_before_first_feature_lookup() -> Result<()> {
        let mut client = Client::new(Scripted::new(VecDeque::from([
            (
                vec![0x10, 1, 0, 0x1b, 0, 0, 0x5a],
                wire(0, 1, &[4, 5, 0x5a]),
            ),
            (wire(0, 0, &[0x22, 1]), wire(0, 0, &[0, 0, 0])),
        ])));
        let mut session = Session::awake(&mut client, 1, false)?;
        assert!(!session.supports(0x2201)?);
        Ok(())
    }

    #[test]
    fn settings_session_stops_when_connection_probe_fails() {
        let mut client = Client::new(Scripted::new(VecDeque::from([(
            vec![0x10, 1, 0, 0x1b, 0, 0, 0x5a],
            vec![0x10, 1, 0x8f, 0, 0x1b, 4, 0],
        )])));
        let error = Session::awake(&mut client, 1, false)
            .err()
            .expect("sleeping device is unavailable");
        assert!(error.to_string().contains("device slot 1"));
        assert_eq!(
            error
                .downcast_ref::<ProtocolError>()
                .expect("protocol error")
                .code,
            4
        );
    }

    #[test]
    fn paired_keyboard_connection_failure_stays_offline_without_legacy_fallback() {
        let receiver_reply = |payload: &[u8]| {
            let mut reply = vec![0; 20];
            reply[..4].copy_from_slice(&[0x11, 0xff, 0x83, 0xb5]);
            reply[4..4 + payload.len()].copy_from_slice(payload);
            reply
        };
        let mut name = vec![0x61, 1, 11];
        name.extend_from_slice(b"MX MCHNCL M");
        let mut exchanges = VecDeque::from([
            (
                vec![0x10, 0xff, 0x83, 0xb5, 0x51, 0, 0],
                receiver_reply(&[0x51, 1, 0x67, 0xb3, 1, 2, 3, 4]),
            ),
            (
                vec![0x10, 0xff, 0x83, 0xb5, 0x61, 1, 0],
                receiver_reply(&name),
            ),
            (
                vec![0x10, 1, 0, 0x1b, 0, 0, 0x5a],
                vec![0x10, 1, 0x8f, 0, 0x1b, 4, 0],
            ),
        ]);
        for slot in 2..=6 {
            exchanges.push_back((
                vec![0x10, 0xff, 0x83, 0xb5, 0x50 + slot, 0, 0],
                vec![0x10, 0xff, 0x8f, 0x83, 0xb5, 8, 0],
            ));
        }
        let receiver = Receiver {
            id: "receiver:c548:example".into(),
            name: "Logitech bolt receiver".into(),
            transport: Transport::Bolt,
            hid_path: "/dev/example".into(),
        };
        for detail in [Discovery::Status, Discovery::Details] {
            let mut client = Client::new(Scripted::new(exchanges.clone()));
            let mut inventory = Inventory::default();
            discover_receiver(&mut client, &receiver, &mut inventory, detail);
            assert!(inventory.warnings.is_empty());
            assert_eq!(inventory.devices.len(), 1);
            let device = &inventory.devices[0];
            assert_eq!(device.name, "MX MCHNCL M");
            assert_eq!(device.slot, Some(1));
            assert_eq!(device.state, DeviceState::Offline);
            assert!(device.battery.is_none());
            assert!(device.capabilities.is_empty());
            assert!(device.warnings[0].contains("unreachable"));
            assert!(client.into_transport().exchanges.is_empty());
        }
    }

    #[test]
    fn duplicate_pairings_keep_live_route_and_device_identity() {
        let offline = Device {
            id: "receiver:c548:example:device:67b3:01020304".into(),
            name: "MX MCHNCL M".into(),
            transport: Transport::Bolt,
            state: DeviceState::Offline,
            receiver_id: Some("receiver:c548:example".into()),
            slot: Some(1),
            hid_path: Some("/dev/example".into()),
            warnings: vec!["Device is unreachable".into()],
            ..Default::default()
        };
        let mut unknown = offline.clone();
        unknown.slot = Some(2);
        unknown.state = DeviceState::Unknown;
        let mut online = offline.clone();
        online.slot = Some(3);
        online.state = DeviceState::Online;
        online.battery = Some(Battery {
            percent: Some(73),
            ..Battery::default()
        });
        online.capabilities = vec!["hidpp-4.5".into(), "fn-lock".into()];
        online.warnings.clear();
        let mut later_online = online.clone();
        later_online.slot = Some(4);
        let mut devices = vec![
            offline.clone(),
            unknown.clone(),
            online.clone(),
            later_online,
        ];
        deduplicate_devices(&mut devices);
        assert_eq!(devices.len(), 1);
        let selected = &devices[0];
        assert_eq!(selected.id, offline.id);
        assert_eq!(selected.slot, Some(3));
        assert_eq!(selected.state, DeviceState::Online);
        assert_eq!(
            selected
                .battery
                .as_ref()
                .and_then(|battery| battery.percent),
            Some(73)
        );
        assert_eq!(selected.capabilities, online.capabilities);
        assert!(selected.warnings.is_empty());

        let mut unavailable = vec![offline, unknown];
        deduplicate_devices(&mut unavailable);
        assert_eq!(unavailable.len(), 1);
        assert_eq!(unavailable[0].slot, Some(2));
        assert_eq!(unavailable[0].state, DeviceState::Unknown);
    }

    #[test]
    fn receiver_enriches_duplicate_identities_only_until_a_route_is_online() {
        for first_online in [false, true] {
            let mut exchanges = Vec::new();
            for slot in 1..=6 {
                let pairing = if slot <= 3 {
                    vec![0x50 + slot, 1, 0x67, 0xb3, 1, 2, 3, u8::from(slot == 3)]
                } else {
                    vec![0x50 + slot]
                };
                exchanges.push((
                    packet(0xff, 0x83, 0xb5, &[0x50 + slot], false),
                    packet(0xff, 0x83, 0xb5, &pairing, true),
                ));
                if slot > 3 {
                    continue;
                }
                exchanges.push((
                    packet(0xff, 0x83, 0xb5, &[0x60 + slot, 1], false),
                    packet(0xff, 0x83, 0xb5, &[0x60 + slot, 1, 0], true),
                ));
                if slot == 2 && first_online {
                    continue;
                }
                let online = slot != 1 || first_online;
                exchanges.push((
                    packet(slot, 0, 0x1b, &[0, 0, 0x5a], false),
                    if online {
                        packet(slot, 0, 0x1b, &[2, 0, 0x5a], false)
                    } else {
                        packet(slot, 0x8f, 0, &[0x1b, 4], false)
                    },
                ));
                if online {
                    for feature in [0x0005_u16, 0x1004, 0x1000, 0x1001] {
                        exchanges.push((
                            packet(slot, 0, 0x0b, &feature.to_be_bytes(), true),
                            packet(slot, 0, 0x0b, &[0], true),
                        ));
                    }
                }
            }
            let receiver = Receiver {
                id: "receiver:c548:example".into(),
                name: "Logitech bolt receiver".into(),
                transport: Transport::Bolt,
                hid_path: "/dev/example".into(),
            };
            let mut client = Client::new(Scripted::new(exchanges));
            let mut inventory = Inventory::default();
            discover_receiver(&mut client, &receiver, &mut inventory, Discovery::Status);
            assert!(client.into_transport().exchanges.is_empty());
            deduplicate_devices(&mut inventory.devices);
            assert!(inventory.warnings.is_empty());
            assert_eq!(inventory.devices.len(), 2);
            assert_eq!(
                inventory.devices[0].slot,
                Some(if first_online { 1 } else { 2 })
            );
            assert_eq!(inventory.devices[1].slot, Some(3));
            assert!(
                inventory
                    .devices
                    .iter()
                    .all(|device| device.state == DeviceState::Online)
            );
        }
    }

    #[test]
    fn setting_acknowledgement_without_correct_readback_is_not_success() -> Result<()> {
        // Sensor advertises 400 and 800 DPI. The write is acknowledged, but
        // firmware still reports 400: that must not be persisted as success.
        let script = Scripted::new(VecDeque::from([
            (wire(7, 1, &[0]), wire(7, 1, &[0, 1, 0x90, 3, 0x20, 0, 0])),
            (wire(7, 3, &[0, 3, 0x20]), wire(7, 3, &[])),
            (wire(7, 2, &[0]), wire(7, 2, &[0, 1, 0x90])),
        ]));
        let mut client = Client::new(script);
        let mut session = Session::new(&mut client, 1);
        session.features.insert(0x2201, Some((7, 0)));
        let error = set_with_session(&mut session, "dpi", "800").expect_err("readback differs");
        assert!(error.to_string().contains("not verified"));
        Ok(())
    }

    #[test]
    fn unsupported_dpi_never_sends_a_set_request() -> Result<()> {
        let script = Scripted::new(VecDeque::from([(
            wire(7, 1, &[0]),
            wire(7, 1, &[0, 1, 0x90, 3, 0x20, 0, 0]),
        )]));
        let mut client = Client::new(script);
        let mut session = Session::new(&mut client, 1);
        session.features.insert(0x2201, Some((7, 0)));
        let error = set_with_session(&mut session, "dpi", "600").expect_err("unsupported DPI");
        assert!(error.to_string().contains("not supported"));
        Ok(())
    }

    #[test]
    fn targeted_settings_reject_unknown_keys_and_dpi_from_another_sensor() -> Result<()> {
        assert!(
            setting(&test_device(), "unknown")
                .expect_err("unknown setting rejected before opening HID")
                .to_string()
                .contains("unknown setting")
        );
        let mut client = Client::new(Scripted::new([(
            wire(7, 2, &[0]),
            wire(7, 2, &[1, 3, 0x20]),
        )]));
        let mut session = Session::new(&mut client, 1);
        session.features.insert(0x2201, Some((7, 0)));
        assert!(
            read_one(&mut session, "dpi")
                .expect_err("wrong sensor index")
                .to_string()
                .contains("sensor 0")
        );
        session.features.insert(0x1982, Some((11, 1)));
        assert!(read_one(&mut session, "backlight").is_err());
        assert!(client.into_transport().exchanges.is_empty());
        Ok(())
    }

    #[test]
    fn haptic_strength_preserves_enable_state_and_verifies_readback() -> Result<()> {
        for enabled in [0, 1] {
            let mut exchanges = vec![
                (wire(12, 1, &[0, 0, 0]), wire(12, 1, &[enabled, 50, 0x54])),
                (wire(12, 2, &[enabled, 75, 0]), wire(12, 2, &[])),
                (wire(12, 1, &[0, 0, 0]), wire(12, 1, &[enabled, 75, 0x54])),
            ];
            if enabled != 0 {
                exchanges.extend([
                    (
                        wire(12, 0, &[0, 0, 0]),
                        wire(12, 0, &[0, 1, 0, 60, 0x08, 0, 0x7f, 0xff]),
                    ),
                    (wire(12, 4, &[1, 0, 0]), wire(12, 4, &[])),
                ]);
            }
            let mut client = Client::new(Scripted::new(exchanges));
            let mut session = Session::new(&mut client, 1);
            session.features.insert(0x19b0, Some((12, 0)));
            assert_eq!(
                set_with_session(&mut session, "haptic-strength", "75")?,
                json!(75)
            );
            assert!(client.into_transport().exchanges.is_empty());
        }
        Ok(())
    }

    #[test]
    fn haptic_preview_respects_zero_strength_and_supported_waveforms() -> Result<()> {
        for (strength, waveforms, preview) in [
            (0, 0u32, None),
            (40, 1 << 4, Some(4)),
            (40, 1 << 30, None),
            (40, 0, None),
        ] {
            let mut exchanges = vec![
                (wire(12, 1, &[0, 0, 0]), wire(12, 1, &[1, 50, 0x54])),
                (wire(12, 2, &[1, strength, 0]), wire(12, 2, &[])),
                (wire(12, 1, &[0, 0, 0]), wire(12, 1, &[1, strength, 0x54])),
            ];
            if strength != 0 {
                let mut capabilities = [0u8; 8];
                capabilities[4..8].copy_from_slice(&waveforms.to_be_bytes());
                exchanges.push((wire(12, 0, &[0, 0, 0]), wire(12, 0, &capabilities)));
            }
            if let Some(waveform) = preview {
                exchanges.push((wire(12, 4, &[waveform, 0, 0]), wire(12, 4, &[])));
            }
            let mut client = Client::new(Scripted::new(exchanges));
            let mut session = Session::new(&mut client, 1);
            session.features.insert(0x19b0, Some((12, 0)));
            assert_eq!(
                set_with_session(&mut session, "haptic-strength", &strength.to_string())?,
                json!(strength)
            );
            assert!(client.into_transport().exchanges.is_empty());
        }
        Ok(())
    }

    #[test]
    fn optional_haptic_preview_failure_keeps_verified_strength_successful() -> Result<()> {
        for fail_play in [false, true] {
            let mut exchanges = vec![
                (wire(12, 1, &[0, 0, 0]), wire(12, 1, &[1, 50, 0x54])),
                (wire(12, 2, &[1, 75, 0]), wire(12, 2, &[])),
                (wire(12, 1, &[0, 0, 0]), wire(12, 1, &[1, 75, 0x54])),
            ];
            if fail_play {
                exchanges.extend([
                    (
                        wire(12, 0, &[0, 0, 0]),
                        wire(12, 0, &[0, 0, 0, 0, 0, 0, 0, 2]),
                    ),
                    (wire(12, 4, &[1, 0, 0]), Vec::new()),
                ]);
            } else {
                exchanges.push((wire(12, 0, &[0, 0, 0]), Vec::new()));
            }
            let mut client = Client::new(Scripted::new(exchanges));
            let mut session = Session::new(&mut client, 1);
            session.features.insert(0x19b0, Some((12, 0)));
            assert_eq!(
                set_with_session(&mut session, "haptic-strength", "75")?,
                json!(75)
            );
            assert!(client.into_transport().exchanges.is_empty());
        }
        Ok(())
    }

    #[test]
    fn haptic_strength_rejects_incorrect_readback() {
        for (enabled, readback) in [(1, [1, 50, 0x54]), (0, [1, 75, 0x54]), (1, [0, 75, 0x54])] {
            let script = Scripted::new(VecDeque::from([
                (wire(12, 1, &[0, 0, 0]), wire(12, 1, &[enabled, 50, 0x54])),
                (wire(12, 2, &[enabled, 75, 0]), wire(12, 2, &[])),
                (wire(12, 1, &[0, 0, 0]), wire(12, 1, &readback)),
            ]));
            let mut client = Client::new(script);
            let mut session = Session::new(&mut client, 1);
            session.features.insert(0x19b0, Some((12, 0)));
            let error = set_with_session(&mut session, "haptic-strength", "75")
                .expect_err("mismatched strength or enable state must fail verification");
            assert!(
                error.to_string().contains("not verified"),
                "{enabled}: {readback:?}"
            );
            assert!(client.into_transport().exchanges.is_empty());
        }
    }

    #[test]
    fn haptic_strength_rejects_invalid_values_before_sending_requests() {
        for value in ["-1", "101", "256", "50.5"] {
            let mut client = Client::new(Scripted::new([]));
            let mut session = Session::new(&mut client, 1);
            session.features.insert(0x19b0, Some((12, 0)));
            assert!(set_with_session(&mut session, "haptic-strength", value).is_err());
        }
    }

    #[test]
    fn haptic_strength_rejects_unknown_configuration_before_writing() {
        for config in [[2, 50, 0x54], [1, 101, 0x54]] {
            let mut client = Client::new(Scripted::new([(
                wire(12, 1, &[0, 0, 0]),
                wire(12, 1, &config),
            )]));
            let mut session = Session::new(&mut client, 1);
            session.features.insert(0x19b0, Some((12, 0)));
            assert!(set_with_session(&mut session, "haptic-strength", "75").is_err());
            assert!(client.into_transport().exchanges.is_empty());
        }
    }

    #[test]
    fn details_and_settings_share_feature_discovery_and_refresh_capabilities() -> Result<()> {
        for (haptic, thumb_version) in [(false, None), (true, Some(0)), (false, Some(1))] {
            let mut exchanges: Vec<_> = FEATURES
                .iter()
                .map(|feature| {
                    let (index, version) = match *feature {
                        0x19b0 if haptic => (12, 0),
                        0x2150 => thumb_version.map_or((0, 0), |version| (19, version)),
                        _ => (0, 0),
                    };
                    (
                        wire(0, 0, &feature.to_be_bytes()),
                        wire(0, 0, &[index, 0, version]),
                    )
                })
                .collect();
            if thumb_version == Some(0) {
                exchanges.push((wire(19, 1, &[]), wire(19, 1, &[0, 7])));
            }
            if haptic {
                exchanges.push((wire(12, 1, &[0, 0, 0]), wire(12, 1, &[1, 50, 0x54])));
            }
            let mut client = Client::new(Scripted::new(exchanges));
            let mut session = Session::new(&mut client, 1);
            let mut device = test_device();
            device.capabilities = vec!["fn-lock".into(), "hidpp-short-reports".into()];
            enrich_session(&mut session, &mut device, Discovery::Details)?;
            assert!(!device.capabilities.iter().any(|cap| cap == "fn-lock"));
            assert!(
                device
                    .capabilities
                    .iter()
                    .any(|cap| cap == "hidpp-short-reports")
            );
            let settings = read_settings(&mut session)?;
            assert_eq!(
                settings.get("haptic-strength"),
                haptic.then_some(&json!(50))
            );
            assert_eq!(
                device.capabilities.contains(&"thumb-wheel".into()),
                thumb_version == Some(0)
            );
            assert_eq!(
                settings.get("thumb-wheel-invert"),
                (thumb_version == Some(0)).then_some(&json!(true))
            );
            assert!(client.into_transport().exchanges.is_empty());
        }
        Ok(())
    }

    #[test]
    fn mechanical_backlight_toggle_preserves_configuration_fields() -> Result<()> {
        let current = [1, 0x1d, 0x38, 0xff, 0, 4, 3, 0, 7, 0, 12, 0];
        let mut disabled = current;
        disabled[0] = 0;
        let script = Scripted::new(VecDeque::from([
            (wire(11, 0, &[]), wire(11, 0, &current)),
            (
                wire(11, 1, &[0, 0x1d, 0xff, 4, 3, 0, 7, 0, 12, 0]),
                wire(11, 1, &[]),
            ),
            (wire(11, 0, &[]), wire(11, 0, &disabled)),
        ]));
        let mut client = Client::new(script);
        let mut session = Session::new(&mut client, 1);
        session.features.insert(0x1982, Some((11, 2)));
        assert_eq!(
            set_with_session(&mut session, "backlight", "off")?,
            json!(false)
        );
        Ok(())
    }

    #[test]
    fn scroll_inversion_preserves_resolution_and_report_target() -> Result<()> {
        let script = Scripted::new(VecDeque::from([
            (wire(8, 1, &[]), wire(8, 1, &[2])),
            (wire(8, 0, &[]), wire(8, 0, &[120, 8, 24])),
            (wire(8, 2, &[6]), wire(8, 2, &[6])),
            (wire(8, 1, &[]), wire(8, 1, &[6])),
        ]));
        let mut client = Client::new(script);
        let mut session = Session::new(&mut client, 1);
        session.features.insert(0x2121, Some((8, 1)));
        assert_eq!(
            set_with_session(&mut session, "scroll-invert", "on")?,
            json!(true)
        );
        Ok(())
    }

    #[test]
    fn thumb_wheel_inversion_preserves_reporting_mode_without_touch_flags() -> Result<()> {
        for (mode, enabled) in [(0, false), (0, true), (1, false), (1, true)] {
            let invert = u8::from(enabled);
            let mut client = Client::new(Scripted::new([
                (wire(19, 1, &[]), wire(19, 1, &[mode, 6 | (1 - invert)])),
                (wire(19, 2, &[mode, invert, 0]), wire(19, 2, &[])),
                (wire(19, 1, &[]), wire(19, 1, &[mode, invert | 2])),
                (wire(19, 1, &[]), wire(19, 1, &[mode, invert | 4])),
            ]));
            let mut session = Session::new(&mut client, 1);
            session.features.insert(0x2150, Some((19, 0)));
            assert_eq!(
                set_with_session(
                    &mut session,
                    "thumb-wheel-invert",
                    if enabled { "on" } else { "off" }
                )?,
                json!(enabled)
            );
            assert_eq!(
                read_one(&mut session, "thumb-wheel-invert")?,
                json!(enabled)
            );
            assert!(client.into_transport().exchanges.is_empty());
        }
        Ok(())
    }

    #[test]
    fn thumb_wheel_inversion_rejects_changed_mode_or_unverified_direction() {
        for readback in [Some([0, 0]), Some([1, 1]), Some([2, 1]), None] {
            let mut client = Client::new(Scripted::new([
                (wire(19, 1, &[]), wire(19, 1, &[0, 0])),
                (wire(19, 2, &[0, 1, 0]), wire(19, 2, &[])),
                (
                    wire(19, 1, &[]),
                    readback.map_or_else(Vec::new, |data| wire(19, 1, &data)),
                ),
            ]));
            let mut session = Session::new(&mut client, 1);
            session.features.insert(0x2150, Some((19, 0)));
            assert!(set_with_session(&mut session, "thumb-wheel-invert", "on").is_err());
            assert!(client.into_transport().exchanges.is_empty());
        }
    }

    #[test]
    fn thumb_wheel_inversion_rejects_unsupported_state_before_writing() {
        for feature in [None, Some((19, 1))] {
            let mut client = Client::new(Scripted::new([]));
            let mut session = Session::new(&mut client, 1);
            session.features.insert(0x2150, feature);
            assert!(read_one(&mut session, "thumb-wheel-invert").is_err());
            assert!(set_with_session(&mut session, "thumb-wheel-invert", "on").is_err());
        }
        let mut client = Client::new(Scripted::new([(wire(19, 1, &[]), wire(19, 1, &[2, 0]))]));
        let mut session = Session::new(&mut client, 1);
        session.features.insert(0x2150, Some((19, 0)));
        assert!(set_with_session(&mut session, "thumb-wheel-invert", "invalid").is_err());
        assert!(set_with_session(&mut session, "thumb-wheel-invert", "on").is_err());
        assert!(client.into_transport().exchanges.is_empty());
    }

    #[test]
    fn details_and_settings_share_scroll_capabilities_but_read_current_mode() -> Result<()> {
        for inversion in [false, true] {
            let mut exchanges = VecDeque::from([(
                wire(8, 0, &[]),
                wire(8, 0, &[0, if inversion { 8 } else { 0 }]),
            )]);
            if inversion {
                exchanges.extend([
                    (wire(8, 1, &[]), wire(8, 1, &[4])),
                    (wire(8, 1, &[]), wire(8, 1, &[0])),
                ]);
            }
            let mut client = Client::new(Scripted::new(exchanges));
            let mut session = Session::new(&mut client, 1);
            session
                .features
                .extend(FEATURES.iter().map(|&id| (id, None)));
            session.features.insert(0x2121, Some((8, 1)));
            let mut device = test_device();
            enrich_session(&mut session, &mut device, Discovery::Details)?;
            assert_eq!(
                device.capabilities.contains(&"scroll-invert".into()),
                inversion
            );
            let settings = read_settings(&mut session)?;
            assert_eq!(
                settings.get("scroll-invert"),
                inversion.then_some(&json!(true))
            );
            if inversion {
                assert_eq!(read_one(&mut session, "scroll-invert")?, json!(false));
            }
            assert!(client.into_transport().exchanges.is_empty());
        }
        Ok(())
    }

    #[test]
    fn scroll_capability_errors_are_retried_before_caching_success() -> Result<()> {
        let mut client = Client::new(Scripted::new([
            (wire(8, 0, &[]), packet(1, 0xff, 8, &[0x0b, 8], true)),
            (wire(8, 0, &[]), wire(8, 0, &[120, 8, 24])),
        ]));
        let mut session = Session::new(&mut client, 1);
        session.features.insert(0x2121, Some((8, 1)));
        assert!(session.supports_scroll_inversion().is_err());
        assert!(session.supports_scroll_inversion()?);
        assert!(session.supports_scroll_inversion()?);
        assert!(client.into_transport().exchanges.is_empty());
        Ok(())
    }

    #[test]
    fn missing_features_are_not_queried_again_within_one_session() -> Result<()> {
        let mut client = Client::new(Scripted::new(VecDeque::from([
            (wire(0, 0, &[0x21, 0x11]), wire(0, 0, &[0, 0, 0])),
            (wire(0, 0, &[0x21, 0x10]), wire(0, 0, &[6, 0, 1])),
        ])));
        let mut session = Session::new(&mut client, 1);
        assert!(!session.supports(0x2111)?);
        assert_eq!(session.smartshift()?, Some((0x2110, 0)));
        assert_eq!(session.smartshift()?, Some((0x2110, 0)));
        assert!(client.into_transport().exchanges.is_empty());
        Ok(())
    }

    #[test]
    fn descriptor_parser_does_not_mistake_usage_payload_for_report_id() {
        assert!(hidpp_descriptor(&[0x06, 0, 0xff, 0x85, 0x11]));
        assert!(!hidpp_descriptor(&[0x0a, 0x85, 0x11]));
        assert!(!hidpp_descriptor(&[0x85]));
        assert!(!hidpp_descriptor(&[0xfe, 0xff]));
        assert!(hidpp_descriptor(&[0x06, 0, 0xff, 0x85, 0x10]));
        assert!(!descriptor_has_report(&[0x06, 0, 0xff, 0x85, 0x10], 0x11));
    }

    #[test]
    fn battery_capabilities_prevent_fake_zero_percent() {
        let coarse = unified_battery(&[15, 1], &[0, 4, 0]).expect("valid test fixture");
        assert_eq!(coarse.percent, None);
        assert_eq!(coarse.level.as_deref(), Some("good"));
        assert_eq!(
            unified_battery(&[15, 3], &[0, 1, 0])
                .expect("valid test fixture")
                .percent,
            Some(0)
        );
        assert_eq!(
            unified_battery(&[15, 3], &[255, 0, 4])
                .expect("valid test fixture")
                .percent,
            None
        );
    }

    #[test]
    fn battery_levels_respect_capabilities_and_bitmask_reporting() {
        let percent_only = unified_battery(&[0, 3], &[75, 8, 0]).expect("valid test fixture");
        assert_eq!(percent_only.percent, Some(75));
        assert_eq!(percent_only.level, None);

        let unsupported = unified_battery(&[3, 1], &[0, 8, 0]).expect("valid test fixture");
        assert_eq!(unsupported.level, None);

        let cumulative = unified_battery(&[15, 1], &[0, 7, 0]).expect("valid test fixture");
        assert_eq!(cumulative.level.as_deref(), Some("good"));

        let masked = unified_battery(&[3, 1], &[0, 15, 0]).expect("valid test fixture");
        assert_eq!(masked.level.as_deref(), Some("low"));
    }

    #[test]
    fn dpi_list_handles_ranges_and_rejects_broken_descriptions() {
        assert_eq!(
            dpi_values(&[0, 1, 0x90, 0xe0, 100, 3, 0x20, 0, 0]).expect("valid test fixture"),
            vec![400, 500, 600, 700, 800]
        );
        assert_eq!(
            dpi_values(&[0, 1, 0x90, 3, 0x20, 0, 0]).expect("valid test fixture"),
            vec![400, 800]
        );
        assert!(dpi_values(&[0, 1, 0x90, 0xe0, 0, 3, 0x20]).is_err());
        assert!(dpi_values(&[0, 1, 0x90, 0xe0, 100]).is_err());
        assert!(dpi_values(&[0, 0xe0, 100, 3, 0x20]).is_err());
    }

    #[test]
    fn bluetooth_identity_is_normalized_and_no_hidraw_path_is_persisted() {
        assert_eq!(
            address("aa:bb:cc:dd:ee:ff").as_deref(),
            Some("AA:BB:CC:DD:EE:FF")
        );
        assert!(address("not an address").is_none());
        let node = Node {
            path: "/dev/hidraw9".into(),
            sys: "/sys/devices/test".into(),
            name: "mouse".into(),
            product: 0xabcd,
            bus: 3,
            unique: "serial".into(),
            physical: "usb-port".into(),
            usb: None,
            has_hidpp: true,
            short_reports: true,
            long_reports: true,
            kernel_slot: None,
        };
        assert_eq!(
            fallback_identity(&node, false),
            ("usb:abcd:serial".into(), false)
        );
        assert!(!fallback_identity(&node, false).0.contains("hidraw"));
    }
}
