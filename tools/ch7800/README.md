# CH7800 UART controller

Keyboard-driven TUI and scriptable CLI for the CH7800 EK board. Uses a WCH-Link **USB-to-UART serial port**, not its debug interface. No audio plays automatically on connection. Volume is user-controlled over the chip's full **0–30** range; playback commands do not change it.

## Wiring

| WCH-Link UART pin | CH7800 EK header | Signal |
|---|---|---|
| TX | J3-4 | CH7800 RX |
| RX | J3-5 | CH7800 TX |
| GND | J3-2 | Common ground |

Use 3.3 V-compatible TTL UART. **Do not connect WCH-Link 3V3/5V power**; power the EK separately, with its J2/JP1 power path established. Do not confuse the WCH-Link's SWD/SDI pins or virtual SDI printf port with its UART RX/TX pins. The board's USB socket is for audio files, not this serial interface.

## Run

```sh
cargo run --manifest-path tools/ch7800/Cargo.toml -- ports
cargo run --manifest-path tools/ch7800/Cargo.toml -- --port /dev/cu.usbmodemYOURID tui
cargo run --manifest-path tools/ch7800/Cargo.toml -- --port /dev/cu.usbmodemYOURID file-count
cargo run --manifest-path tools/ch7800/Cargo.toml -- --port /dev/cu.usbmodemYOURID volume 7
cargo run --manifest-path tools/ch7800/Cargo.toml -- --port /dev/cu.usbmodemYOURID track 1
```

Use the serial device shown by `ports` (usually `/dev/cu.*` on macOS, `/dev/ttyACM*` on Linux). UART defaults to **9600 8N1**, no hardware flow control. If changed previously, pass `--baud RATE`. The tool automatically sends a disposable wake-up request before its first command and after 4 seconds of inactivity; `--wake` forces this before **every** command for low-power configurations. After changing baud, power-cycle or reset and reconnect at the new rate. WCH-LinkE's manual does not list the CH7800's optional 256000 baud, so it is not offered.

TUI keys: `←/→` previous/next, `p` play, `Space` pause, `x` stop, `+/-` volume, `s` status, `v` volume, `f` files, `:` enter any CLI command, `q` quit. For example, `:track 1`, `:sequence 1 3 2`, `:version`, `:loop-current on`. `Esc` cancels input. `ch7800 --help` lists all commands and parameters, including loop, shuffle, reset, and baud settings. Dangerous persistent actions require `--confirm` in the CLI **and** TUI command line.

While typing after `:`, press `Tab` to complete a unique command or argument. For multiple matches, the first Tab extends the common prefix and shows suggestions; press Tab again to cycle, or `Shift-Tab` to cycle backwards. This works for command names (`:tra` → `:track `), `loop-current on/off`, supported baud rates, and flags such as `--confirm` and `--remember`. Type track numbers and volume levels normally, then press Enter to execute. Press `?` or enter `:help` for a scrollable command reference; use `↑/↓` or Page Up/Down to scroll and Esc to close it. Invalid commands produce a short error rather than dumping CLI usage into the UART log.

`+/-` and CLI `volume-up`/`volume-down` read the current level, set the next level with the CH7800's absolute volume command (`0x16`), then read it back. This avoids relying on relative opcodes `0x14`/`0x15`, which the connected firmware did not acknowledge or apply. For full datasheet coverage, `raw-volume-up` and `raw-volume-down` still send those original opcodes in both CLI and the TUI `:` prompt. The tool never imposes a 7/30 limit.

**Hardware note:** The software does not impose a current or volume limit. If you use a current-limited supply, set an appropriate volume yourself before playing. A reset or factory reset stops playback first, but may restore volume 30; `config.txt` and board switches can also control playback independently of this tool. The earlier hardware test was deliberately done at 7/30; that was a test precaution, not a TUI setting.

If the TUI still says **"current-limited ceiling (7/30)"**, quit it with `q` and rebuild/relaunch with the `cargo run ... tui` command above; an existing process cannot update its code. If a *raw* relative volume command gets no ACK, its effect is unknown—query with `v` before repeating it. If volume **queries** also time out, check the selected WCH-Link UART port, power/GND, TX/RX, and `--baud`.

## Protocol notes

Commands are binary frames, not ASCII hex. ACK (`7E 41 EF`) only means a frame was received; it does **not** prove that audio played. Responses and unsolicited startup/track-completion/error frames are decoded. The manufacturer datasheet V2.1 contradicts itself about track playback (`0x03` vs `0x13`), reset (`0x0C` vs `0x1C`), and factory reset (`0xD1` vs `0xC1`); this tool follows its command table and worked examples (`0x13`, `0x0C`, `0xD1`). Validate persistent commands against your actual firmware before relying on them.

On the tested firmware, `7E 30 00 02 ...` is returned **when track 2 finishes** (including each loop iteration or playback started by a board button). The TUI displays **track 2 finished** while retaining `0x30`, which is not explained in the available datasheet. Consecutive identical notifications are collapsed into one line with a count (for example, `×8`).

Source: [manufacturer CH7800 documentation](https://www.ch2003.com/h-col-124.html); [WCH-Link manual](https://www.olimex.com/Products/RISC-V/WCH/WCH-LinkE/resources/WCH-LinkUserManual.PDF).

Tests: `cargo test --manifest-path tools/ch7800/Cargo.toml`.

Embedded firmware can use the [separate `no_std` UART API](../ch7800-protocol/README.md). Run its tests with `cargo test --manifest-path tools/ch7800-protocol/Cargo.toml`.
