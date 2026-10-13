# rnode-linux

A Linux userspace LoRa daemon that implements the [RNode](https://unsigned.io/rnode/) interface. It exposes a PTY serial device over which [KISS](https://en.wikipedia.org/wiki/KISS_(TNC)) frames can be sent and received, driving a LoRa radio attached via SPI. This makes any Linux single-board computer (SBC) with an SPI-connected LoRa module behave as a standard RNode, compatible with [Reticulum](https://reticulum.network), [Sideband](https://unsigned.io/software/Sideband.html), and any other software that speaks KISS to an RNode.

## Why rnode-linux?

The original [RNode Firmware](https://github.com/markqvist/RNode_Firmware) runs on microcontrollers (ESP32, nRF52, AVR). While these are excellent for dedicated radio hardware, there are situations where running the RNode interface directly on a Linux host is preferable:

- **Single-board computers** like Raspberry Pi, BeagleBone, or ROCK boards that already have SPI buses and GPIO pins exposed can drive a LoRa module directly, without needing a separate microcontroller.
- **Custom gateways and infrastructure nodes** can combine the RNode radio interface with other Linux services in a single device.
- **Development and testing** — a native Linux binary is easier to instrument, debug, and profile than microcontroller firmware.

rnode-linux is a Rust port of the relevant parts of the Arduino firmware, preserving wire compatibility with the KISS protocol as implemented by Reticulum's `RNodeInterface`. The host software sees the same serial interface and speaks the same protocol — it cannot tell the difference between a hardware RNode and rnode-linux.

## Supported Hardware

### LoRa Radio

The current implementation targets the **Semtech SX1262** LoRa transceiver. The driver is a direct port of the Arduino `sx126x.cpp` driver, preserving the same opcode sequences, register addresses, and initialization flow.

The codebase is designed to support additional radio chips in the future via the `Radio` trait. Adding support for the SX1276/SX1278 (LoRa) or SX1280 (2.4 GHz LoRa) would involve implementing this trait for a new driver module.

### SPI Connection

The radio communicates with the host over SPI. Any Linux device with an available SPI bus can be used. Common examples:

| Board | SPI Device | Notes |
|-------|-----------|-------|
| Raspberry Pi | `/dev/spidev0.0` | SPI0, CE0. Enable with `raspi-config` or `dtparam=spi=on` in `/boot/config.txt` |
| Raspberry Pi (CE1) | `/dev/spidev0.1` | SPI0, CE1 |
| BeagleBone Black | `/dev/spidev1.0` | SPI1. Enable via device tree overlay |
| ROCK Pi / Radxa | `/dev/spidev2.0` | Varies by model, check GPIO documentation |

The SX1262 uses **SPI Mode 0** (CPOL=0, CPHA=0) and supports clock speeds up to 16 MHz.

### GPIO Pins

Four GPIO pins are required to interface with the SX1262:

| Signal | Direction | Purpose |
|--------|-----------|---------|
| **RESET** | Output | Resets the radio module. Pulsed low then high (10ms each) during initialization. |
| **BUSY** | Input | Indicates the radio is processing a command. Polled after every SPI transaction. |
| **DIO1** | Input | Interrupt line signaling RX packet ready, TX complete, or timeout. |
| **RXEN** | Output (optional) | Controls an external RX/TX antenna switch, if present on the module. Set to `-1` or omit to disable. |

GPIO is accessed via Linux sysfs (`/sys/class/gpio/`) for maximum compatibility across SBCs. The pins are specified as GPIO line numbers (not physical pin numbers). Consult your board's pinout documentation to find the correct GPIO numbers.

## Building

### Prerequisites

- Rust toolchain (1.70+): install via [rustup](https://rustup.rs/)
- Linux kernel with SPI and GPIO sysfs support (standard on all modern SBC kernels)

### Compile

```bash
cargo build --release
```

The binary is at `target/release/rnode-linux`.

### Cross-compilation

For a Raspberry Pi (ARM):

```bash
rustup target add armv7-unknown-linux-gnueabihf
cargo build --release --target armv7-unknown-linux-gnueabihf
```

For a 64-bit ARM board:

```bash
rustup target add aarch64-unknown-linux-gnu
cargo build --release --target aarch64-unknown-linux-gnu
```

You may need the appropriate cross-compilation toolchain (e.g., `gcc-arm-linux-gnueabihf` or `gcc-aarch64-linux-gnu`).

## Configuration

rnode-linux is configured via a TOML file. Copy the example and edit it for your hardware:

```bash
cp config.example.toml config.toml
```

### Full Configuration Reference

```toml
[spi]
# Path to the SPI device file.
device = "/dev/spidev0.0"

# SPI clock speed in Hz. The SX1262 supports up to 16 MHz.
speed_hz = 16_000_000

[gpio]
# GPIO chip name (usually "gpiochip0" on Raspberry Pi).
chip = "gpiochip0"

# GPIO line numbers (NOT physical pin numbers).
# Use `gpioinfo` or your board's pinout docs to find the right numbers.
reset_line = 17    # Connected to SX1262 RESET pin
busy_line = 27     # Connected to SX1262 BUSY pin
dio0_line = 22     # Connected to SX1262 DIO1 pin

# Optional RX-enable pin for modules with an antenna switch.
# Set to -1 or omit entirely to disable.
# rxen_line = 5

[radio]
# Which radio driver to use. Currently supported: "sx1262"
driver = "sx1262"

# Hardware-specific radio settings. These are properties of the physical
# module and do not change between radio links.

# Number of preamble symbols. Higher values improve sensitivity at the
# cost of airtime.
preamble_symbols = 18

# Enable CRC checking on received packets.
enable_crc = true

# Path to the saved radio settings file. Radio link parameters (frequency,
# bandwidth, spreading factor, coding rate, TX power) are configured via
# KISS commands from the host and can be persisted here.
# If this file exists at startup, the radio auto-configures and goes online.
# If absent, the radio starts offline and waits for KISS configuration.
radio_settings = "/var/lib/rnode/radio_settings.toml"

# SX1262-specific hardware settings.
# These only apply when driver = "sx1262".
[radio.sx1262]

# Enable TCXO (temperature-compensated crystal oscillator).
# Set to true if your module has a TCXO (e.g., Waveshare SX1262 HAT).
enable_tcxo = false

# TCXO supply voltage. Only used when enable_tcxo = true.
# Options: "1.6V", "1.7V", "1.8V", "2.2V", "2.4V", "2.7V", "3.0V", "3.3V"
# tcxo_voltage = "3.3V"

# Use DIO2 as the RF switch control. Most SX1262 modules use this.
dio2_as_rf_switch = true

[pty]
# Optional symlink pointing to the PTY slave device.
# This gives the serial port a stable, predictable name.
symlink = "/tmp/rnode0"

[csma]
# Enable CSMA/CA (carrier sense multiple access with collision avoidance).
enabled = true

# Slot time range for random backoff (milliseconds).
slot_min_ms = 24
slot_max_ms = 100

[logging]
# Log level: "error", "warn", "info", "debug", "trace"
# debug: per-packet RX/TX summaries; trace: hex dump of payloads
# RUST_LOG env var overrides this if set.
log_level = "info"
```

### Mapping Your Hardware to the Config

Here's a step-by-step process for wiring up a LoRa module and writing the config:

#### 1. Identify Your SPI Bus

Check which SPI devices are available:

```bash
ls /dev/spidev*
```

If nothing shows up, enable SPI on your board. On Raspberry Pi:

```bash
sudo raspi-config
# → Interface Options → SPI → Enable
```

Or add to `/boot/config.txt`:

```
dtparam=spi=on
```

Use the device path (e.g., `/dev/spidev0.0`) as `[spi] device`.

#### 2. Wire the SPI Pins

Connect your LoRa module's SPI pins to the SBC's SPI bus:

| SX1262 Pin | Raspberry Pi Pin (SPI0) | GPIO |
|------------|------------------------|------|
| MOSI | Pin 19 (GPIO 10) | — (handled by SPI driver) |
| MISO | Pin 21 (GPIO 9) | — (handled by SPI driver) |
| SCK | Pin 23 (GPIO 11) | — (handled by SPI driver) |
| NSS (CS) | Pin 24 (GPIO 8, CE0) | — (handled by SPI driver) |
| RESET | Pin 11 (GPIO 17) | `reset_line = 17` |
| BUSY | Pin 13 (GPIO 27) | `busy_line = 27` |
| DIO1 | Pin 15 (GPIO 22) | `dio0_line = 22` |
| RXEN | (varies) | `rxen_line = <gpio>` or omit |
| VCC | Pin 1 (3.3V) | — |
| GND | Pin 6 (GND) | — |

The SPI signals (MOSI, MISO, SCK, NSS) are handled by the Linux SPI driver and don't need GPIO configuration. Only RESET, BUSY, DIO1, and optionally RXEN need to be specified in the config.

#### 3. Find GPIO Numbers

GPIO numbers are the logical line numbers, **not** physical pin numbers. On Raspberry Pi, the GPIO number matches the Broadcom (BCM) numbering. On other boards, use:

```bash
# List all GPIO lines and their names
gpioinfo

# Or check the board-specific documentation
cat /sys/kernel/debug/gpio
```

#### 4. Check TCXO

Some LoRa modules (like the Waveshare SX1262 HAT) have a TCXO instead of a plain crystal. Check your module's datasheet or schematic. If it has a TCXO:

```toml
[radio.sx1262]
enable_tcxo = true
tcxo_voltage = "3.3V"   # Match your module's TCXO voltage
```

If unsure, try with `enable_tcxo = false` first. If the radio fails to initialize or produces garbled transmissions, try enabling it.

#### 5. Permissions

The daemon needs access to the SPI device and GPIO sysfs. Either run as root or set up udev rules:

```bash
# /etc/udev/rules.d/99-rnode.rules
SUBSYSTEM=="spidev", MODE="0666"
KERNEL=="gpiochip*", MODE="0666"
```

Then reload rules:

```bash
sudo udevadm control --reload-rules
sudo udevadm trigger
```

## Running

```bash
# With default config.toml in current directory
rnode-linux

# With a specific config file
rnode-linux --config /etc/rnode/config.toml

# Override log level via environment variable
RUST_LOG=debug rnode-linux --config config.toml
```

### Logging

The log level is set in `config.toml` under `[logging]`:

```toml
[logging]
log_level = "info"
```

The `RUST_LOG` environment variable overrides the config file if set.

| Level | What's logged |
|-------|--------------|
| `error` | Fatal errors only |
| `warn` | Recoverable issues (TX failures, config problems) |
| `info` | Startup, radio state changes, config save/delete |
| `debug` | Per-packet summaries — RX: size, RSSI, SNR; TX: size, frame count |
| `trace` | Full hex dump of every packet payload |

Module-specific filtering is also supported:

```toml
# Only show debug for the main event loop, info for everything else
log_level = "info,rnode_linux=debug"

# Trace radio SPI transactions, debug for packets
log_level = "info,rnode_linux=debug,rnode_linux::radio=trace"
```

Example `debug` output:

```
[INFO ] Radio online (loaded from settings)
[DEBUG] RX packet #1: 42 bytes, RSSI=-67 dBm, SNR=9.5 dB
[DEBUG] TX packet: 38 bytes, 1 frame(s), seq=0
[DEBUG] RX packet #2: 128 bytes, RSSI=-82 dBm, SNR=6.0 dB
```

Example `trace` output (adds hex payloads):

```
[DEBUG] RX packet #1: 42 bytes, RSSI=-67 dBm, SNR=9.5 dB
[TRACE] RX payload: [00, 31, A4, FF, 2B, 8C, ...]
```

On startup, the daemon will:

1. Open the SPI device and initialize GPIO pins.
2. Reset the radio module.
3. Check for saved radio settings (`radio_settings` path in config):
   - **If found**: Load the settings, configure the radio, and go online automatically.
   - **If not found**: Start with the radio offline, waiting for KISS commands.
4. Create a PTY serial device and (optionally) a symlink.
5. Print the PTY path and enter the main event loop.

### Connecting with Reticulum

Point Reticulum's `RNodeInterface` at the PTY:

```ini
# ~/.reticulum/config

[interfaces]
  [[RNode LoRa Interface]]
    type = RNodeInterface
    port = /tmp/rnode0
    frequency = 915000000
    bandwidth = 125000
    spreading_factor = 7
    coding_rate = 5
    txpower = 2
```

Reticulum will send the appropriate KISS configuration commands to set up the radio, and the radio will come online.

## Radio Configuration Model

rnode-linux mimics the original RNode firmware's configuration model:

### Startup Behavior

Radio link parameters (frequency, bandwidth, spreading factor, coding rate, TX power) are **not** stored in `config.toml`. Instead, they follow the same lifecycle as the original firmware's EEPROM:

1. **Locked state** — On first run (no saved settings), the radio is offline. All link parameters are unset. The `RadioState(0x01)` command will be rejected until all parameters are configured.

2. **Configuration via KISS** — The host application (e.g., Reticulum) sends KISS commands to set frequency, bandwidth, SF, CR, and TX power. Once all parameters are set, the host sends `RadioState(0x01)` to bring the radio online.

3. **Persistence** — The host can send `CMD_CONF_SAVE` (0x53) to write the current settings to the `radio_settings` file. On subsequent startups, the daemon loads these settings automatically and brings the radio online without waiting for KISS configuration.

4. **Reset** — `CMD_CONF_DELETE` (0x54) deletes the saved settings. The radio will start offline on the next restart.

### Supported KISS Commands

| Command | Byte | Payload | Description |
|---------|------|---------|-------------|
| Data | `0x00` | Raw bytes | Packet data for TX; received packets are delivered with this command |
| Frequency | `0x01` | 4 bytes BE | Set frequency in Hz. Send 0x00000000 to query. |
| Bandwidth | `0x02` | 4 bytes BE | Set bandwidth in Hz. Send 0x00000000 to query. |
| TX Power | `0x03` | 1 byte | Set TX power in dBm (0–22). Send 0xFF to query. |
| Spreading Factor | `0x04` | 1 byte | Set SF (5–12). Send 0xFF to query. |
| Coding Rate | `0x05` | 1 byte | Set CR (5–8). Send 0xFF to query. |
| Radio State | `0x06` | 1 byte | 0x00=off, 0x01=on, 0xFF=query |
| Radio Lock | `0x07` | — | Query lock status (locked if params incomplete) |
| Detect | `0x08` | 0x73 | Device presence handshake |
| Implicit Header | `0x09` | 1 byte | Set implicit header mode length (0=explicit) |
| Leave | `0x0A` | — | Host disconnect signal |
| ST Airtime Lock | `0x0B` | 2 bytes BE | Short-term airtime limit (pct × 100) |
| LT Airtime Lock | `0x0C` | 2 bytes BE | Long-term airtime limit (pct × 100) |
| Promiscuous | `0x0E` | 1 byte | Enable (0x01) or disable (0x00) promiscuous mode |
| Ready | `0x0F` | — | Query TX queue readiness |
| Stat RX | `0x21` | — | Query RX packet count |
| Stat TX | `0x22` | — | Query TX packet count |
| Stat RSSI | `0x23` | — | Query current RSSI |
| Stat SNR | `0x24` | — | Query last packet SNR |
| Random | `0x40` | — | Get random byte from radio |
| Board | `0x47` | — | Query board identifier |
| Platform | `0x48` | — | Query platform identifier |
| FW Version | `0x50` | — | Query firmware version |
| Conf Save | `0x53` | — | Save radio settings to file |
| Conf Delete | `0x54` | — | Delete saved radio settings |
| Reset | `0x55` | 0xF8 | Reset radio hardware |

All KISS frames are delimited by `FEND` (0xC0) with standard `FESC`/`TFEND`/`TFESC` escaping.

## Architecture

```
┌─────────────┐     KISS/PTY      ┌───────────────────────────────────┐
│  Reticulum  │◄─────────────────►│          rnode-linux              │
│  Sideband   │   /tmp/rnode0     │                                   │
│  etc.       │                   │  ┌──────────┐  ┌───────────────┐  │
└─────────────┘                   │  │  KISS    │  │  Packet Queue │  │
                                  │  │  Parser  │  │  & Splitter   │  │
                                  │  └────┬─────┘  └───────┬───────┘  │
                                  │       │                │          │
                                  │  ┌────▼────────────────▼───────┐  │
                                  │  │      CSMA/CA Engine         │  │
                                  │  └────────────┬────────────────┘  │
                                  │               │                   │
                                  │  ┌────────────▼────────────────┐  │
                                  │  │    Radio Driver (SX1262)    │  │
                                  │  └──────┬──────────────┬───────┘  │
                                  │         │              │          │
                                  └─────────┼──────────────┼──────────┘
                                        SPI │         GPIO │
                                  ┌─────────▼──────────────▼──────────┐
                                  │        SX1262 LoRa Module         │
                                  │            ~~~antenna~~~          │
                                  └───────────────────────────────────┘
```

### Source Layout

The repository is a Cargo workspace. `crates/rnode-core` holds the hardware-independent RNode
logic and the `Radio`/`Storage` traits; `crates/rnode-linux` is the daemon. The daemon is being
moved onto `rnode-core` (issue #24); until then its modules are:

```
crates/rnode-core/src/
├── lib.rs           # Crate root
├── radio.rs         # Radio trait: the modem contract implemented by drivers and simulators
├── storage.rs       # RadioSettings persistence trait (CMD_CONF_SAVE / CMD_CONF_DELETE)
├── identity.rs      # Platform / MCU / board / firmware version reported to the host
└── node.rs          # Node: the RNode state machine driven by the platform layer

crates/rnode-linux/src/
├── main.rs          # CLI entry point, event loop, KISS command dispatch
├── lib.rs           # Library crate root (public module re-exports)
├── config.rs        # TOML config parsing, RadioSettings, RadioParams
├── kiss.rs          # KISS protocol parser and frame encoder
├── packet.rs        # TX frame splitting, RX reassembly, packet queue
├── csma.rs          # CSMA/CA with airtime tracking
├── pty.rs           # PTY creation and symlink management
├── spi.rs           # Linux spidev wrapper
├── gpio.rs          # GPIO sysfs wrapper
├── error.rs         # Error types
└── radio/
    ├── mod.rs       # Radio trait (extensibility point for new chips)
    ├── sx126x.rs    # SX1262 driver (port of Arduino sx126x.cpp)
    └── types.rs     # Bandwidth codes, TCXO voltage enums

crates/rnode-linux/tests/
├── kiss_tests.rs          # 43 tests — KISS parser/encoder
├── packet_tests.rs        # 19 tests — packet queue, splitting, reassembly
├── csma_tests.rs          # 14 tests — CSMA and airtime calculations
├── config_tests.rs        # 14 tests — config parsing, settings persistence
└── radio_types_tests.rs   #  9 tests — bandwidth/TCXO type conversions
```

## Extending to Other Radio Chips

The radio driver system uses Rust generics for zero-cost abstraction. The `Radio` trait in `src/radio/mod.rs` defines the interface that all drivers must implement, and `main.rs` dispatches to a monomorphized `run_with_radio<R: Radio>()` based on the `driver` field in the config.

To add support for a new chip (e.g., SX1276 or SX1280):

1. Create a new module (e.g., `src/radio/sx127x.rs`).
2. Implement the `Radio` trait for your driver struct.
3. Add the module to `src/radio/mod.rs`.
4. Add a chip-specific config section (e.g., `Sx127xConfig`) to `src/config.rs`.
5. Add a match arm in `run()` in `main.rs` to construct your driver and call `run_with_radio()`.
6. Set `driver = "sx1276"` in `config.toml` and add `[radio.sx1276]` with chip-specific settings.

The existing SX1262 driver (`src/radio/sx126x.rs`) serves as a comprehensive reference implementation.

## Test Coverage

Code coverage reports are generated with [cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov), which uses rustc's built-in LLVM instrumentation for accurate source-based coverage. This is **fully optional** — if you don't need coverage reports, you don't need to install anything extra.

### One-Time Setup

```bash
# Install the cargo subcommand
cargo install cargo-llvm-cov

# Install the LLVM tools component
rustup component add llvm-tools-preview
```

### Generating Reports

Cargo aliases are provided in `.cargo/config.toml` for convenience:

```bash
# Generate an HTML report (output in target/llvm-cov/html/index.html)
cargo coverage

# Print a coverage summary to the terminal
cargo coverage-summary

# Generate an LCOV report for CI services (output: lcov.info)
cargo coverage-lcov
```

These are equivalent to running `cargo llvm-cov --html`, `cargo llvm-cov`, and `cargo llvm-cov --lcov` directly. All standard `cargo llvm-cov` flags are available if you need more control.

### Viewing the Report

After running `cargo coverage`, open the HTML report:

```bash
open target/llvm-cov/html/index.html    # macOS
xdg-open target/llvm-cov/html/index.html  # Linux
```

## License

This project is licensed under the [GNU General Public License v3.0 or later](https://www.gnu.org/licenses/gpl-3.0.html).

The SX1262 radio driver is derived from code by Sandeep Mistry, Mark Qvist, and Jacob Eva, licensed under the MIT license.
