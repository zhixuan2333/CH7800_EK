#![no_std]
#![no_main]

//! CH32V003 -> CH7800: wait for the player to boot, play a track, and use the
//! CH7800 BUSY pin to confirm that audio actually started (ACK alone does not).
//! USART1 on PD5 (TX) / PD6 (RX), 9600 8N1. PA1 reads the CH7800 BUSY pin (J3-6).
//!
//! BUSY polarity is not documented in the material we have, so the firmware
//! samples the idle level after boot and treats any stable change as "playing".
//! Observed on the EK: idle high, low ~30 ms after Track while audio plays.
//! If BUSY does not move, it asks the chip (0x42) as a second opinion and
//! retries the wake/volume/track sequence a few times. Before playing, it runs a
//! UART self-test (version, file count, volume) so link problems show up on their own.

use ch7800_protocol::{Command, Controller, Response, UartTx};
use core::ptr::{read_volatile, write_volatile};

const HCLK: u32 = 24_000_000;
const VOLUME: u8 = 7; // Conservative while the supply is marginal.
const TRACK: u16 = 2; // Track 1 browns out the shared 3V3 rail; track 2 plays.
const MAX_ATTEMPTS: u8 = 3;
const BUSY_TIMEOUT_MS: u32 = 1500;

const FLASH_ACTLR: usize = 0x4002_2000;
const RCC_CFGR0: usize = 0x4002_1004;
const RCC_APB2PCENR: usize = 0x4002_1018;
const RCC_RSTSCKR: usize = 0x4002_1024;
const GPIOA_CFGLR: usize = 0x4001_0800;
const GPIOA_INDR: usize = 0x4001_0808;
const GPIOD_CFGLR: usize = 0x4001_1400;
const USART1_STATR: usize = 0x4001_3800;
const USART1_DATAR: usize = 0x4001_3804;
const USART1_BRR: usize = 0x4001_3808;
const USART1_CTLR1: usize = 0x4001_380c;
const STK_CTLR: usize = 0xe000_f000;
const STK_CNT: usize = 0xe000_f008;

#[cfg(target_arch = "riscv32")]
core::arch::global_asm!(
    r#"
    .section .init, "ax"
    .global _start
_start:
    la sp, _stack_top
    la a0, _sbss
    la a1, _ebss
1:  bgeu a0, a1, 2f
    sw zero, 0(a0)
    addi a0, a0, 4
    j 1b
2:  la a0, _sdata
    la a1, _edata
    la a2, _sidata
3:  bgeu a0, a1, 4f
    lw t0, 0(a2)
    sw t0, 0(a0)
    addi a0, a0, 4
    addi a2, a2, 4
    j 3b
4:  j main
"#
);

/// Stage values written to `DEBUG.stage`.
mod stage {
    pub const WAIT_STARTUP: u8 = 1;
    /// Version / file count / volume queries (no audio).
    pub const UART_TEST: u8 = 9;
    pub const WAKE: u8 = 2;
    pub const VOLUME: u8 = 3;
    pub const TRACK: u8 = 4;
    pub const WAIT_BUSY: u8 = 5;
    /// BUSY changed: audio is playing.
    pub const PLAYING: u8 = 6;
    /// Chip reports "playing" (0x42) but BUSY never moved: check J3-6 -> PA1 wiring.
    pub const PLAYING_BUSY_STUCK: u8 = 7;
    /// Track finished (BUSY back to idle or completion frame).
    pub const FINISHED: u8 = 8;
    /// All attempts failed: neither BUSY nor 0x42 says it is playing.
    pub const FAILED: u8 = 0xee;
}

/// Inspectable over SWD (e.g. `wlink`/`minichlink` memory read of `DEBUG`).
#[no_mangle]
static mut DEBUG: Debug = Debug {
    stage: 0,
    startup_seen: 0,
    acks: 0,
    frames: 0,
    last_code: 0,
    last_data: [0; 2],
    busy: 0,
    busy_idle: 0xff,
    attempts: 0,
    play_state: 0xff,
    error_code: 0,
    finished: 0,
    reset_flags: 0,
    version: [0xff; 2],
    file_count: [0xff; 2],
    volume_read: 0xff,
    busy_edges: 0,
    busy_start_ms: 0xffff,
    busy_len_ms: 0,
};

#[repr(C)]
struct Debug {
    stage: u8,
    /// Count of CH7800 startup frames (0x3F). >1 means the CH7800 rebooted: power.
    startup_seen: u8,
    acks: u8,
    frames: u8,
    last_code: u8,
    last_data: [u8; 2],
    /// Current BUSY level (PA1).
    busy: u8,
    /// BUSY level sampled after boot, before any play command.
    busy_idle: u8,
    attempts: u8,
    /// Last 0x42 answer: 0 stopped, 1 playing, 2 paused, 0xFF never answered.
    play_state: u8,
    /// Last 0x40 device error code (2 = asleep, 5 = out of range, 6 = not found...).
    error_code: u8,
    /// Track completions observed.
    finished: u8,
    /// RCC_RSTSCKR[31:24] at boot: bit3 (0x08) = power-on/brown-out reset.
    reset_flags: u8,
    /// UART self-test answers (0x46, 0x4A, 0x43). 0xFF = no answer: RX/TX wiring or baud.
    version: [u8; 2],
    file_count: [u8; 2],
    volume_read: u8,
    /// BUSY level changes seen after the last Track command.
    busy_edges: u8,
    /// ms from the Track command to the first BUSY change (0xFFFF = never).
    busy_start_ms: u16,
    /// How long BUSY stayed away from idle (ms): roughly the audible play time.
    busy_len_ms: u16,
}

fn debug() -> &'static mut Debug {
    unsafe { &mut *core::ptr::addr_of_mut!(DEBUG) }
}

fn reg_write(addr: usize, value: u32) {
    unsafe { write_volatile(addr as *mut u32, value) }
}

fn reg_read(addr: usize) -> u32 {
    unsafe { read_volatile(addr as *const u32) }
}

fn now() -> u32 {
    reg_read(STK_CNT)
}

fn busy_raw() -> u8 {
    (reg_read(GPIOA_INDR) >> 1 & 1) as u8
}

struct Usart1;

impl UartTx for Usart1 {
    type Error = core::convert::Infallible;

    fn write(&mut self, bytes: &[u8]) -> Result<(), Self::Error> {
        for &byte in bytes {
            while reg_read(USART1_STATR) & (1 << 7) == 0 {} // TXE
            reg_write(USART1_DATAR, u32::from(byte));
        }
        while reg_read(USART1_STATR) & (1 << 6) == 0 {} // TC
        Ok(())
    }
}

fn record(response: &Response) {
    let debug = debug();
    match *response {
        Response::Ack => debug.acks = debug.acks.wrapping_add(1),
        Response::Frame { code, .. } => {
            debug.frames = debug.frames.wrapping_add(1);
            debug.last_code = code;
            let data = response.payload();
            let n = data.len().min(2);
            debug.last_data[..n].copy_from_slice(&data[..n]);
            let value = data.get(1).copied().unwrap_or(0);
            match code {
                0x3f => debug.startup_seen = debug.startup_seen.wrapping_add(1),
                0x40 => debug.error_code = value,
                0x42 => debug.play_state = value,
                0x43 => debug.volume_read = value,
                0x46 => debug.version[..n].copy_from_slice(&data[..n]),
                0x4a => debug.file_count[..n].copy_from_slice(&data[..n]),
                0x30 | 0x3d | 0x3e => debug.finished = debug.finished.wrapping_add(1),
                _ => {}
            }
        }
    }
}

fn set_stage(stage: u8) {
    debug().stage = stage;
}

/// BUSY edge timing relative to the last Track command (SysTick ticks).
struct BusyWatch {
    armed: bool,
    idle: u8,
    last: u8,
    since: u32,
    left_idle_at: u32,
}

static mut WATCH: BusyWatch = BusyWatch {
    armed: false,
    idle: 0,
    last: 0,
    since: 0,
    left_idle_at: 0,
};

fn watch() -> &'static mut BusyWatch {
    unsafe { &mut *core::ptr::addr_of_mut!(WATCH) }
}

fn arm_busy_watch(idle: u8) {
    let w = watch();
    *w = BusyWatch { armed: true, idle, last: busy_raw(), since: now(), left_idle_at: 0 };
    let debug = debug();
    debug.busy_edges = 0;
    debug.busy_start_ms = 0xffff;
    debug.busy_len_ms = 0;
}

fn update_busy_watch(level: u8) {
    let w = watch();
    if !w.armed || level == w.last {
        return;
    }
    w.last = level;
    let t = now();
    let debug = debug();
    debug.busy_edges = debug.busy_edges.saturating_add(1);
    if level != w.idle {
        w.left_idle_at = t;
        if debug.busy_start_ms == 0xffff {
            debug.busy_start_ms = (t.wrapping_sub(w.since) / (HCLK / 1000)).min(0xfffe) as u16;
        }
    } else {
        debug.busy_len_ms = (t.wrapping_sub(w.left_idle_at) / (HCLK / 1000)).min(0xffff) as u16;
    }
}

/// Service the UART for `ms` milliseconds; stop early (returning true) if `until` matches.
/// Also keeps `DEBUG.busy` current so it is meaningful at every stage.
fn pump(chip: &mut Controller<Usart1>, ms: u32, until: impl Fn(&Response) -> bool) -> bool {
    let start = now();
    let ticks = ms * (HCLK / 1000);
    while now().wrapping_sub(start) < ticks {
        let status = reg_read(USART1_STATR);
        if status & (1 << 3) != 0 {
            let _ = reg_read(USART1_DATAR); // ORE: clear by reading DATAR
        }
        if status & (1 << 5) != 0 {
            if let Some(response) = chip.receive(reg_read(USART1_DATAR) as u8) {
                record(&response);
                if until(&response) {
                    return true;
                }
            }
        }
        let level = busy_raw();
        debug().busy = level;
        update_busy_watch(level);
    }
    false
}

/// Wait until BUSY has differed from `idle` continuously for 20 ms (debounced),
/// while still servicing the UART. Returns false on timeout.
fn wait_busy_change(chip: &mut Controller<Usart1>, idle: u8, timeout_ms: u32) -> bool {
    let mut stable_ms = 0;
    for _ in 0..timeout_ms {
        pump(chip, 1, |_| false);
        if busy_raw() != idle {
            stable_ms += 1;
            if stable_ms >= 20 {
                return true;
            }
        } else {
            stable_ms = 0;
        }
    }
    false
}

fn send(chip: &mut Controller<Usart1>, command: &Command<'_>) {
    let _ = chip.send(command);
    // >=20 ms between frames (>=50 ms after a wake request); also collects the ACK.
    pump(chip, 100, |_| false);
}

/// Send a query and wait for its answer frame (recorded by `record`). False on timeout.
fn query(chip: &mut Controller<Usart1>, command: &Command<'_>) -> bool {
    let code = command.response_code();
    let _ = chip.send(command);
    let answered = pump(chip, 300, |r| matches!(r, Response::Frame { code: c, .. } if Some(*c) == code));
    pump(chip, 30, |_| false); // keep >=20 ms before the next frame
    answered
}

/// Ask the chip whether it is playing. Returns the 0x42 state, or None on timeout.
fn query_state(chip: &mut Controller<Usart1>) -> Option<u8> {
    query(chip, &Command::Status).then(|| debug().play_state)
}

#[no_mangle]
extern "C" fn main() -> ! {
    // Reset cause, then clear the flags (RMVF) so the next reset is distinguishable.
    debug().reset_flags = (reg_read(RCC_RSTSCKR) >> 24) as u8;
    reg_write(RCC_RSTSCKR, reg_read(RCC_RSTSCKR) | 1 << 24);

    // 24 MHz needs one flash wait state; set it before raising HCLK.
    reg_write(FLASH_ACTLR, reg_read(FLASH_ACTLR) & !0b11 | 0b01);
    // SYSCLK = HSI 24 MHz, HCLK undivided (reset default divides by 3).
    reg_write(RCC_CFGR0, 0);
    // Clocks: AFIO, GPIOA, GPIOD, USART1.
    reg_write(
        RCC_APB2PCENR,
        reg_read(RCC_APB2PCENR) | 1 | 1 << 2 | 1 << 5 | 1 << 14,
    );
    // PA1 floating input (BUSY).
    reg_write(GPIOA_CFGLR, reg_read(GPIOA_CFGLR) & !(0xf << 4) | 0x4 << 4);
    // PD5 AF push-pull 50 MHz (TX), PD6 floating input (RX).
    reg_write(
        GPIOD_CFGLR,
        reg_read(GPIOD_CFGLR) & !(0xff << 20) | 0xb << 20 | 0x4 << 24,
    );
    reg_write(USART1_BRR, HCLK / 9600);
    reg_write(USART1_CTLR1, 1 << 13 | 1 << 3 | 1 << 2); // UE, TE, RE
    // SysTick free-running up-counter on HCLK.
    reg_write(STK_CTLR, 0b101);

    let mut chip = Controller::new(Usart1);

    set_stage(stage::WAIT_STARTUP);
    // Startup notification, or give up after 3 s and try anyway.
    if pump(&mut chip, 3000, |r| matches!(r, Response::Frame { code: 0x3f, .. })) {
        pump(&mut chip, 200, |_| false);
    }

    // Idle BUSY level, before anything has been asked to play.
    let idle = busy_raw();
    debug().busy_idle = idle;

    // UART round trip without starting audio: proves TX, RX and baud independently of power.
    set_stage(stage::UART_TEST);
    send(&mut chip, &Command::Status); // disposable wake-up request
    query(&mut chip, &Command::Version);
    query(&mut chip, &Command::FileCount);
    query(&mut chip, &Command::GetVolume);

    let mut result = stage::FAILED;
    for attempt in 1..=MAX_ATTEMPTS {
        debug().attempts = attempt;

        set_stage(stage::WAKE);
        send(&mut chip, &Command::Status); // disposable wake-up request
        set_stage(stage::VOLUME);
        send(&mut chip, &Command::Volume { level: VOLUME, remember: false });
        set_stage(stage::TRACK);
        let _ = chip.send(&Command::Track(TRACK));
        arm_busy_watch(idle);

        set_stage(stage::WAIT_BUSY);
        if wait_busy_change(&mut chip, idle, BUSY_TIMEOUT_MS) {
            result = stage::PLAYING;
            break;
        }
        // BUSY did not move. Ask the chip directly.
        if query_state(&mut chip) == Some(1) {
            result = stage::PLAYING_BUSY_STUCK;
            break;
        }
        // Not playing: let the rail recover, then retry the whole sequence.
        pump(&mut chip, 500, |_| false);
    }
    set_stage(result);

    let finished_at_start = debug().finished;
    loop {
        pump(&mut chip, 50, |_| false);
        if result == stage::PLAYING {
            let done = debug().finished != finished_at_start || busy_raw() == idle;
            if done {
                set_stage(stage::FINISHED);
            }
        }
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
