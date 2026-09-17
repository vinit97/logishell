// Compile the real wizard against the simulated backend. Keep its focused
// unit tests here so they run once, alongside the PTY integration tests.
include!("../../src/wizard.rs");

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mouse() -> Device {
        serde_json::from_value(json!({
            "id": "test-mouse", "name": "Test mouse", "transport": "usb",
            "state": "online", "capabilities": [], "warnings": []
        }))
        .expect("valid fixture")
    }

    #[test]
    fn connection_menu_only_offers_actions_supported_by_the_transport() {
        let mut device = mouse();
        for transport in [Transport::Usb, Transport::Unknown] {
            device.transport = transport;
            assert!(connection_actions(&device).is_empty());
        }
        for transport in [Transport::Bolt, Transport::Unifying] {
            device.transport = transport;
            let actions = connection_actions(&device);
            assert_eq!(actions.len(), 1);
            assert!(matches!(actions[0].0, ConnectionAction::Unpair));
        }
        device.transport = Transport::Bluetooth;
        for state in [
            DeviceState::Online,
            DeviceState::Offline,
            DeviceState::Unknown,
        ] {
            device.state = state;
            let actions = connection_actions(&device);
            assert_eq!(actions.len(), 2);
            assert_eq!(
                matches!(actions[0].0, ConnectionAction::Disconnect),
                state == DeviceState::Online
            );
            assert!(matches!(actions[1].0, ConnectionAction::Unpair));
        }
    }

    #[test]
    fn last_wheel_choice_replaces_conflicting_draft_values() {
        for initial_mode in ["ratchet", "free-spin"] {
            let device = mouse();
            let actual = Settings::from([
                ("wheel-mode".into(), json!(initial_mode)),
                ("smartshift".into(), json!(false)),
                ("smartshift-sensitivity".into(), json!(255)),
            ]);
            let mut draft = Draft::default();
            draft.stage_setting(&device, "wheel-mode", "free-spin", &actual);
            draft.stage_setting(&device, "smartshift-sensitivity", "12", &actual);
            draft.stage_setting(&device, "smartshift", "on", &actual);
            assert_eq!(draft.count(), 1);
            assert_eq!(draft.preview(&device, &actual)["smartshift"], json!(true));
            assert_eq!(
                draft.preview(&device, &actual)["smartshift-sensitivity"],
                json!("Device default")
            );
            assert_eq!(
                draft.preview(&device, &actual)["wheel-mode"],
                json!("ratchet")
            );
            // Explicitly selecting free-spin afterwards must run after enabling SmartShift.
            draft.stage_setting(&device, "wheel-mode", "free-spin", &actual);
            let changes = config::ordered_settings(&draft.devices[&device.id].edits.settings);
            assert_eq!(changes[0].0, "smartshift");
            assert_eq!(changes[1], ("wheel-mode".into(), "free-spin".into()));
            draft.stage_setting(&device, "smartshift-sensitivity", "12", &actual);
            draft.stage_setting(&device, "smartshift", "off", &actual);
            assert_eq!(
                draft.preview(&device, &actual)["smartshift-sensitivity"],
                json!(255)
            );
        }
    }

    #[test]
    fn conflicting_nickname_is_rejected_before_any_apply() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config.toml");
        let mut saved = config::Config::default();
        saved.devices.insert(
            "another-device".into(),
            config::DeviceConfig {
                alias: Some("mouse".into()),
                ..Default::default()
            },
        );
        config::save(&path, &saved)?;
        let mut draft = Draft::default();
        draft.stage_alias(&mouse(), "MOUSE".into(), "");
        assert!(draft.validate(&path).is_err());
        assert_eq!(config::load(&path)?, saved);
        Ok(())
    }

    #[test]
    fn dpi_choices_use_only_the_reported_sensor_values() {
        let settings = Settings::from([("dpi-supported".into(), json!([125, 1000, 25600]))]);
        assert_eq!(
            value_options(&SETTINGS[0], &settings)
                .iter()
                .map(|(value, _)| value.as_str())
                .collect::<Vec<_>>(),
            ["125", "1000", "25600"]
        );
        assert!(value_options(&SETTINGS[0], &Settings::new()).is_empty());
    }

    #[test]
    fn sensitivity_keeps_current_value_and_explains_disabled_sentinel() {
        let settings = Settings::from([("smartshift-sensitivity".into(), json!(12))]);
        let options = value_options(&SETTINGS[3], &settings);
        assert_eq!(options.iter().filter(|(value, _)| value == "12").count(), 1);
        assert_eq!(
            options
                .iter()
                .find(|(value, _)| value == "255")
                .map(|(_, item)| item.label.as_str()),
            Some("Off")
        );
        assert_eq!(display_value("smartshift-sensitivity", &json!(255)), "Off");
        assert_eq!(raw_value(&json!(true)), "on");
    }
}
