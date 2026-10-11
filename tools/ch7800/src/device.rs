use std::io::{Read, Write};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use ch7800_protocol::{Command, Parser, Response};

pub struct Device {
    port: Box<dyn serialport::SerialPort>,
    parser: Parser,
    pending: Vec<Response>,
    last_write: Option<Instant>,
    wake: bool,
}

impl Device {
    pub fn open(path: &str, baud: u32, wake: bool) -> Result<Self> {
        let port = serialport::new(path, baud)
            .timeout(Duration::from_millis(20))
            .open()
            .with_context(|| format!("opening serial port {path}"))?;
        Ok(Self {
            port,
            parser: Parser::default(),
            pending: Vec::new(),
            last_write: None,
            wake,
        })
    }

    /// Drain incoming bytes, including asynchronous startup / track-finished notifications.
    pub fn poll(&mut self) -> Result<Vec<Response>> {
        let received = self.read_port()?;
        let mut output = std::mem::take(&mut self.pending);
        output.extend(received);
        Ok(output)
    }

    fn read_port(&mut self) -> Result<Vec<Response>> {
        let mut output = Vec::new();
        let mut bytes = [0; 128];
        match self.port.read(&mut bytes) {
            Ok(n) => {
                for byte in &bytes[..n] {
                    if let Some(message) = self.parser.push(*byte) {
                        output.push(message);
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {}
            Err(error) => return Err(error).context("reading UART"),
        }
        Ok(output)
    }

    fn write(&mut self, command: &Command<'_>) -> Result<()> {
        if let Some(last) = self.last_write {
            let delay = Duration::from_millis(25).saturating_sub(last.elapsed());
            if !delay.is_zero() {
                sleep(delay);
            }
        }
        let frame = command
            .frame()
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        self.port
            .write_all(frame.as_bytes())
            .context("writing UART")?;
        self.port.flush().context("flushing UART")?;
        self.last_write = Some(Instant::now());
        Ok(())
    }

    fn send(&mut self, command: &Command<'_>) -> Result<Vec<Response>> {
        // The first frame after low-power sleep is discarded. After inactivity,
        // send a harmless request first; never retry a volume/transport command
        // blindly because it may have executed even if its ACK was lost.
        if needs_wake(self.wake, self.last_write) {
            self.write(&Command::Status)?;
            sleep(Duration::from_millis(60));
            let probe_responses = self.read_port()?;
            self.pending.extend(
                probe_responses
                    .into_iter()
                    .filter(|response| !is_probe_reply(response)),
            );
        }
        self.write(command)?;
        let deadline = Instant::now() + Duration::from_millis(550);
        let mut responses = Vec::new();
        let mut saw_ack = false;
        let mut saw_data = false;
        while Instant::now() < deadline {
            let batch = self.read_port()?;
            let found = batch.iter().any(|r| matches!(r, Response::Frame { code, .. } if Some(*code) == command.response_code()));
            let ack = batch.iter().any(|r| matches!(r, Response::Ack));
            let error = batch
                .iter()
                .any(|r| matches!(r, Response::Frame { code: 0x40, .. }));
            responses.extend(batch);
            saw_ack |= ack;
            saw_data |= found;
            if found || error || (command.response_code().is_none() && ack) {
                break;
            }
        }
        if let Some(error) = responses
            .iter()
            .find(|r| matches!(r, Response::Frame { code: 0x40, .. }))
        {
            bail!("{error}");
        }
        if command.response_code().is_some() && !saw_data {
            bail!("no data response within 550 ms; check power, RX/TX, baud rate or sleep mode");
        }
        if command.response_code().is_none() && !saw_ack {
            bail!("no ACK within 550 ms; command effect is unknown—do not repeat volume changes blindly. Query volume/status, then check power, RX/TX, baud rate or sleep mode");
        }
        let mut output = std::mem::take(&mut self.pending);
        output.extend(responses);
        Ok(output)
    }

    /// Forward a command without changing volume as a side effect.
    pub fn execute(&mut self, command: &Command<'_>) -> Result<Vec<Response>> {
        if matches!(command, Command::Reset | Command::FactoryReset) {
            // Avoid resetting during playback.
            self.send(&Command::Stop)?;
        }
        self.send(command)
    }

    /// Prefer absolute volume for interactive controls: some CH7800 firmware
    /// does not acknowledge or apply the 0x14/0x15 relative commands.
    pub fn adjust_volume(&mut self, delta: i8) -> Result<Vec<Response>> {
        let before = self.send(&Command::GetVolume)?;
        let current = volume_from(&before).context("volume query returned no usable level")?;
        let target = step_volume(current, delta);
        if target == current {
            return Ok(before);
        }

        let set_result = self.send(&Command::Volume {
            level: target,
            remember: false,
        });
        // Even if ACK is lost, read back rather than retrying a possibly
        // executed command or claiming failure when the level actually changed.
        let after = self.send(&Command::GetVolume)?;
        let reported = volume_from(&after).context("volume read-back returned no usable level")?;
        if reported != target {
            bail!(
                "requested volume {target}/30, but device reports {reported}/30 (set command: {})",
                match set_result {
                    Ok(_) => "ACK received".to_string(),
                    Err(error) => error.to_string(),
                }
            );
        }
        Ok(after)
    }
}

fn volume_from(responses: &[Response]) -> Option<u8> {
    responses.iter().find_map(|response| match response {
        Response::Frame {
            code: 0x43,
            data,
            len: 2,
        } => u8::try_from(u16::from_be_bytes([data[0], data[1]]))
            .ok()
            .filter(|volume| *volume <= 30),
        _ => None,
    })
}

fn step_volume(current: u8, delta: i8) -> u8 {
    (i16::from(current) + i16::from(delta)).clamp(0, 30) as u8
}

fn needs_wake(force: bool, last_write: Option<Instant>) -> bool {
    force || last_write.is_none_or(|last| last.elapsed() >= Duration::from_secs(4))
}

fn is_probe_reply(response: &Response) -> bool {
    matches!(response, Response::Ack | Response::Frame { code: 0x42, .. })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wake_after_idle_or_when_forced() {
        assert!(needs_wake(false, None));
        assert!(needs_wake(
            false,
            Some(Instant::now() - Duration::from_secs(5))
        ));
        assert!(!needs_wake(false, Some(Instant::now())));
        assert!(needs_wake(true, Some(Instant::now())));
    }

    #[test]
    fn wake_probe_only_drops_its_own_responses() {
        let data = [0; 30];
        assert!(is_probe_reply(&Response::Ack));
        assert!(is_probe_reply(&Response::Frame {
            code: 0x42,
            data,
            len: 2
        }));
        assert!(!is_probe_reply(&Response::Frame {
            code: 0x30,
            data,
            len: 2
        }));
        assert!(!is_probe_reply(&Response::Frame {
            code: 0x3f,
            data,
            len: 2
        }));
    }

    #[test]
    fn extracts_only_valid_volume_responses() {
        let mut data = [0; 30];
        data[1] = 7;
        assert_eq!(
            volume_from(&[
                Response::Ack,
                Response::Frame {
                    code: 0x43,
                    data,
                    len: 2
                }
            ]),
            Some(7)
        );
        data[1] = 31;
        assert_eq!(
            volume_from(&[Response::Frame {
                code: 0x43,
                data,
                len: 2
            }]),
            None
        );
    }

    #[test]
    fn steps_across_full_datasheet_volume_range() {
        assert_eq!(step_volume(7, 1), 8);
        assert_eq!(step_volume(0, -1), 0);
        assert_eq!(step_volume(30, 1), 30);
    }
}
