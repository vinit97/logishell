//! Small HID++ transport, with no device-model assumptions.
//!
//! Packet framing follows Logitech's public HID++ 1.0 receiver specification
//! and cpg-docs HID++ 2.0 root feature documentation. Device feature adapters
//! live in `device`; this layer knows only requests, responses, and events.

use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::VecDeque,
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    time::{Duration, Instant},
};

const CLIENT_ID: u8 = 0x0b;
pub const REQUEST_TIMEOUT: Duration = Duration::from_millis(700);
const PING_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub device: u8,
    pub command: u8,
    pub address: u8,
    pub data: Vec<u8>,
}

impl Packet {
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let expected = match bytes.first()? {
            0x10 => 7,
            0x11 => 20,
            _ => return None,
        };
        if bytes.len() != expected {
            return None;
        }
        Some(Self {
            device: bytes[1],
            command: bytes[2],
            address: bytes[3],
            data: bytes[4..].to_vec(),
        })
    }

    fn encode(device: u8, command: u8, address: u8, data: &[u8], long: bool) -> Result<Vec<u8>> {
        let len = if long { 20 } else { 7 };
        ensure!(data.len() <= len - 4, "HID++ request payload is too large");
        let mut bytes = vec![0; len];
        bytes[..4].copy_from_slice(&[if long { 0x11 } else { 0x10 }, device, command, address]);
        bytes[4..4 + data.len()].copy_from_slice(data);
        Ok(bytes)
    }
}

#[derive(Debug)]
pub struct ProtocolError {
    pub version: u8,
    pub code: u8,
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let description = match (self.version, self.code) {
            (1, 1) => "invalid command (possibly a HID++ 1.0 device)",
            (1, 2) => "unsupported register",
            (1, 4) => {
                "device is unreachable; wake it and select this receiver's Easy-Switch channel"
            }
            (1, 7) | (2, 8) => "device is busy",
            (1, 8) => "device is offline or pairing slot is empty",
            (1, 9) => "device resources unavailable",
            (1, 12) => "incorrect pairing passkey",
            (2, 2) => "invalid argument",
            (2, 6) => "unsupported feature",
            (2, 7) => "unsupported function",
            _ => "device rejected the request",
        };
        write!(
            f,
            "HID++ {} error 0x{:02x}: {description}",
            self.version, self.code
        )
    }
}

impl std::error::Error for ProtocolError {}

#[derive(Debug)]
pub(crate) struct RequestTimeout {
    device: u8,
    command: u8,
    address: u8,
}

impl std::fmt::Display for RequestTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "HID++ request timed out (device 0x{:02x}, command 0x{:02x}, address 0x{:02x})",
            self.device, self.command, self.address
        )
    }
}

impl std::error::Error for RequestTimeout {}

pub(crate) fn legacy_error(error: &anyhow::Error, codes: &[u8]) -> bool {
    error
        .downcast_ref::<ProtocolError>()
        .is_some_and(|error| error.version == 1 && codes.contains(&error.code))
}

pub trait Transport {
    fn send(&mut self, report: &[u8]) -> Result<()>;
    fn receive(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>>;
}

pub struct Hidraw {
    file: File,
}

impl Hidraw {
    pub fn open(path: &str) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .with_context(|| {
                format!(
                    "cannot open {path}; check Logitech device-access permissions (see packaging/README.md)"
                )
            })?;
        Ok(Self { file })
    }

    fn ready(&self, events: i16, timeout: Duration) -> io::Result<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            let mut descriptor = libc::pollfd {
                fd: self.file.as_raw_fd(),
                events,
                revents: 0,
            };
            // SAFETY: descriptor points to one valid pollfd for this call;
            // the owned file remains open for the duration of poll.
            let result = unsafe {
                libc::poll(
                    &mut descriptor,
                    1,
                    remaining.as_millis().clamp(1, i32::MAX as u128) as i32,
                )
            };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if descriptor.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "HID device disconnected",
                ));
            }
            return Ok(result != 0 && descriptor.revents & events != 0);
        }
    }
}

impl Transport for Hidraw {
    fn send(&mut self, report: &[u8]) -> Result<()> {
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            match self.file.write(report) {
                Ok(n) if n == report.len() => return Ok(()),
                Ok(_) => bail!("incomplete HID report write"),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    ensure!(
                        self.ready(
                            libc::POLLOUT,
                            deadline.saturating_duration_since(Instant::now())
                        )?,
                        "timed out writing HID report"
                    );
                }
                Err(error) => return Err(error).context("writing HID report"),
            }
            ensure!(Instant::now() < deadline, "timed out writing HID report");
        }
    }

    fn receive(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
        let deadline = Instant::now() + timeout;
        loop {
            if !self.ready(
                libc::POLLIN,
                deadline.saturating_duration_since(Instant::now()),
            )? {
                return Ok(None);
            }
            let mut bytes = [0u8; 64];
            match self.file.read(&mut bytes) {
                Ok(0) => bail!("HID device disconnected"),
                Ok(n) => return Ok(Some(bytes[..n].to_vec())),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                    ) => {}
                Err(error) => return Err(error).context("reading HID report"),
            }
        }
    }
}

pub struct Client<T: Transport> {
    transport: T,
    events: VecDeque<Packet>,
    client_id: u8,
    direct_addressing: bool,
}

impl<T: Transport> Client<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            events: VecDeque::new(),
            client_id: CLIENT_ID,
            direct_addressing: false,
        }
    }

    pub fn with_client_id(transport: T, client_id: u8) -> Result<Self> {
        ensure!(
            (1..=15).contains(&client_id),
            "HID++ software ID must be 1..15"
        );
        Ok(Self {
            client_id,
            ..Self::new(transport)
        })
    }

    /// Direct Bluetooth/USB peripherals may reply with index 0 instead of
    /// 0xff. Never enable this alias for a receiver with independently routed slots.
    pub fn with_direct_addressing(mut self) -> Self {
        self.direct_addressing = true;
        self
    }

    #[cfg(test)]
    pub(crate) fn into_transport(self) -> T {
        self.transport
    }

    pub fn request(
        &mut self,
        device: u8,
        command: u8,
        address: u8,
        data: &[u8],
        long: bool,
    ) -> Result<Vec<u8>> {
        self.request_with_timeout(device, command, address, data, long, REQUEST_TIMEOUT)
    }

    fn request_with_timeout(
        &mut self,
        device: u8,
        command: u8,
        address: u8,
        data: &[u8],
        long: bool,
        timeout: Duration,
    ) -> Result<Vec<u8>> {
        self.transport
            .send(&Packet::encode(device, command, address, data, long)?)?;
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let raw = if remaining.is_zero() {
                None
            } else {
                self.transport.receive(remaining)?
            };
            let raw = raw.ok_or(RequestTimeout {
                device,
                command,
                address,
            })?;
            let Some(mut packet) = Packet::decode(&raw) else {
                continue;
            };
            if self.direct_addressing && packet.device == 0 {
                packet.device = 0xff;
            }
            if packet.device == device {
                if packet.command == command && packet.address == address {
                    // Pairing-information replies echo their subregister. This
                    // prevents a delayed reply to the previous slot matching.
                    if command == 0x83
                        && address == 0xb5
                        && !data.is_empty()
                        && packet.data.first() != data.first()
                    {
                        continue;
                    }
                    if command == 0
                        && address >> 4 == 1
                        && data.len() >= 3
                        && packet.data[2] != data[2]
                    {
                        // Root protocol probes echo the caller's marker.
                        continue;
                    }
                    return Ok(packet.data);
                }
                if matches!(packet.command, 0x8f | 0xff)
                    && packet.address == command
                    && packet.data[0] == address
                {
                    return Err(ProtocolError {
                        version: if packet.command == 0x8f { 1 } else { 2 },
                        code: packet.data[1],
                    }
                    .into());
                }
            }
            // Ignore replies belonging to other applications. Keep bounded
            // unsolicited events needed by pairing and button diversion. Losing
            // one can lose a key release, so overflow must stop the operation.
            if packet.command < 0x80 && packet.address & 0x0f == 0
                || packet.device == 0xff && matches!(packet.command, 0x40..=0x54)
            {
                if self.events.len() == 128 {
                    // The caller must stop processing input and release held
                    // keys. Discard the incomplete stream explicitly so cleanup
                    // requests have room for any further notifications.
                    self.events.clear();
                    bail!(
                        "HID++ notification queue overflowed; stopping because device events were lost"
                    );
                }
                self.events.push_back(packet);
            }
        }
    }

    pub fn feature(&mut self, device: u8, index: u8, function: u8, data: &[u8]) -> Result<Vec<u8>> {
        ensure!(function < 16, "invalid HID++ function");
        self.request(device, index, function << 4 | self.client_id, data, true)
    }

    /// Probe the protocol before feature discovery. Receiver routes support
    /// short reports; direct Bluetooth interfaces may expose only long ones.
    /// Allow extra time for the initial wireless connection without extending
    /// the deadline of every settings request.
    pub fn ping(&mut self, device: u8, long: bool) -> Result<(u8, u8)> {
        let data = self.request_with_timeout(
            device,
            0,
            0x10 | self.client_id,
            &[0, 0, 0x5a],
            long,
            PING_TIMEOUT,
        )?;
        Ok((data[0], data[1]))
    }

    pub fn root_feature(&mut self, device: u8, id: u16) -> Result<Option<(u8, u8)>> {
        let data = self.feature(device, 0, 0, &id.to_be_bytes())?;
        Ok((data[0] != 0).then_some((data[0], data[2])))
    }

    pub fn register(
        &mut self,
        device: u8,
        register: u8,
        data: &[u8],
        long: bool,
    ) -> Result<Vec<u8>> {
        self.request(
            device,
            if long { 0x83 } else { 0x81 },
            register,
            data,
            false,
        )
    }

    pub fn write_register(&mut self, device: u8, register: u8, data: &[u8]) -> Result<()> {
        let long = data.len() > 3;
        self.request(device, if long { 0x82 } else { 0x80 }, register, data, long)?;
        Ok(())
    }

    pub fn next_event(&mut self, timeout: Duration) -> Result<Option<Packet>> {
        if let Some(packet) = self.events.pop_front() {
            return Ok(Some(packet));
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            let Some(raw) = self.transport.receive(remaining)? else {
                return Ok(None);
            };
            if let Some(mut packet) = Packet::decode(&raw) {
                if self.direct_addressing && packet.device == 0 {
                    packet.device = 0xff;
                }
                return Ok(Some(packet));
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    pub fn packet(device: u8, command: u8, address: u8, data: &[u8], long: bool) -> Vec<u8> {
        Packet::encode(device, command, address, data, long).expect("valid test packet")
    }

    /// Event-driven receiver scripts record writes while supplying notifications
    /// and replies in wire order. Tests explicitly inspect the resulting writes.
    pub struct Recording {
        pub requests: Vec<Vec<u8>>,
        pub responses: VecDeque<Vec<u8>>,
    }

    impl Recording {
        pub fn new(responses: impl IntoIterator<Item = Vec<u8>>) -> Self {
            Self {
                requests: Vec::new(),
                responses: responses.into_iter().collect(),
            }
        }
    }

    impl Transport for Recording {
        fn send(&mut self, report: &[u8]) -> Result<()> {
            self.requests.push(report.to_vec());
            Ok(())
        }

        fn receive(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
            let response = self.responses.pop_front();
            if response.is_none() {
                std::thread::sleep(timeout.min(Duration::from_millis(5)));
            }
            Ok(response)
        }
    }

    /// A command/readback script rejects every unexpected request and exposes
    /// remaining exchanges so tests verify the complete protocol interaction.
    pub struct Scripted {
        pub exchanges: VecDeque<(Vec<u8>, Vec<u8>)>,
        pending: Option<Vec<u8>>,
    }

    impl Scripted {
        pub fn new(exchanges: impl IntoIterator<Item = (Vec<u8>, Vec<u8>)>) -> Self {
            Self {
                exchanges: exchanges.into_iter().collect(),
                pending: None,
            }
        }
    }

    impl Transport for Scripted {
        fn send(&mut self, report: &[u8]) -> Result<()> {
            assert!(
                self.pending.is_none(),
                "previous scripted reply was not consumed"
            );
            let (expected, response) = self.exchanges.pop_front().expect("unexpected device write");
            assert_eq!(report, expected, "unexpected scripted request bytes");
            self.pending = Some(response);
            Ok(())
        }

        fn receive(&mut self, _: Duration) -> Result<Option<Vec<u8>>> {
            Ok(self.pending.take())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        expected: Vec<u8>,
        replies: VecDeque<Vec<u8>>,
    }
    impl Transport for Fake {
        fn send(&mut self, report: &[u8]) -> Result<()> {
            assert_eq!(report, self.expected);
            Ok(())
        }
        fn receive(&mut self, _: Duration) -> Result<Option<Vec<u8>>> {
            Ok(self.replies.pop_front())
        }
    }
    fn packet(device: u8, command: u8, address: u8, data: &[u8]) -> Vec<u8> {
        Packet::encode(device, command, address, data, true).expect("valid test fixture")
    }

    struct PingFake(Fake);
    impl Transport for PingFake {
        fn send(&mut self, report: &[u8]) -> Result<()> {
            self.0.send(report)
        }
        fn receive(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
            assert!(timeout > REQUEST_TIMEOUT && timeout <= PING_TIMEOUT);
            self.0.receive(timeout)
        }
    }

    #[test]
    fn ping_uses_transport_report_format_and_matches_probe_marker() -> Result<()> {
        for (device, long, expected, replies) in [
            (
                5,
                false,
                vec![0x10, 5, 0, 0x1b, 0, 0, 0x5a],
                vec![
                    packet(5, 0, 0x1b, &[4, 5, 0x11]), // Another probe's marker.
                    packet(5, 0, 0x1b, &[4, 5, 0x5a]),
                ],
            ),
            (
                0xff,
                true,
                packet(0xff, 0, 0x1b, &[0, 0, 0x5a]),
                vec![vec![0x10, 0xff, 0, 0x1b, 4, 5, 0x5a]],
            ),
        ] {
            let mut client = Client::new(PingFake(Fake {
                expected,
                replies: replies.into(),
            }));
            assert_eq!(client.ping(device, long)?, (4, 5), "device {device}");
        }
        Ok(())
    }

    #[test]
    fn unreachable_ping_is_an_error_not_a_legacy_protocol_response() {
        let mut client = Client::new(PingFake(Fake {
            expected: vec![0x10, 1, 0, 0x1b, 0, 0, 0x5a],
            replies: VecDeque::from([vec![0x10, 1, 0x8f, 0, 0x1b, 4, 0]]),
        }));
        let error = client.ping(1, false).expect_err("device is unreachable");
        let protocol_error = error
            .downcast_ref::<ProtocolError>()
            .expect("protocol error");
        assert_eq!((protocol_error.version, protocol_error.code), (1, 4));
        assert!(error.to_string().contains("Easy-Switch"));
    }

    #[test]
    fn matches_device_function_and_software_id_and_ignores_input() {
        let mut client = Client::new(Fake {
            expected: packet(2, 5, 0x1b, &[0]),
            replies: VecDeque::from([
                vec![2, 2, 0xff, 5, 0x1b, 7, 0],
                packet(3, 5, 0x1b, &[1]),
                packet(2, 5, 0x1a, &[2]),
                packet(2, 5, 0x2b, &[3]),
                packet(2, 5, 0x1b, &[42]),
            ]),
        });
        assert_eq!(
            client.feature(2, 5, 1, &[0]).expect("valid test fixture")[0],
            42
        );
    }

    #[test]
    fn direct_index_zero_alias_is_never_applied_to_receiver_routes() -> Result<()> {
        let make_transport = || Fake {
            expected: packet(0xff, 5, 0x0b, &[]),
            replies: VecDeque::from([packet(0, 5, 0x0b, &[42])]),
        };
        let mut direct = Client::new(make_transport()).with_direct_addressing();
        assert_eq!(direct.feature(0xff, 5, 0, &[])?[0], 42);
        let mut receiver = Client::new(make_transport());
        assert!(receiver.feature(0xff, 5, 0, &[]).is_err());
        Ok(())
    }

    #[test]
    fn matches_error_to_original_request() {
        let mut client = Client::new(Fake {
            expected: packet(1, 2, 0x0b, &[]),
            replies: VecDeque::from([
                packet(1, 0xff, 2, &[0x0a, 7]),
                packet(1, 0xff, 2, &[0x0b, 6]),
            ]),
        });
        let err = client
            .feature(1, 2, 0, &[])
            .expect_err("expected test failure");
        assert!(!err.is::<RequestTimeout>());
        assert_eq!(
            err.downcast_ref::<ProtocolError>()
                .expect("valid test fixture")
                .code,
            6
        );
    }

    #[test]
    fn truncated_or_unrelated_packets_cannot_become_responses() {
        for length in 0..20 {
            let bytes = vec![0x11; length];
            assert!(Packet::decode(&bytes).is_none());
        }
        assert!(Packet::decode(&[0x12; 20]).is_none());
        let mut client = Client::new(Fake {
            expected: packet(1, 0, 0x1b, &[]),
            replies: VecDeque::new(),
        });
        let error = client
            .feature(1, 0, 1, &[])
            .expect_err("missing reply must time out");
        assert!(error.to_string().contains("timed out"));
        assert!(error.context("read reporting").is::<RequestTimeout>());
    }

    #[test]
    fn retains_unsolicited_pairing_events_while_waiting_for_ack() {
        let mut client = Client::new(Fake {
            expected: Packet::encode(0xff, 0x80, 0xb2, &[1, 0, 30], false)
                .expect("valid test fixture"),
            replies: VecDeque::from([packet(0xff, 0x4a, 1, &[0]), packet(0xff, 0x80, 0xb2, &[0])]),
        });
        client
            .write_register(0xff, 0xb2, &[1, 0, 30])
            .expect("valid test fixture");
        assert_eq!(
            client
                .next_event(Duration::from_millis(1))
                .expect("valid test fixture")
                .expect("valid test fixture")
                .command,
            0x4a
        );
    }

    #[test]
    fn notification_overflow_fails_instead_of_dropping_a_release() -> Result<()> {
        let mut replies = VecDeque::from([packet(2, 9, 0, &[])]);
        // Unrelated receiver notifications can arrive while a worker waits for
        // its heartbeat reply. The oldest release must never be silently lost.
        replies.extend((0..128).map(|_| packet(0xff, 0x4a, 1, &[0])));
        replies.push_back(packet(2, 9, 0x2b, &[0, 0x53, 1, 0, 0]));
        let mut client = Client::new(Fake {
            expected: packet(2, 9, 0x2b, &[0, 0x53]),
            replies,
        });
        let error = client.feature(2, 9, 2, &[0, 0x53]).expect_err("overflow");
        assert!(error.to_string().contains("notification queue overflowed"));
        assert!(
            client.events.is_empty(),
            "cleanup requests need room for new notifications"
        );
        Ok(())
    }

    #[test]
    fn full_notification_queue_preserves_every_event_without_overflow() -> Result<()> {
        let mut replies: VecDeque<_> = (0..128)
            .map(|index| packet(0xff, 0x4a, 1, &[index]))
            .collect();
        replies.push_back(packet(2, 9, 0x2b, &[0, 0x53, 1, 0, 0]));
        let mut client = Client::new(Fake {
            expected: packet(2, 9, 0x2b, &[0, 0x53]),
            replies,
        });
        client.feature(2, 9, 2, &[0, 0x53])?;
        for index in 0..128 {
            let event = client
                .next_event(Duration::from_millis(1))?
                .expect("queued notification");
            assert_eq!(event.data[0], index);
        }
        assert!(client.events.is_empty());
        Ok(())
    }
}
