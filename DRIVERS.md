# Radio Driver Guide

> **Note:** this guide describes the pre-workspace driver architecture of the daemon; source
> paths below are relative to `crates/rnode-linux/`. Drivers are moving to the `Radio` trait in
> `crates/rnode-core` (issue #24), and this guide will be rewritten with that change.

This document explains why rnode-linux uses a pluggable radio driver architecture, how it works, and how to add support for new LoRa radio chips.

---

## Table of Contents

- [Why Multiple Drivers?](#why-multiple-drivers)
- [Architecture Overview](#architecture-overview)
- [The `Radio` Trait](#the-radio-trait)
- [How Driver Dispatch Works](#how-driver-dispatch-works)
- [Hardware Abstraction: SPI and GPIO](#hardware-abstraction-spi-and-gpio)
- [Adding a New Driver: Step by Step](#adding-a-new-driver-step-by-step)
- [SX1262 Driver Walkthrough](#sx1262-driver-walkthrough)

---

## Why Multiple Drivers?

LoRa is a modulation technique defined by Semtech, but Semtech sells several different radio ICs that implement it. Other manufacturers make compatible chips too. While all of these radios perform the same high-level function — modulating and demodulating LoRa packets — their low-level hardware interfaces are **completely different**.

Here's what varies between chips:

| Aspect | SX1262 | SX1276/78 | SX1280 |
|---|---|---|---|
| **SPI protocol** | Opcode-based (single opcode byte + params) | Register-addressed (address + data) | Opcode-based (similar to SX1262 but different opcodes) |
| **Frequency range** | 150–960 MHz | 137–1020 MHz | 2.4 GHz ISM band |
| **TX power range** | −9 to +22 dBm | −4 to +20 dBm (SX1276) | −18 to +12.5 dBm |
| **PA topology** | Integrated high-power PA | Separate RFO and PA_BOOST pins | Integrated PA |
| **FIFO/buffer** | 256-byte FIFO via opcode | 256-byte FIFO via register 0x00 | 256-byte FIFO via opcode |
| **Init sequence** | Calibrate → set packet type → configure | Set mode → write register bank | Similar to SX1262, different opcodes |
| **IRQ handling** | Read via opcode (OP_GET_IRQ_STATUS) | Read register (REG_IRQ_FLAGS) | Read via opcode |
| **TCXO control** | DIO3 opcode | Not applicable (external) | DIO3 opcode (different) |
| **Bandwidth codes** | Enumerated (0x00–0x0A) | Register field (mantissa/exponent) | Different enumeration |

A single hardcoded driver cannot handle these differences. The SPI command sequences, register addresses, initialization order, and even the way you read an RSSI value are fundamentally different between chip families.

rnode-linux solves this with a **trait-based driver architecture**: a common interface that all drivers implement, with the rest of the daemon operating entirely through that interface.

---

## Architecture Overview

The system has four layers:

```
┌─────────────────────────────────────────────────┐
│              Application Layer                  │
│                                                 │
│  PTY ↔ KISS Parser → Packet Queue → TX/RX Loop  │
│                                                 │
│  All application code is generic over R: Radio  │
├─────────────────────────────────────────────────┤
│              Radio Trait                        │
│                                                 │
│  pub trait Radio {                               │
│      fn begin(&mut self, freq: u32) -> ...      │
│      fn set_frequency(&mut self, f: u32) -> ... │
│      fn read_packet(&mut self, buf) -> ...      │
│      ...                                        │
│  }                                              │
├────────────────┬────────────────────────────────┤
│  SX1262 Driver │  (future) SX1276  │  SX1280   │
│  src/radio/    │  Driver           │  Driver    │
│  sx126x.rs     │                   │            │
├────────────────┴────────────────────────────────┤
│          Hardware Abstraction                   │
│                                                 │
│  SpiDevice (spidev)    RadioGpio (sysfs GPIO)   │
│  src/spi.rs            src/gpio.rs              │
└─────────────────────────────────────────────────┘
```

### Compile-Time Dispatch (Zero-Cost Generics)

rnode-linux uses **Rust generics** rather than trait objects (`dyn Radio`) for driver dispatch. This means:

1. `run()` reads the `driver` field from config and constructs a **concrete** driver type
2. It calls `run_with_radio::<Sx126x>(radio, config)` (or `<Sx127x>`, etc.)
3. The Rust compiler **monomorphizes** `run_with_radio` — it generates a separate, fully-optimized copy of the entire event loop for each radio type
4. There is **zero runtime overhead** — no vtable lookups, no dynamic dispatch, no indirection

This is important for an embedded-adjacent application that runs tight TX/RX polling loops on low-power hardware.

```
config.toml                    Compiled Binary
┌──────────────┐
│ driver =     │    ┌──────────────────────────────────────────┐
│   "sx1262"  ─┼───▶│ run_with_radio::<Sx126x>(radio, config) │
└──────────────┘    │   (monomorphized, fully inlined)         │
                    └──────────────────────────────────────────┘

                    ┌──────────────────────────────────────────┐
(future)       ───▶ │ run_with_radio::<Sx127x>(radio, config) │
driver="sx1276"     │   (separate optimized copy)             │
                    └──────────────────────────────────────────┘
```

---

## The `Radio` Trait

The `Radio` trait (`src/radio/mod.rs`) is the contract that every driver must satisfy. It has 34 methods, described below in five groups:

### Lifecycle Methods

These control the radio's overall state:

```rust
/// Initialize the radio at the given frequency (Hz).
/// This is where the full init sequence happens: reset, calibrate,
/// set packet type, configure modulation, set up IRQs, etc.
fn begin(&mut self, frequency: u32) -> Result<()>;

/// Shut down the radio. Typically puts it into deep sleep.
fn end(&mut self) -> Result<()>;

/// Enter standby mode (oscillator running, ready for commands).
fn standby(&mut self) -> Result<()>;

/// Enter sleep mode (lowest power, needs re-init to wake).
fn sleep(&mut self) -> Result<()>;
```

**What `begin()` must do** (see [SX1262 example](#initialization-beginselffrequency)):
- Reset the hardware via GPIO
- Verify the chip is present (e.g., read sync word register)
- Run any required calibration
- Set the radio to LoRa packet mode
- Configure sync word, IRQ routing, LNA, and buffer addresses
- Set the initial frequency and default TX power

### Transmit Methods

These handle sending packets:

```rust
/// Prepare for a new TX packet. Resets the FIFO write pointer
/// and sets header mode (implicit or explicit).
fn begin_packet(&mut self, implicit_header: bool) -> Result<()>;

/// Write payload data into the TX FIFO buffer.
/// Returns the number of bytes actually written (may be less than
/// data.len() if the FIFO is full).
fn write_packet(&mut self, data: &[u8]) -> Result<usize>;

/// Finalize and transmit. This is a blocking call that waits for
/// the TX_DONE interrupt (or times out). Returns true on success.
fn end_packet(&mut self) -> Result<bool>;
```

The TX flow is always: `begin_packet()` → one or more `write_packet()` calls → `end_packet()`.

### Receive Methods

These handle incoming packets:

```rust
/// Enter continuous receive mode. The radio listens indefinitely
/// until a packet arrives or you call standby/sleep.
fn receive(&mut self) -> Result<()>;

/// Read a received packet into `buf`. Returns byte count.
fn read_packet(&mut self, buf: &mut [u8]) -> Result<usize>;

/// Get the number of unread bytes from the last received packet.
fn available(&mut self) -> Result<usize>;

/// Handle a DIO0 rising-edge interrupt.
/// Called when the GPIO interrupt fires. Should:
/// 1. Read and clear the IRQ status register
/// 2. Check for CRC errors (return None if invalid)
/// 3. Read the packet from the FIFO into an internal buffer
/// 4. Return Some(packet_length) if valid
fn handle_dio0_rise(&mut self) -> Result<Option<usize>>;
```

The main event loop calls `handle_dio0_rise()` when DIO0 goes high, then reads the packet data with `read_packet()`.

### Configuration Methods

These set and query radio link parameters. The daemon calls these in response to KISS commands from the host:

```rust
fn set_frequency(&mut self, freq: u32) -> Result<()>;
fn get_frequency(&self) -> u32;

fn set_bandwidth(&mut self, bw: u32) -> Result<()>;  // bw in Hz
fn get_signal_bandwidth(&self) -> u32;

fn set_spreading_factor(&mut self, sf: u8) -> Result<()>;  // 5–12
fn get_spreading_factor(&self) -> u8;

fn set_coding_rate(&mut self, cr: u8) -> Result<()>;  // 5–8 (= 4/5 to 4/8)
fn get_coding_rate(&self) -> u8;

fn set_tx_power(&mut self, dbm: i8) -> Result<()>;
fn get_tx_power(&self) -> i8;

fn set_preamble_length(&mut self, symbols: u16) -> Result<()>;
fn enable_crc(&mut self) -> Result<()>;
fn disable_crc(&mut self) -> Result<()>;
fn set_implicit_header_mode(&mut self, payload_length: u8) -> Result<()>;
fn set_explicit_header_mode(&mut self) -> Result<()>;
```

**Important**: Configuration methods are called individually as KISS commands arrive. Your driver must update both its internal state **and** the hardware registers immediately. For example, when `set_bandwidth()` is called, the SX1262 driver updates its stored bandwidth code, recalculates the low-data-rate optimization flag, and writes the new modulation parameters to the radio in one call.

### Status Methods

These provide signal quality and diagnostic information:

```rust
fn current_rssi(&mut self) -> Result<i16>;     // dBm, live reading
fn packet_rssi(&mut self) -> Result<i16>;      // dBm, last packet
fn packet_rssi_raw(&mut self) -> Result<u8>;   // Raw register value
fn current_rssi_raw(&mut self) -> Result<u8>;  // Raw register value
fn packet_snr_raw(&mut self) -> Result<u8>;    // Raw register value
fn packet_snr(&mut self) -> Result<f32>;       // dB, last packet
fn random(&mut self) -> Result<u8>;            // Hardware RNG byte

/// Data Carrier Detect: returns true if a LoRa signal is currently
/// being received (preamble or header detected).
fn dcd(&mut self) -> Result<bool>;
```

The RSSI and SNR values are used for KISS status reporting and for logging. The `dcd()` method is used by the CSMA/CA system to detect channel activity before transmitting.

---

## How Driver Dispatch Works

### Configuration

The `[radio]` section in `config.toml` has a `driver` field that selects the chip:

```toml
[radio]
driver = "sx1262"       # Selects which Radio implementation to use
preamble_symbols = 18   # Shared across all drivers
enable_crc = true       # Shared across all drivers

[radio.sx1262]          # SX1262-specific hardware settings
enable_tcxo = false
dio2_as_rf_switch = true
```

Each driver has its own config sub-table for chip-specific settings. These are things that depend on the physical module's hardware design, not the radio link parameters (which are configured via KISS).

In `src/config.rs`, the shared and per-driver configs are defined as:

```rust
pub struct RadioConfig {
    #[serde(default = "default_driver")]
    pub driver: String,           // "sx1262", "sx1276", etc.

    pub preamble_symbols: u16,    // Shared
    pub enable_crc: bool,         // Shared
    pub radio_settings: Option<String>,  // Shared

    #[serde(default)]
    pub sx1262: Sx1262Config,     // Chip-specific (ignored if driver != "sx1262")
}

pub struct Sx1262Config {
    pub enable_tcxo: bool,
    pub tcxo_voltage: Option<String>,
    pub dio2_as_rf_switch: bool,
}
```

### Dispatch in `main.rs`

The `run()` function is the only place in the codebase that knows about concrete driver types:

```rust
fn run(config: Config, spi_log: Option<&str>) -> error::Result<()> {
    setup_signal_handlers();

    let spi = SpiDevice::open(&config.spi.device, config.spi.speed_hz, spi_log)?;
    let gpio = RadioGpio::new(/* ... */)?;

    // --- This is the dispatch point ---
    match config.radio.driver.as_str() {
        "sx1262" => {
            let tcxo_voltage = config.radio.sx1262.tcxo_voltage
                .as_deref()
                .and_then(TcxoVoltage::from_str)
                .unwrap_or(TcxoVoltage::V3_3);

            let radio = Sx126x::new(
                spi, gpio,
                config.radio.sx1262.enable_tcxo,
                tcxo_voltage,
                config.radio.sx1262.dio2_as_rf_switch,
            );
            run_with_radio(radio, config)  // <-- monomorphized for Sx126x
        }
        other => Err(/* unknown driver error */),
    }
}
```

### The Generic Event Loop

Everything downstream of `run()` is generic:

```rust
fn run_with_radio<R: Radio>(mut radio: R, config: Config) -> error::Result<()> {
    // Load saved settings, set up PTY, enter event loop...
    // All radio operations go through the Radio trait:
    radio.begin(freq)?;
    radio.set_bandwidth(bw)?;
    radio.receive()?;
    // ...
}

fn handle_command<R: Radio>(cmd: KissCommand, radio: &mut R, /* ... */) { ... }
fn check_dio0<R: Radio>(_radio: &R) -> error::Result<bool> { ... }
```

The compiler generates optimized, specialized code for each `R` — there's no dynamic dispatch at runtime.

---

## Hardware Abstraction: SPI and GPIO

Drivers don't talk directly to Linux device files. Instead, they use two abstraction structs that are passed into their constructors.

### `SpiDevice` (`src/spi.rs`)

Wraps the Linux `spidev` interface. Provides the primitives that radio drivers need:

| Method | Purpose | Used By |
|---|---|---|
| `transfer(tx, rx)` | Full-duplex SPI transfer | Register reads, FIFO reads |
| `write(data)` | Write-only SPI transfer | Opcodes, register writes |
| `execute_opcode(op, buf)` | Send opcode + payload (write-only) | SX1262: every command |
| `execute_opcode_read(op, n)` | Send opcode, read N response bytes | SX1262: IRQ status, RSSI |
| `read_register(op, addr)` | Read a 16-bit-addressed register | SX1262: sync word, LNA |
| `write_register(op, addr, val)` | Write a 16-bit-addressed register | SX1262: OCP, sync word |
| `write_fifo(op, offset, data)` | Write to packet FIFO buffer | SX1262: TX data |
| `read_fifo(op, offset, n)` | Read from packet FIFO buffer | SX1262: RX data |

**Note**: The higher-level methods like `execute_opcode` and `read_register` encode the SX1262's SPI protocol (opcode byte + address bytes + NOP padding). If your radio uses a different SPI protocol (e.g., the SX1276's simple address+data format), you can use the low-level `transfer()` and `write()` methods directly, or add new helper methods to `SpiDevice`.

### `RadioGpio` (`src/gpio.rs`)

Manages the GPIO lines that connect the Linux host to the radio module:

| Line | Direction | Purpose |
|---|---|---|
| `reset` | Output | Hardware reset (active low pulse) |
| `busy` | Input | SX1262 busy indicator (wait before SPI commands) |
| `dio0` | Input (rising edge) | Interrupt: RX done, TX done |
| `rxen` | Output (optional) | RX antenna switch enable |

`RadioGpio` provides convenience methods:

```rust
gpio.reset_radio()?;    // Pulse reset low for 10ms, high for 10ms
gpio.wait_on_busy()?;   // Spin until busy line goes low (100ms timeout)
gpio.rx_ant_enable()?;  // Assert rxen if present
```

**For other chips**: The `busy` line is specific to the SX126x family. The SX1276 doesn't have one — it's always ready after a short delay. Your driver can simply ignore `gpio.busy` if your chip doesn't use it. The `reset` and `dio0` lines are universal across LoRa radios.

---

## Adding a New Driver: Step by Step

This walkthrough adds a hypothetical SX1276 driver. The SX1276 is a common, well-documented LoRa chip with a simpler register-based SPI interface.

### Step 1: Create the Module File

Create `src/radio/sx127x.rs`:

```rust
//! SX1276/SX1278 LoRa radio driver for Linux.

use crate::error::{RNodeError, Result};
use crate::gpio::RadioGpio;
use crate::radio::Radio;
use crate::spi::SpiDevice;

pub struct Sx127x {
    spi: SpiDevice,
    gpio: RadioGpio,
    frequency: u32,
    txp: i8,
    sf: u8,
    bw: u8,
    cr: u8,
    preamble_length: u16,
    implicit_header_mode: bool,
    crc_enabled: bool,
    // ... any chip-specific state
}
```

Compare with the SX1262 driver's struct in `src/radio/sx126x.rs` (line 85). The SX1262 needs additional state like `ldro` (low data rate optimization), `fifo_tx_addr_ptr`, `preinit_done`, and TCXO settings. Your struct should hold whatever state your chip requires.

### Step 2: Implement the Constructor

```rust
impl Sx127x {
    pub fn new(spi: SpiDevice, gpio: RadioGpio) -> Self {
        Self {
            spi,
            gpio,
            frequency: 0,
            txp: 17,
            sf: 7,
            bw: 0x07,  // 125 kHz for SX1276
            cr: 1,
            preamble_length: 18,
            implicit_header_mode: false,
            crc_enabled: true,
        }
    }
}
```

The SX1276 doesn't need TCXO or DIO2 config, so the constructor is simpler than the SX1262's (see `sx126x.rs` line 109).

### Step 3: Add SPI Helper Methods

The SX1276 uses a simpler register-addressed SPI protocol compared to the SX1262's opcode protocol. You'll likely want internal helpers:

```rust
impl Sx127x {
    /// Read a single register. SX1276 protocol: send address byte
    /// (bit 7 = 0 for read), receive data byte.
    fn read_reg(&mut self, addr: u8) -> Result<u8> {
        let tx = [addr & 0x7F, 0x00];
        let mut rx = [0u8; 2];
        self.spi.transfer(&tx, &mut rx)?;
        Ok(rx[1])
    }

    /// Write a single register. SX1276 protocol: send address byte
    /// (bit 7 = 1 for write), then data byte.
    fn write_reg(&mut self, addr: u8, value: u8) -> Result<()> {
        let tx = [addr | 0x80, value];
        self.spi.write(&tx)
    }

    /// Burst write to FIFO (register 0x00).
    fn write_fifo(&mut self, data: &[u8]) -> Result<()> {
        let mut tx = Vec::with_capacity(1 + data.len());
        tx.push(0x80); // FIFO address with write bit
        tx.extend_from_slice(data);
        self.spi.write(&tx)
    }
}
```

Notice how this uses `SpiDevice::transfer()` and `SpiDevice::write()` directly, unlike the SX1262 driver which goes through `execute_opcode()`. Different chips, different protocols — same underlying SPI bus.

### Step 4: Implement the `Radio` Trait

This is the bulk of the work. Each method translates the trait's abstract operations into chip-specific register reads and writes.

Here are a few key methods with annotations:

```rust
impl Radio for Sx127x {
    fn begin(&mut self, frequency: u32) -> Result<()> {
        // Reset the radio
        self.gpio.reset_radio()?;

        // Verify chip is present by reading the version register
        let version = self.read_reg(0x42)?; // REG_VERSION
        if version != 0x12 {
            return Err(RNodeError::Radio(
                format!("SX1276 not detected (version: 0x{:02X})", version)
            ));
        }

        // Enter sleep mode to access LoRa registers
        self.write_reg(0x01, 0x80)?; // REG_OP_MODE: sleep + LoRa mode

        // Set frequency
        self.set_frequency(frequency)?;

        // Configure LNA boost
        self.write_reg(0x0C, self.read_reg(0x0C)? | 0x03)?;

        // Set auto AGC
        self.write_reg(0x26, 0x04)?; // REG_MODEM_CONFIG_3

        // Set TX power
        self.set_tx_power(self.txp)?;

        // Enter standby
        self.standby()?;

        Ok(())
    }

    fn set_frequency(&mut self, freq: u32) -> Result<()> {
        self.frequency = freq;
        // SX1276 formula: frf = freq * 2^19 / 32MHz
        let frf = ((freq as u64) << 19) / 32_000_000;
        self.write_reg(0x06, ((frf >> 16) & 0xFF) as u8)?; // REG_FRF_MSB
        self.write_reg(0x07, ((frf >> 8) & 0xFF) as u8)?;  // REG_FRF_MID
        self.write_reg(0x08, (frf & 0xFF) as u8)?;         // REG_FRF_LSB
        Ok(())
    }

    fn set_tx_power(&mut self, level: i8) -> Result<()> {
        // SX1276 has two PA paths: RFO and PA_BOOST.
        // Most modules use PA_BOOST (up to +20 dBm).
        let level = level.clamp(2, 20);
        if level > 17 {
            // Enable +20 dBm mode
            self.write_reg(0x4D, 0x87)?; // REG_PA_DAC
            self.write_reg(0x09, 0x80 | ((level - 5) as u8 & 0x0F))?;
        } else {
            self.write_reg(0x4D, 0x84)?; // Normal mode
            self.write_reg(0x09, 0x80 | ((level - 2) as u8 & 0x0F))?;
        }
        self.txp = level;
        Ok(())
    }

    // Compare with the SX1262 version (sx126x.rs line 516):
    // The SX1262 uses PA config opcodes, OCP registers, and TX param
    // opcodes — a completely different sequence for the same logical
    // operation. This is exactly why the trait abstraction exists.

    fn handle_dio0_rise(&mut self) -> Result<Option<usize>> {
        // Read IRQ flags register
        let irq = self.read_reg(0x12)?; // REG_IRQ_FLAGS
        // Clear all IRQ flags
        self.write_reg(0x12, irq)?;

        // Check for CRC error
        if (irq & 0x20) != 0 { // IRQ_PAYLOAD_CRC_ERROR
            return Ok(None);
        }

        // Read packet length
        let length = if self.implicit_header_mode {
            self.read_reg(0x13)? as usize // REG_RX_NB_BYTES
        } else {
            self.read_reg(0x13)? as usize
        };

        // Set FIFO address to current RX address
        let current_addr = self.read_reg(0x10)?; // REG_FIFO_RX_CURRENT_ADDR
        self.write_reg(0x0D, current_addr)?;     // REG_FIFO_ADDR_PTR

        Ok(Some(length))
    }

    // ... implement remaining methods ...
}
```

### Step 5: Register the Module

Add your module to `src/radio/mod.rs`:

```rust
pub mod sx126x;
pub mod sx127x;  // ← Add this line
pub mod types;
```

### Step 6: Add Chip-Specific Config

In `src/config.rs`, add a config struct for your chip's hardware settings:

```rust
/// SX1276-specific hardware configuration.
#[derive(Debug, Deserialize, Clone)]
pub struct Sx1276Config {
    /// Which PA output pin to use: "pa_boost" or "rfo".
    /// Most modules use PA_BOOST; only use RFO if your board routes it.
    #[serde(default = "default_pa_output")]
    pub pa_output: String,

    /// Maximum TX power in dBm. Hardware-dependent, usually 17 or 20.
    #[serde(default = "default_max_tx_power")]
    pub max_tx_power: i8,
}

fn default_pa_output() -> String { "pa_boost".to_string() }
fn default_max_tx_power() -> i8 { 17 }

impl Default for Sx1276Config {
    fn default() -> Self {
        Self {
            pa_output: default_pa_output(),
            max_tx_power: default_max_tx_power(),
        }
    }
}
```

Add the field to `RadioConfig`:

```rust
pub struct RadioConfig {
    pub driver: String,
    pub preamble_symbols: u16,
    pub enable_crc: bool,
    pub radio_settings: Option<String>,
    pub sx1262: Sx1262Config,
    pub sx1276: Sx1276Config,  // ← Add this
}
```

### Step 7: Wire Into Driver Dispatch

In `src/main.rs`, add a match arm in `run()`:

```rust
match config.radio.driver.as_str() {
    "sx1262" => {
        // ... existing SX1262 construction ...
        run_with_radio(radio, config)
    }
    "sx1276" => {
        let radio = Sx127x::new(spi, gpio);
        // Could pass config.radio.sx1276.pa_output, etc. to constructor
        info!("Using SX1276 radio driver");
        run_with_radio(radio, config)
    }
    other => Err(/* ... */),
}
```

Add the import at the top of `main.rs`:

```rust
use rnode_linux::radio::sx127x::Sx127x;
```

### Step 8: Update Configuration Files

Add a `[radio.sx1276]` section to `config.example.toml`:

```toml
[radio]
driver = "sx1276"
preamble_symbols = 18
enable_crc = true
radio_settings = "/var/lib/rnode/radio_settings.toml"

# SX1276-specific hardware settings
[radio.sx1276]
pa_output = "pa_boost"
max_tx_power = 17
```

### Step 9: Add Tests

Create `tests/sx127x_tests.rs` (or add to `tests/radio_types_tests.rs`). At minimum, test:

- Config parsing for `[radio.sx1276]`
- Driver dispatch selects the right type
- Any frequency/bandwidth conversion math your driver uses

Since the `Radio` trait methods require real SPI hardware, most driver testing will be integration-level. The unit tests focus on the math and config parsing.

### Step 10: Update Documentation

- Add your chip to the driver table in `README.md`
- Update the `Supported drivers` error message in the `run()` match
- Update `DRIVERS.md` if you encounter patterns worth documenting

---

## SX1262 Driver Walkthrough

The existing SX1262 driver (`src/radio/sx126x.rs`) is a direct port of the Arduino `sx126x.cpp` from RNode_Firmware. Here's an annotated tour of its key sections.

### Structure and State (line 85)

```rust
pub struct Sx126x {
    spi: SpiDevice,           // SPI bus handle
    gpio: RadioGpio,          // GPIO lines (reset, busy, dio0, rxen)
    frequency: u32,           // Current frequency in Hz
    txp: i8,                  // Current TX power in dBm
    sf: u8,                   // Spreading factor register value (5–12)
    bw: u8,                   // Bandwidth register code (0x00–0x0A)
    cr: u8,                   // Coding rate register value (1–4, meaning 4/5 to 4/8)
    ldro: u8,                 // Low Data Rate Optimization (0 or 1)
    preamble_length: u16,     // Preamble symbols
    implicit_header_mode: u8, // 0 = explicit, 1 = implicit
    payload_length: u8,       // Current TX payload length
    crc_mode: u8,             // 0 = disabled, 1 = enabled
    fifo_tx_addr_ptr: u8,     // Write pointer into TX FIFO
    fifo_rx_addr_ptr: u8,     // Read pointer into RX FIFO
    packet_buf: [u8; 255],    // Internal packet buffer
    packet_index: usize,      // Read cursor into packet_buf
    preinit_done: bool,       // Whether pre-init check has passed
    enable_tcxo: bool,        // Config: use TCXO
    tcxo_voltage: TcxoVoltage,// Config: TCXO voltage
    dio2_as_rf_switch: bool,  // Config: DIO2 controls RF switch
    ocp_value: u8,            // Over-current protection value
}
```

The driver stores **all mutable radio state** internally. When the daemon calls `set_spreading_factor(10)`, the driver updates `self.sf`, recalculates LDRO, and immediately writes the new modulation parameters to the radio via SPI.

### Initialization (`begin()`, line 310)

The `begin()` method follows the SX1262 datasheet's recommended init sequence:

```
gpio.reset_radio()          ← Hardware reset pulse
    │
    ▼
pre_init()                  ← Poll sync word register until chip responds
    │                         (confirms SPI communication works)
    ▼
calibrate()                 ← Full RC + PLL + ADC calibration
calibrate_image(freq)       ← Image rejection calibration for freq band
enable_tcxo_internal()      ← Configure DIO3 as TCXO control (if enabled)
    │
    ▼
lora_mode()                 ← Set packet type to LoRa (opcode 0x8A)
standby()                   ← Enter STDBY_XOSC mode
set_sync_word()             ← Write 0x1424 to sync word registers
    │
    ▼
DIO2 RF switch              ← Enable DIO2 as antenna switch (if configured)
rx_ant_enable()             ← Assert RXEN GPIO (if present)
set_frequency(freq)         ← Program the PLL
set_tx_power(2)             ← Default 2 dBm
enable_crc()                ← Enable CRC checking
    │
    ▼
LNA boost                   ← Write 0x96 to LNA register
Buffer base addresses        ← Set TX/RX buffer start to 0x00
Modulation params            ← Write SF/BW/CR/LDRO
Packet params                ← Write preamble/header/payload/CRC config
IRQ setup                    ← Route RX_DONE to DIO0
```

### SPI Command Pattern

Every SPI transaction follows the same pattern in the SX1262 driver:

```rust
fn execute_opcode(&mut self, opcode: u8, buffer: &[u8]) -> Result<()> {
    self.wait_on_busy()?;   // ← Always wait for busy line to clear first
    self.spi.execute_opcode(opcode, buffer)
}
```

The `wait_on_busy()` is critical — the SX1262 asserts its BUSY pin after every SPI command while it processes internally. Sending a new command before BUSY clears causes undefined behavior.

If your chip doesn't have a busy line (like the SX1276), you can skip this step or replace it with a fixed delay.

### Frequency Calculation (line 456)

The SX1262 calculates frequency register value using:

```
frf = frequency / FREQ_STEP
    = frequency / (XTAL_FREQ / 2^25)
    = frequency * 2^25 / 32_000_000
```

This is written as a 4-byte big-endian value via opcode `0x86`:

```rust
fn set_frequency(&mut self, freq: u32) -> Result<()> {
    self.frequency = freq;
    let frf = (freq as f64 / FREQ_STEP) as u32;
    let buf = [
        ((frf >> 24) & 0xFF) as u8,
        ((frf >> 16) & 0xFF) as u8,
        ((frf >> 8) & 0xFF) as u8,
        (frf & 0xFF) as u8,
    ];
    self.execute_opcode(OP_RF_FREQ, &buf)
}
```

The SX1276 uses a different formula (`freq * 2^19 / 32MHz`) and writes to three separate registers instead of using an opcode. Same result, completely different implementation.

### Low Data Rate Optimization (line 252)

When the LoRa symbol duration exceeds 16ms (low bandwidth + high spreading factor), the SX1262 requires Low Data Rate Optimization to be enabled. The driver calculates this automatically:

```rust
fn handle_low_data_rate(&mut self) {
    let bw_hz = /* convert self.bw code to Hz */;
    let symbol_duration = (1 << self.sf) as f64 / (bw_hz as f64 / 1000.0);
    self.ldro = if symbol_duration > 16.0 { 0x01 } else { 0x00 };
}
```

This is called automatically by `set_bandwidth()` and `set_spreading_factor()`, so the caller never needs to manage it manually. If your radio chip handles LDRO differently (or doesn't need it), implement this logic accordingly in your driver.

### TX Power Configuration (line 516)

The SX1262's power amplifier configuration is one of the more complex chip-specific operations:

```
1. Read and patch the TX clamp register (SX1262 errata §15.2)
2. Write PA config: duty cycle, HP max, device select, PA LUT
3. Clamp power to −9..+22 dBm range
4. Write OCP (over-current protection) value
5. Write TX params: power level + PA ramp time
```

This is completely different from the SX1276, which simply writes a power level to a single register (with a mode bit for RFO vs PA_BOOST). Each chip family has its own PA topology and the driver must handle it correctly.

---

## Summary

To add support for a new LoRa radio chip in rnode-linux:

1. **Create** `src/radio/your_chip.rs` with your driver struct
2. **Implement** all 34 methods of the `Radio` trait
3. **Register** the module in `src/radio/mod.rs`
4. **Add** a chip-specific config struct in `src/config.rs`
5. **Add** a match arm in `run()` in `src/main.rs`
6. **Update** `config.example.toml` with your chip's settings section
7. **Test** with `cargo test` and on real hardware

The existing SX1262 driver (`src/radio/sx126x.rs`, ~560 lines) is your best reference. Start by reading `begin()` and `set_frequency()` to understand the SPI command patterns, then implement the remaining methods following your chip's datasheet.
