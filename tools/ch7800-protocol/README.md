# `ch7800-protocol` (embedded UART API)

`no_std`, allocation-free CH7800 command encoder and response parser. No runtime, serial driver, timer, or heap required. The desktop CLI/TUI at `../ch7800` uses this **same crate** for its wire format.

```toml
[dependencies]
ch7800-protocol = { path = "../ch7800-protocol" }
```

Implement `UartTx` for your UART driver, then feed received bytes to `Controller::receive` from your main loop/interrupt task. The controller does not own the RX peripheral or timer:

```rust,ignore
use ch7800_protocol::{Command, Controller, Response, UartTx};

struct MyUart { /* MCU-specific UART */ }
impl UartTx for MyUart {
    type Error = MyUartError;
    fn write(&mut self, bytes: &[u8]) -> Result<(), Self::Error> {
        // Send the complete byte slice, in order, over 8N1 UART.
        my_uart_write_all(bytes)
    }
}

let mut chip = Controller::new(MyUart { /* ... */ });
// If needed for your power supply, choose a suitable volume first.
chip.send(&Command::Volume { level: 7, remember: false })?;
// Wait for ACK; allow at least 20 ms between frames.
chip.send(&Command::GetVolume)?;
// Feed *every* incoming byte to chip.receive(byte).
chip.send(&Command::Track(1))?;
```

`Controller` does not impose a volume limit or change volume before playback. `Command::Volume` accepts 0–30. If your hardware requires a limit, apply and verify it in your application; a chip `config.txt` can override UART settings. Physical buttons and other controllers can start playback independently.

For more control, `Command::frame().as_bytes()` and `Parser::push(byte)` are public. `Command::Sequence(&[1, 3, 2])` accepts up to 30 byte-sized root-directory track numbers; normal `Command::Track(999)` uses a two-byte number. The maximum frame is 35 bytes. `Response::payload()` returns only the received data bytes; ACK is `Response::Ack`.

Use 9600 8N1 by default, 3.3 V-compatible TTL signals, with no flow control. Allow **at least 20 ms between commands**, and wait for the startup frame or about 2 s after power-on. For low-power mode, send a disposable request, wait **at least 50 ms**, then send the real request within 5 s. Handle timeouts and unsolicited frames in your firmware. UART TX and RX pins must be crossed, with common ground; do not power the EK through the WCH-Link power pin.

Test with `cargo test --manifest-path tools/ch7800-protocol/Cargo.toml` from the repo root.
