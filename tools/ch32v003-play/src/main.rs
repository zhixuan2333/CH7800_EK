#![no_std]
#![no_main]

//! CH32V003 -> CH7800 motion/button-triggered player. The MCU sleeps in standby until
//! the SB612B PIR output (TEST pin: 3.3 V while motion is detected) raises PD7, or a
//! button shorting PC5 to PC6 is pressed, then plays a random track 1..=TRACKS and
//! goes back to standby. One play per edge: while the PIR stays high (repeatable
//! trigger) or a button is held, nothing replays. Triggers during playback are ignored.
//! USART1 on PD5 (TX) / PD6 (RX), 9600 8N1. PA1 reads the CH7800 BUSY pin (J3-6).
//!
//! BUSY polarity is not documented in the material we have, so the firmware
//! samples the idle level after waking the chip and treats any stable change as
//! "playing". Observed on the EK: idle high, low ~30 ms after Track while audio plays.
//! If BUSY does not move, it asks the chip (0x42) as a second opinion and
//! retries the wake/volume/track sequence a few times. At boot it runs a
//! UART self-test (version, file count, volume) so link problems show up on their own.
//!
//! PD7 is NRST from the factory. On first boot the firmware rewrites the USER
//! option byte so PD7 is a plain GPIO, then resets once.
//! The debugger cannot attach in standby: flash within BOOT_AWAKE_MS of power-up
//! or while a track is playing.

use ch7800_protocol::{Command, Controller, Response, UartTx};
use core::ptr::{read_volatile, write_volatile};

const HCLK: u32 = 24_000_000;
const VOLUME: u8 = 30; // 0..=30 (max). VCC fed from USB 5V; 7 was for the back-fed 3V3 rail.
const TRACKS: u32 = 6; // Random pick from 1..=TRACKS (at most 8: see `track_counts`).
const MAX_ATTEMPTS: u8 = 3;
const BUSY_TIMEOUT_MS: u32 = 1500;
/// Stay awake this long after reset so a debugger can attach before the first standby.
const BOOT_AWAKE_MS: u32 = 5000;
/// Give up waiting for a track to end after this long (10 min).
const MAX_PLAY_MS: u32 = 600_000;

const FLASH_ACTLR: usize = 0x4002_2000;
const FLASH_KEYR: usize = 0x4002_2004;
const FLASH_OBKEYR: usize = 0x4002_2008;
const FLASH_STATR: usize = 0x4002_200c;
const FLASH_CTLR: usize = 0x4002_2010;
const FLASH_KEY1: u32 = 0x4567_0123;
const FLASH_KEY2: u32 = 0xcdef_89ab;
/// Option bytes: RDPR, USER, DATA0, DATA1, WRPR0..3 (each value + complement).
const OB_BASE: usize = 0x1fff_f800;
const ESIG_UNIID1: usize = 0x1fff_f7e8;
const RCC_CFGR0: usize = 0x4002_1004;
const RCC_APB2PCENR: usize = 0x4002_1018;
const RCC_APB1PCENR: usize = 0x4002_101c;
const RCC_RSTSCKR: usize = 0x4002_1024;
const PWR_CTLR: usize = 0x4000_7000;
const AFIO_EXTICR: usize = 0x4001_0008;
const EXTI_EVENR: usize = 0x4001_0404;
const EXTI_RTENR: usize = 0x4001_0408;
const EXTI_FTENR: usize = 0x4001_040c;
const EXTI_INTFR: usize = 0x4001_0414;
const GPIOA_CFGLR: usize = 0x4001_0800;
const GPIOA_INDR: usize = 0x4001_0808;
const GPIOA_OUTDR: usize = 0x4001_080c;
const GPIOC_CFGLR: usize = 0x4001_1000;
const GPIOC_INDR: usize = 0x4001_1008;
const GPIOC_OUTDR: usize = 0x4001_100c;
const GPIOD_CFGLR: usize = 0x4001_1400;
const GPIOD_INDR: usize = 0x4001_1408;
const GPIOD_OUTDR: usize = 0x4001_140c;
const USART1_STATR: usize = 0x4001_3800;
const USART1_DATAR: usize = 0x4001_3804;
const USART1_BRR: usize = 0x4001_3808;
const USART1_CTLR1: usize = 0x4001_380c;
const PFIC_CFGR: usize = 0xe000_e048;
const PFIC_SCTLR: usize = 0xe000_ed10;
const STK_CTLR: usize = 0xe000_f000;
const STK_CNT: usize = 0xe000_f008;

/// PD7 trigger input (SB612B TEST pin), rising edge.
const TRIGGER_PIN: u32 = 7;
/// Push button wired across PC5 (+) and PC6 (-): PC6 is driven low, PC5 is a
/// pull-up input, so pressing shorts PC5 low (falling edge).
const BUTTON_SENSE: u32 = 5;
const BUTTON_DRIVE: u32 = 6;
/// EXTI lines that wake from standby (line n = pin n of the port picked in AFIO_EXTICR).
const WAKE_LINES: u32 = 1 << TRIGGER_PIN | 1 << BUTTON_SENSE;

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
    /// In standby, waiting for PD7 / the PC5-PC6 button. Only visible if a debugger catches it on the way in.
    pub const STANDBY: u8 = 0x0a;
    /// All attempts failed: neither BUSY nor 0x42 says it is playing.
    pub const FAILED: u8 = 0xee;
}

/// Inspectable over SWD (e.g. `probe-rs read b8 0x20000000 32`).
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
    wakes: 0,
    plays: 0,
    last_track: 0,
    option_user: 0,
    busy_start_ms: 0xffff,
    busy_len_ms: 0,
    track_counts: [0; 8],
    wake_pins: 0xff,
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
    /// BUSY level sampled after the wake request, before the Track command.
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
    /// Standby wake-ups by PD7 or the PC5-PC6 button.
    wakes: u8,
    /// Tracks started (any result).
    plays: u8,
    last_track: u8,
    /// USER option byte at boot; bits 4:3 = 11 means PD7 is GPIO.
    option_user: u8,
    /// ms from the Track command to the first BUSY change (0xFFFF = never).
    busy_start_ms: u16,
    /// How long BUSY stayed away from idle (ms): roughly the audible play time.
    busy_len_ms: u16,
    /// How often each track 1..=TRACKS was picked.
    track_counts: [u8; 8],
    /// `trigger_pins()` right after the last wake: shows which input woke it
    /// (a short button press may already be released by then).
    wake_pins: u8,
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

/// xorshift32 state. Stirred with SysTick whenever a UART byte arrives: the CH7800
/// runs on its own clock, so the arrival phase against HSI is jitter, not a constant.
static mut RNG: u32 = 0;

fn stir(x: u32) {
    let rng = unsafe { &mut *core::ptr::addr_of_mut!(RNG) };
    *rng = rng.rotate_left(7) ^ x;
}

fn random_track() -> u16 {
    let rng = unsafe { &mut *core::ptr::addr_of_mut!(RNG) };
    let mut x = *rng ^ now();
    if x == 0 {
        x = 0x6d2b_79f5;
    }
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *rng = x;
    (x % TRACKS) as u16 + 1
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
            stir(now());
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

/// Wait for the end of the track: a completion frame, or (if `use_busy`) BUSY back
/// at `idle` for 50 ms. Gives up after MAX_PLAY_MS.
fn wait_done(chip: &mut Controller<Usart1>, idle: u8, use_busy: bool) {
    let finished_at_start = debug().finished;
    let mut idle_ms = 0;
    for _ in 0..MAX_PLAY_MS / 10 {
        pump(chip, 10, |_| false);
        if debug().finished != finished_at_start {
            return;
        }
        if use_busy {
            idle_ms = if busy_raw() == idle { idle_ms + 10 } else { 0 };
            if idle_ms >= 50 {
                return;
            }
        }
    }
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

/// Wake the CH7800, play `track`, confirm it started, and wait for it to end.
/// Returns the final stage.
fn play(chip: &mut Controller<Usart1>, track: u16) -> u8 {
    {
        let debug = debug();
        debug.plays = debug.plays.wrapping_add(1);
        debug.last_track = track as u8;
        let slot = &mut debug.track_counts[usize::from(track - 1) & 7];
        *slot = slot.wrapping_add(1);
    }

    for attempt in 1..=MAX_ATTEMPTS {
        debug().attempts = attempt;

        set_stage(stage::WAKE);
        send(chip, &Command::Status); // disposable wake-up request
        // Idle BUSY level, after the wake-up and before anything has been asked to play.
        let idle = busy_raw();
        debug().busy_idle = idle;
        set_stage(stage::VOLUME);
        send(chip, &Command::Volume { level: VOLUME, remember: false });
        set_stage(stage::TRACK);
        let _ = chip.send(&Command::Track(track));
        arm_busy_watch(idle);

        set_stage(stage::WAIT_BUSY);
        if wait_busy_change(chip, idle, BUSY_TIMEOUT_MS) {
            set_stage(stage::PLAYING);
            wait_done(chip, idle, true);
            return stage::FINISHED;
        }
        // BUSY did not move. Ask the chip directly.
        if query_state(chip) == Some(1) {
            set_stage(stage::PLAYING_BUSY_STUCK);
            wait_done(chip, idle, false);
            return stage::PLAYING_BUSY_STUCK;
        }
        // Not playing: let the rail recover, then retry the whole sequence.
        pump(chip, 500, |_| false);
    }
    stage::FAILED
}

fn flash_wait() {
    while reg_read(FLASH_STATR) & 1 != 0 {} // BSY
}

fn ob_read(index: usize) -> u8 {
    unsafe { read_volatile((OB_BASE + 2 * index) as *const u16) as u8 }
}

/// Make PD7 a GPIO (USER.RST_MODE = 11) instead of NRST. Rewrites the option
/// bytes (keeping RDPR unprotected and the other bytes as they were) and resets
/// so they load. Does nothing once the stored value is already right.
fn ensure_pd7_gpio() {
    let user = ob_read(1);
    debug().option_user = user;
    if user & 0x18 == 0x18 {
        return;
    }
    let mut bytes = [0u8; 8];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = ob_read(i);
    }
    bytes[0] = 0xa5; // RDPR: no read protection
    bytes[1] = user | 0x18;

    reg_write(FLASH_KEYR, FLASH_KEY1);
    reg_write(FLASH_KEYR, FLASH_KEY2);
    reg_write(FLASH_OBKEYR, FLASH_KEY1);
    reg_write(FLASH_OBKEYR, FLASH_KEY2);
    flash_wait();
    reg_write(FLASH_CTLR, reg_read(FLASH_CTLR) | 1 << 5); // OBER
    reg_write(FLASH_CTLR, reg_read(FLASH_CTLR) | 1 << 6); // STRT
    flash_wait();
    reg_write(FLASH_CTLR, reg_read(FLASH_CTLR) & !(1 << 5) | 1 << 4); // OBPG
    for (i, &b) in bytes.iter().enumerate() {
        // The complement byte is generated by hardware.
        unsafe { write_volatile((OB_BASE + 2 * i) as *mut u16, u16::from(b)) };
        flash_wait();
    }
    reg_write(FLASH_CTLR, reg_read(FLASH_CTLR) & !(1 << 4) | 1 << 7); // LOCK
    reg_write(PFIC_CFGR, 0xbeef_0080); // system reset: option bytes load at reset
    loop {}
}

/// HSI 24 MHz, HCLK undivided. Also needed after standby, which falls back to HSI.
fn clock_init() {
    // 24 MHz needs one flash wait state; set it before raising HCLK.
    reg_write(FLASH_ACTLR, reg_read(FLASH_ACTLR) & !0b11 | 0b01);
    // SYSCLK = HSI 24 MHz, HCLK undivided (reset default divides by 3).
    reg_write(RCC_CFGR0, 0);
}

fn gpio_init() {
    // Clocks: AFIO, GPIOA, GPIOC, GPIOD, USART1; PWR for standby.
    reg_write(
        RCC_APB2PCENR,
        reg_read(RCC_APB2PCENR) | 1 | 1 << 2 | 1 << 4 | 1 << 5 | 1 << 14,
    );
    reg_write(RCC_APB1PCENR, reg_read(RCC_APB1PCENR) | 1 << 28);
    // Unused pins: input with pull-down (OUTDR = 0) so they do not float in standby.
    // PA1 floating input (BUSY, driven by the CH7800), PA2 pull-down.
    reg_write(GPIOA_CFGLR, reg_read(GPIOA_CFGLR) & !(0xff << 4) | 0x4 << 4 | 0x8 << 8);
    reg_write(GPIOA_OUTDR, 0);
    // Port C: PC6 push-pull output low (button "-"), PC5 pull-up input (button "+",
    // idle high), the rest pull-down inputs. Current flows only while pressed.
    reg_write(GPIOC_CFGLR, 0x8888_8888 & !(0xf << 4 * BUTTON_DRIVE) | 0x2 << 4 * BUTTON_DRIVE);
    reg_write(GPIOC_OUTDR, 1 << BUTTON_SENSE);
    // PD1 is SWIO: leave it alone. PD0/PD2-4 pull-down, PD5 AF push-pull (TX),
    // PD6 floating input (RX), PD7 pull-down input (PIR, idle low).
    reg_write(
        GPIOD_CFGLR,
        reg_read(GPIOD_CFGLR) & 0xf0 | 0x8 | 0x888 << 8 | 0xb << 20 | 0x4 << 24 | 0x8 << 28,
    );
    reg_write(GPIOD_OUTDR, reg_read(GPIOD_OUTDR) & 1 << 1);
    // Wake-up events (no interrupt handler needed): EXTI7 <- PD7 rising edge,
    // EXTI5 <- PC5 falling edge. AFIO_EXTICR: 2 bits per line, 10 = PC, 11 = PD.
    reg_write(
        AFIO_EXTICR,
        reg_read(AFIO_EXTICR) & !(0b11 << 2 * BUTTON_SENSE | 0b11 << 2 * TRIGGER_PIN)
            | 0b10 << 2 * BUTTON_SENSE
            | 0b11 << 2 * TRIGGER_PIN,
    );
    reg_write(EXTI_RTENR, reg_read(EXTI_RTENR) | 1 << TRIGGER_PIN);
    reg_write(EXTI_FTENR, reg_read(EXTI_FTENR) | 1 << BUTTON_SENSE);
    reg_write(EXTI_EVENR, reg_read(EXTI_EVENR) | WAKE_LINES);
}

/// Wake-source pin levels: bit0 PC5 (0 = button pressed), bit1 PD7 (1 = motion).
fn trigger_pins() -> u8 {
    let c = reg_read(GPIOC_INDR) >> BUTTON_SENSE & 1;
    let d = reg_read(GPIOD_INDR) >> TRIGGER_PIN & 1;
    (c | d << 1) as u8
}

fn wfi() {
    #[cfg(target_arch = "riscv32")]
    unsafe {
        core::arch::asm!("wfi")
    };
}

/// Enter standby until a PD7 rising edge or a PC5-PC6 button press. Edges seen before
/// this call (e.g. button bounce or presses during playback) are dropped.
fn standby() {
    set_stage(stage::STANDBY);
    reg_write(EXTI_INTFR, WAKE_LINES);
    reg_write(PWR_CTLR, reg_read(PWR_CTLR) | 1 << 1); // PDDS: standby on deep sleep
    reg_write(PFIC_SCTLR, reg_read(PFIC_SCTLR) | 1 << 2); // SLEEPDEEP
    // WCH's __WFE: SETEVENT + WFI-as-WFE consumes any stale event, then sleep for real.
    let sctlr = reg_read(PFIC_SCTLR);
    reg_write(PFIC_SCTLR, sctlr | 1 << 3 | 1 << 5); // WFITOWFE, SETEVENT
    reg_write(PFIC_SCTLR, reg_read(PFIC_SCTLR) & !(1 << 5) | sctlr & 1 << 5);
    wfi();
    reg_write(PFIC_SCTLR, reg_read(PFIC_SCTLR) | 1 << 3);
    wfi();
    clock_init();
}

#[no_mangle]
extern "C" fn main() -> ! {
    // Reset cause, then clear the flags (RMVF) so the next reset is distinguishable.
    debug().reset_flags = (reg_read(RCC_RSTSCKR) >> 24) as u8;
    reg_write(RCC_RSTSCKR, reg_read(RCC_RSTSCKR) | 1 << 24);

    ensure_pd7_gpio();
    clock_init();
    gpio_init();
    reg_write(USART1_BRR, HCLK / 9600);
    reg_write(USART1_CTLR1, 1 << 13 | 1 << 3 | 1 << 2); // UE, TE, RE
    // SysTick free-running up-counter on HCLK.
    reg_write(STK_CTLR, 0b101);
    let boot = now();
    stir(reg_read(ESIG_UNIID1) ^ reg_read(ESIG_UNIID1 + 4) ^ reg_read(ESIG_UNIID1 + 8));

    let mut chip = Controller::new(Usart1);

    set_stage(stage::WAIT_STARTUP);
    // Startup notification, or give up after 3 s and try anyway.
    if pump(&mut chip, 3000, |r| matches!(r, Response::Frame { code: 0x3f, .. })) {
        pump(&mut chip, 200, |_| false);
    }

    // UART round trip without starting audio: proves TX, RX and baud independently of power.
    set_stage(stage::UART_TEST);
    send(&mut chip, &Command::Status); // disposable wake-up request
    query(&mut chip, &Command::Version);
    query(&mut chip, &Command::FileCount);
    query(&mut chip, &Command::GetVolume);

    // Debugger window before the first standby.
    while now().wrapping_sub(boot) < BOOT_AWAKE_MS * (HCLK / 1000) {
        pump(&mut chip, 10, |_| false);
    }

    loop {
        standby();
        let debug = debug();
        debug.wakes = debug.wakes.wrapping_add(1);
        debug.wake_pins = trigger_pins();
        stir(now());
        let result = play(&mut chip, random_track());
        set_stage(result);
        // Let the PIR output settle; a still-high output needs to fall and rise again.
        // A button held down does not replay: only a new press (falling edge) wakes.
        pump(&mut chip, 100, |_| false);
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
