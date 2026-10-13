# rnode-linux

rnode-linux turns a Linux computer with a LoRa modem on its SPI bus into an
[RNode](https://unsigned.io/rnode/). The daemon drives the modem directly and exposes a
pseudo-terminal (PTY) that speaks the RNode KISS host protocol. Reticulum's `RNodeInterface`, or
any other RNode host, uses that PTY exactly like the serial port of a USB-attached RNode running
RNode_Firmware. On the air, rnode-linux interoperates with real RNodes.

The supported modem is the Semtech SX1262. The reference platform is the Clockwork uConsole (CM5)
with the HackerGadgets AIO V2 extension board. A simulated radio lets you run everything, including
Reticulum end-to-end tests, on any Linux or macOS machine without radio hardware.

Behaviour follows [RNode_Firmware](https://github.com/markqvist/RNode_Firmware) v1.86 (commit
`56775c5`) wherever a host or another radio can observe it. The deliberate differences are listed in
[Differences from RNode_Firmware](#differences-from-rnode_firmware).

## Contents

- [Why rnode-linux](#why-rnode-linux)
- [How it works](#how-it-works)
- [Workspace layout](#workspace-layout)
- [Safety and radio regulations](#safety-and-radio-regulations)
- [Quick start without hardware](#quick-start-without-hardware)
- [Building](#building)
- [Setting up the LoRa hardware](#setting-up-the-lora-hardware)
- [Configuration reference](#configuration-reference)
- [Running with Reticulum](#running-with-reticulum)
- [Running as a systemd service](#running-as-a-systemd-service)
- [Logging](#logging)
- [Recording SPI traffic](#recording-spi-traffic)
- [Simulation and tests](#simulation-and-tests)
- [Code coverage](#code-coverage)
- [Hardware test tools](#hardware-test-tools)
- [Differences from RNode_Firmware](#differences-from-rnode_firmware)
- [Limitations](#limitations)
- [Troubleshooting](#troubleshooting)
- [Contributing](#contributing)
- [License](#license)

## Why rnode-linux

An RNode is usually a microcontroller board with a LoRa modem, connected to the computer running
Reticulum over USB serial or Bluetooth. Many single-board computers and handhelds already have a
LoRa modem wired straight to their SPI bus, with no microcontroller in between. rnode-linux lets
that modem act as an RNode:

- **No extra hardware.** The Linux host drives the modem itself; there is no microcontroller, no
  firmware to flash and no USB serial link.
- **Unchanged host software.** Reticulum's `RNodeInterface` opens the PTY as if it were a serial
  RNode. Reticulum needs no patches or plugins.
- **Interoperability.** On-air framing, modulation parameters, sync word, preamble length,
  low-data-rate optimisation and CSMA/CA follow the firmware, so real RNodes hear rnode-linux and
  rnode-linux hears them.
- **Linux tooling.** Configuration lives in a TOML file, the daemon runs under systemd, logs go to
  the journal with optional per-packet detail, and an SPI recorder captures every modem transaction.
- **Testability.** The RNode logic is a pure Rust library tested against values derived from the
  firmware. A simulated radio runs the whole daemon in CI, and under real Reticulum on demand.

rnode-linux is not a Reticulum interface and not a new protocol: it is the modem side of the
existing RNode protocol, running on Linux instead of a microcontroller.

## How it works

```mermaid
flowchart LR
    subgraph Host["Host software"]
        RNS["Reticulum<br/>RNodeInterface"]
    end
    PTY["PTY<br/>e.g. /run/rnode/rnode0 → /dev/pts/3"]
    subgraph Daemon["rnode-linux daemon (crates/rnode-linux)"]
        Loop["poll() event loop"]
        Node["Node&lt;R, S&gt;<br/>crates/rnode-core"]
        Store["TomlStorage<br/>saved radio settings"]
        Driver["R: Radio<br/>rnode-sx126x or rnode-sim"]
    end
    Modem["SX1262 on spidev + GPIO<br/>or the simulated air"]
    Air(("LoRa air"))

    RNS <-->|"KISS frames"| PTY
    PTY <--> Loop
    Loop -->|"host bytes, IRQs, ticks"| Node
    Node -->|"KISS replies and data"| Loop
    Node <--> Store
    Node -->|"Radio trait calls"| Driver
    Driver <-->|"SPI, GPIO"| Modem
    Modem <--> Air
```

The work is split into layers:

- **`rnode-core`** decides *what* an RNode does: the KISS host protocol, the TX queue, the RNode
  on-air framing (one header byte, packets over 254 bytes split into two LoRa frames), PHY timing,
  CSMA/CA and airtime accounting. It does no I/O: the platform passes the time in and calls it when
  the host sends bytes, when the radio signals, and periodically.
- **A radio driver** implements the `rnode_core::Radio` trait for one modem: SPI opcodes,
  registers, GPIO timing and chip quirks. See [DRIVERS.md](DRIVERS.md).
- **The daemon** reads the config, opens spidev, the GPIO character device and the PTY, then runs
  the event loop: it waits on the PTY, the modem's IRQ line and the node's next wake-up time, and
  feeds whatever happened into the node.

A packet's round trip through the stack:

```mermaid
sequenceDiagram
    participant H as Reticulum
    participant L as Event loop
    participant N as Node (rnode-core)
    participant R as SX1262 driver
    participant M as SX1262

    H->>L: KISS CMD_DATA frame on the PTY
    L->>N: handle_host_input(bytes, now)
    Note over N: KISS decode, TX queue (≤ 508 bytes per packet)
    loop every 3 ms while the radio is on
        L->>N: tick_with(now, shutdown requested?)
        N->>R: dcd(now), current_rssi()
        Note over N: CSMA/CA: noise floor, DIFS, contention window
    end
    N->>R: transmit(frame) for each LoRa frame
    R->>M: SPI: write FIFO, SetTx, poll until TX done
    N->>R: receive(None)
    M-->>L: DIO1 rising edge (RX done) via the gpiochip fd
    L->>N: service_radio(now)
    N->>R: poll_rx()
    R-->>N: RxFrame (payload, RSSI, SNR)
    Note over N: split reassembly
    L->>N: take_host_output()
    L-->>H: CMD_STAT_RSSI, CMD_STAT_SNR, CMD_DATA
```

The event loop also calls `service_radio()` on every iteration, so a missed IRQ costs at most one
loop iteration of latency. While the radio is on, the loop wakes every 3 ms (firmware
`STATUS_INTERVAL_MS`) to sample carrier detect and RSSI for CSMA/CA, as the firmware does. While it
is off, the loop waits for at most 50 ms between checks.

## Workspace layout

| Path | Contents |
| --- | --- |
| [`crates/rnode-core`](crates/rnode-core) | Hardware-independent RNode logic: KISS host protocol, TX queue, split framing, PHY timing, CSMA/CA, the `Node<R, S>` state machine, and the `Radio` and `Storage` traits. No I/O; time and randomness are passed in, so tests are deterministic. |
| [`crates/rnode-sx126x`](crates/rnode-sx126x) | SX1262 driver over `embedded-hal` 1.0 that mirrors RNode_Firmware `sx126x.cpp`. |
| [`crates/rnode-sim`](crates/rnode-sim) | Simulated LoRa radio. Daemons on one host share an "air" directory of Unix datagram sockets. |
| [`crates/rnode-linux`](crates/rnode-linux) | The daemon: config, GPIO character device, spidev, PTY, saved settings and event loop. |
| [`crates/xtask`](crates/xtask) | Repository automation: `cargo xtask rns-e2e` runs two simulated daemons under two Reticulum instances. |
| [`tests/e2e/kiss_probe.py`](tests/e2e/kiss_probe.py) | Standalone KISS probe for a daemon PTY. |
| [`tools/hwtest`](tools/hwtest) | Hardware bring-up for the uConsole AIO V2, RTL-SDR capture and LoRa burst analysis. Outside the Cargo workspace. |
| [`config.example.toml`](config.example.toml), [`config.uconsole-aio-v2.toml`](config.uconsole-aio-v2.toml) | Example daemon configurations. |
| [`contrib/rnode-linux.service`](contrib/rnode-linux.service) | Hardened systemd unit. |
| [`DRIVERS.md`](DRIVERS.md) | How radio drivers work and how to add one. |
| [`AGENTS.md`](AGENTS.md) | Rules for changing this repository. |

## Safety and radio regulations

- **Transmissions are regulated.** Frequency, bandwidth, TX power, duty cycle and antenna gain must
  be legal where you operate. The examples in this repository use 914.875 MHz, 125 kHz and 2 dBm,
  which suits the 902–928 MHz ISM band of the US and some other countries in ITU region 2. They are
  illegal in other regions: Europe, for example, uses 863–870 MHz with duty-cycle limits.
- **rnode-linux enforces no regional rules.** The host (Reticulum) chooses the frequency,
  bandwidth, spreading factor, coding rate and TX power over KISS, or they come from the saved
  settings file. The daemon only clamps TX power to the SX1262's −9…22 dBm range. Reticulum's
  `airtime_limit_short` and `airtime_limit_long` options can help you stay within a duty cycle.
- **Attach an antenna or a dummy load before transmitting.** Transmitting into an open RF port can
  damage the power amplifier.
- **Start with low power** (the examples use 2 dBm) and raise it only as far as you need and are
  allowed.

## Quick start without hardware

The `sim` driver replaces the modem with a simulated one, so you can try the daemon on any Linux
or macOS machine. Build it:

```sh
cargo build --release --locked -p rnode-linux
```

Save this as `sim-a.toml`:

```toml
[radio]
driver = "sim"

[radio.sim]
air_dir = "target/rnode-air"
node_name = "a"

[pty]
symlink = "target/rnode-a"

[storage]
radio_settings = "target/rnode-a-settings.toml"

[logging]
log_level = "debug"
```

Copy it to `sim-b.toml` and change `a` to `b` in `node_name`, `symlink` and `radio_settings`. Start
both daemons from the repository root; relative paths resolve against the daemon's working
directory:

```sh
target/release/rnode-linux --config sim-a.toml &
target/release/rnode-linux --config sim-b.toml &
```

Each daemon logs its PTY and creates the symlink:

```text
[2026-10-13T19:23:46Z INFO  rnode_linux] loaded config from sim-a.toml
[2026-10-13T19:23:46Z INFO  rnode_linux] PTY slave: /dev/pts/1
```

Now point two Reticulum instances at `target/rnode-a` and `target/rnode-b`, as described in
[Running with Reticulum](#running-with-reticulum), with the same radio parameters on both. Give each
instance its own config directory (`rnsd --config ./rns-a`) with `share_instance = No`, so that the
second one does not attach to the first. The daemons log when Reticulum turns the radio on, and
every packet at `debug`; here daemon A sends and daemon B receives:

```text
[2026-10-13T19:23:48Z INFO  rnode_core::node] radio online (host request): 914.875 MHz, 125 kHz, SF7, CR 4/5, 2 dBm
[2026-10-13T19:23:53Z DEBUG rnode_core::node] TX packet: 255 bytes, 2 frames, header 0xB1 (seq 11, split): ok
[2026-10-13T19:23:53Z DEBUG rnode_core::node] RX packet: 255 bytes, 2 frames, header 0xB1 (seq 11, split), RSSI -70 dBm, SNR 7.00 dB
```

Instead of Reticulum, [`tests/e2e/kiss_probe.py`](tests/e2e/kiss_probe.py) can configure a daemon
and send test packets; see [Simulation and tests](#simulation-and-tests). Stop the daemons with
`kill %1 %2`: each puts its radio to sleep and removes its symlink and socket.

## Building

You need Rust 1.85 or newer (the workspace uses edition 2024).

```sh
cargo build --release --locked -p rnode-linux
sudo install -m 0755 target/release/rnode-linux /usr/local/bin/rnode-linux
```

The `sx1262` driver needs Linux: it uses spidev and the GPIO character device
(`/dev/gpiochipN`). On other systems the daemon builds and runs, but only with the `sim` driver. The
reference uConsole builds the daemon natively.

```text
$ rnode-linux --help
RNode Linux daemon

Usage: rnode-linux [OPTIONS]

Options:
  -c, --config <CONFIG>  Path to the TOML configuration file [default: config.toml]
      --spilog <SPILOG>  Record SPI transactions to a CSV file (radio.driver = "sx1262" only)
  -h, --help             Print help
  -V, --version          Print version

$ rnode-linux --version
rnode-linux 0.2.0 (reports RNode firmware 1.86 to hosts)
```

- `--config` defaults to `config.toml` in the working directory.
- `--spilog` records every SPI transaction of the `sx1262` driver; see
  [Recording SPI traffic](#recording-spi-traffic). With `radio.driver = "sim"` the daemon refuses
  to start, because the simulator has no SPI bus.
- `--version` prints the daemon's version and the RNode firmware version it reports to hosts.

On a fatal error, such as an invalid config or a device that cannot be opened, the daemon prints
one `fatal:` line to stderr, naming each cause once, and exits with status 1:

```text
fatal: load nope.toml: failed to read nope.toml: No such file or directory (os error 2)
```

### Stopping the daemon

SIGINT and SIGTERM stop the daemon cleanly, with exit status 0. A shutdown request ends a burst of
transmissions between packets: the packet on the air is finished first, including the second
frame of a split packet, so a packet is never cut in half. The daemon then drops the packets still
queued (one `INFO` line with the count, and one `DEBUG` line per packet), puts the radio to sleep
and removes the PTY symlink. Here SIGTERM arrived while the first of four queued 200-byte packets
was on the air at SF11, and the daemon exited 1.1 s later:

```text
[2026-10-13T19:21:34Z DEBUG rnode_core::node] TX packet: 200 bytes, 1 frame, header 0x90 (seq 9): ok
[2026-10-13T19:21:34Z INFO  rnode_core::node] shutting down with 3 TX packets queued; dropping them
[2026-10-13T19:21:34Z DEBUG rnode_core::node] dropping 200-byte TX packet: shutting down
[2026-10-13T19:21:34Z DEBUG rnode_core::node] dropping 200-byte TX packet: shutting down
[2026-10-13T19:21:34Z DEBUG rnode_core::node] dropping 200-byte TX packet: shutting down
[2026-10-13T19:21:34Z INFO  rnode_core::node] radio offline (shutdown): 914.875 MHz, 125 kHz, SF11, CR 4/5, 2 dBm
```

RNode_Firmware sends every queued packet once a burst starts; stopping between packets is one of
the [deliberate differences](#differences-from-rnode_firmware).

The packet on the air can take minutes at the slowest settings: a 255-byte frame at SF12 and
7.8 kHz is on the air for about 147 s, longer than systemd's default 90 s stop timeout, after which
systemd kills the daemon in the middle of the transmission (issue #59). If you use such settings,
raise `TimeoutStopSec=` in the systemd unit.

## Setting up the LoRa hardware

### What the daemon needs from the board

The `sx1262` driver talks to the modem through one spidev device and up to five GPIO lines on one
GPIO chip. Two of the modem's own pins, DIO2 and DIO3, are configured inside the chip and need no
host GPIO.

```mermaid
flowchart LR
    subgraph Linux["Linux host"]
        SPIDEV["/dev/spidevB.C<br/>[spi] device"]
        CHIP["/dev/gpiochipN<br/>[gpio] chip"]
    end
    subgraph Board["Radio module and board"]
        BUS["SCK, MOSI, MISO, NSS"]
        NRESET["NRESET"]
        BUSY["BUSY"]
        DIO1["DIO1"]
        RXEN["RXEN, some modules"]
        PWR["power switch, some boards"]
        DIO2["DIO2"]
        DIO3["DIO3"]
        SWITCH["RF switch"]
        TCXO["TCXO"]
    end
    SPIDEV --- BUS
    CHIP ---|"reset"| NRESET
    CHIP ---|"busy"| BUSY
    CHIP ---|"dio1"| DIO1
    CHIP -.-|"rxen"| RXEN
    CHIP -.-|"power"| PWR
    DIO2 -.-|"dio2_as_rf_switch"| SWITCH
    DIO3 -.-|"tcxo_voltage"| TCXO
```

| Modem signal | Host side | Config key | How the daemon uses it |
| --- | --- | --- | --- |
| SCK, MOSI, MISO, NSS | spidev device; the kernel drives chip select | `[spi] device`, `speed_hz` | SPI mode 0, 8-bit words |
| NRESET | GPIO output | `[gpio] reset` | Held high; pulsed low for 10 ms by `begin()` |
| BUSY | GPIO input | `[gpio] busy` | Polled before every command, up to `busy_timeout_ms` |
| DIO1 | GPIO input with rising-edge events | `[gpio] dio1` | RX-done interrupt; wakes the event loop |
| RXEN (optional) | GPIO output | `[gpio] rxen` | Driven high when the radio starts and on every receive, never low (firmware `rxAntEnable()`) |
| Power enable (optional) | GPIO output | `[gpio] power` | Driven high before the other lines are requested; held while the daemon runs |
| DIO2 | inside the module | `[radio.sx1262] dio2_as_rf_switch` | SX1262 drives the RF switch from DIO2 |
| DIO3 | inside the module | `[radio.sx1262] tcxo_voltage` | SX1262 powers the TCXO from DIO3 at this voltage |

GPIO lines are offsets on a GPIO chip: what `gpioinfo` lists, not physical header pin numbers and
not the global numbers of the old sysfs GPIO interface.

### uConsole CM5 with the HackerGadgets AIO V2

The AIO V2 has an SX1262 with a 1.8 V TCXO powered from DIO3 and an RF switch driven by DIO2. It
has no RXEN line. The board switches its peripherals on and off with GPIOs on the CM5's RP1:

| GPIO | Function |
| --- | --- |
| 16 | LoRa (SX1262) power |
| 7 | RTL-SDR power |
| 23 | internal USB power |
| 27 | GPS power |

1. **Enable SPI1.** The SX1262 sits on SPI1, chip select 0, which appears as `/dev/spidev1.0`. On
   the reference unit `/boot/firmware/config.txt` enables it with:

   ```text
   dtoverlay=spi1-1cs
   ```

   Reboot and check that `/dev/spidev1.0` exists.

2. **Find the GPIO chip.** The modem's control lines are on the RP1 GPIO controller, whose chip
   label is `pinctrl-rp1`. On the reference image that is `/dev/gpiochip15`; `gpiodetect` from the
   gpiod tools shows the number on yours. The lines are RESET = 25, BUSY = 24 and DIO1 = 26. If the
   chip number changes between kernels, set `chip = "pinctrl-rp1"`: the daemon also accepts a chip
   label.

3. **Mind the power rail.** The AIO setup on the reference unit drives all four rails low at boot
   from `config.txt` (`gpio=7,16,23,27=op,dl` in the `[cm5]` section), so the SX1262 has no power
   until something drives GPIO16 high. `power = 16` makes the daemon do that: it drives the line
   high, waits `power_on_delay_ms`, then resets the modem. `pinctrl get 16` shows the current state
   (`dh` means driven high). With the rail off, `begin()` fails.

4. **Install the config.** [`config.uconsole-aio-v2.toml`](config.uconsole-aio-v2.toml) has the
   whole mapping:

   ```toml
   # Clockwork uConsole CM5 with HackerGadgets AIO V2 SX1262.

   [spi]
   device = "/dev/spidev1.0"
   speed_hz = 8_000_000

   [gpio]
   chip = "gpiochip15" # label: pinctrl-rp1
   power = 16
   power_on_delay_ms = 100
   power_off_on_exit = false
   reset = 25
   busy = 24
   dio1 = 26

   [radio]
   driver = "sx1262"
   avoid_interference = false

   [radio.sx1262]
   tcxo_voltage = "1.8V"
   dio2_as_rf_switch = true
   ocp = 0x28

   [pty]
   symlink = "/run/rnode/rnode0"
   # For an rnsd running as another user: SupplementaryGroups=dialout in the unit, then
   # group = "dialout"
   # mode = "0660"

   [storage]
   radio_settings = "/var/lib/rnode/radio_settings.toml"

   [logging]
   log_level = "info"
   ```

   ```sh
   sudo install -m 0644 config.uconsole-aio-v2.toml /etc/rnode-linux.toml
   ```

5. **Try it in the foreground.** The config puts the symlink in `/run/rnode` and the saved settings
   in `/var/lib/rnode`. systemd creates both for the service; for a manual run, create them for
   your user first:

   ```sh
   sudo install -d -o "$USER" /run/rnode /var/lib/rnode
   rnode-linux --config /etc/rnode-linux.toml
   ```

   Your user needs read/write access to `/dev/spidev1.0` and `/dev/gpiochip15` (see
   [device access](#service-account-and-device-access)). The daemon logs the config file and the
   PTY, and waits with the radio off. It logs `radio online (host request): …` when a host turns
   the radio on, or `radio online (saved settings): …` at startup if a host saved its settings
   with `CMD_CONF_SAVE` earlier. Then continue with
   [Running with Reticulum](#running-with-reticulum) and
   [Running as a systemd service](#running-as-a-systemd-service).

The [hardware test tools](#hardware-test-tools) can check the modem, the TCXO and the RF output
before you involve the daemon; [`tools/hwtest/README.md`](tools/hwtest/README.md) describes the
procedure.

**Validation status.** On 2026-10-13 the daemon from `main` (commit `7bfb6f7`) passed a validation
run on the reference uConsole AIO V2 at 914.875 MHz, 2 dBm, 125 kHz, SF7, CR 4/5:

- KISS detection, configuration, statistics and status pushes behaved as on an RNode;
- packets of 1, 254, 255 and 508 bytes went out as one or two LoRa frames, and 509 bytes were
  dropped;
- Reticulum's `RNodeInterface` came up and sent an announce;
- settings saved with `CMD_CONF_SAVE` brought the radio up after a restart;
- an RTL-SDR capture showed 24-symbol preambles, sync word `0x12` and frame durations within
  0.34 ms of the Semtech time on air.

Reception over the air has not been tested yet: the setup has no second LoRa transmitter.

### Mapping your own board

1. **Collect the wiring** from the module datasheet and your board's schematic:
   - which SPI controller and chip select the module uses;
   - which SoC GPIOs connect to NRESET, BUSY and DIO1, and to RXEN or a power enable if the board
     has them;
   - whether DIO2 drives the RF switch;
   - whether DIO3 powers a TCXO, and at which voltage.

2. **Enable the SPI controller and a spidev node** with your board's device-tree overlay. On a
   Raspberry Pi, `dtparam=spi=on` gives `/dev/spidev0.0` and `/dev/spidev0.1`, and the `spi1-*cs`
   overlays give `/dev/spidev1.*`. The name is `spidev<bus>.<chip select>`, so the chip select is
   part of `[spi] device`. Check with `ls -l /dev/spidev*`.

3. **Translate GPIO names into chip and line.** `gpiodetect` lists the chips and their labels;
   `gpioinfo <chip>` lists the lines. On Raspberry Pi boards, BCM `GPIO25` is line 25 of the main
   GPIO chip (`pinctrl-rp1` on the Pi 5 and CM5). All lines must be on the one chip named by
   `[gpio] chip`.

4. **Write the config.** Start from [`config.example.toml`](config.example.toml) and fill in:

   | Your board has | Set |
   | --- | --- |
   | Module on SPI bus B, chip select C | `[spi] device = "/dev/spidevB.C"` |
   | NRESET, BUSY, DIO1 on chip N, lines r, b, d | `[gpio] chip = "gpiochipN"`, `reset = r`, `busy = b`, `dio1 = d` |
   | Separate RX-enable pin | `[gpio] rxen = <line>` |
   | Switched power for the module | `[gpio] power = <line>`, and `power_on_delay_ms` if it needs longer to settle |
   | TCXO powered from DIO3 | `[radio.sx1262] tcxo_voltage = "1.8V"`, or the voltage in the module datasheet |
   | Plain crystal, no TCXO | leave `tcxo_voltage` unset |
   | RF switch driven by DIO2 | `dio2_as_rf_switch = true` (the default) |
   | RF switch driven some other way | `dio2_as_rf_switch = false` |

   Modules with separate RX-enable and TX-enable pins are usually wired with TXEN driven by DIO2 and
   RXEN by a host GPIO. rnode-linux supports exactly that, like RNode_Firmware; it has no TXEN line.
   Use the TCXO voltage from the module's datasheet: a missing or wrong `tcxo_voltage` leaves the
   modem without a working reference clock, and too high a voltage can damage the TCXO.

5. **Check it** with `RUST_LOG=debug rnode-linux --config config.toml --spilog spi.csv` and a host,
   and see [Troubleshooting](#troubleshooting) if `begin()` fails.

## Configuration reference

The daemon reads one TOML file. It contains only how the hardware is wired and how the daemon
behaves. Radio link parameters (frequency, bandwidth, spreading factor, coding rate, TX power) are
not in it: the host sets them over KISS, as with any RNode, and `CMD_CONF_SAVE` stores them in the
[saved settings file](#saved-radio-settings).

- Unknown sections and keys are errors, so typos fail at startup instead of being ignored.
- Relative paths resolve against the daemon's working directory, not the config file's location.
- Integers may use TOML's `_` separators and `0x` prefixes (`8_000_000`, `0x28`).

[`config.example.toml`](config.example.toml) lists every key with a comment.

### `[spi]`

Required when `radio.driver = "sx1262"`; ignored otherwise.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `device` | path | required | spidev device, e.g. `"/dev/spidev1.0"` |
| `speed_hz` | integer | `8_000_000` | SPI clock in Hz; must be greater than 0. The SX1262 accepts up to 16 MHz; lower it if long wires cause errors |

The mode (SPI mode 0) and word size (8 bits) are fixed.

### `[gpio]`

Required when `radio.driver = "sx1262"`; ignored otherwise. Line numbers are offsets on `chip`.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `chip` | string | `"gpiochip0"` | GPIO chip: a name such as `"gpiochip15"` (opened as `/dev/gpiochip15`), an absolute path, or a chip label such as `"pinctrl-rp1"`. A label is looked up in `/dev/gpiochip*` in name order. Chips whose label cannot be read are skipped with a `debug` line, and listed in the error if no chip matches |
| `reset` | integer | required | NRESET line, an output held high |
| `busy` | integer | required | BUSY line, an input |
| `dio1` | integer | required | DIO1 line, an input watched for rising edges (RX done) |
| `rxen` | integer | unset | Optional RX-enable output, driven high on receive |
| `power` | integer | unset | Optional power-rail enable output, driven high before the other lines are requested |
| `power_on_delay_ms` | integer | `100` | Wait after driving `power` high, before resetting the modem; only used when `power` is set |
| `power_off_on_exit` | bool | `false` | Drive `power` low when the daemon exits. The default leaves the rail alone so restarts do not power-cycle the modem |

The daemon holds the lines for as long as it runs. When it exits, it releases them, and what the
lines do then depends on the kernel's GPIO driver; use `power_off_on_exit` if the rail must be off.

### `[radio]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `driver` | string | `"sx1262"` | `"sx1262"` for the hardware driver, `"sim"` for the simulator |
| `avoid_interference` | bool | `false` | Interference avoidance at startup; see below |

**`avoid_interference`** decides whether CSMA/CA treats strong non-LoRa signals as a busy channel
(firmware `avoid_interference`). When it is on, the channel counts as busy while the RSSI is more
than 11 dB above the measured noise floor and no LoRa carrier is detected (firmware
`medium_free()`).

- The default is off, the firmware's effective default: RNode_Firmware enables avoidance only when
  `rnodeconf --ia-enable` has written its EEPROM flag, which erased EEPROM does not have.
- Turning it on can stop the node from transmitting over strong interference, at a cost. The noise
  floor is measured from 128 RSSI samples taken every 3 ms, so for about 384 ms after the radio
  starts or resets every transmission waits. After that, TX is deferred for as long as the
  interference lasts, which on a noisy host can be indefinitely.
- The host can change the running value with `CMD_DIS_IA`. Unlike the firmware, which stores the
  value in EEPROM and reboots, rnode-linux keeps it until the daemon exits (including across
  `CMD_RESET`) and logs it at `INFO`. The config file stays the persistent setting.

### `[radio.sx1262]`

Board options of the SX1262 driver. All of them are optional.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `tcxo_voltage` | string | unset (crystal) | Voltage DIO3 supplies to the TCXO: one of `"1.6V"`, `"1.7V"`, `"1.8V"`, `"2.2V"`, `"2.4V"`, `"2.7V"`, `"3.0V"`, `"3.3V"`. The trailing `V` is optional and `"3"` means 3.0 V. Validated when `driver = "sx1262"` |
| `dio2_as_rf_switch` | bool | `true` | Let the SX1262 drive the RF switch from DIO2 (firmware `setDio2AsRfSwitchCtrl()`) |
| `ocp` | integer | `0x28` | Over-current protection register value, in 2.5 mA steps (`0x28` = 100 mA); firmware `OCP_TUNED` |
| `busy_timeout_ms` | integer | `100` | How long to wait for BUSY to go low before a command fails with `SX126x BUSY timeout` (firmware `waitOnBusy()` waits 100 ms) |
| `tx_timeout_ms` | integer | `20000` | Minimum TX-done wait (firmware `LORA_MODEM_TIMEOUT_MS`). The driver waits for `max(tx_timeout_ms, 2 × airtime + 1000 ms)`, so long frames at SF12 are not cut short |

### `[radio.sim]`

Options of the simulated radio, used when `driver = "sim"`.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `air_dir` | path | `"target/rnode-air"` | Directory shared by all simulated radios that should hear each other. It must be private: mode `0700`, owned by the daemon's user and not a symlink. The daemon creates it if it is missing |
| `node_name` | string | `"rnode-linux"` | Unique name of this radio; its socket is `<air_dir>/<node_name>.sock`. No `/`, not empty, not `.` or `..` |
| `rssi_dbm` | integer | `-70` | RSSI reported for received packets, and as the channel RSSI while a matching transmission is on the air. The idle channel reads −110 dBm |
| `snr_db` | float | `7.0` | SNR reported for received packets, in 0.25 dB steps |
| `loss` | float | `0.0` | Probability, from 0.0 to 1.0, that this radio drops an otherwise good frame. Validated when `driver = "sim"` |

The socket path `<air_dir>/<node_name>.sock` must fit in 103 bytes, so keep `air_dir` short; macOS
in particular limits Unix socket paths.

### `[pty]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `symlink` | path | unset | Publish the PTY under this path as a symlink. A symlink already there, for example from an earlier run, is replaced; anything else makes startup fail and is left untouched. On exit the symlink is removed if it still points to this daemon's PTY. Without it the PTY name, logged as `PTY slave: /dev/pts/N`, changes on every start |
| `group` | string | unset | Give the PTY to this group, so a host running as another user can open it. The group must exist when the config is loaded, and the daemon must be a member |
| `mode` | string | unset | Permission bits of the PTY as an octal string, e.g. `"0660"`. The owner must keep read and write; setuid, setgid and sticky bits are rejected |

The daemon puts the PTY in raw mode and keeps its own handle on it, so hosts can connect, disconnect
and reconnect while the daemon runs. Output produced while no host reads the PTY is dropped once
the kernel's PTY buffer is full. The first dropped frame logs one `WARN` line; the warning is
logged again only after the PTY has taken a whole frame, so a host that reads a trickle does not
flood the log. `RNodeInterface` flushes stale input when it opens the port anyway.

The symlink rules keep a mistyped path from deleting a file:

```text
fatal: open PTY: refusing to replace /run/rnode/rnode0 with the PTY symlink: it is a regular file, not a symlink
```

If another program replaces the symlink while the daemon runs, the daemon leaves it alone on exit:

```text
[2026-10-13T19:24:02Z WARN  rnode_linux::pty] leaving /run/rnode/rnode0 in place: it is no longer the symlink to this daemon's PTY /dev/pts/0
```

### `[storage]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `radio_settings` | path | unset | File for the settings the host saves with `CMD_CONF_SAVE` (the firmware's EEPROM) |

Without `radio_settings`, `CMD_CONF_SAVE` stores nothing, and the daemon says so at startup:

```text
[2026-10-13T19:24:02Z WARN  rnode_linux] storage.radio_settings is not set: radio settings the host saves with CMD_CONF_SAVE are not kept, so they are lost on restart and on CMD_RESET
```

The radio then stays off after a restart or a `CMD_RESET` until a host configures it again.

### `[logging]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `log_level` | string | `"info"` | Log filter in `RUST_LOG` syntax; see [Logging](#logging). `RUST_LOG`, when set, replaces it |

### Validation and startup warnings

`rnode-linux` refuses to start, with `fatal: load <file>: …`, when:

- the file is not valid TOML, or has an unknown section or key;
- `radio.driver` is neither `"sx1262"` nor `"sim"`;
- `radio.driver = "sx1262"` and `[spi]` or `[gpio]` is missing, or `tcxo_voltage` is not one of the
  listed voltages;
- `radio.driver = "sim"` and `loss` is outside 0.0–1.0;
- `spi.speed_hz` is 0;
- `pty.mode` is not an octal permission string, sets setuid, setgid or sticky, or takes read or
  write access away from the owner;
- `pty.group` does not exist.

Devices, GPIO lines and directories are only opened afterwards; failures there are also fatal (see
[Troubleshooting](#troubleshooting)). So is `--spilog` with the `sim` driver. The daemon starts but
logs a `WARN` line when:

- `storage.radio_settings` is not set, so saved settings are lost on restart and on `CMD_RESET`;
- `pty.mode` gives other users read or write access (prefer `group` with `"0660"`);
- `pty.group` is set without `pty.mode`, since the PTY's default mode, usually `0620`, gives the
  group no read access.

### Saved radio settings

`CMD_CONF_SAVE` writes the current link parameters to `storage.radio_settings`, and the daemon
loads them at startup, like the firmware's EEPROM:

```toml ignore
frequency = 914875000
bandwidth = 125000
spreading_factor = 8
coding_rate = 5
tx_power = 2
```

When the file holds a complete set, the radio comes up without a host at startup and after a
`CMD_RESET`, logging `radio online (saved settings): …`; this is the firmware's TNC mode.
`CMD_CONF_DELETE` removes the file. The daemon writes a temporary file next to
it and renames it into place, with mode `0600`, and creates missing parent directories. A missing
file means nothing is saved; a corrupt one is ignored with a `WARN` line.

`rnodeconf --tnc` cannot configure rnode-linux (see [Limitations](#limitations)). To provision TNC
mode, write this file yourself, or have a host configure the radio and send `CMD_CONF_SAVE`.

## Running with Reticulum

Add an `RNodeInterface` to Reticulum's config (`~/.reticulum/config`, or the config directory of
your `rnsd`) with the PTY symlink as its port:

```text
[interfaces]
  [[rnode-linux]]
    type = RNodeInterface
    enabled = yes
    port = /run/rnode/rnode0
    frequency = 914875000
    bandwidth = 125000
    txpower = 2
    spreadingfactor = 8
    codingrate = 5
```

The radio values are lab examples for the US 902–928 MHz band; use values that are legal where you
are. Use Reticulum's own key names: `spreadingfactor` and `codingrate`, without underscores.
Without `enabled = yes`, `rnsd` skips the interface (`Skipping disabled interface "rnode-linux"`);
with `spreading_factor` or `coding_rate` it logs `Invalid spreading factor configured …` and
`Invalid coding rate configured …`, and exits. This block, with only `port` changed, brings the
interface up with `rnsd` 1.5.5 and the sim driver:

```text
[Notice]   RNodeInterface[rnode-linux] is configured and powered up
```

When Reticulum opens the port it detects the RNode (rnode-linux reports firmware version 1.86),
sends the frequency, bandwidth, TX power, spreading factor and coding rate, turns the radio on and
checks the values the daemon reports back. The daemon logs `radio online (host request): …` with
the settings it applied. RNodes that should hear each other need the same frequency, bandwidth and
spreading factor.

- `bandwidth` must be one the SX1262 supports: 7800, 10400, 15600, 20800, 31250, 41700, 62500,
  125000, 250000 or 500000 Hz. The modem rounds other values up, Reticulum sees a different value
  and aborts with "the reported radio parameters did not match your configuration".
- `txpower` may be at most 22 dBm, the SX1262 limit. Higher values are clamped and abort the same
  way.
- `airtime_limit_short` and `airtime_limit_long` work as with any RNode.
- Start the daemon before Reticulum, or let Reticulum retry: it tries to reopen the port every
  5 seconds.
- A restarted daemon has a new PTY, and the symlink follows it. On Linux, Reticulum sees the old PTY
  hang up, reopens `port` and configures the radio again. On macOS it does not notice, so restart
  Reticulum after restarting a simulated daemon.
- Several Reticulum programs cannot share one PTY. Run a shared `rnsd` instance and let the other
  programs connect to it.

## Running as a systemd service

[`contrib/rnode-linux.service`](contrib/rnode-linux.service) runs the daemon as an unprivileged
`rnode` user, with a read-only file system, a private `/tmp` and access to only the listed devices.
systemd creates `/run/rnode` (`RuntimeDirectory=`) for the PTY symlink and `/var/lib/rnode`
(`StateDirectory=`) for the saved settings. Install the binary as in [Building](#building) and the
config at `/etc/rnode-linux.toml`, as in the
[uConsole steps](#uconsole-cm5-with-the-hackergadgets-aio-v2).

### Service account and device access

Create the account the unit runs as:

```sh
sudo useradd --system --no-create-home --shell /usr/sbin/nologin rnode
```

It needs read/write access to the spidev device and the GPIO chip. On Raspberry Pi OS, which the
uConsole images are based on, udev already gives `/dev/spidev*` to group `spi` and
`/dev/gpiochip*` to group `gpio`. Check with `ls -l /dev/spidev1.0 /dev/gpiochip15`, then add the
account to those groups:

```sh
sudo usermod -aG spi,gpio rnode
```

On other distributions, add a udev rule such as `/etc/udev/rules.d/60-rnode-linux.rules`, adjusted
to your devices, and reload the rules with `sudo udevadm control --reload && sudo udevadm trigger`:

```text
SUBSYSTEM=="spidev", KERNEL=="spidev1.0", GROUP="rnode", MODE="0660"
SUBSYSTEM=="gpio", KERNEL=="gpiochip15", GROUP="rnode", MODE="0660"
```

Also check the unit's `DeviceAllow=` lines. They list the devices of the two example configs, and
with `DevicePolicy=closed` the daemon cannot open any other spidev or GPIO chip.

### PTY permissions for rnsd

The PTY belongs to the daemon's user and, by default, only that user can read it (issue #47). If
`rnsd` runs as a different user, share a group with it. This example uses `dialout`, the usual group
for serial ports, which RNode users are often in already:

1. In `/etc/rnode-linux.toml`, set `group = "dialout"` and `mode = "0660"` under `[pty]`.
2. In the unit, uncomment `SupplementaryGroups=dialout`, so that the daemon may hand the PTY to that
   group.
3. Add the user that runs `rnsd` to the group with `sudo usermod -aG dialout <rnsd-user>`, then log
   in again or restart its service.

Avoid modes such as `"0666"`, which let every local user send packets through your radio; the
daemon warns about them.

### Installing the unit

```sh
sudo install -m 0644 contrib/rnode-linux.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now rnode-linux
journalctl -u rnode-linux -f
```

To start Reticulum after the daemon, add `After=rnode-linux.service` and
`Wants=rnode-linux.service` to the `rnsd` unit.

On stop, systemd waits 90 s by default before it kills the daemon. The daemon exits within one
packet's airtime of SIGTERM (see [Stopping the daemon](#stopping-the-daemon)), which is enough
unless you use the slowest modulations: at SF12 and 7.8 kHz one 255-byte frame takes about 147 s.
For such settings add a longer `TimeoutStopSec=` to the unit (issue #59).

The unit's file system is read-only apart from `/run/rnode` and `/var/lib/rnode`, so keep the
symlink, the settings file and any `--spilog` file in those directories, for example
`ExecStart=/usr/local/bin/rnode-linux --config /etc/rnode-linux.toml --spilog /var/lib/rnode/spi.csv`.

## Logging

The daemon logs to stderr; under systemd that goes to the journal. `[logging] log_level` takes the
same filter syntax as `RUST_LOG`: a level (`error`, `warn`, `info`, `debug`, `trace` or `off`), or
comma-separated `target=level` directives such as `"warn,rnode_core::node=debug"`. If `RUST_LOG` is
set in the environment, it replaces `log_level` entirely. The config is read before logging starts,
so errors in it appear as `fatal:` lines on stderr.

| Level | What you get |
| --- | --- |
| `warn` | Problems: radio driver errors (rate-limited), modem TX timeouts, failed settings writes, a host that stopped reading the PTY |
| `info` (default) | Startup, the radio going online and offline with the reason and the settings, retuning by the host, interference-avoidance changes, packets dropped at shutdown, recovery of a failing radio call |
| `debug` | One line per packet received, per packet sent and per packet dropped, with the reason |
| `trace` | Everything above, plus a hex dump of every received and sent payload; see the [warning](#trace-logs-contain-your-traffic) below |

### What `info` shows

At the default level the daemon logs its startup, every radio state change and every retune, but
nothing per packet. These lines come from two simulated daemons set up as in the quick start, on
Linux, with a test script as the host. The script configured daemon A, toggled interference
avoidance, sent packets, retuned the radio, turned it off and queued one more packet:

```text
[2026-10-13T19:23:46Z INFO  rnode_linux] loaded config from sim-a.toml
[2026-10-13T19:23:46Z INFO  rnode_linux] PTY slave: /dev/pts/1
[2026-10-13T19:23:48Z INFO  rnode_core::node] radio online (host request): 914.875 MHz, 125 kHz, SF7, CR 4/5, 2 dBm
[2026-10-13T19:23:49Z INFO  rnode_core::node] host enabled interference avoidance (CMD_DIS_IA) until the process exits
[2026-10-13T19:23:49Z INFO  rnode_core::node] host disabled interference avoidance (CMD_DIS_IA) until the process exits
[2026-10-13T19:23:57Z INFO  rnode_core::node] radio settings changed: 914.875 MHz, 125 kHz, SF8, CR 4/6, 2 dBm
[2026-10-13T19:23:57Z INFO  rnode_core::node] radio offline (host request): 914.875 MHz, 125 kHz, SF8, CR 4/6, 2 dBm
[2026-10-13T19:24:01Z INFO  rnode_core::node] shutting down with 1 TX packet queued; dropping it
```

Daemon B came up from its saved settings, without a host:

```text
[2026-10-13T19:23:46Z INFO  rnode_core::node] radio online (saved settings): 914.875 MHz, 125 kHz, SF7, CR 4/5, 2 dBm
[2026-10-13T19:24:01Z INFO  rnode_core::node] radio offline (shutdown): 914.875 MHz, 125 kHz, SF7, CR 4/5, 2 dBm
```

- `radio online (…)` and `radio offline (…)` give the reason: `host request` (`CMD_RADIO_STATE`),
  `saved settings` (startup or a reset with saved settings), `reset` (`CMD_RESET` or a modem TX
  timeout) or `shutdown`.
- `radio settings changed` appears when a host retunes the radio while it is on. It is logged
  100 ms after the first configuration command of a burst, once per burst, and only if the
  settings differ from the last line, so Reticulum's whole reconfiguration gives one line.

### Packets at `debug`

At `debug` every packet gets a line. Sizes are payload bytes without the RNode header. The header
byte carries the random sequence number in its high nibble and the split flag in bit 0. Received
packets show the RSSI and SNR that the daemon also sends to the host. From daemon A, then B:

```text
[2026-10-13T19:23:49Z DEBUG rnode_core::node] TX packet: 1 byte, 1 frame, header 0xD0 (seq 13): ok
[2026-10-13T19:23:54Z DEBUG rnode_core::node] TX packet: 508 bytes, 2 frames, header 0x21 (seq 2, split): ok
[2026-10-13T19:23:55Z DEBUG rnode_core::kiss] dropping 509-byte host data frame: exceeds the 508-byte MTU
[2026-10-13T19:24:01Z DEBUG rnode_core::node] dropping 29-byte TX packet: shutting down
```

```text
[2026-10-13T19:23:51Z DEBUG rnode_core::node] RX packet: 254 bytes, 1 frame, header 0x20 (seq 2), RSSI -70 dBm, SNR 7.00 dB
[2026-10-13T19:23:54Z DEBUG rnode_core::node] RX packet: 508 bytes, 2 frames, header 0x21 (seq 2, split), RSSI -70 dBm, SNR 7.00 dB
```

A failed transmission says what the host was told. Here the simulated air directory was deleted
under a running daemon:

```text
[2026-10-13T19:18:45Z WARN  rnode_core::node] radio transmit failed: sim radio I/O error: No such file or directory (os error 2)
[2026-10-13T19:18:45Z DEBUG rnode_core::node] TX packet: 5 bytes, 1 frame, header 0x60 (seq 6): radio error on frame 1/1: sim radio I/O error: No such file or directory (os error 2); sent ERROR_TXFAILED
```

A modem TX timeout reads `modem timeout on frame 1/2; sent ERROR_MODEM_TIMEOUT, ERROR_TXFAILED;
hard reset` instead.

Dropped packets are logged where they are dropped:

| Logged by | Message | Why |
| --- | --- | --- |
| `rnode_core::kiss` | `dropping N-byte host data frame: exceeds the 508-byte MTU` | The host sent more than an RNode packet holds; the firmware drops it silently too |
| `rnode_core::queue` | `dropping empty host data frame` | Empty `CMD_DATA` frame |
| `rnode_core::queue` | `dropping N-byte host data frame: exceeds the 255-byte MTU` | Promiscuous mode only allows single frames |
| `rnode_core::queue` | `dropping N-byte host data frame: TX queue full (P/200 packets, B/6144 bytes)` | The host outran the radio; the queue has the firmware's limits |
| `rnode_core::framing` | `dropping N-byte incomplete split packet (seq S): …` | The second half of a split packet never arrived, or another packet interrupted it |
| `rnode_core::framing` | `dropping empty LoRa frame: no RNode header` | A zero-length frame was received |
| `rnode_core::node` | `dropping N-byte TX packet: radio offline; sent ERROR_TXFAILED` | Data arrived while the radio was off |
| `rnode_core::node` | `dropping N-byte TX packet: exceeds the 255-byte single-frame MTU in promiscuous mode; sent ERROR_TXFAILED` | Oversized raw frame in promiscuous mode |
| `rnode_core::node` | `dropping N-byte promiscuous RX frame: exceeds the 255-byte single-frame MTU` | Oversized frame received in promiscuous mode |
| `rnode_core::node` | `dropping N-byte TX packet: queue cleared by reset` | `CMD_RESET` or a modem TX timeout emptied the queue |
| `rnode_core::node` | `dropping N-byte TX packet: shutting down` | The packet was still queued when the daemon stopped |

### Payload dumps at `trace`

At `trace`, each `RX packet` and `TX packet` line is followed by a dump of the payload in the style
of `hexdump -C`:

```text
[2026-10-13T19:23:53Z TRACE rnode_core::node] RX payload, 255 bytes:
    0000  00 01 02 03 04 05 06 07  08 09 0A 0B 0C 0D 0E 0F  |................|
    0010  10 11 12 13 14 15 16 17  18 19 1A 1B 1C 1D 1E 1F  |................|
    …
    00F0  F0 F1 F2 F3 F4 F5 F6 F7  F8 F9 FA 00 01 02 03     |...............|
```

#### Trace logs contain your traffic

`trace` writes every payload the node sends or receives to the log: Reticulum announces, link
setup, destination hashes and anything else that crosses the radio. Use it for short debugging
sessions only, never leave it on in production, and keep such logs private. Under systemd the dumps
end up in the journal, which keeps them until it is rotated away. For a debugging session, prefer
running the daemon in the foreground with its log in a private file that you delete afterwards:

```sh
(umask 077; RUST_LOG=trace rnode-linux --config /etc/rnode-linux.toml 2> trace.log)
```

### Warnings

- **Radio errors are rate-limited.** Carrier detect, RSSI and the receive poll run every few
  milliseconds, so a failing module would otherwise log hundreds of lines a second. For each of
  them the first failure is logged, then at most one summary every 10 s while the failures go on,
  and an `INFO` line once the call has worked for 10 s:

  ```text
  WARN  radio DCD failed: simulated DCD error
  WARN  radio DCD failed 3334 more times in the last 10.0 s: simulated DCD error
  WARN  radio DCD failed 3334 more times in the last 10.0 s: simulated DCD error
  INFO  radio DCD recovered after 8000 failures in 24.0 s
  ```

  These lines come from rnode-core's logging tests, which inject the errors: the simulated radio
  never fails these calls, and on hardware it takes a faulty module. The three calls are logged as
  `radio DCD`, `radio RSSI` and `radio RX service`.
- `modem timed out during TX; hard-resetting the node and dropping N queued TX packets`: the modem
  never reported TX done. Like the firmware, the node resets, which clears the queue and reloads
  the saved settings.
- `radio begin failed: …`, `radio operation failed: …`, `radio transmit failed: …`: a driver error,
  such as `SX126x BUSY timeout` or `SX126x not detected`. These are not rate-limited, because they
  happen only on a start, a retune or a transmission. The daemon keeps running. When starting the
  radio or a transmission fails, the host also gets a KISS error (`ERROR_INITRADIO` or
  `ERROR_TXFAILED`).
- `dropping host output because the PTY is not draining`: nobody reads the PTY, which is expected
  while no host is attached. It is logged once per stall, and again only after the PTY has taken a
  whole frame:

  ```text
  [2026-10-13T19:17:58Z WARN  rnode_linux::pty] dropping host output because the PTY is not draining
  ```

- `leaving <path> in place: it is no longer the symlink to this daemon's PTY …`: something replaced
  the PTY symlink while the daemon ran; see [`[pty]`](#pty).
- `failed to save radio settings: …` and `failed to delete radio settings: …`: the settings file or
  its directory is not writable. `ignoring corrupt radio settings file …` means the file could not
  be parsed and the daemon started without saved settings.

Logging never changes what the daemon sends to the host or the radio. Below `debug` the per-packet
code costs only a level check.

## Recording SPI traffic

`--spilog <file>` records every SPI transaction between the `sx1262` driver and the modem in a CSV
file, which helps when bringing up a new board. It works only with the `sx1262` driver; with
`radio.driver = "sim"` the daemon refuses to start:

```sh
rnode-linux --config /etc/rnode-linux.toml --spilog /var/lib/rnode/spi.csv
```

```text
fatal: --spilog records SPI transactions, but radio.driver = "sim" has no SPI bus
```

The file starts with the header `timestamp_us,direction,tx,rx`. Each row is one segment of a
transaction:

| Column | Contents |
| --- | --- |
| `timestamp_us` | Microseconds since the recorder started |
| `direction` | Always `TXRX`: SPI is full duplex, so every segment sends and receives |
| `tx` | Bytes sent, as lowercase hex without separators |
| `rx` | Bytes received during the same segment |

A register read is one transaction with two segments, so it gives two rows. The sync-word probe at
the start of `begin()` sends `1d074000` (`ReadRegister` of address `0x0740` plus a status byte),
then `00`; the second row's `rx` is the register value, `14` on a working SX1262. Delays inside a
transaction are not recorded.

The recorder flushes every row. If writing fails, it logs one `WARN` and the trace is incomplete
from then on; the daemon keeps running. While the radio is on, the daemon reads the interrupt
status, carrier detect and RSSI every 3 ms, which is about 1,000 transactions or 2,000 rows per
second, a few megabytes per minute. Record debugging sessions, not normal operation.

## Simulation and tests

### The simulated radio

`radio.driver = "sim"` replaces the SX1262 with [`rnode-sim`](crates/rnode-sim). Each daemon binds
a Unix datagram socket `<node_name>.sock` in `air_dir`. A transmission is sent to every other socket
in that directory as two datagrams: a carrier-start record and, after the LoRa time on air, the
frame itself. A simulated receiver gets the frame only if it was listening for the whole frame, was
not transmitting, and uses the same frequency, bandwidth, spreading factor and low-data-rate
optimisation as the sender. Overlapping transmissions collide and are lost. So the simulator
catches the timing, CSMA/CA and modulation mismatches that a plain loopback would hide.

The simulator works without root or radio hardware, on Linux and macOS, and in containers. Every
daemon pointed at the same `air_dir` hears the others.

### Tests

CI runs these four steps on every pull request, in this order:

```sh
cargo fmt --all --check
cargo build --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

- `rnode-core` checks the KISS protocol, framing, CSMA/CA, PHY timing and logging against values
  derived from the firmware.
- `rnode-sx126x` checks the exact SPI bytes the driver sends against a mocked bus.
- `rnode-sim` runs simulated radios against each other.
- `rnode-linux` covers config parsing, storage, the PTY and its symlink, the GPIO chip lookup, the
  SPI recorder, the command line and the event loop, including shutdown during a TX burst. Its
  `docs_examples` test parses every daemon config in this README and in [DRIVERS.md](DRIVERS.md)
  with the real config loader, checks the option names of the Reticulum example and the relative
  links in both, so the documentation cannot drift from the code.
- The smoke test [`crates/rnode-linux/tests/sim_daemon_smoke.rs`](crates/rnode-linux/tests/sim_daemon_smoke.rs)
  starts two daemon processes on the simulated radio. It checks that KISS data frames of 1, 254,
  255 and 508 bytes travel between their PTYs in both directions, and that a 509-byte frame is
  dropped. It is part of `cargo test`, so it runs in CI.

### Reticulum end-to-end test

`cargo xtask rns-e2e` runs real Reticulum against two simulated daemons. It needs a Reticulum
checkout and [uv](https://docs.astral.sh/uv/), so it is run by hand, not in CI; a run takes about
five minutes:

```sh
cargo xtask rns-e2e --reticulum ../Reticulum --keep-logs
```

- Without `--reticulum` it uses `$RNODE_RETICULUM`, or `../Reticulum` next to the repository.
- It builds the daemon, starts two instances on a private air directory under `target/rns-e2e`, and
  creates a virtual environment with `pyserial` and `cryptography`.
- At 125 kHz with SF7, and with SF11, where the firmware's low-data-rate rule and the textbook
  one disagree, it first probes the KISS boundary sizes (1, 254, 255, 508 and 509 bytes) in both
  directions. Then two Reticulum processes discover each other, establish a link, exchange packets
  and transfer a 4 KiB resource.
- Logs stay in `target/rns-e2e` when a run fails, and with `--keep-logs` also when it passes.

[`crates/xtask/README.md`](crates/xtask/README.md) has the details and typical timings.
[`tests/e2e/kiss_probe.py`](tests/e2e/kiss_probe.py) also works on its own against any daemon PTY,
real hardware included:

```sh
python3 tests/e2e/kiss_probe.py --port /run/rnode/rnode0 --frequency 914875000 \
  --bandwidth 125000 --txpower 2 --sf 7 --cr 5 --sizes 1,254,255,508,509 --count 2
```

It needs `pyserial`, prints every reply it decodes, and turns the radio off before it exits.
`--config-commands` also sends `CMD_CONF_SAVE`, `CMD_CONF_DELETE` and `CMD_RESET`, which leaves the
daemon without saved settings. With a real modem the probe transmits, so attach an antenna and use
values legal where you are.

## Code coverage

Coverage uses [cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov). Install it once:

```sh
cargo install cargo-llvm-cov
rustup component add llvm-tools-preview
```

It needs the LLVM tools of the toolchain that compiles the tests. Rust from rustup has them as the
`llvm-tools-preview` component. With a Rust from a distribution or Homebrew, run the commands
through rustup (`rustup run stable cargo coverage`), or point `LLVM_COV` and `LLVM_PROFDATA` at a
matching LLVM.

The workspace defines three aliases in [`.cargo/config.toml`](.cargo/config.toml):

| Command | Runs | Output |
| --- | --- | --- |
| `cargo coverage-summary` | `cargo llvm-cov` | Per-file summary in the terminal |
| `cargo coverage` | `cargo llvm-cov --html` | HTML report in `target/llvm-cov/html/` |
| `cargo coverage-lcov` | `cargo llvm-cov --lcov --output-path lcov.info` | `lcov.info` for editors and CI tools (git-ignored) |

The aliases accept further `cargo llvm-cov` options, e.g. `cargo coverage -p rnode-core --open`.

## Hardware test tools

[`tools/hwtest`](tools/hwtest) holds the checks used to validate the SX1262 driver on the uConsole
AIO V2, outside the Cargo workspace so that CI does not need Linux GPIO crates:

- `sx126x-bringup` runs the driver directly, without the daemon: it resets and detects the modem,
  configures it, samples RSSI and carrier detect, transmits test frames and times them against the
  Semtech time-on-air formula, and checks the low-data-rate boundaries. It refuses frequencies
  outside 902–928 MHz and powers above 10 dBm.
- `bin/soapysdr_capture.c` records IQ samples from the AIO V2's RTL-SDR.
- `lora_capture_analyze.py` finds LoRa bursts in a capture and checks duration, occupied bandwidth,
  preamble, sync word and frequency offset.

Build the bring-up tool on the target:

```sh
cargo build --manifest-path tools/hwtest/sx126x-bringup/Cargo.toml
tools/hwtest/sx126x-bringup/target/debug/sx126x-bringup --mode bringup
```

[`tools/hwtest/README.md`](tools/hwtest/README.md) has the full procedure and the recorded results.

## Differences from RNode_Firmware

rnode-linux matches RNode_Firmware v1.86 wherever a host or another radio can observe the
behaviour, except for these deliberate differences:

**Host protocol** (most of these are listed in the module docs of `rnode_core::node`):

- KISS configuration commands retune the radio immediately while it is on.
- The RX and TX packet counters (`CMD_STAT_RX`, `CMD_STAT_TX`) count.
- An exactly 508-byte packet goes out as two LoRa frames. The firmware adds a third, header-only
  frame that carries no data and only leaves receivers with a stale half packet.
- Oversized raw frames in promiscuous mode are rejected with `ERROR_TXFAILED` or dropped, not
  truncated.
- The single-byte replies to `CMD_RANDOM` and `CMD_IMPLICIT` are KISS-escaped, so a value of
  `0xC0` cannot break the host's framing.
- Implicit-header receive is used only in promiscuous mode, so normal split RNode frames always keep
  explicit LoRa headers.
- `CMD_DIS_IA` changes interference avoidance for the running daemon only and sends no reply. The
  firmware writes the setting to EEPROM and reboots; here the persistent setting is
  `[radio] avoid_interference`.
- A shutdown request cuts a burst of transmissions short. The firmware's `flush_queue()` sends
  every queued packet once a burst starts. rnode-linux stops between packets, never between the two
  frames of a split packet, and drops the packets still queued; see
  [Stopping the daemon](#stopping-the-daemon).
- `CMD_RESET` resets the node's state and reloads the saved settings without restarting the
  process, so the PTY stays open.
- The node identifies as a Linux platform: platform `0x60`, MCU `0x61` and board `0x60`, values
  that no firmware build uses, with firmware version 1.86.

**SX1262 driver** (crate docs of `rnode-sx126x`):

- Every `tcxo_voltage` uses the SX1262 datasheet's encoding. The firmware's board constants map
  2.4 V and 2.7 V to the 3.0 V setting, which it never selects.
- The TX-done wait scales with the frame's airtime instead of the firmware's fixed 20 s, which can
  abort valid long frames at SF12 and narrow bandwidths.
- A stuck BUSY line fails the command with `SX126x BUSY timeout` after `busy_timeout_ms`, where the
  firmware carries on.
- Carrier detect forgets a preamble detection once the preamble interrupt clears, so a completed
  short packet cannot make the next packet's preamble look stale.

Some firmware behaviour that surprises people is the same in rnode-linux: `CMD_STAT_RSSI` before
the first received packet reports −36 dBm (the firmware's −292 dBm placeholder after the RSSI
offset wraps); `CMD_STAT_CHTM`, `CMD_STAT_PHYPRM` and `CMD_STAT_CSMA` are only pushed by the node,
never answered on request; and data frames over 508 bytes are dropped without an error.

## Limitations

- **`rnodeconf` does not work.** It starts by reading the RNode's EEPROM with `CMD_ROM_READ`, which
  rnode-linux does not implement, and stops with "Could not download EEPROM from device". There is no
  EEPROM to provision, sign or update: the hardware description is the config file,
  `--tnc`/`--normal` correspond to the [saved settings file](#saved-radio-settings), and
  `--ia-enable`/`--ia-disable` correspond to `[radio] avoid_interference`.
- **Commands for microcontroller boards are ignored:** display, framebuffer, Bluetooth, Wi-Fi, LEDs,
  device signatures, firmware hashes and firmware updates.
- **SX1262 only.** Other modems need a driver; see [DRIVERS.md](DRIVERS.md).
- **One modem per daemon.** For several modems, run one daemon per modem, each with its own config,
  PTY and settings file.
- **TX power tops out at 22 dBm**, the SX1262 without an external amplifier. A configurable ceiling
  and amplifier support are tracked in issue #48.
- **One GPIO chip.** All of a modem's GPIO lines must be on the chip named by `[gpio] chip`.
- **No TXEN line.** The RF switch is driven by DIO2, optionally with an RX-enable GPIO.
- **Hardware support needs Linux;** other systems can only run the simulator.
- **One host at a time.** The PTY is a serial port, so only one program can use it.

## Troubleshooting

| Symptom | Cause and fix |
| --- | --- |
| `fatal: load config.toml: failed to read config.toml: No such file or directory (os error 2)` | No config in the working directory. Pass `--config /etc/rnode-linux.toml` |
| ``fatal: load …: … unknown field `frequency`, expected one of `driver`, `avoid_interference`, `sx1262`, `sim` `` | Link parameters do not belong in the config; the host sets them. Other unknown fields are usually typos |
| `fatal: load …: invalid config: [spi] is required when radio.driver = "sx1262"` | `driver` defaults to `"sx1262"`. Add `[spi]` and `[gpio]`, or set `driver = "sim"` |
| `fatal: load …: invalid config: pty.group "…" is not a known group` | Create the group, or fix the name |
| `fatal: sx1262 hardware daemon runs only on Linux` | Use the `sim` driver on other systems |
| `fatal: --spilog records SPI transactions, but radio.driver = "sim" has no SPI bus` | `--spilog` only works with the `sx1262` driver |
| `fatal: open SPI device /dev/spidev1.0: spi io: No such file or directory (os error 2)` | SPI is not enabled (see the overlay step), or the daemon's user lacks access to the device |
| `fatal: open GPIO lines: …` | Wrong chip or line number, missing permission on `/dev/gpiochipN`, or another program (another daemon, a Meshtastic or LoRa service) holds the lines |
| `fatal: open GPIO lines: gpio chip "…" not found` | No chip has that label. Chips whose label could not be read are listed after it, with the reason |
| `fatal: open PTY: symlink … failed: No such file or directory (os error 2)` | The symlink's directory does not exist. Under systemd it is created by `RuntimeDirectory=`; for a manual run create it |
| `fatal: open PTY: refusing to replace … with the PTY symlink: it is a regular file, not a symlink` | `[pty] symlink` points at an existing file that is not a symlink. Fix the path; the daemon never deletes such a file |
| `fatal: open simulated radio: simulated air_dir must be private to its owner (mode 0700)` | Run `chmod 700` on the directory, or let the daemon create it |
| `WARN … radio begin failed: SX126x not detected` | The modem does not answer on SPI: no power (on the AIO V2, set `power = 16`), wrong `[spi] device`, wrong reset line, or bad wiring. Record a `--spilog` and look at the sync-word reads |
| `WARN … radio begin failed: SX126x BUSY timeout` | BUSY stays high: wrong `busy` line, no power, or a TCXO the modem cannot start (check `tcxo_voltage`) |
| `WARN … radio DCD failed: …`, `radio RSSI failed: …`, `radio RX service failed: …` | The modem stopped answering while the radio was on: power, wiring or SPI. Repeats are summarised every 10 s; an `INFO … recovered after …` line follows once the calls work again |
| `WARN … modem timed out during TX` | The modem never reported TX done. Check `tcxo_voltage`, the power supply and `tx_timeout_ms`. The node resets after each timeout |
| Reticulum: "the reported radio parameters did not match your configuration" | Use an SX1262 bandwidth and at most 22 dBm (see [Running with Reticulum](#running-with-reticulum)) |
| Reticulum: `Invalid spreading factor configured`, or `Skipping disabled interface` | Use the keys `spreadingfactor` and `codingrate`, and add `enabled = yes` (see [Running with Reticulum](#running-with-reticulum)) |
| Reticulum: "Could not open serial port", or permission denied | The daemon is not running, `port` is not the symlink, or the PTY permissions do not let the `rnsd` user in (see [PTY permissions for rnsd](#pty-permissions-for-rnsd)) |
| Two RNodes do not hear each other | They must share frequency, bandwidth and spreading factor. With `RUST_LOG=debug`, a `TX packet` line on one side and no `RX packet` line on the other points at RF: antenna, distance or power |
| `WARN … dropping host output because the PTY is not draining` | No host is reading the PTY. Harmless while none is attached |
| The daemon takes long to stop | It finishes the packet on the air, which takes seconds at high spreading factors and minutes at SF12 with the narrowest bandwidths; see [Stopping the daemon](#stopping-the-daemon) |

For a closer look, run the daemon in the foreground with `RUST_LOG=trace` and `--spilog`, and
compare the SPI log with the sequence `begin()` sends in
[`crates/rnode-sx126x/src/lib.rs`](crates/rnode-sx126x/src/lib.rs).

## Contributing

Changes go through pull requests on Forgejo (`infra/rnode-linux`), and CI runs the four
[test steps](#tests) on each. [AGENTS.md](AGENTS.md) has the rules: test layout, firmware
references, and tests for every behaviour change. New radio drivers start with
[DRIVERS.md](DRIVERS.md).

## License

GPL-3.0-or-later; see the `license` field in [`Cargo.toml`](Cargo.toml).
