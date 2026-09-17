//! BlueZ integration. Protocol references:
//! <https://bluez.readthedocs.io/en/latest/device-api/>
//! <https://bluez.readthedocs.io/en/latest/adapter-api/>
//! <https://bluez.readthedocs.io/en/latest/agent-api/>

use crate::model::{Battery, Device, DeviceState, Inventory, Transport};
use crate::terminal::{Choice, PairingUi};
use anyhow::{Context, Result, anyhow, ensure};
use std::{collections::HashMap, future::Future, time::Duration};
use tokio::{
    sync::{Mutex, watch},
    time::timeout,
};
use zbus::{
    Connection, Proxy,
    fdo::{DBusProxy, ManagedObjects, ObjectManagerProxy},
    message::Header,
    zvariant::{ObjectPath, OwnedObjectPath, OwnedValue},
};

const DEVICE: &str = "org.bluez.Device1";
const ADAPTER: &str = "org.bluez.Adapter1";
const BATTERY: &str = "org.bluez.Battery1";
const AGENT_PATH: &str = "/org/logishell/agent";
const CALL_TIMEOUT: Duration = Duration::from_secs(10);
const HID_UUIDS: [&str; 2] = [
    "00001124-0000-1000-8000-00805f9b34fb",
    "00001812-0000-1000-8000-00805f9b34fb",
];
type Properties = HashMap<String, OwnedValue>;

async fn bus() -> Result<Connection> {
    timeout(CALL_TIMEOUT, Connection::system())
        .await.context("timed out connecting to the system D-Bus")?
        .context("cannot reach the system D-Bus; Bluetooth requires the system bus and bluetooth.service")
}

async fn objects(connection: &Connection) -> Result<ManagedObjects> {
    let proxy = ObjectManagerProxy::builder(connection)
        .destination("org.bluez")?
        .path("/")?
        .build()
        .await?;
    timeout(CALL_TIMEOUT, proxy.get_managed_objects()).await
        .context("BlueZ did not respond within 10 seconds")?
        .context("cannot read BlueZ devices; check `systemctl status bluetooth.service` and D-Bus access")
}

async fn proxy<'a>(
    connection: &'a Connection,
    path: &'a str,
    interface: &'a str,
) -> Result<Proxy<'a>> {
    Ok(Proxy::new(connection, "org.bluez", path, interface).await?)
}

/// Ordinary BlueZ methods share one deadline; pairing and connection attempts
/// retain their longer, cancellable deadlines at the call site.
async fn call(
    proxy: &Proxy<'_>,
    method: &str,
    body: &(impl serde::Serialize + zbus::zvariant::DynamicType),
) -> Result<()> {
    timeout(CALL_TIMEOUT, proxy.call::<_, _, ()>(method, body))
        .await
        .with_context(|| format!("Bluetooth {method} timed out"))?
        .with_context(|| format!("Bluetooth {method} failed"))
}

fn property<'a, T: TryFrom<&'a OwnedValue>>(properties: &'a Properties, name: &str) -> Option<T> {
    properties
        .get(name)
        .and_then(|value| T::try_from(value).ok())
}

/// Vendor namespaces differ: USB 046d is Logitech, Bluetooth SIG 046d is not.
fn logitech_modalias(modalias: &str) -> bool {
    let value = modalias.to_ascii_lowercase();
    value.starts_with("usb:v046dp") || value.starts_with("bluetooth:v01dap")
}

fn logitech_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    let words: Vec<_> = name
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    words
        .iter()
        .any(|word| matches!(*word, "logitech" | "logi"))
        || matches!(
            words.as_slice(),
            [
                "mx",
                "master" | "mechanical" | "mchncl" | "anywhere" | "keys" | "vertical" | "ergo",
                ..
            ] | ["ergo", "k860", ..]
                | ["pop", "keys" | "mouse" | "icon", ..]
                | ["pebble", "keys" | "mouse" | "2", ..]
                | ["wave", "keys", ..]
                | ["signature", ..]
                | ["lift", ..]
                | ["keys", "to", "go", ..]
                | ["k380" | "m720" | "m535", ..]
        )
}

/// Some devices omit their Device ID until paired. Such candidates remain explicitly uncertain.
fn identification(properties: &Properties) -> Option<bool> {
    let icon = property::<&str>(properties, "Icon").unwrap_or_default();
    let appearance = property::<u16>(properties, "Appearance");
    let class = property::<u32>(properties, "Class");
    // Headsets can also advertise HID for media buttons. That does not make them mice.
    let remote_name = property::<&str>(properties, "Name")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let excluded_name = remote_name
        .split(|character: char| !character.is_ascii_alphanumeric())
        .any(|word| {
            matches!(
                word,
                "headset"
                    | "headphones"
                    | "earbuds"
                    | "speaker"
                    | "gamepad"
                    | "joystick"
                    | "webcam"
            )
        });
    if excluded_name
        || icon.starts_with("audio-")
        || matches!(icon, "input-gaming" | "input-tablet")
        || matches!(appearance, Some(0x03c3..=0x03c9))
        || class.is_some_and(|value| value & 0x1f00 == 0x0400)
    {
        return None;
    }
    let keyboard_or_mouse = matches!(icon, "input-keyboard" | "input-mouse")
        || matches!(appearance, Some(0x03c1 | 0x03c2))
        || class.is_some_and(|value| value & 0x1f00 == 0x0500 && value & 0xc0 != 0);
    let input = keyboard_or_mouse
        || property::<&zbus::zvariant::Array<'_>>(properties, "UUIDs").is_some_and(|uuids| {
            uuids
                .inner()
                .iter()
                .filter_map(|value| <&str>::try_from(value).ok())
                .any(|uuid| HID_UUIDS.iter().any(|hid| uuid.eq_ignore_ascii_case(hid)))
        });
    if !input {
        return None;
    }
    // A declared non-Logitech vendor must never be overridden by a friendly alias.
    if let Some(modalias) =
        property::<&str>(properties, "Modalias").filter(|value| !value.is_empty())
    {
        return logitech_modalias(modalias).then_some(false);
    }
    logitech_name(property::<&str>(properties, "Name").unwrap_or_default()).then_some(true)
}

fn clean_label(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(160)
        .collect()
}

fn normalize_address(address: &str) -> Result<String> {
    let address = address.strip_prefix("bt:").unwrap_or(address);
    let parts: Vec<_> = address.split(':').collect();
    ensure!(
        parts.len() == 6
            && parts
                .iter()
                .all(|part| part.len() == 2 && part.bytes().all(|byte| byte.is_ascii_hexdigit())),
        "invalid Bluetooth address; use AA:BB:CC:DD:EE:FF or bt:AA:BB:CC:DD:EE:FF"
    );
    Ok(address.to_ascii_uppercase())
}

fn inventory_from(objects: &ManagedObjects) -> Inventory {
    let mut inventory = Inventory::default();
    for interfaces in objects.values() {
        let Some(properties) = interfaces.get(DEVICE) else {
            continue;
        };
        let Some(uncertain) = identification(properties) else {
            continue;
        };
        let Some(address) =
            property::<&str>(properties, "Address").and_then(|value| normalize_address(value).ok())
        else {
            continue;
        };
        let online = property::<bool>(properties, "Connected");
        let mut warnings = Vec::new();
        if uncertain {
            warnings.push("Logitech identity inferred from its advertised name and input services; vendor ID is unavailable".into());
        }
        if property::<bool>(properties, "Paired") != Some(true) {
            warnings.push("not paired; Bluetooth identity may change after pairing".into());
        }
        let battery = interfaces
            .get(BATTERY)
            .and_then(|battery| property::<u8>(battery, "Percentage"))
            .filter(|value| *value <= 100)
            .map(|percent| Battery {
                percent: Some(percent),
                stale: online != Some(true),
                ..Battery::default()
            });
        let remote_name =
            property::<&str>(properties, "Name").filter(|value| !value.trim().is_empty());
        if remote_name.is_none() {
            warnings.push("remote device name is unavailable; identification uses its vendor ID and input services".into());
        }
        // Alias is writable by the user. Preserve the advertised device identity.
        let name = clean_label(
            remote_name
                .or_else(|| property::<&str>(properties, "Alias"))
                .unwrap_or("Logitech Bluetooth input device"),
        );
        inventory.devices.push(Device {
            id: format!("bt:{address}"),
            name,
            transport: Transport::Bluetooth,
            state: match online {
                Some(true) => DeviceState::Online,
                Some(false) => DeviceState::Offline,
                None => DeviceState::Unknown,
            },
            battery,
            receiver_id: None,
            slot: None,
            hid_path: None,
            bluetooth_address: Some(address),
            firmware: None,
            capabilities: vec![
                "bluetooth-connect".into(),
                "bluetooth-disconnect".into(),
                "bluetooth-pair".into(),
            ],
            warnings,
        });
    }
    inventory
        .devices
        .sort_by(|left, right| left.id.cmp(&right.id));
    // BlueZ may know one peripheral through multiple adapters. Mutating operations reject
    // that ambiguity below; inventory keeps one logical identity and explains it.
    inventory.devices.dedup_by(|next, prior| {
        if next.id != prior.id { return false; }
        prior.warnings.push("known through multiple Bluetooth adapters; remove the unused adapter record before changing its connection".into());
        if next.state == DeviceState::Online { prior.state = next.state; prior.battery = next.battery.clone(); }
        true
    });
    inventory
}

pub async fn inventory() -> Result<Inventory> {
    let connection = bus().await?;
    Ok(inventory_from(&objects(&connection).await?))
}

/// A dedicated connection ensures dropping a canceled scan also releases its BlueZ session.
struct Discovery {
    connection: Connection,
    started: Vec<String>,
}

impl Discovery {
    async fn stop(&mut self) -> Vec<String> {
        let mut warnings = Vec::new();
        while let Some(path) = self.started.last().cloned() {
            if let Err(error) = async {
                let adapter = proxy(&self.connection, &path, ADAPTER).await?;
                call(&adapter, "StopDiscovery", &()).await
            }
            .await
            {
                warnings.push(format!(
                    "could not release Bluetooth discovery on {path}: {error}"
                ));
            }
            // Keep this path in the guard while StopDiscovery is in flight. Cancellation
            // during cleanup must still release it and all remaining caller-owned sessions.
            self.started.pop();
        }
        warnings
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        if self.started.is_empty() {
            return;
        }
        let connection = self.connection.clone();
        let paths = std::mem::take(&mut self.started);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                for path in paths {
                    if let Ok(adapter) = proxy(&connection, &path, ADAPTER).await {
                        let _ = call(&adapter, "StopDiscovery", &()).await;
                    }
                }
                let _ = connection.close().await;
            });
        }
    }
}

async fn scan_with_ui(timeout_secs: u64, ui: &PairingUi) -> Result<Inventory> {
    ensure!(
        (1..=300).contains(&timeout_secs),
        "Bluetooth scan timeout must be between 1 and 300 seconds"
    );
    ensure!(!ui.cancelled(), "Bluetooth scan canceled");
    let connection = bus().await?;
    let current = objects(&connection).await?;
    let mut discovery = Discovery {
        connection,
        started: Vec::new(),
    };
    let mut paths: Vec<_> = current
        .iter()
        .filter_map(|(path, interfaces)| {
            interfaces
                .get(ADAPTER)
                .filter(|properties| property::<bool>(properties, "Powered") == Some(true))
                .map(|_| path.to_string())
        })
        .collect();
    paths.sort();
    ensure!(
        !paths.is_empty(),
        "no powered Bluetooth adapter; enable Bluetooth before scanning"
    );
    let outcome = tokio::select! {
        biased;
        _ = ui.cancelled_signal() => Err(anyhow!("Bluetooth scan canceled")),
        signal = tokio::signal::ctrl_c() => {
            signal.context("cannot listen for cancellation")
                .and_then(|_| Err(anyhow!("Bluetooth scan canceled")))
        },
        result = async {
            for path in paths {
                // Record before awaiting so cancellation while StartDiscovery is in flight also cleans up.
                discovery.started.push(path.clone());
                let adapter = proxy(&discovery.connection, &path, ADAPTER).await?;
                call(&adapter, "StartDiscovery", &()).await?;
            }
            tokio::time::sleep(Duration::from_secs(timeout_secs)).await;
            Ok::<_, anyhow::Error>(inventory_from(&objects(&discovery.connection).await?))
        } => result,
    };
    // Cancel only the scan work above. Keep ownership until BlueZ discovery has
    // stopped and its connection has closed before returning to the setup menu.
    let warnings = discovery.stop().await;
    // Closing the private bus connection also releases any session whose StopDiscovery failed.
    let _ = discovery.connection.clone().close().await;
    let mut inventory = outcome?;
    inventory.warnings.extend(warnings);
    Ok(inventory)
}

fn resolve(objects: &ManagedObjects, address: &str) -> Result<OwnedObjectPath> {
    let address = normalize_address(address)?;
    let mut matches = objects.iter().filter_map(|(path, interfaces)| {
        let properties = interfaces.get(DEVICE)?;
        let found = normalize_address(property::<&str>(properties, "Address")?).ok()?;
        (found == address && identification(properties).is_some()).then_some(path.clone())
    });
    let path = matches.next().ok_or_else(|| {
        anyhow!("{address} is not known as a Logitech input device; pair it through setup")
    })?;
    ensure!(
        matches.next().is_none(),
        "{address} is known on multiple Bluetooth adapters; remove its unused adapter record before continuing"
    );
    Ok(path)
}

fn device_address(device: &Device) -> Result<String> {
    ensure!(
        device.transport == Transport::Bluetooth,
        "connect/disconnect applies to Bluetooth devices; receiver devices reconnect automatically"
    );
    normalize_address(device.bluetooth_address.as_deref().unwrap_or(&device.id))
}

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum AgentError {
    Rejected(String),
    Canceled(String),
    #[zbus(error)]
    Bus(zbus::Error),
}

struct TerminalAgent {
    target: OwnedObjectPath,
    bluez_owner: String,
    canceled: watch::Sender<bool>,
    prompt_cancel: watch::Sender<u64>,
    prompt_lock: Mutex<()>,
    ui: PairingUi,
}

impl TerminalAgent {
    fn validate_sender(&self, header: &Header<'_>) -> std::result::Result<(), AgentError> {
        if header
            .sender()
            .is_some_and(|sender| sender.as_str() == self.bluez_owner)
        {
            Ok(())
        } else {
            Err(AgentError::Rejected(
                "only BlueZ may call this pairing agent".into(),
            ))
        }
    }

    fn validate(
        &self,
        device: &ObjectPath<'_>,
        header: &Header<'_>,
    ) -> std::result::Result<(), AgentError> {
        self.validate_sender(header)?;
        if device.as_str() != self.target.as_str() {
            return Err(AgentError::Rejected(
                "device does not match this pairing session".into(),
            ));
        }
        if *self.canceled.borrow() || self.ui.cancelled() {
            return Err(AgentError::Canceled("pairing canceled".into()));
        }
        Ok(())
    }

    async fn guarded_prompt<T>(
        &self,
        prompt: impl Future<Output = std::result::Result<T, AgentError>>,
    ) -> std::result::Result<T, AgentError> {
        let mut canceled = self.canceled.subscribe();
        let mut prompt_cancel = self.prompt_cancel.subscribe();
        let prompt_epoch = *prompt_cancel.borrow();
        if *canceled.borrow() || self.ui.cancelled() {
            return Err(AgentError::Canceled("pairing canceled".into()));
        }
        tokio::select! {
            biased;
            _ = canceled.changed() => Err(AgentError::Canceled("pairing canceled".into())),
            _ = prompt_cancel.changed() => Err(AgentError::Canceled("authentication prompt canceled".into())),
            _ = self.ui.cancelled_signal() => Err(AgentError::Canceled("pairing canceled".into())),
            result = async {
                let _lock = self.prompt_lock.lock().await;
                prompt.await
            } => {
                if *canceled.borrow() || self.ui.cancelled() || *prompt_cancel.borrow() != prompt_epoch {
                    Err(AgentError::Canceled("authentication prompt canceled".into()))
                } else {
                    result
                }
            },
        }
    }

    async fn prompt(&self, message: &str) -> std::result::Result<String, AgentError> {
        self.guarded_prompt(async {
            self.ui
                .input("Bluetooth authentication", message)
                .await
                .map_err(|error| AgentError::Rejected(error.to_string()))?
                .ok_or_else(|| AgentError::Canceled("pairing canceled".into()))
        })
        .await
    }

    async fn confirm(&self, message: &str) -> std::result::Result<(), AgentError> {
        match self
            .guarded_prompt(async {
                self.ui
                    .choose(
                        "Confirm Bluetooth pairing",
                        message,
                        vec![
                            Choice {
                                label: "Decline".into(),
                                detail: String::new(),
                            },
                            Choice {
                                label: "Allow".into(),
                                detail: String::new(),
                            },
                        ],
                    )
                    .await
                    .map_err(|error| AgentError::Rejected(error.to_string()))
            })
            .await?
        {
            Some(1) => Ok(()),
            None => Err(AgentError::Canceled("pairing canceled".into())),
            Some(_) => Err(AgentError::Rejected("confirmation declined".into())),
        }
    }

    fn display(&self, message: &str) -> std::result::Result<(), AgentError> {
        self.ui
            .status("Bluetooth authentication", message)
            .map_err(|error| AgentError::Rejected(error.to_string()))
    }
}

#[zbus::interface(name = "org.bluez.Agent1")]
impl TerminalAgent {
    fn release(&self, #[zbus(header)] header: Header<'_>) -> std::result::Result<(), AgentError> {
        self.validate_sender(&header)?;
        self.canceled.send_replace(true);
        self.ui
            .dismiss()
            .map_err(|error| AgentError::Rejected(error.to_string()))?;
        Ok(())
    }

    async fn request_pin_code(
        &self,
        device: ObjectPath<'_>,
        #[zbus(header)] header: Header<'_>,
    ) -> std::result::Result<String, AgentError> {
        self.validate(&device, &header)?;
        let pin = self.prompt("Bluetooth PIN: ").await?;
        if pin.is_empty()
            || pin.len() > 16
            || !pin
                .chars()
                .all(|character| character.is_ascii_alphanumeric())
        {
            return Err(AgentError::Rejected(
                "PIN must contain 1–16 ASCII letters or digits".into(),
            ));
        }
        Ok(pin)
    }

    fn display_pin_code(
        &self,
        device: ObjectPath<'_>,
        pincode: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> std::result::Result<(), AgentError> {
        self.validate(&device, &header)?;
        self.display(&format!(
            "Type PIN {} on the Bluetooth keyboard, then press Enter.",
            clean_label(pincode)
        ))
    }

    async fn request_passkey(
        &self,
        device: ObjectPath<'_>,
        #[zbus(header)] header: Header<'_>,
    ) -> std::result::Result<u32, AgentError> {
        self.validate(&device, &header)?;
        let value = self.prompt("Bluetooth passkey (0–999999): ").await?;
        value
            .parse::<u32>()
            .ok()
            .filter(|value| *value <= 999_999)
            .ok_or_else(|| AgentError::Rejected("passkey must be a number from 0 to 999999".into()))
    }

    fn display_passkey(
        &self,
        device: ObjectPath<'_>,
        passkey: u32,
        entered: u16,
        #[zbus(header)] header: Header<'_>,
    ) -> std::result::Result<(), AgentError> {
        self.validate(&device, &header)?;
        self.display(&format!(
            "Type {passkey:06} on the Bluetooth keyboard, then press Enter ({entered} digits entered)."
        ))
    }

    async fn request_confirmation(
        &self,
        device: ObjectPath<'_>,
        passkey: u32,
        #[zbus(header)] header: Header<'_>,
    ) -> std::result::Result<(), AgentError> {
        self.validate(&device, &header)?;
        self.confirm(&format!("Does the device show {passkey:06}?"))
            .await
    }

    async fn request_authorization(
        &self,
        device: ObjectPath<'_>,
        #[zbus(header)] header: Header<'_>,
    ) -> std::result::Result<(), AgentError> {
        self.validate(&device, &header)?;
        self.confirm("Allow pairing with the selected Bluetooth device?")
            .await
    }

    async fn authorize_service(
        &self,
        device: ObjectPath<'_>,
        uuid: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> std::result::Result<(), AgentError> {
        self.validate(&device, &header)?;
        if HID_UUIDS.iter().any(|hid| uuid.eq_ignore_ascii_case(hid)) {
            return Ok(());
        }
        self.confirm(&format!(
            "Allow Bluetooth service {} for this device?",
            clean_label(uuid)
        ))
        .await
    }

    fn cancel(&self, #[zbus(header)] header: Header<'_>) -> std::result::Result<(), AgentError> {
        self.validate_sender(&header)?;
        // BlueZ also uses Cancel to dismiss DisplayPasskey after the keyboard has entered
        // it. End the current prompt, not the whole session: service authorization may follow.
        self.prompt_cancel
            .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
        self.ui
            .dismiss()
            .map_err(|error| AgentError::Rejected(error.to_string()))?;
        Ok(())
    }
}

// Keeps cancellation safe even when the caller drops the entire pairing future.
struct PairingCleanup {
    connection: Option<Connection>,
    target: OwnedObjectPath,
    canceled: watch::Sender<bool>,
    pair_pending: bool,
}

async fn cleanup_pairing(connection: Connection, target: OwnedObjectPath, cancel_pending: bool) {
    if cancel_pending && let Ok(device) = proxy(&connection, target.as_str(), DEVICE).await {
        let _ = call(&device, "CancelPairing", &()).await;
    }
    if let Ok(manager) = proxy(&connection, "/org/bluez", "org.bluez.AgentManager1").await
        && let Ok(path) = ObjectPath::try_from(AGENT_PATH)
    {
        let _ = call(&manager, "UnregisterAgent", &(path,)).await;
    }
    let _ = connection
        .object_server()
        .remove::<TerminalAgent, _>(AGENT_PATH)
        .await;
    // Name-owner loss also releases the application-local agent if BlueZ stopped responding.
    let _ = connection.close().await;
}

impl PairingCleanup {
    async fn finish(&mut self) {
        self.canceled.send_replace(true);
        if let Some(connection) = self.connection.as_ref().cloned() {
            cleanup_pairing(connection, self.target.clone(), self.pair_pending).await;
            self.connection = None;
        }
    }
}

impl Drop for PairingCleanup {
    fn drop(&mut self) {
        self.canceled.send_replace(true);
        if let Some(connection) = self.connection.take() {
            let target = self.target.clone();
            let cancel_pending = self.pair_pending;
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(cleanup_pairing(connection, target, cancel_pending));
            }
        }
    }
}

/// Grant reconnect permission only after this invocation has successfully paired its target.
async fn complete_pairing<V, T, F>(verify_paired: V, trust: F) -> Result<()>
where
    V: Future<Output = Result<bool>>,
    T: Future<Output = Result<()>>,
    F: FnOnce() -> T,
{
    let paired = verify_paired.await.context(
        "pairing succeeded, but verifying its paired state failed; device trust was not changed",
    )?;
    ensure!(
        paired,
        "pairing reported success, but BlueZ has not confirmed its paired state; device trust was not changed"
    );
    trust().await.context("pairing succeeded, but automatic trust setup failed; the device remains paired and may need authorization to reconnect")
}

pub async fn pair_with_ui(timeout_secs: u64, ui: PairingUi) -> Result<()> {
    ensure!(
        (1..=300).contains(&timeout_secs),
        "pairing timeout must be between 1 and 300 seconds"
    );
    ensure!(!ui.cancelled(), "Bluetooth pairing canceled");
    ui.status(
        "Find a Bluetooth device",
        "Put the device in Bluetooth pairing mode. Hold its Easy-Switch button until the light blinks. Scanning…",
    )?;
    let inventory = scan_with_ui(timeout_secs.min(10), &ui).await?;
    ensure!(
        !inventory.devices.is_empty(),
        "no Logitech input devices found; enable pairing mode and try again"
    );
    let index = ui
        .choose(
            "Choose a Bluetooth device",
            "",
            inventory
                .devices
                .iter()
                .map(|device| Choice {
                    label: device.name.clone(),
                    detail: device.bluetooth_address.clone().unwrap_or_default(),
                })
                .collect(),
        )
        .await?
        .context("Bluetooth pairing canceled")?;
    let selected = inventory
        .devices
        .get(index)
        .context("invalid device selection")?;
    let address = device_address(selected)?;
    ensure!(!ui.cancelled(), "Bluetooth pairing canceled");
    let connection = bus().await?;
    let current = objects(&connection).await?;
    let path = resolve(&current, &address)?;
    let properties = current
        .get(&path)
        .and_then(|interfaces| interfaces.get(DEVICE))
        .context("Bluetooth device disappeared")?;
    ensure!(
        property::<bool>(properties, "Paired") != Some(true),
        "{address} is already paired; reconnect it from setup"
    );
    let dbus = DBusProxy::new(&connection).await?;
    let bluez_owner = timeout(CALL_TIMEOUT, dbus.get_name_owner("org.bluez".try_into()?))
        .await
        .context("looking up the Bluetooth service owner timed out")??
        .to_string();
    let (canceled, _) = watch::channel(false);
    let mut cleanup = PairingCleanup {
        connection: Some(connection.clone()),
        target: path.clone(),
        canceled: canceled.clone(),
        pair_pending: false,
    };
    // Every result after the cleanup guard exists passes through finish below,
    // including registration errors and cancellation before Pair is called.
    let outcome = async {
        ensure!(!ui.cancelled(), "Bluetooth pairing canceled");
        let agent = TerminalAgent {
            target: path.clone(),
            bluez_owner,
            canceled: canceled.clone(),
            prompt_cancel: watch::channel(0).0,
            prompt_lock: Mutex::new(()),
            ui: ui.clone(),
        };
        connection.object_server().at(AGENT_PATH, agent).await?;
        let manager = proxy(&connection, "/org/bluez", "org.bluez.AgentManager1").await?;
        let agent_path = ObjectPath::try_from(AGENT_PATH)?;
        call(&manager, "RegisterAgent", &(agent_path, "KeyboardDisplay")).await?;
        let device = proxy(&connection, path.as_str(), DEVICE).await?;
        ensure!(!ui.cancelled(), "Bluetooth pairing canceled");
        ui.status("Pairing Bluetooth device", &format!("Connecting to {}…", selected.name))?;
        cleanup.pair_pending = true;
        let (outcome, cancel_pending) = tokio::select! {
            biased;
            _ = ui.cancelled_signal() => {
                (Err(anyhow!("Bluetooth pairing canceled")), true)
            },
            signal = tokio::signal::ctrl_c() => {
                (signal.context("cannot listen for cancellation").and_then(|_| Err(anyhow!("Bluetooth pairing canceled"))), true)
            },
            result = timeout(Duration::from_secs(timeout_secs), device.call::<_, _, ()>("Pair", &())) => {
                match result {
                    Ok(result) => (result.context("Bluetooth pairing failed"), false),
                    Err(_) => (Err(anyhow!("Bluetooth pairing timed out")), true),
                }
            },
        };
        cleanup.pair_pending = cancel_pending;
        outcome?;
        // Read Paired directly rather than from the Device proxy's property cache.
        // Trust belongs only to this explicitly selected, newly paired device.
        complete_pairing(
            async {
                let properties = proxy(
                    &connection,
                    path.as_str(),
                    "org.freedesktop.DBus.Properties",
                )
                .await?;
                let value: OwnedValue =
                    timeout(CALL_TIMEOUT, properties.call("Get", &(DEVICE, "Paired")))
                        .await
                        .context("confirming the paired state timed out")??;
                Ok(bool::try_from(&value)?)
            },
            || async {
                ensure!(!ui.cancelled(), "pairing canceled; device trust was not changed");
                timeout(CALL_TIMEOUT, device.set_property("Trusted", true))
                    .await
                    .context("setting Bluetooth device trust timed out")??;
                Ok(())
            },
        )
        .await
    }
    .await;
    cleanup.finish().await;
    outcome
}

pub async fn connect(device: &Device) -> Result<()> {
    let address = device_address(device)?;
    let connection = bus().await?;
    let current = objects(&connection).await?;
    let path = resolve(&current, &address)?;
    if current
        .get(&path)
        .and_then(|interfaces| interfaces.get(DEVICE))
        .and_then(|properties| property::<bool>(properties, "Connected"))
        == Some(true)
    {
        return Ok(());
    }
    let device = proxy(&connection, path.as_str(), DEVICE).await?;
    let (outcome, cancel_pending) = tokio::select! {
        result = timeout(Duration::from_secs(30), device.call::<_, _, ()>("Connect", &())) => {
            match result {
                Ok(result) => (result.context("cannot connect Bluetooth device; wake it and check its selected host"), false),
                Err(_) => (Err(anyhow!("Bluetooth connection timed out")), true),
            }
        },
        _ = tokio::signal::ctrl_c() => (Err(anyhow!("Bluetooth connection canceled")), true),
    };
    if cancel_pending {
        // BlueZ documents Disconnect as cancellation for an in-flight Connect. Do not
        // disconnect on an ordinary method error: another client may own that connection.
        let _ = call(&device, "Disconnect", &()).await;
    }
    outcome
}

pub async fn disconnect(device: &Device) -> Result<()> {
    let address = device_address(device)?;
    let connection = bus().await?;
    let path = resolve(&objects(&connection).await?, &address)?;
    let device = proxy(&connection, path.as_str(), DEVICE).await?;
    call(&device, "Disconnect", &()).await
}

/// The CLI must obtain explicit confirmation before calling this irreversible operation.
pub async fn unpair(device: &Device) -> Result<()> {
    let address = device_address(device)?;
    let connection = bus().await?;
    let current = objects(&connection).await?;
    let path = resolve(&current, &address)?;
    let properties = current
        .get(&path)
        .and_then(|interfaces| interfaces.get(DEVICE))
        .context("Bluetooth device disappeared")?;
    let adapter_path = property::<&ObjectPath<'_>>(properties, "Adapter")
        .context("BlueZ did not report this device's adapter")?;
    let adapter = proxy(&connection, adapter_path.as_str(), ADAPTER).await?;
    call(&adapter, "RemoveDevice", &(path,)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::names::OwnedInterfaceName;
    use zbus::zvariant::Value;

    fn value<'a>(value: impl Into<Value<'a>>) -> OwnedValue {
        OwnedValue::try_from(value.into()).expect("valid fixture value")
    }

    fn props(name: &str, modalias: Option<&str>, hid: bool) -> Properties {
        let mut properties = HashMap::from([
            ("Name".into(), value(name)),
            ("Address".into(), value("AA:BB:CC:DD:EE:FF")),
        ]);
        if let Some(modalias) = modalias {
            properties.insert("Modalias".into(), value(modalias));
        }
        if hid {
            properties.insert("UUIDs".into(), value(vec![HID_UUIDS[1]]));
        }
        properties
    }

    fn managed(path: &str, properties: Properties) -> ManagedObjects {
        HashMap::from([(
            OwnedObjectPath::try_from(path).expect("object path"),
            HashMap::from([(
                OwnedInterfaceName::try_from(DEVICE).expect("interface"),
                properties,
            )]),
        )])
    }

    #[test]
    fn addresses_are_canonical_and_validate_exact_octets() {
        assert_eq!(
            normalize_address("bt:aa:bb:cc:dd:ee:ff").expect("valid"),
            "AA:BB:CC:DD:EE:FF"
        );
        for value in [
            "a:bb:cc:dd:ee:ff",
            "GG:BB:CC:DD:EE:FF",
            "AA:BB:CC:DD:EE",
            "mouse",
            "AA:BB:CC:DD:EE:FF:00",
        ] {
            assert!(normalize_address(value).is_err());
        }
    }

    #[test]
    fn vendor_namespaces_and_input_capabilities_are_required() {
        for (name, modalias, hid, expected) in [
            ("Custom mouse", "usb:v046DpB034d0001", true, Some(false)),
            (
                "Custom mouse",
                "bluetooth:v01DAp0001d0001",
                true,
                Some(false),
            ),
            ("Logitech headset", "usb:v046Dp0001d0001", false, None),
            ("Logitech keyboard", "bluetooth:v046Dp0001d0001", true, None),
            ("Logitech keyboard", "usb:v1234p0001d0001", true, None),
        ] {
            assert_eq!(
                identification(&props(name, Some(modalias), hid)),
                expected,
                "{name}, {modalias}"
            );
        }
    }

    #[test]
    fn headset_media_buttons_do_not_make_a_supported_input_device() {
        let mut properties = props("Logitech headset", Some("usb:v046Dp0001d0001"), true);
        properties.insert("Icon".into(), value("audio-headset"));
        assert_eq!(identification(&properties), None);
        properties.remove("Icon");
        properties.insert("Appearance".into(), OwnedValue::from(0x03c4_u16));
        assert_eq!(identification(&properties), None);
    }

    #[test]
    fn excluded_device_names_cannot_be_admitted_by_hid_or_vendor_id() {
        for name in [
            "Logitech Headset",
            "Logi Gamepad",
            "Logitech Joystick",
            "MX Headphones",
        ] {
            assert_eq!(
                identification(&props(name, Some("usb:v046dp0001d0001"), true)),
                None
            );
            assert_eq!(identification(&props(name, None, true)), None);
        }
    }

    #[test]
    fn vendor_identification_does_not_require_a_known_or_available_name() {
        for name in ["", "Future mouse", "Another Logitech model"] {
            assert_eq!(
                identification(&props(name, Some("usb:v046dp0001d0001"), true)),
                Some(false)
            );
        }
        for name in [
            "POP Keys",
            "Pebble Mouse 2",
            "Wave Keys",
            "Signature M650",
            "LIFT",
            "K380",
            "M720 Triathlon",
        ] {
            assert_eq!(
                identification(&props(name, None, true)),
                Some(true),
                "{name}"
            );
            assert_eq!(
                identification(&props(name, Some("usb:v1234p0001d0001"), true)),
                None
            );
        }
    }

    #[test]
    fn names_alone_are_not_identity_and_fallback_is_explicit() {
        assert_eq!(identification(&props("Logitech", None, false)), None);
        assert_eq!(
            identification(&props("logitech MX Master", None, true)),
            Some(true)
        );
        assert_eq!(
            identification(&props("MX Keys Mini", None, true)),
            Some(true)
        );
        assert_eq!(identification(&props("Another keyboard", None, true)), None);
        assert_eq!(
            identification(&props("biological keyboard", None, true)),
            None
        );
        let inventory = inventory_from(&managed(
            "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF",
            props("Logitech", None, true),
        ));
        assert!(
            inventory.devices[0]
                .warnings
                .iter()
                .any(|warning| warning.contains("inferred"))
        );
        assert!(inventory.devices[0].battery.is_none());
    }

    #[test]
    fn address_resolution_does_not_assume_hci_zero_and_rejects_ambiguity() {
        let mut objects = managed(
            "/org/bluez/hci2/dev_AA_BB_CC_DD_EE_FF",
            props("Logitech", None, true),
        );
        assert_eq!(
            resolve(&objects, "aa:bb:cc:dd:ee:ff")
                .expect("known")
                .as_str(),
            "/org/bluez/hci2/dev_AA_BB_CC_DD_EE_FF"
        );
        assert!(resolve(&objects, "11:22:33:44:55:66").is_err());
        objects.extend(managed(
            "/org/bluez/hci1/dev_AA_BB_CC_DD_EE_FF",
            props("Logitech", None, true),
        ));
        assert!(resolve(&objects, "AA:BB:CC:DD:EE:FF").is_err());
        assert_eq!(inventory_from(&objects).devices.len(), 1);
    }

    #[test]
    fn offline_battery_is_stale_and_out_of_range_is_unknown() {
        let path = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF";
        let mut properties = props("Logitech", None, true);
        properties.insert("Connected".into(), OwnedValue::from(false));
        let mut objects = managed(path, properties);
        let interfaces = objects.values_mut().next().expect("device");
        interfaces.insert(
            OwnedInterfaceName::try_from(BATTERY).expect("interface"),
            HashMap::from([("Percentage".into(), OwnedValue::from(42_u8))]),
        );
        let device = inventory_from(&objects).devices.remove(0);
        let battery = device.battery.expect("battery");
        assert_eq!(battery.percent, Some(42));
        assert!(battery.stale);
        assert_eq!(battery.charging, None);
        objects
            .values_mut()
            .next()
            .expect("device")
            .get_mut(BATTERY)
            .expect("battery")
            .insert("Percentage".into(), OwnedValue::from(255_u8));
        assert!(inventory_from(&objects).devices[0].battery.is_none());
    }

    #[test]
    fn target_name_variations_are_discovered_without_vendor_id() {
        for name in [
            "MX Master 4",
            "mx master 4",
            "MX_Master_4",
            "MX-Master-4",
            "Logitech MX Master 4",
            "MX Master 4 for Mac",
            "MX Mechanical Mini",
            "MX_MECHANICAL_MINI",
            "MX MCHNCL M",
            "mx mechanical mini for mac",
            "Logi MX Mechanical Mini",
            "Logitech K380",
            "MX Master 3S",
            "MX Keys Mini",
            "MX Anywhere 3",
        ] {
            assert_eq!(
                identification(&props(name, None, true)),
                Some(true),
                "{name}"
            );
        }
        for name in [
            "MX Masterpiece",
            "MX Mechanically",
            "Mechanical Mini",
            "Other keyboard",
        ] {
            assert_eq!(identification(&props(name, None, true)), None, "{name}");
        }
    }

    #[test]
    fn user_bluetooth_alias_does_not_replace_model_identity() {
        for (name, alias) in [
            ("MX Mechanical Mini", "Work keyboard"),
            ("Unrelated Logitech keyboard", "MX Mechanical Mini"),
        ] {
            let mut properties = props(name, Some("usb:v046dp0001d0001"), true);
            properties.insert("Alias".into(), value(alias));
            let objects = managed("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF", properties);
            assert_eq!(inventory_from(&objects).devices[0].name, name);
        }
    }

    fn terminal_agent() -> (
        TerminalAgent,
        tokio::sync::mpsc::UnboundedReceiver<crate::terminal::Event>,
    ) {
        let (ui, screens) = crate::terminal::channel();
        (
            TerminalAgent {
                target: OwnedObjectPath::try_from("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF")
                    .expect("path"),
                bluez_owner: ":1.10".into(),
                canceled: watch::channel(false).0,
                prompt_cancel: watch::channel(0).0,
                prompt_lock: Mutex::new(()),
                ui,
            },
            screens,
        )
    }

    fn callback(sender: &str) -> zbus::Message {
        zbus::Message::method_call(AGENT_PATH, "Cancel")
            .expect("method")
            .sender(sender)
            .expect("sender")
            .build(&())
            .expect("message")
    }

    #[tokio::test]
    async fn pairing_agent_rejects_other_senders_and_other_devices() {
        let (agent, _screens) = terminal_agent();
        assert!(agent.cancel(callback(":1.11").header()).is_err());
        let unrelated =
            ObjectPath::try_from("/org/bluez/hci0/dev_11_22_33_44_55_66").expect("path");
        assert!(
            agent
                .authorize_service(unrelated, HID_UUIDS[1], callback(":1.10").header())
                .await
                .is_err()
        );
        assert!(!*agent.canceled.borrow());
    }

    #[tokio::test]
    async fn interactive_agent_routes_pin_passkey_and_confirmation_through_screens() {
        use crate::terminal::Event;

        let (agent, mut screens) = terminal_agent();
        let target = ObjectPath::try_from(agent.target.as_str()).expect("path");
        let message = callback(":1.10");

        let (pin, ()) = tokio::join!(
            agent.request_pin_code(target.clone(), message.header()),
            async {
                let Some(Event::Input { message, reply, .. }) = screens.recv().await else {
                    panic!("expected an in-screen PIN input");
                };
                assert!(message.contains("PIN"));
                reply.send(Some("a123".into())).expect("PIN reply");
            }
        );
        assert_eq!(pin.expect("PIN"), "a123");

        let (passkey, ()) = tokio::join!(
            agent.request_passkey(target.clone(), message.header()),
            async {
                let Some(Event::Input { message, reply, .. }) = screens.recv().await else {
                    panic!("expected an in-screen passkey input");
                };
                assert!(message.contains("passkey"));
                reply.send(Some("000042".into())).expect("passkey reply");
            }
        );
        assert_eq!(passkey.expect("passkey"), 42);

        let (confirmation, ()) = tokio::join!(
            agent.request_confirmation(target, 42, message.header()),
            async {
                let Some(Event::Choose {
                    message,
                    choices,
                    reply,
                    ..
                }) = screens.recv().await
                else {
                    panic!("expected an in-screen confirmation menu");
                };
                assert!(message.contains("000042"));
                assert!(!message.contains("[y/N]"));
                assert_eq!(choices[0].label, "Decline");
                assert_eq!(choices[1].label, "Allow");
                reply.send(Some(1)).expect("confirmation reply");
            }
        );
        confirmation.expect("approved pairing");
    }

    #[tokio::test]
    async fn interactive_display_cancel_dismisses_screen_without_canceling_pairing() {
        use crate::terminal::Event;

        let (agent, mut screens) = terminal_agent();
        let ui = agent.ui.clone();
        let target = ObjectPath::try_from(agent.target.as_str()).expect("path");
        let message = callback(":1.10");

        agent
            .display_passkey(target.clone(), 42, 4, message.header())
            .expect("display");
        let Some(Event::Status {
            message: instruction,
            ..
        }) = screens.recv().await
        else {
            panic!("expected an in-screen authentication status");
        };
        assert!(instruction.contains("000042"));
        assert!(instruction.contains("4 digits entered"));
        agent.cancel(message.header()).expect("dismiss display");
        assert!(matches!(screens.recv().await, Some(Event::Dismiss)));
        assert!(!ui.cancelled());
        assert!(!*agent.canceled.borrow());
        agent
            .authorize_service(target.clone(), HID_UUIDS[1], message.header())
            .await
            .expect("HID authorization remains allowed");

        ui.cancel();
        assert!(
            agent
                .authorize_service(target, HID_UUIDS[1], message.header())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn cancellation_releases_pending_authentication_without_waiting_for_input() {
        use crate::terminal::Event;

        for source in ["ui", "prompt", "session"] {
            let (agent, mut screens) = terminal_agent();
            let target = ObjectPath::try_from(agent.target.as_str()).expect("path");
            let message = callback(":1.10");
            let (response, _pending_reply) = tokio::join!(
                agent.request_passkey(target.clone(), message.header()),
                async {
                    let Some(Event::Input { reply, .. }) = screens.recv().await else {
                        panic!("expected authentication input");
                    };
                    match source {
                        "ui" => agent.ui.cancel(),
                        "prompt" => agent.cancel(message.header()).expect("cancel prompt"),
                        _ => agent.release(message.header()).expect("release agent"),
                    }
                    reply
                }
            );
            assert!(matches!(response, Err(AgentError::Canceled(_))), "{source}");
            assert_eq!(
                agent
                    .authorize_service(target, HID_UUIDS[1], message.header())
                    .await
                    .is_ok(),
                source == "prompt",
            );
        }
    }

    #[tokio::test]
    async fn cancellation_wins_when_a_prompt_reply_becomes_ready_at_the_same_time() {
        for source in ["ui", "prompt", "session"] {
            let (agent, _screens) = terminal_agent();
            let response = agent
                .guarded_prompt(async {
                    match source {
                        "ui" => agent.ui.cancel(),
                        "prompt" => agent
                            .cancel(callback(":1.10").header())
                            .expect("cancel prompt"),
                        _ => agent
                            .release(callback(":1.10").header())
                            .expect("release agent"),
                    }
                    Ok(())
                })
                .await;
            assert!(matches!(response, Err(AgentError::Canceled(_))), "{source}");
        }
    }

    #[tokio::test]
    async fn trust_requires_confirmed_pairing_and_reports_partial_success() {
        use std::cell::Cell;
        let cases: [(Result<bool>, bool, &[&str], usize); 4] = [
            (Ok(false), false, &["not confirmed"], 0),
            (
                Err(anyhow!("device disappeared")),
                false,
                &["device trust was not changed"],
                0,
            ),
            (Ok(true), false, &[], 1),
            (
                Ok(true),
                true,
                &["pairing succeeded", "device remains paired"],
                1,
            ),
        ];
        for (paired, trust_fails, messages, expected_writes) in cases {
            let writes = Cell::new(0);
            let result = complete_pairing(async { paired }, || async {
                writes.set(writes.get() + 1);
                ensure!(!trust_fails, "permission denied");
                Ok(())
            })
            .await;
            if messages.is_empty() {
                result.expect("paired and trusted");
            } else {
                let error = result
                    .expect_err("unconfirmed or incomplete pairing")
                    .to_string();
                assert!(
                    messages.iter().all(|message| error.contains(message)),
                    "{error}"
                );
            }
            assert_eq!(writes.get(), expected_writes);
        }
    }
}
