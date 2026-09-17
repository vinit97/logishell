use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    Bluetooth,
    Bolt,
    Unifying,
    Usb,
    #[default]
    Unknown,
}

impl std::fmt::Display for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(match self {
            Self::Bluetooth => "bluetooth",
            Self::Bolt => "bolt",
            Self::Unifying => "unifying",
            Self::Usb => "usb",
            Self::Unknown => "unknown",
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeviceState {
    Online,
    Offline,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Battery {
    pub percent: Option<u8>,
    pub level: Option<String>,
    pub charging: Option<bool>,
    pub stale: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub transport: Transport,
    pub state: DeviceState,
    pub battery: Option<Battery>,
    pub receiver_id: Option<String>,
    pub slot: Option<u8>,
    pub hid_path: Option<String>,
    pub bluetooth_address: Option<String>,
    pub firmware: Option<String>,
    pub capabilities: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Receiver {
    pub id: String,
    pub name: String,
    pub transport: Transport,
    pub hid_path: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Inventory {
    pub devices: Vec<Device>,
    pub receivers: Vec<Receiver>,
    pub warnings: Vec<String>,
}

pub type Settings = BTreeMap<String, serde_json::Value>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Model {
    MxMaster4,
    MxMechanicalMini,
}

pub fn classify(name: &str) -> Option<Model> {
    let normalized = name
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join(" ");
    let name = normalized
        .strip_prefix("logitech ")
        .or_else(|| normalized.strip_prefix("logi "))
        .unwrap_or(&normalized);
    let name = name
        .strip_suffix(" for mac")
        .or_else(|| name.strip_suffix(" for business"))
        .unwrap_or(name);
    match name {
        "mx master 4" => Some(Model::MxMaster4),
        "mx mechanical mini" | "mx mchncl m" => Some(Model::MxMechanicalMini),
        _ => None,
    }
}

/// Inventory establishes Logitech identity; adapters probe the actual features.
pub fn require_supported(device: &Device) -> Result<()> {
    if device.transport == Transport::Unknown {
        bail!(
            "{} has no supported Logitech connection; use Bluetooth, Bolt, Unifying, or a USB HID interface",
            device.name
        );
    }
    Ok(())
}

pub fn validate_setting(device: &Device, key: &str, value: &str) -> Result<()> {
    require_supported(device)?;
    crate::config::validate_setting(key, value)?;
    // In particular, DPI bounds and steps belong to the device's reported
    // sensor table. The backend checks availability and readback before saving.
    Ok(())
}

pub fn require_remappable(device: &Device) -> Result<()> {
    require_supported(device)?;
    if device.hid_path.is_none() {
        bail!(
            "{} has no accessible HID interface for remapping",
            device.name
        );
    }
    if !device.capabilities.iter().any(|capability| {
        matches!(
            capability.as_str(),
            "button-diversion" | "feature:1b04" | "thumb-wheel"
        )
    }) {
        bail!(
            "{} does not report supported HID++ controls or a thumb wheel for remapping",
            device.name
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn known_name_aliases_do_not_conflate_products() {
        for (name, expected) in [
            ("Logitech MX Master 4", Some(Model::MxMaster4)),
            ("MX Mechanical Mini", Some(Model::MxMechanicalMini)),
            ("MX MCHNCL M", Some(Model::MxMechanicalMini)),
            ("MX Mechanical Mini for Mac", Some(Model::MxMechanicalMini)),
            ("MX Master 3S", None),
            ("MX Master 40", None),
            ("MX Mechanical", None),
            ("MX Keys Mini", None),
            ("mouse", None),
        ] {
            assert_eq!(classify(name), expected, "{name}");
        }
    }

    #[test]
    fn support_and_dpi_validation_do_not_depend_on_model_names() {
        let mut device = Device {
            name: "Unlisted Logitech mouse".into(),
            hid_path: Some("/dev/test-only".into()),
            ..Default::default()
        };
        for transport in [
            Transport::Usb,
            Transport::Unifying,
            Transport::Bolt,
            Transport::Bluetooth,
        ] {
            device.transport = transport;
            assert!(require_supported(&device).is_ok());
            // A gaming sensor or older mouse can have a different DPI table.
            assert!(validate_setting(&device, "dpi", "25600").is_ok());
            assert!(validate_setting(&device, "dpi", "125").is_ok());
        }
        assert!(validate_setting(&device, "dpi", "0").is_err());
        assert!(require_remappable(&device).is_err());
        device.capabilities.push("feature:1b04".into());
        assert!(require_remappable(&device).is_ok());
        device.hid_path = None;
        assert!(require_remappable(&device).is_err());
        device.transport = Transport::Unknown;
        assert!(require_supported(&device).is_err());
    }
}
