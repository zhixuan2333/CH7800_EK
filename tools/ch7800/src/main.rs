mod device;
mod tui;

use anyhow::{bail, Result};
use ch7800_protocol::Command;
use clap::{Parser, Subcommand};
use device::Device;

#[derive(Parser)]
#[command(about = "CH7800 UART controller")]
struct Cli {
    /// WCH-Link USB serial port, e.g. /dev/cu.usbmodem... on macOS
    #[arg(short, long, global = true)]
    port: Option<String>,
    /// Current CH7800 UART rate (default 9600)
    #[arg(long, default_value_t = 9600, global = true)]
    baud: u32,
    /// Force a wake-up frame before every command (normally automatic after idle)
    #[arg(long, global = true)]
    wake: bool,
    #[command(subcommand)]
    action: Action,
}

#[derive(Parser)]
struct Palette {
    #[command(subcommand)]
    action: Action,
}

#[derive(Clone, Subcommand)]
enum Action {
    /// Show available serial devices
    Ports,
    /// Keyboard-controlled terminal interface
    Tui,
    Next,
    Previous,
    Track {
        number: u16,
    },
    Play,
    Pause,
    Stop,
    /// Raise volume one step using the reliable set-volume command
    VolumeUp,
    /// Lower volume one step using the reliable set-volume command
    VolumeDown,
    /// Send the datasheet's raw 0x14 relative command (may be ignored by firmware)
    RawVolumeUp,
    /// Send the datasheet's raw 0x15 relative command (may be ignored by firmware)
    RawVolumeDown,
    /// Set volume from 0 to 30
    Volume {
        level: u8,
        #[arg(long)]
        remember: bool,
    },
    LoopTrack {
        number: u16,
    },
    LoopCurrent {
        #[arg(value_parser = ["on", "off"])]
        mode: String,
    },
    LoopAll,
    Shuffle,
    /// Play up to 30 root-directory files, each numbered 1..255
    Sequence {
        tracks: Vec<u8>,
    },
    Status,
    GetVolume,
    FileCount,
    Version,
    /// Save a new UART rate; reopen at that rate after resetting or power-cycling
    SetBaud {
        rate: u32,
        #[arg(long)]
        confirm: bool,
    },
    /// Stop playback then reset; may restore volume 30 until set again
    Reset {
        #[arg(long)]
        confirm: bool,
    },
    /// Stop playback then erase saved chip settings (NOT audio files)
    FactoryReset {
        #[arg(long)]
        confirm: bool,
    },
}

impl Action {
    fn command(&self) -> Result<Command<'_>> {
        Ok(match self {
            Self::Ports | Self::Tui => bail!("not a UART command"),
            Self::Next => Command::Next,
            Self::Previous => Command::Previous,
            Self::Track { number } => Command::Track(*number),
            Self::Play => Command::Play,
            Self::Pause => Command::Pause,
            Self::Stop => Command::Stop,
            Self::VolumeUp | Self::VolumeDown => bail!("interactive volume step"),
            Self::RawVolumeUp => Command::VolumeUp,
            Self::RawVolumeDown => Command::VolumeDown,
            Self::Volume { level, remember } => Command::Volume {
                level: *level,
                remember: *remember,
            },
            Self::LoopTrack { number } => Command::LoopTrack(*number),
            Self::LoopCurrent { mode } => Command::LoopCurrent(mode == "on"),
            Self::LoopAll => Command::LoopAll,
            Self::Shuffle => Command::Shuffle,
            Self::Sequence { tracks } => Command::Sequence(tracks),
            Self::Status => Command::Status,
            Self::GetVolume => Command::GetVolume,
            Self::FileCount => Command::FileCount,
            Self::Version => Command::Version,
            Self::SetBaud { rate, confirm } => {
                if !confirm {
                    bail!("set-baud changes saved settings; pass --confirm");
                }
                let index = [1200, 2400, 4800, 9600, 19200, 38400, 57600, 115200]
                    .iter()
                    .position(|r| r == rate)
                    .ok_or_else(|| anyhow::anyhow!("unsupported WCH-Link rate"))?;
                Command::Baud(index as u8)
            }
            Self::Reset { confirm } => {
                if !confirm {
                    bail!("reset can restore loud volume; pass --confirm");
                }
                Command::Reset
            }
            Self::FactoryReset { confirm } => {
                if !confirm {
                    bail!("factory-reset erases saved settings; pass --confirm");
                }
                Command::FactoryReset
            }
        })
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if matches!(cli.action, Action::Ports) {
        for port in serialport::available_ports()? {
            println!("{}", port.port_name);
        }
        return Ok(());
    }
    let path = cli
        .port
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("specify --port; run `ch7800 ports` to list devices"))?;
    let mut device = Device::open(path, cli.baud, cli.wake)?;
    if matches!(cli.action, Action::Tui) {
        return tui::run(device, path);
    }
    if matches!(cli.action, Action::VolumeUp | Action::VolumeDown) {
        let delta = if matches!(cli.action, Action::VolumeUp) {
            1
        } else {
            -1
        };
        for response in device.adjust_volume(delta)? {
            println!("{response}");
        }
        return Ok(());
    }
    let command = cli.action.command()?;
    for response in device.execute(&command)? {
        println!("{response}");
    }
    Ok(())
}
