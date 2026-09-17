//! Unifying receiver pairing, independently implemented from Logitech's public
//! HID++ 1.0 specification, sections 3.3, 4.1, 4.3 and 4.5:
//! <https://lekensteyn.nl/files/logitech/logitech_hidpp10_specification_for_Unifying_Receivers.pdf>
//! Receiver interoperability research also masks the device-kind nibble:
//! <https://github.com/pwr-Solaar/Solaar/blob/master/lib/logitech_receiver/receiver.py>
//!
//! Unlike Bolt, Unifying has no candidate-selection phase or authentication
//! challenge. Opening its pairing lock admits the next compatible requesting
//! device. Require explicit terminal consent and verify a newly occupied slot.

use super::{ReceiverUi, finish_pairing, hex};
use crate::{
    hidpp::{Client, Transport as HidTransport, legacy_error},
    model::{Device, Transport},
};
use anyhow::{Context, Result, bail, ensure};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Pairing {
    product: u16,
    kind: u8,
    serial: [u8; 4],
}

fn record<T: HidTransport>(client: &mut Client<T>, slot: u8) -> Result<Option<Pairing>> {
    ensure!((1..=6).contains(&slot), "invalid Unifying receiver slot");
    let data = match client.register(0xff, 0xb5, &[0x20 + slot - 1], true) {
        Ok(data) => data,
        Err(error) if legacy_error(&error, &[3, 8]) => {
            return Ok(None);
        }
        Err(error) => return Err(error).context("cannot verify Unifying pairing table"),
    };
    ensure!(data.len() >= 8, "truncated Unifying pairing record");
    if data[1..].iter().all(|byte| *byte == 0) {
        return Ok(None);
    }
    let serial = client.register(0xff, 0xb5, &[0x30 + slot - 1], true)?;
    ensure!(serial.len() >= 5, "truncated Unifying device serial");
    Ok(Some(Pairing {
        product: u16::from_be_bytes([data[3], data[4]]),
        // Reserved upper bits must not turn a valid keyboard or mouse into a
        // different kind (or cause its just-created pairing to be removed).
        kind: data[7] & 0x0f,
        serial: serial[1..5].try_into().expect("length checked"),
    }))
}

fn table<T: HidTransport>(client: &mut Client<T>) -> Result<Vec<Option<Pairing>>> {
    (1..=6).map(|slot| record(client, slot)).collect()
}

fn new_pairing(
    before: &[Option<Pairing>],
    after: &[Option<Pairing>],
) -> Result<Option<(u8, Pairing)>> {
    ensure!(
        before.len() == 6 && after.len() == 6,
        "incomplete Unifying pairing table"
    );
    let mut added = None;
    for (index, (previous, current)) in before.iter().zip(after).enumerate() {
        if previous.is_some() {
            ensure!(
                previous == current,
                "an existing receiver pairing changed during pairing; refresh status before continuing"
            );
        } else if let Some(current) = current {
            ensure!(
                added.is_none(),
                "multiple devices paired; cannot identify the intended device, inspect `logishell status`"
            );
            ensure!(
                current.product != 0 && current.serial.iter().any(|byte| *byte != 0),
                "new pairing has no verifiable device identity; inspect `logishell status`"
            );
            added = Some((index as u8 + 1, current.clone()));
        }
    }
    Ok(added)
}

fn remove<T: HidTransport>(client: &mut Client<T>, slot: u8, expected: &Pairing) -> Result<()> {
    ensure!(
        record(client, slot)?.as_ref() == Some(expected),
        "receiver slot identity changed; refusing to unpair its new occupant"
    );
    client.write_register(0xff, 0xb2, &[3, slot, 0])?;
    ensure!(
        record(client, slot)?.is_none(),
        "receiver acknowledged unpair but its pairing remains present"
    );
    Ok(())
}

pub(super) fn run<T: HidTransport, U: ReceiverUi + ?Sized>(
    client: &mut Client<T>,
    ui: &mut U,
    timeout: u8,
) -> Result<()> {
    let before = table(client)?;
    ensure!(
        before.iter().any(Option::is_none),
        "the Unifying receiver has no unused pairing slots; explicitly unpair a device first"
    );
    ensure!(ui.confirm(
        "Open this receiver for pairing?",
        "Unifying pairs the next compatible device requesting pairing, without a candidate list. Keep other devices out of pairing mode. After confirming, switch your intended keyboard or mouse off and on, or select its pairing channel.",
    )?, "pairing cancelled");
    let flags = client.register(0xff, 0, &[], false)?;
    let result = (|| -> Result<()> {
        client.write_register(0xff, 0, &[flags[0], flags[1] | 1, flags[2]])?;
        ensure!(!ui.cancelled()?, "pairing cancelled");
        client.write_register(0xff, 0xb2, &[1, 0, timeout])?;
        ui.status(
            "Find your device",
            "The receiver is ready. Switch your intended keyboard or mouse off and on, or select its pairing channel.",
        )?;
        let deadline = Instant::now() + Duration::from_secs(u64::from(timeout));
        loop {
            ensure!(
                !ui.cancelled()?,
                "pairing cancelled; check status if the device completed pairing concurrently"
            );
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(!remaining.is_zero(), "Unifying pairing timed out");
            let Some(event) = client.next_event(remaining.min(Duration::from_millis(200)))? else {
                continue;
            };
            if event.device != 0xff || event.command != 0x4a {
                continue;
            }
            ensure!(
                event.data[0] == 0,
                "Unifying pairing failed with receiver error 0x{:02x}",
                event.data[0]
            );
            if event.address & 1 != 0 {
                continue;
            }
            // A delayed old close event must not make an existing pairing look
            // successful. The full table must contain exactly one new identity.
            let Some((slot, pairing)) = new_pairing(&before, &table(client)?)? else {
                continue;
            };
            if !matches!(pairing.kind, 1 | 2 | 3 | 8) {
                remove(client, slot, &pairing).context("an unsupported device paired; its automatic removal failed, inspect receiver status")?;
                bail!(
                    "the receiver paired a device other than a keyboard, mouse, numpad or trackball; that new pairing was removed"
                );
            }
            ui.status(
                "Device paired",
                &format!(
                    "Unifying pairing verified in slot {slot} (wireless product {:04x}, serial {}).",
                    pairing.product,
                    hex(&pairing.serial)
                ),
            )?;
            return Ok(());
        }
    })();
    // Both cleanup requests are attempted even if either fails. The receiver's
    // own finite lock timeout also closes it if the process is interrupted.
    let close = client.write_register(0xff, 0xb2, &[2, 0, 0]);
    let restore = client.write_register(0xff, 0, &flags[..3]);
    finish_pairing(
        result,
        [
            (
                "closing the receiver pairing lock could not be confirmed",
                close,
            ),
            ("restoring receiver notification flags failed", restore),
        ],
    )
}

pub(super) fn unpair(device: &Device) -> Result<()> {
    ensure!(
        device.transport == Transport::Unifying,
        "device is not paired through Unifying"
    );
    let slot = device.slot.context("device has no receiver slot")?;
    let mut client = super::open_route(device)?;
    let pairing = record(&mut client, slot)?.context("receiver slot is already empty")?;
    let receiver_id = device
        .receiver_id
        .as_deref()
        .context("device has no receiver identity")?;
    ensure!(
        pairing.serial.iter().any(|byte| *byte != 0)
            && device.id == format!("{receiver_id}:device:{}", hex(&pairing.serial)),
        "device identity changed; refresh status before unpairing"
    );
    remove(&mut client, slot, &pairing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hidpp::testing::Recording as Fake;

    struct FakeUi {
        consent: bool,
        cancelled: bool,
    }
    impl ReceiverUi for FakeUi {
        fn status(&mut self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }

        fn select(
            &mut self,
            _: &str,
            _: &str,
            _: Vec<crate::terminal::Choice>,
        ) -> Result<Option<usize>> {
            Ok(Some(usize::from(self.consent)))
        }
        fn cancelled(&mut self) -> Result<bool> {
            Ok(self.cancelled)
        }
    }
    fn packet(command: u8, address: u8, data: &[u8]) -> Vec<u8> {
        crate::hidpp::testing::packet(0xff, command, address, data, data.len() > 3)
    }
    fn empty_table() -> Vec<Vec<u8>> {
        (1..=6).map(|_| packet(0x8f, 0x83, &[0xb5, 8, 0])).collect()
    }
    fn mouse() -> Pairing {
        Pairing {
            product: 0x405e,
            kind: 2,
            serial: [1, 2, 3, 4],
        }
    }

    #[test]
    fn table_difference_rejects_overwrites_and_ambiguous_completion() {
        let empty = vec![None; 6];
        let mut one = empty.clone();
        one[2] = Some(mouse());
        assert_eq!(
            new_pairing(&empty, &one).expect("new pairing"),
            Some((3, mouse()))
        );
        assert_eq!(new_pairing(&one, &one).expect("no change"), None);
        let mut two = one.clone();
        two[4] = Some(mouse());
        assert!(new_pairing(&empty, &two).is_err());
        one[2].as_mut().expect("mouse").serial = [5, 6, 7, 8];
        assert!(new_pairing(&two, &one).is_err());
    }

    #[test]
    fn successful_completion_verifies_new_record_and_restores_flags() {
        successful_completion(2);
    }

    #[test]
    fn upper_kind_bits_do_not_cause_a_keyboard_to_be_unpaired() {
        successful_completion(0x81);
    }

    fn successful_completion(raw_kind: u8) {
        let mut replies = empty_table();
        replies.extend([
            packet(0x81, 0, &[0x10, 0x08, 0x40]),
            packet(0x80, 0, &[]),
            packet(0x80, 0xb2, &[]),
            packet(0x4a, 1, &[]),
            packet(0x4a, 0, &[]),
            packet(0x83, 0xb5, &[0x20, 1, 8, 0x40, 0x5e, 0, 0, raw_kind]),
            packet(0x83, 0xb5, &[0x30, 1, 2, 3, 4]),
        ]);
        replies.extend(empty_table().into_iter().take(5));
        replies.extend([packet(0x80, 0xb2, &[]), packet(0x80, 0, &[])]);
        let mut client = Client::new(Fake::new(replies));
        run(
            &mut client,
            &mut FakeUi {
                consent: true,
                cancelled: false,
            },
            10,
        )
        .expect("pairing completes");
        let fake = client.into_transport();
        assert!(
            fake.requests
                .contains(&vec![0x10, 0xff, 0x80, 0xb2, 1, 0, 10])
        );
        assert!(
            !fake
                .requests
                .iter()
                .any(|packet| packet[2..5] == [0x80, 0xb2, 3])
        );
        assert_eq!(
            &fake.requests[fake.requests.len() - 2..],
            &[
                vec![0x10, 0xff, 0x80, 0xb2, 2, 0, 0],
                vec![0x10, 0xff, 0x80, 0, 0x10, 0x08, 0x40],
            ]
        );
    }

    #[test]
    fn declining_consent_never_writes_a_register() {
        let mut client = Client::new(Fake::new(empty_table()));
        assert!(
            run(
                &mut client,
                &mut FakeUi {
                    consent: false,
                    cancelled: false
                },
                10
            )
            .is_err()
        );
        assert!(
            client
                .into_transport()
                .requests
                .iter()
                .all(|packet| packet[2] == 0x83)
        );
    }

    #[test]
    fn cancellation_restores_flags_and_never_opens_the_lock() {
        let mut replies = empty_table();
        replies.extend([
            packet(0x81, 0, &[0, 8, 0]),
            packet(0x80, 0, &[]),
            packet(0x80, 0xb2, &[]),
            packet(0x80, 0, &[]),
        ]);
        let mut client = Client::new(Fake::new(replies));
        assert!(
            run(
                &mut client,
                &mut FakeUi {
                    consent: true,
                    cancelled: true
                },
                10
            )
            .is_err()
        );
        let writes = client.into_transport().requests;
        assert!(!writes.contains(&vec![0x10, 0xff, 0x80, 0xb2, 1, 0, 10]));
        assert_eq!(writes.last(), Some(&vec![0x10, 0xff, 0x80, 0, 0, 8, 0]));
    }

    #[test]
    fn unknown_register_is_not_mistaken_for_an_empty_slot() {
        let mut client = Client::new(Fake::new([packet(0x8f, 0x83, &[0xb5, 2, 0])]));
        assert!(record(&mut client, 1).is_err());
    }

    #[test]
    fn receiver_pairing_error_still_closes_lock_and_restores_flags() {
        let mut replies = empty_table();
        replies.extend([
            packet(0x81, 0, &[0, 8, 0]),
            packet(0x80, 0, &[]),
            packet(0x80, 0xb2, &[]),
            packet(0x4a, 0, &[2, 0, 0]),
            packet(0x80, 0xb2, &[]),
            packet(0x80, 0, &[]),
        ]);
        let mut client = Client::new(Fake::new(replies));
        let error = run(
            &mut client,
            &mut FakeUi {
                consent: true,
                cancelled: false,
            },
            10,
        )
        .expect_err("receiver rejected device");
        assert!(error.to_string().contains("receiver error 0x02"));
        let writes = client.into_transport().requests;
        assert_eq!(
            &writes[writes.len() - 2..],
            &[
                vec![0x10, 0xff, 0x80, 0xb2, 2, 0, 0],
                vec![0x10, 0xff, 0x80, 0, 0, 8, 0],
            ]
        );
    }

    #[test]
    fn unpair_does_not_remove_a_slot_that_changed_identity() {
        let mut client = Client::new(Fake::new([
            packet(0x83, 0xb5, &[0x20, 1, 8, 0x40, 0x5e, 0, 0, 2]),
            packet(0x83, 0xb5, &[0x30, 5, 6, 7, 8]),
        ]));
        assert!(remove(&mut client, 1, &mouse()).is_err());
        assert!(
            client
                .into_transport()
                .requests
                .iter()
                .all(|packet| packet[2] == 0x83)
        );
    }
}
