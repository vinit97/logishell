use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub const MAX_THUMB_WHEEL_INTERVAL_MS: u16 = 5000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema_version: u32,
    #[serde(default)]
    pub devices: BTreeMap<String, DeviceConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: 1,
            devices: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeviceConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default)]
    pub settings: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bindings: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub thumb_wheel_interval_ms: u16,
}

fn is_zero(value: &u16) -> bool {
    *value == 0
}

/// Keep the last explicitly requested wheel behavior internally consistent.
pub fn record_setting(device: &mut DeviceConfig, key: &str, value: &str) {
    if key == "smartshift" {
        device.settings.remove("smartshift-sensitivity");
        if matches!(value, "on" | "true") {
            device.settings.remove("wheel-mode");
        }
    } else if key == "smartshift-sensitivity" {
        device.settings.remove("smartshift");
    }
    device.settings.insert(key.to_owned(), value.to_owned());
}

/// Preserve the requested threshold when enabling SmartShift, then restore the
/// user's wheel mode after any enable operation that selects ratchet mode.
pub fn ordered_settings(settings: &BTreeMap<String, String>) -> Vec<(String, String)> {
    let mut ordered: Vec<_> = settings
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    ordered.sort_by_key(|(key, _)| match key.as_str() {
        "smartshift-sensitivity" => 1,
        "smartshift" => 2,
        "wheel-mode" => 3,
        _ => 0,
    });
    ordered
}

pub fn path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .context("HOME must be an absolute path")?;
    Ok(home.join(".config/logishell/config.toml"))
}

pub fn load(path: &Path) -> Result<Config> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(error) => {
            return Err(error).with_context(|| format!("read configuration {}", path.display()));
        }
    };
    let config: Config = toml::from_str(&text)
        .with_context(|| format!("invalid configuration {}", path.display()))?;
    validate(&config)?;
    Ok(config)
}

pub fn validate(config: &Config) -> Result<()> {
    if config.schema_version != 1 {
        bail!(
            "unsupported configuration schema {}; this build supports schema 1",
            config.schema_version
        );
    }
    let mut aliases = BTreeSet::new();
    for (id, device) in &config.devices {
        if id.trim().is_empty() {
            bail!("device identifiers cannot be empty");
        }
        if device.thumb_wheel_interval_ms > MAX_THUMB_WHEEL_INTERVAL_MS {
            bail!(
                "device {id}: thumb_wheel_interval_ms must be between 0 and {MAX_THUMB_WHEEL_INTERVAL_MS}"
            );
        }
        if let Some(alias) = &device.alias {
            if alias.is_empty()
                || !alias
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                bail!("alias for {id} must contain only ASCII letters, numbers, '-' or '_'");
            }
            if !aliases.insert(alias.to_ascii_lowercase()) {
                bail!("duplicate alias {alias}");
            }
        }
        for (key, value) in &device.settings {
            validate_setting(key, value).with_context(|| format!("device {id}"))?;
        }
        if let (Some(enabled), Some(threshold)) = (
            device.settings.get("smartshift"),
            device.settings.get("smartshift-sensitivity"),
        ) {
            let enabled = matches!(enabled.as_str(), "on" | "true");
            if enabled == (threshold.parse::<u8>()? == 255) {
                bail!(
                    "device {id}: smartshift and smartshift-sensitivity disagree (255 disables automatic disengagement)"
                );
            }
        }
        crate::remap::validate_bindings(&device.bindings)
            .with_context(|| format!("device {id}: invalid button bindings"))?;
    }
    Ok(())
}

pub fn validate_setting(key: &str, value: &str) -> Result<()> {
    match key {
        "dpi" => {
            let value = value
                .parse::<u16>()
                .context("dpi must be an integer between 1 and 65535")?;
            if value == 0 {
                bail!("dpi must be greater than zero");
            }
        }
        "smartshift-sensitivity" => {
            let n: u8 = value
                .parse()
                .context("smartshift-sensitivity must be an integer from 1 through 255")?;
            if n == 0 {
                bail!("smartshift-sensitivity must be greater than zero");
            }
        }
        "haptic-strength" => {
            let strength: u8 = value
                .parse()
                .context("haptic-strength must be an integer from 0 through 100")?;
            if strength > 100 {
                bail!("haptic-strength must be an integer from 0 through 100");
            }
        }
        "wheel-mode" => {
            if !["ratchet", "free-spin"].contains(&value) {
                bail!("wheel-mode must be ratchet or free-spin");
            }
        }
        "smartshift" | "scroll-invert" | "thumb-wheel-invert" | "fn-lock" | "backlight" => {
            if !["true", "false", "on", "off"].contains(&value) {
                bail!("{key} must be on/off or true/false");
            }
        }
        _ => bail!(
            "unknown setting {key}; use `logishell config get <device>` to list supported settings"
        ),
    }
    Ok(())
}

pub fn reset(path: &Path) -> Result<Option<PathBuf>> {
    let backup = match fs::read(path) {
        Ok(contents) => {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos();
            let backup = path.with_extension(format!("toml.{nonce}.bak"));
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&backup)
                .context("back up configuration before reset")?;
            file.write_all(&contents)
                .and_then(|()| file.sync_all())
                .context("write configuration backup; original file was not changed")?;
            Some(backup)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("read configuration before reset"),
    };
    save(path, &Config::default())?;
    Ok(backup)
}

pub fn save(path: &Path, config: &Config) -> Result<()> {
    validate(config)?;
    let text = toml::to_string_pretty(config)?;
    let parent = path
        .parent()
        .context("configuration path has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let filename = path
        .file_name()
        .context("configuration path has no file name")?
        .to_string_lossy();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let temporary = parent.join(format!(".{filename}.{}.{nonce}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .with_context(|| format!("create temporary configuration {}", temporary.display()))?;
    let result = (|| -> Result<()> {
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, path)
            .with_context(|| format!("replace configuration {}", path.display()))?;
        fs::File::open(parent)?.sync_all().context(
            "configuration was replaced, but directory durability could not be confirmed",
        )?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_roundtrip_without_losing_other_devices() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("nested/config.toml");
        let mut config = Config::default();
        let first = DeviceConfig {
            alias: Some("mouse".into()),
            settings: BTreeMap::from([("dpi".into(), "1600".into())]),
            bindings: BTreeMap::from([("back".into(), "key:ctrl+c".into())]),
            ..Default::default()
        };
        config.devices.insert("receiver:abc:slot:1".into(), first);
        config
            .devices
            .insert("bt:AA:BB:CC:DD:EE:FF".into(), DeviceConfig::default());
        save(&path, &config)?;
        assert_eq!(config, load(&path)?);
        assert!(!toml::to_string(&DeviceConfig::default())?.contains("bindings"));
        Ok(())
    }

    #[test]
    fn rejects_invalid_config_without_replacing_file() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config.toml");
        save(&path, &Config::default())?;
        let before = fs::read(&path)?;
        let config = Config {
            schema_version: 9,
            ..Default::default()
        };
        assert!(save(&path, &config).is_err());
        assert_eq!(before, fs::read(path)?);
        assert!(toml::from_str::<Config>("schema_version = 1\ntypo = true").is_err());
        Ok(())
    }

    #[test]
    fn rejects_colliding_aliases_and_invalid_values() {
        let mut config = Config::default();
        for (id, alias) in [("one", "Mouse"), ("two", "mouse")] {
            config.devices.entry(id.into()).or_default().alias = Some(alias.into());
        }
        assert!(validate(&config).is_err());
        for (key, values, valid) in [
            ("dpi", &["0", "-1"][..], false),
            ("fn-lock", &["maybe"][..], false),
            ("haptic-strength", &["0", "25", "100"][..], true),
            (
                "haptic-strength",
                &["-1", "101", "256", "25.5", "high"][..],
                false,
            ),
        ] {
            for value in values {
                assert_eq!(validate_setting(key, value).is_ok(), valid, "{key}={value}");
            }
        }
    }

    #[test]
    fn wheel_settings_keep_the_last_explicit_intent() {
        let mut device = DeviceConfig::default();
        record_setting(&mut device, "smartshift-sensitivity", "20");
        record_setting(&mut device, "smartshift", "off");
        assert!(!device.settings.contains_key("smartshift-sensitivity"));
        record_setting(&mut device, "smartshift-sensitivity", "30");
        assert!(!device.settings.contains_key("smartshift"));
        record_setting(&mut device, "wheel-mode", "free-spin");
        record_setting(&mut device, "smartshift", "on");
        assert!(!device.settings.contains_key("wheel-mode"));
    }

    #[test]
    fn manually_edited_wheel_conflicts_are_rejected() {
        let mut config = Config::default();
        for (enabled, threshold, valid) in [
            ("off", "20", false),
            ("off", "255", true),
            ("on", "255", false),
            ("off", "0255", true),
            ("on", "0255", false),
            ("off", "+255", true),
            ("on", "+255", false),
        ] {
            config.devices.entry("mouse".into()).or_default().settings = BTreeMap::from([
                ("smartshift".into(), enabled.into()),
                ("smartshift-sensitivity".into(), threshold.into()),
            ]);
            assert_eq!(validate(&config).is_ok(), valid, "{enabled}, {threshold}");
        }
    }

    #[test]
    fn reset_preserves_invalid_bytes_and_earlier_backups() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("config.toml");
        let original = b"\xffinvalid TOML";
        fs::write(&path, original)?;
        let backup = reset(&path)?.context("existing config backed up")?;
        assert_eq!(fs::read(&backup)?, original);
        assert_eq!(fs::metadata(&backup)?.permissions().mode() & 0o777, 0o600);
        assert_eq!(load(&path)?, Config::default());
        let defaults = fs::read(&path)?;
        let second = reset(&path)?.context("second backup")?;
        assert_ne!(backup, second);
        assert_eq!(fs::read(&backup)?, original);
        assert_eq!(fs::read(second)?, defaults);
        Ok(())
    }

    #[test]
    fn reset_creates_missing_config_and_leaves_original_if_backup_fails() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir()?;
        let parent = directory.path().join("nested");
        let path = parent.join("config.toml");
        assert!(reset(&path)?.is_none());
        assert_eq!(load(&path)?, Config::default());
        fs::write(&path, b"invalid original")?;
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o500))?;
        let result = reset(&path);
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))?;
        assert!(result.is_err());
        assert_eq!(fs::read(path)?, b"invalid original");
        Ok(())
    }

    #[test]
    fn thumb_wheel_interval_is_optional_and_bounded() -> Result<()> {
        let mut config: Config = toml::from_str("schema_version = 1\n[devices.mouse]\n")?;
        assert_eq!(config.devices["mouse"].thumb_wheel_interval_ms, 0);
        assert!(!toml::to_string(&config)?.contains("thumb_wheel_interval_ms"));
        for interval in [0, 250, 5000, 5001] {
            config
                .devices
                .get_mut("mouse")
                .expect("fixture")
                .thumb_wheel_interval_ms = interval;
            assert_eq!(validate(&config).is_ok(), interval <= 5000);
            assert_eq!(
                toml::from_str::<Config>(&toml::to_string(&config)?)?,
                config
            );
        }
        Ok(())
    }
}
