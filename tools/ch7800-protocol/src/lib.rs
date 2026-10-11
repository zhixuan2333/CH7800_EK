#![no_std]

//! CH7800 binary UART protocol, independent of an operating system or UART driver.
//! Set the chip to 9600 8N1 by default. Transmit frames >=20 ms apart; wait for
//! its startup notification before sending controls. The caller owns the UART and clock.

use core::fmt;

pub const MAX_FRAME: usize = 35; // 30 data bytes + 5 framing bytes

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    TrackRange,
    VolumeRange,
    SequenceRange,
    BaudRange,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TrackRange => "track must be 1..=999",
            Self::VolumeRange => "volume must be 0..=30",
            Self::SequenceRange => "sequence needs 1..=30 tracks numbered 1..=255",
            Self::BaudRange => "unsupported WCH-Link baud rate index",
        })
    }
}

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    bytes: [u8; MAX_FRAME],
    len: usize,
}

impl Frame {
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

#[derive(Clone, Debug)]
pub enum Command<'a> {
    Next,
    Previous,
    Track(u16),
    VolumeUp,
    VolumeDown,
    Volume { level: u8, remember: bool },
    LoopTrack(u16),
    Baud(u8),
    Reset,
    Play,
    Pause,
    LoopAll,
    Stop,
    Shuffle,
    LoopCurrent(bool),
    Sequence(&'a [u8]),
    FactoryReset,
    Status,
    GetVolume,
    FileCount,
    Version,
}

impl Command<'_> {
    pub fn frame(&self) -> Result<Frame> {
        use Command::*;
        let (op, data): (u8, &[u8]) = match self {
            Next => (0x11, &[0, 0]),
            Previous => (0x12, &[0, 0]),
            Track(n) => (0x13, &track(*n)?),
            VolumeUp => (0x14, &[0, 0]),
            VolumeDown => (0x15, &[0, 0]),
            Volume { level, remember } => {
                if *level > 30 {
                    return Err(Error::VolumeRange);
                }
                (0x16, &[u8::from(*remember), *level])
            }
            LoopTrack(n) => (0x18, &track(*n)?),
            Baud(index) if *index <= 7 => (0x0b, &[0, *index]),
            Baud(_) => return Err(Error::BaudRange),
            Reset => (0x0c, &[0, 0]),
            Play => (0x1d, &[0, 0]),
            Pause => (0x1e, &[0, 0]),
            LoopAll => (0x21, &[0, 0]),
            Stop => (0x26, &[0, 0]),
            Shuffle => (0x28, &[0, 1]),
            LoopCurrent(on) => (0x29, &[0, u8::from(!*on)]),
            Sequence(items) => {
                if items.is_empty() || items.len() > 30 || items.contains(&0) {
                    return Err(Error::SequenceRange);
                }
                (0x31, items)
            }
            FactoryReset => (0xd1, &[0, 0]),
            Status => (0x42, &[0, 0]),
            GetVolume => (0x43, &[0, 0]),
            FileCount => (0x4a, &[0, 0]),
            Version => (0x46, &[0, 0]),
        };
        let len = data.len();
        let mut bytes = [0; MAX_FRAME];
        bytes[..4].copy_from_slice(&[0x7e, op, 0, len as u8]);
        bytes[4..4 + len].copy_from_slice(data);
        bytes[4 + len] = 0xef;
        Ok(Frame {
            bytes,
            len: len + 5,
        })
    }

    pub fn response_code(&self) -> Option<u8> {
        match self {
            Self::Status => Some(0x42),
            Self::GetVolume => Some(0x43),
            Self::FileCount => Some(0x4a),
            Self::Version => Some(0x46),
            _ => None,
        }
    }

    pub fn starts_audio(&self) -> bool {
        matches!(
            self,
            Self::Next
                | Self::Previous
                | Self::Track(_)
                | Self::LoopTrack(_)
                | Self::Play
                | Self::LoopAll
                | Self::Shuffle
                | Self::Sequence(_)
        )
    }
}

fn track(n: u16) -> Result<[u8; 2]> {
    if !(1..=999).contains(&n) {
        return Err(Error::TrackRange);
    }
    Ok([(n >> 8) as u8, n as u8])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Response {
    Ack,
    Frame { code: u8, data: [u8; 30], len: u8 },
}

impl Response {
    pub fn payload(&self) -> &[u8] {
        match self {
            Self::Ack => &[],
            Self::Frame { data, len, .. } => &data[..usize::from(*len)],
        }
    }
}

impl fmt::Display for Response {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ack => f.write_str("ACK (received, not proof of playback)"),
            Self::Frame { code, .. } => {
                let data = self.payload();
                let value = data.iter().fold(0u32, |n, b| (n << 8) | u32::from(*b));
                match *code {
                    0x3f if data.len() == 2 => {
                        write!(f, "startup: {} files, device {}", data[0], data[1])
                    }
                    0x3d | 0x3e => write!(f, "track finished: {value}"),
                    0x30 if data.len() == 2 => {
                        write!(f, "track {value} finished (0x30; observed, undocumented)")
                    }
                    0x40 if data.len() == 2 => write!(
                        f,
                        "device error: {} (code {})",
                        error_name(data[1]),
                        data[1]
                    ),
                    0x42 if data.len() == 2 => write!(
                        f,
                        "playback: {}",
                        match data[1] {
                            1 => "playing",
                            2 => "paused",
                            0 => "stopped",
                            _ => "unknown",
                        }
                    ),
                    0x43 if data.len() == 2 => write!(f, "volume: {value}/30"),
                    0x4a if data.len() == 2 => write!(f, "file count: {value}"),
                    0x46 if data.len() == 2 => {
                        write!(f, "firmware version: {:02X}.{:02X}", data[0], data[1])
                    }
                    _ => {
                        write!(f, "undocumented UART frame 0x{code:02X}; data:")?;
                        for byte in data {
                            write!(f, " {byte:02X}")?;
                        }
                        Ok(())
                    }
                }
            }
        }
    }
}

fn error_name(code: u8) -> &'static str {
    match code {
        1 => "initializing/busy",
        2 => "asleep",
        3 => "incomplete frame",
        4 => "checksum error",
        5 => "track out of range",
        6 => "file not found",
        10 => "entering sleep",
        _ => "unknown",
    }
}

pub struct Parser {
    buffer: [u8; MAX_FRAME],
    used: usize,
}

/// Implement this for your MCU's UART transmitter (blocking, DMA, or queued).
/// `write` must preserve the order of all bytes in one frame.
pub trait UartTx {
    type Error;
    fn write(&mut self, bytes: &[u8]) -> core::result::Result<(), Self::Error>;
}

#[derive(Debug, PartialEq, Eq)]
pub enum SendError<E> {
    InvalidCommand(Error),
    Uart(E),
}

/// Allocation-free UART API. Feed RX bytes as they arrive; do not mix RX with TX.
/// Caller enforces >=20 ms between sends and handles timeouts and low-power wake-up.
pub struct Controller<T> {
    tx: T,
    parser: Parser,
}

impl<T: UartTx> Controller<T> {
    pub fn new(tx: T) -> Self {
        Self {
            tx,
            parser: Parser::default(),
        }
    }

    pub fn send(&mut self, command: &Command<'_>) -> core::result::Result<(), SendError<T::Error>> {
        let frame = command.frame().map_err(SendError::InvalidCommand)?;
        self.tx.write(frame.as_bytes()).map_err(SendError::Uart)
    }

    pub fn receive(&mut self, byte: u8) -> Option<Response> {
        self.parser.push(byte)
    }

    pub fn transmitter_mut(&mut self) -> &mut T {
        &mut self.tx
    }

    pub fn into_transmitter(self) -> T {
        self.tx
    }
}

impl Default for Parser {
    fn default() -> Self {
        Self {
            buffer: [0; MAX_FRAME],
            used: 0,
        }
    }
}

impl Parser {
    pub fn push(&mut self, byte: u8) -> Option<Response> {
        if self.used == 0 {
            if byte == 0x7e {
                self.buffer[0] = byte;
                self.used = 1;
            }
            return None;
        }
        if self.used == MAX_FRAME {
            self.resync(byte);
            return None;
        }
        self.buffer[self.used] = byte;
        self.used += 1;
        if self.used == 3 && self.buffer[..3] == [0x7e, 0x41, 0xef] {
            self.used = 0;
            return Some(Response::Ack);
        }
        if self.used >= 4 {
            let len = usize::from(self.buffer[2]) << 8 | usize::from(self.buffer[3]);
            if len > 30 || self.used > len + 5 {
                self.resync(byte);
                return None;
            }
            if self.used == len + 5 {
                if byte == 0xef {
                    let mut data = [0; 30];
                    data[..len].copy_from_slice(&self.buffer[4..4 + len]);
                    let response = Response::Frame {
                        code: self.buffer[1],
                        data,
                        len: len as u8,
                    };
                    self.used = 0;
                    return Some(response);
                }
                self.resync(byte);
            }
        }
        None
    }

    fn resync(&mut self, byte: u8) {
        self.used = 0;
        if byte == 0x7e {
            self.buffer[0] = byte;
            self.used = 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate std;
    use std::string::ToString;
    use std::vec::Vec;

    #[test]
    fn frames_and_limits() {
        assert_eq!(
            Command::Track(1).frame().unwrap().as_bytes(),
            [0x7e, 0x13, 0, 2, 0, 1, 0xef]
        );
        assert_eq!(
            Command::Sequence(&[1, 3, 2]).frame().unwrap().as_bytes(),
            [0x7e, 0x31, 0, 3, 1, 3, 2, 0xef]
        );
        assert_eq!(
            Command::Version.frame().unwrap().as_bytes(),
            [0x7e, 0x46, 0, 2, 0, 0, 0xef]
        );
        assert!(Command::Volume {
            level: 31,
            remember: false
        }
        .frame()
        .is_err());
        assert!(Command::Volume {
            level: 30,
            remember: false
        }
        .frame()
        .is_ok());
        assert!(Command::Sequence(&[0]).frame().is_err());
    }

    #[test]
    fn parses_fragmented_and_concatenated_responses() {
        let mut parser = Parser::default();
        let mut results = Vec::new();
        for b in [0xff, 0x7e, 0x41, 0xef, 0x7e, 0x43, 0, 2, 0, 7, 0xef] {
            if let Some(r) = parser.push(b) {
                results.push(r);
            }
        }
        assert_eq!(results[0], Response::Ack);
        assert_eq!(results[1].payload(), [0, 7]);
    }

    struct MockUart(Vec<u8>);
    impl UartTx for MockUart {
        type Error = ();
        fn write(&mut self, bytes: &[u8]) -> core::result::Result<(), ()> {
            self.0.extend_from_slice(bytes);
            Ok(())
        }
    }

    #[test]
    fn embedded_controller_round_trip() {
        let mut controller = Controller::new(MockUart(Vec::new()));
        controller.send(&Command::GetVolume).unwrap();
        assert_eq!(
            controller.transmitter_mut().0,
            [0x7e, 0x43, 0, 2, 0, 0, 0xef]
        );
        let mut reply = None;
        for byte in [0x7e, 0x43, 0, 2, 0, 7, 0xef] {
            reply = controller.receive(byte).or(reply);
        }
        assert_eq!(reply.unwrap().payload(), &[0, 7]);
        controller.send(&Command::Track(1)).unwrap();
        controller.send(&Command::VolumeUp).unwrap();
        controller
            .send(&Command::Volume {
                level: 30,
                remember: false,
            })
            .unwrap();
        controller.send(&Command::Play).unwrap();
    }

    #[test]
    fn datasheet_command_coverage() {
        let cases: &[(Command<'_>, u8)] = &[
            (Command::Next, 0x11),
            (Command::Previous, 0x12),
            (Command::Track(999), 0x13),
            (Command::VolumeUp, 0x14),
            (Command::VolumeDown, 0x15),
            (
                Command::Volume {
                    level: 7,
                    remember: true,
                },
                0x16,
            ),
            (Command::LoopTrack(1), 0x18),
            (Command::Baud(3), 0x0b),
            (Command::Reset, 0x0c),
            (Command::Play, 0x1d),
            (Command::Pause, 0x1e),
            (Command::LoopAll, 0x21),
            (Command::Stop, 0x26),
            (Command::Shuffle, 0x28),
            (Command::LoopCurrent(false), 0x29),
            (Command::Sequence(&[1, 2]), 0x31),
            (Command::FactoryReset, 0xd1),
            (Command::Status, 0x42),
            (Command::GetVolume, 0x43),
            (Command::FileCount, 0x4a),
            (Command::Version, 0x46),
        ];
        for (command, opcode) in cases {
            let frame = command.frame().unwrap();
            let bytes = frame.as_bytes();
            assert_eq!(bytes[0], 0x7e);
            assert_eq!(bytes[1], *opcode);
            assert_eq!(bytes[3] as usize, bytes.len() - 5);
            assert_eq!(bytes[bytes.len() - 1], 0xef);
        }
        assert_eq!(
            Command::Sequence(&[1; 30])
                .frame()
                .unwrap()
                .as_bytes()
                .len(),
            MAX_FRAME
        );
        assert_eq!(Command::Baud(8).frame(), Err(Error::BaudRange));
        assert_eq!(Command::Track(0).frame(), Err(Error::TrackRange));
    }

    #[test]
    fn parser_recovers_after_bad_length() {
        let mut parser = Parser::default();
        let mut response = None;
        for byte in [0x7e, 0x43, 0xff, 0xff, 0x7e, 0x43, 0, 2, 0, 7, 0xef] {
            response = parser.push(byte).or(response);
        }
        assert_eq!(response.unwrap().payload(), [0, 7]);
    }

    #[test]
    fn observed_track_completion_keeps_opcode_visible() {
        let mut data = [0; 30];
        data[1] = 2;
        let response = Response::Frame {
            code: 0x30,
            data,
            len: 2,
        };
        assert_eq!(
            response.to_string(),
            "track 2 finished (0x30; observed, undocumented)"
        );
    }
}
