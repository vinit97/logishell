//! Explicit Bolt discovery/authentication workflow.
//!
//! Register numbers and event fields are interoperability facts published by
//! Solaar's receiver protocol research. This is an independent state machine:
//! it gathers candidates, requires selection, rejects ambiguous/malformed
//! authentication events, and verifies the receiver slot before success.

use super::{ReceiverUi, clean_name, finish_pairing, hex};
use crate::{
    hidpp::{Client, Packet, Transport as HidTransport, legacy_error},
    model::Device,
    terminal::Choice,
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Default)]
struct Candidate {
    name: Option<String>,
    address: Option<[u8; 6]>,
    kind: u8,
    product: u16,
    authentication: u8,
}

impl Candidate {
    fn complete(&self) -> bool {
        self.address.is_some() && self.name.is_some()
    }
    fn supported(&self) -> bool {
        // A Bolt receiver supplies the authenticated protocol device kind.
        // Marketing names vary between firmware and regional models.
        matches!(self.kind, 1 | 2)
    }
}

const MAX_DISCOVERY_RECORDS: usize = 64;

#[derive(Default)]
struct Discovery {
    // Counters identify announcement fragments, not physical devices. A device
    // may advertise repeatedly with a fresh counter during a single scan.
    pending: BTreeMap<u16, Candidate>,
    devices: BTreeMap<[u8; 6], Candidate>,
}

impl Discovery {
    fn has_supported_device(&self) -> bool {
        self.devices.values().any(Candidate::supported)
    }

    fn into_supported_devices(self) -> Vec<Candidate> {
        self.devices
            .into_values()
            .filter(Candidate::supported)
            .collect()
    }
}

fn same_details(first: &Candidate, second: &Candidate) -> bool {
    first.address == second.address
        && first.kind == second.kind
        && first.product == second.product
        && first.authentication == second.authentication
}

fn discovery_record(event: &Packet, discovery: &mut Discovery) -> Result<()> {
    ensure!(event.data.len() >= 3, "truncated Bolt discovery event");
    let counter = u16::from_le_bytes([event.address, event.data[0]]);
    if !matches!(event.data[1], 0 | 1) {
        return Ok(()); // Reserved fragments do not consume pending records.
    }
    ensure!(
        discovery.pending.contains_key(&counter) || discovery.pending.len() < MAX_DISCOVERY_RECORDS,
        "too many incomplete Bolt discovery records"
    );
    let candidate = discovery.pending.entry(counter).or_default();
    match event.data[1] {
        0 => {
            ensure!(event.data.len() >= 15, "truncated Bolt discovery details");
            let details = Candidate {
                kind: event.data[3],
                product: u16::from_le_bytes([event.data[4], event.data[5]]),
                address: Some(event.data[6..12].try_into().expect("length checked")),
                authentication: event.data[14],
                name: None,
            };
            ensure!(
                candidate.address.is_none() || same_details(candidate, &details),
                "conflicting Bolt discovery details for one announcement; retry pairing"
            );
            if let Some(previous) = details
                .address
                .and_then(|address| discovery.devices.get(&address))
            {
                ensure!(
                    same_details(previous, &details),
                    "conflicting Bolt discovery details for one device; retry pairing"
                );
            }
            candidate.kind = details.kind;
            candidate.product = details.product;
            candidate.address = details.address;
            candidate.authentication = details.authentication;
        }
        1 => {
            let end = 3 + usize::from(event.data[2]);
            ensure!(end <= event.data.len(), "truncated Bolt discovery name");
            let name = clean_name(&event.data[3..end]);
            ensure!(!name.is_empty(), "empty Bolt discovery name");
            candidate.name = Some(name);
        }
        _ => unreachable!("reserved fragments were excluded"),
    }
    if candidate.complete() {
        let candidate = discovery
            .pending
            .remove(&counter)
            .expect("pending candidate exists");
        let address = candidate
            .address
            .expect("complete candidate has an address");
        if let Some(previous) = discovery.devices.get(&address) {
            ensure!(
                same_details(previous, &candidate),
                "conflicting Bolt discovery details for one device; retry pairing"
            );
        } else {
            ensure!(
                discovery.devices.len() < MAX_DISCOVERY_RECORDS,
                "too many distinct Bolt devices requested pairing"
            );
            discovery.devices.insert(address, candidate);
        }
    }
    Ok(())
}

fn passkey_instruction(candidate: &Candidate, data: &[u8]) -> Result<String> {
    let digits = data
        .get(..6)
        .context("truncated Bolt authentication passkey")?;
    ensure!(
        digits.iter().all(u8::is_ascii_digit),
        "invalid Bolt authentication passkey"
    );
    let text = std::str::from_utf8(digits)?;
    if candidate.authentication & 1 != 0 {
        ensure!(
            candidate.kind == 1,
            "unexpected keyboard authentication for a mouse"
        );
        Ok(format!(
            "On the keyboard being paired, type {text} and press Enter."
        ))
    } else {
        ensure!(
            candidate.kind == 2,
            "unsupported Bolt keyboard authentication method"
        );
        let number = text
            .parse::<u16>()
            .context("invalid mouse authentication code")?;
        ensure!(
            number < 1024,
            "receiver returned a mouse authentication code outside the requested 10-bit range"
        );
        let clicks: Vec<_> = (0..10)
            .rev()
            .map(|bit| {
                if number & (1 << bit) == 0 {
                    "left"
                } else {
                    "right"
                }
            })
            .collect();
        Ok(format!(
            "On the mouse being paired, click: {}. Then press its left and right buttons together.",
            clicks.join(", ")
        ))
    }
}

fn receiver_event<T: HidTransport, U: ReceiverUi + ?Sized>(
    client: &mut Client<T>,
    ui: &mut U,
    deadline: Instant,
) -> Result<Option<Packet>> {
    ensure!(!ui.cancelled()?, "pairing cancelled");
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Ok(None);
    }
    let event = client.next_event(remaining.min(Duration::from_millis(200)))?;
    Ok(event.filter(|event| event.device == 0xff))
}

fn discover<T: HidTransport, U: ReceiverUi + ?Sized>(
    client: &mut Client<T>,
    ui: &mut U,
    timeout: u8,
) -> Result<Vec<Candidate>> {
    client.write_register(0xff, 0xc0, &[timeout, 1, 0])?;
    let deadline = Instant::now() + Duration::from_secs(u64::from(timeout));
    let mut candidates = Discovery::default();
    let mut settle = None;
    loop {
        let now = Instant::now();
        // A short scan or a device found near its deadline must still offer
        // complete candidates; the settling window cannot extend the scan.
        if now >= deadline || settle.is_some_and(|time| now >= time) {
            break;
        }
        let Some(event) = receiver_event(client, ui, deadline)? else {
            continue;
        };
        match event.command {
            0x4f => {
                discovery_record(&event, &mut candidates)?;
                if candidates.has_supported_device() && settle.is_none() {
                    settle = Some(Instant::now() + Duration::from_secs(2));
                }
            }
            0x53 => {
                ensure!(
                    event.data[0] == 0,
                    "Bolt discovery failed with error 0x{:02x}",
                    event.data[0]
                );
                if event.address != 0 {
                    break;
                }
            }
            _ => {}
        }
    }
    let candidates = candidates.into_supported_devices();
    ensure!(
        !candidates.is_empty(),
        "no Logitech keyboard or mouse requested Bolt pairing"
    );
    Ok(candidates)
}

fn authenticate<T: HidTransport, U: ReceiverUi + ?Sized>(
    client: &mut Client<T>,
    ui: &mut U,
    candidate: &Candidate,
    timeout: u8,
) -> Result<u8> {
    let address = candidate
        .address
        .context("discovered device has no pairing address")?;
    let mut payload = [0u8; 16];
    payload[0] = 1; // Pair; slot zero requests an unused slot, never overwrite.
    payload[2..8].copy_from_slice(&address);
    payload[8] = candidate.authentication;
    payload[9] = if candidate.kind == 1 { 20 } else { 10 };
    client.write_register(0xff, 0xc1, &payload)?;
    let deadline = Instant::now() + Duration::from_secs(u64::from(timeout));
    let mut prompted = false;
    loop {
        ensure!(Instant::now() < deadline, "Bolt pairing timed out");
        let Some(event) = receiver_event(client, ui, deadline)? else {
            continue;
        };
        match event.command {
            0x4d => {
                ensure!(
                    !prompted,
                    "receiver changed its authentication challenge; pairing cancelled"
                );
                ui.status(
                    "Confirm on your device",
                    &passkey_instruction(candidate, &event.data)?,
                )?;
                prompted = true;
            }
            0x54 => {
                ensure!(
                    event.data[0] == 0,
                    "Bolt authentication failed with error 0x{:02x}",
                    event.data[0]
                );
                match event.address {
                    0 => {}
                    2 => {
                        ensure!(
                            prompted,
                            "receiver reported pairing without authenticated user confirmation"
                        );
                        let slot = *event
                            .data
                            .get(7)
                            .context("truncated Bolt pairing completion")?;
                        ensure!(
                            (1..=6).contains(&slot),
                            "receiver assigned invalid pairing slot {slot}"
                        );
                        let pairing = client.register(0xff, 0xb5, &[0x50 + slot], true).context(
                            "pairing completion received, but receiver slot could not be verified",
                        )?;
                        ensure!(
                            pairing.len() >= 8
                                && pairing[1] & 0x0f == candidate.kind
                                && u16::from_le_bytes([pairing[2], pairing[3]])
                                    == candidate.product
                                && pairing[4..8].iter().any(|byte| *byte != 0),
                            "pairing completion did not produce the expected device record"
                        );
                        return Ok(slot);
                    }
                    _ => bail!("receiver cancelled Bolt pairing without completion"),
                }
            }
            _ => {}
        }
    }
}

pub(super) fn run<T: HidTransport, U: ReceiverUi + ?Sized>(
    client: &mut Client<T>,
    ui: &mut U,
    timeout: u8,
) -> Result<()> {
    let flags = client.register(0xff, 0, &[], false)?;
    let result = (|| -> Result<()> {
        ensure!(!ui.cancelled()?, "pairing cancelled");
        ui.status(
            "Find your device",
            &format!("Hold the device's Easy-Switch button until its light blinks. Searching for up to {timeout} seconds."),
        )?;
        client.write_register(0xff, 0, &[flags[0], flags[1] | 1, flags[2]])?;
        let candidates = discover(client, ui, timeout)?;
        client.write_register(0xff, 0xc0, &[0, 2, 0])?;
        let chosen = ui
            .select(
                "Choose a device",
                "",
                candidates
                    .iter()
                    .map(|candidate| Choice {
                        label: candidate
                            .name
                            .clone()
                            .unwrap_or_else(|| "Unnamed device".into()),
                        detail: format!(
                            "{} · {}",
                            if candidate.kind == 1 {
                                "Keyboard"
                            } else {
                                "Mouse"
                            },
                            hex(&candidate.address.unwrap_or_default())
                        ),
                    })
                    .collect(),
            )?
            .context("pairing cancelled")?;
        let candidate = candidates
            .get(chosen)
            .context("invalid pairing selection")?;
        ensure!(!ui.cancelled()?, "pairing cancelled");
        ui.status(
            "Pairing your device",
            "Waiting for the receiver's authentication instructions.",
        )?;
        let slot = authenticate(client, ui, candidate, timeout)?;
        ui.status(
            "Device paired",
            &format!("Authenticated pairing confirmed in receiver slot {slot}."),
        )?;
        Ok(())
    })();
    // Cleanup is attempted even after a protocol error or user cancellation.
    // Bolt C1 is always a long register, including cancel and unpair.
    let mut cancel = [0u8; 16];
    cancel[0] = 2;
    let stop_pairing = client.write_register(0xff, 0xc1, &cancel);
    let stop_discovery = client.write_register(0xff, 0xc0, &[0, 2, 0]);
    let restore_flags = client.write_register(0xff, 0, &flags[..3]);
    // A completed pairing may legitimately reject a redundant cancel command.
    let stop_pairing = if result.is_ok() { Ok(()) } else { stop_pairing };
    finish_pairing(
        result,
        [
            ("pairing cancellation could not be confirmed", stop_pairing),
            (
                "discovery cancellation could not be confirmed",
                stop_discovery,
            ),
            ("receiver notification restoration failed", restore_flags),
        ],
    )
}

pub(super) fn unpair(device: &Device) -> Result<()> {
    let slot = device.slot.context("device has no receiver slot")?;
    ensure!((1..=6).contains(&slot), "invalid Bolt receiver slot");
    let mut client = super::open_route(device)?;
    let mut payload = [0u8; 16];
    payload[0] = 3;
    payload[1] = slot;
    client.write_register(0xff, 0xc1, &payload)?;
    match client.register(0xff, 0xb5, &[0x50 + slot], true) {
        Err(error) if legacy_error(&error, &[2, 3, 8]) => Ok(()),
        Ok(data) if data[1..].iter().all(|byte| *byte == 0) => Ok(()),
        Err(error) => {
            Err(error).context("unpair acknowledged but receiver state could not be verified")
        }
        Ok(_) => bail!("receiver acknowledged unpair but the pairing remains present"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hidpp::testing::{Recording as Fake, packet};
    use std::collections::VecDeque;

    fn event(command: u8, address: u8, data: &[u8]) -> Vec<u8> {
        packet(0xff, command, address, data, true)
    }

    #[derive(Default)]
    struct FakeUi {
        prompts: Vec<String>,
        choices: Vec<Choice>,
        cancel: bool,
        decline: bool,
    }
    impl ReceiverUi for FakeUi {
        fn status(&mut self, _: &str, message: &str) -> Result<()> {
            self.prompts.push(message.into());
            Ok(())
        }
        fn select(&mut self, _: &str, _: &str, choices: Vec<Choice>) -> Result<Option<usize>> {
            self.choices = choices;
            Ok((!self.decline).then_some(0))
        }
        fn cancelled(&mut self) -> Result<bool> {
            Ok(self.cancel)
        }
    }
    fn mouse() -> Candidate {
        Candidate {
            name: Some("MX Master 4".into()),
            address: Some([1, 2, 3, 4, 5, 6]),
            kind: 2,
            product: 0xb042,
            authentication: 0,
        }
    }

    #[test]
    fn discovery_fragments_are_correlated_by_counter() {
        let mut candidates = Discovery::default();
        let mut details = [0; 16];
        details[3] = 2;
        details[6..12].copy_from_slice(&[1, 2, 3, 4, 5, 6]);
        discovery_record(
            &Packet::decode(&event(0x4f, 1, &details)).expect("valid test fixture"),
            &mut candidates,
        )
        .expect("valid test fixture");
        let mut name = vec![0, 1, 11];
        name.extend_from_slice(b"MX Master 4");
        discovery_record(
            &Packet::decode(&event(0x4f, 2, &name)).expect("valid test fixture"),
            &mut candidates,
        )
        .expect("valid test fixture");
        assert!(!candidates.pending[&1].complete());
        assert!(!candidates.pending[&2].complete());
        assert!(candidates.devices.is_empty());
        discovery_record(
            &Packet::decode(&event(0x4f, 1, &name)).expect("valid test fixture"),
            &mut candidates,
        )
        .expect("valid test fixture");
        assert!(!candidates.pending.contains_key(&1));
        assert!(!candidates.pending[&2].complete());
        assert!(candidates.devices[&[1, 2, 3, 4, 5, 6]].complete());
        assert!(candidates.has_supported_device());
    }

    fn fragments(counter: u16, candidate: &Candidate) -> [Vec<u8>; 2] {
        let [low, high] = counter.to_le_bytes();
        let mut details = [0; 16];
        details[0] = high;
        details[3] = candidate.kind;
        details[4..6].copy_from_slice(&candidate.product.to_le_bytes());
        details[6..12].copy_from_slice(&candidate.address.expect("test address"));
        details[14] = candidate.authentication;
        let name = candidate.name.as_deref().expect("test name").as_bytes();
        let mut named = vec![high, 1, name.len() as u8];
        named.extend_from_slice(name);
        [event(0x4f, low, &details), event(0x4f, low, &named)]
    }

    fn add(discovery: &mut Discovery, report: &[u8]) -> Result<()> {
        discovery_record(
            &Packet::decode(report).context("invalid test report")?,
            discovery,
        )
    }

    #[test]
    fn repeated_announcements_have_one_candidate_and_do_not_exhaust_record_limit() {
        let mut discovery = Discovery::default();
        // Reproduce repeated counters in the user's trace and exceed the old
        // raw-record bound to verify repeated advertisements stay bounded.
        for counter in 0..300 {
            let mut reports = fragments(counter, &mouse());
            if counter % 2 != 0 {
                reports.reverse();
            }
            for report in reports {
                add(&mut discovery, &report).expect("valid repeated announcement");
            }
            assert_eq!(discovery.devices.len(), 1);
            assert!(discovery.pending.is_empty());
        }
        assert_eq!(discovery.into_supported_devices().len(), 1);
    }

    #[test]
    fn identical_names_with_different_addresses_remain_distinct() {
        let mut discovery = Discovery::default();
        for (counter, address) in [(1, [1, 2, 3, 4, 5, 6]), (2, [6, 5, 4, 3, 2, 1])] {
            for report in fragments(
                counter,
                &Candidate {
                    address: Some(address),
                    ..mouse()
                },
            ) {
                add(&mut discovery, &report).expect("different mouse with same name");
            }
        }
        let devices = discovery.into_supported_devices();
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].name, devices[1].name);
        assert_ne!(devices[0].address, devices[1].address);
    }

    #[test]
    fn repeated_address_cannot_change_identity_or_authentication_details() {
        for changed in [
            Candidate { kind: 1, ..mouse() },
            Candidate {
                product: 0xbeef,
                ..mouse()
            },
            Candidate {
                authentication: 1,
                ..mouse()
            },
        ] {
            let mut discovery = Discovery::default();
            for report in fragments(1, &mouse()) {
                add(&mut discovery, &report).expect("original device");
            }
            let reports = fragments(2, &changed);
            add(&mut discovery, &reports[1]).expect("name arrives first");
            assert!(add(&mut discovery, &reports[0]).is_err());
            assert_eq!(discovery.devices.len(), 1);
        }
    }

    #[test]
    fn one_counter_cannot_combine_conflicting_device_details() {
        let mut discovery = Discovery::default();
        add(&mut discovery, &fragments(1, &mouse())[0]).expect("first fragment");
        let changed = Candidate {
            address: Some([6, 5, 4, 3, 2, 1]),
            ..mouse()
        };
        assert!(add(&mut discovery, &fragments(1, &changed)[0]).is_err());
        assert!(discovery.devices.is_empty());
    }

    #[test]
    fn discovery_bounds_pending_fragments_and_distinct_devices() {
        let mut pending = Discovery::default();
        let mut distinct = Discovery::default();
        for counter in 0..MAX_DISCOVERY_RECORDS as u16 {
            add(&mut pending, &fragments(counter, &mouse())[1]).expect("bounded pending name");
            let candidate = Candidate {
                address: Some([counter as u8, 2, 3, 4, 5, 6]),
                ..mouse()
            };
            for report in fragments(counter, &candidate) {
                add(&mut distinct, &report).expect("bounded distinct device");
            }
        }
        assert!(add(&mut pending, &fragments(100, &mouse())[1]).is_err());
        let new = Candidate {
            address: Some([100, 2, 3, 4, 5, 6]),
            ..mouse()
        };
        add(&mut distinct, &fragments(100, &new)[0]).expect("new details");
        assert!(add(&mut distinct, &fragments(100, &new)[1]).is_err());
        assert_eq!(distinct.devices.len(), MAX_DISCOVERY_RECORDS);
        // Unknown fragments neither create nor overwrite records.
        add(&mut pending, &event(0x4f, 100, &[0, 2, 0])).expect("reserved fragment");
        assert_eq!(pending.pending.len(), MAX_DISCOVERY_RECORDS);
    }

    #[test]
    fn discovery_omits_incomplete_and_unsupported_devices() {
        let mut discovery = Discovery::default();
        for report in fragments(1, &Candidate { kind: 7, ..mouse() }) {
            add(&mut discovery, &report).expect("unsupported device");
        }
        add(&mut discovery, &fragments(2, &mouse())[1]).expect("name without details");
        assert!(!discovery.has_supported_device());
        assert!(discovery.into_supported_devices().is_empty());
    }

    #[test]
    fn full_discovery_shows_one_choice_for_repeated_device_and_cancel_cleans_up() {
        let mut replies = vec![
            event(0x81, 0, &[0x10, 0x08, 0x40]),
            event(0x80, 0, &[]),
            event(0x80, 0xc0, &[]),
        ];
        for counter in 1..=17 {
            replies.extend(fragments(counter, &mouse()));
        }
        replies.extend([
            event(0x53, 1, &[0]),
            event(0x80, 0xc0, &[]),
            event(0x82, 0xc1, &[]),
            event(0x80, 0xc0, &[]),
            event(0x80, 0, &[]),
        ]);
        let mut client = Client::new(Fake::new(replies));
        let mut ui = FakeUi {
            decline: true,
            ..Default::default()
        };
        assert!(
            run(&mut client, &mut ui, 10)
                .expect_err("cancelled selection")
                .to_string()
                .contains("cancelled")
        );
        assert_eq!(ui.choices.len(), 1);
        let writes = client.into_transport().requests;
        assert!(!writes.iter().any(|report| report[2..5] == [0x82, 0xc1, 1]));
        assert_eq!(writes[writes.len() - 3][2..5], [0x82, 0xc1, 2]);
        assert_eq!(writes[writes.len() - 2][2..7], [0x80, 0xc0, 0, 2, 0]);
        assert_eq!(writes[writes.len() - 1][2..7], [0x80, 0, 0x10, 0x08, 0x40]);
    }

    #[test]
    fn short_discovery_keeps_complete_candidates_when_settling_would_exceed_timeout() {
        let mut replies = vec![event(0x80, 0xc0, &[])];
        replies.extend(fragments(1, &mouse()));
        // No receiver close event: the local scan deadline must still return
        // this device, even though its two-second settle time has not elapsed.
        let mut client = Client::new(Fake::new(replies));
        let candidates =
            discover(&mut client, &mut FakeUi::default(), 1).expect("completed discovery");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].address, mouse().address);
    }

    #[test]
    fn discovery_accepts_keyboard_and_mouse_kinds_without_a_model_allowlist() {
        for kind in [1, 2] {
            assert!(
                Candidate {
                    name: Some("Future Logitech input device".into()),
                    kind,
                    ..mouse()
                }
                .supported()
            );
        }
        for kind in [0, 4, 7, 255] {
            assert!(!Candidate { kind, ..mouse() }.supported());
        }
    }

    #[test]
    fn authenticates_mouse_then_requires_a_real_pairing_record() {
        let mut completed = [0; 16];
        completed[7] = 2;
        let fake = Fake::new(VecDeque::from([
            event(0x82, 0xc1, &[]),
            event(0x4d, 0, b"000001"),
            event(0x54, 2, &completed),
            event(0x83, 0xb5, &[0x52, 2, 0x42, 0xb0, 1, 2, 3, 4]),
        ]));
        let mut client = Client::new(fake);
        let mut ui = FakeUi::default();
        assert_eq!(
            authenticate(&mut client, &mut ui, &mouse(), 10).expect("valid test fixture"),
            2
        );
        assert!(ui.prompts[0].contains("left, right. Then"));
    }

    #[test]
    fn completion_without_challenge_is_rejected() {
        let mut completed = [0; 16];
        completed[7] = 1;
        let mut client = Client::new(Fake::new(VecDeque::from([
            event(0x82, 0xc1, &[]),
            event(0x54, 2, &completed),
        ])));
        assert!(
            authenticate(&mut client, &mut FakeUi::default(), &mouse(), 10)
                .expect_err("expected test failure")
                .to_string()
                .contains("without authenticated")
        );
    }

    #[test]
    fn cancellation_and_malformed_challenge_are_errors() {
        let mut client = Client::new(Fake::new(VecDeque::from([event(0x82, 0xc1, &[])])));
        let mut ui = FakeUi {
            cancel: true,
            ..Default::default()
        };
        assert!(
            authenticate(&mut client, &mut ui, &mouse(), 10)
                .expect_err("expected test failure")
                .to_string()
                .contains("cancelled")
        );
        assert!(passkey_instruction(&mouse(), b"abcdef").is_err());
        assert!(passkey_instruction(&mouse(), b"123456").is_err());
        let keyboard = Candidate {
            kind: 1,
            authentication: 1,
            ..mouse()
        };
        assert!(
            passkey_instruction(&keyboard, b"123456")
                .expect("valid test fixture")
                .contains("123456 and press Enter")
        );
    }
}
