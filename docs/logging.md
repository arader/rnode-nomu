# Logging

The daemon logs to stderr; under systemd that goes to the journal. `[logging] log_level` takes the
same filter syntax as `RUST_LOG`: a level (`error`, `warn`, `info`, `debug`, `trace` or `off`), or
comma-separated `target=level` directives such as `"warn,rnode_core::node=debug"`. If `RUST_LOG` is
set in the environment, it replaces `log_level` entirely. The config is read before logging starts,
so errors in it appear as `fatal:` lines on stderr.

| Level | What you get |
| --- | --- |
| `warn` | Problems: radio driver errors (rate-limited), modem TX timeouts, failed settings writes, a host that stopped reading the PTY, EEPROM writes and wipes the daemon refuses, firmware-update and signing commands it ignores |
| `info` (default) | Startup, the radio going online and offline with the reason and the settings, retuning by the host, interference-avoidance changes, a host signing the EEPROM, packets dropped at shutdown, recovery of a failing radio call |
| `debug` | One line per packet received, per packet sent and per packet dropped, with the reason, received frames that failed their CRC included; two lines per wait for channel access |
| `trace` | Everything above, plus a hex dump of every received and sent payload; see the [warning](#trace-logs-contain-your-traffic) below |

## What `info` shows

At the default level the daemon logs its startup, every radio state change and every retune, but
nothing per packet. These lines come from two simulated daemons set up as in the
[quick start without hardware](development.md#quick-start-without-hardware), on
Linux, with a test script as the host. The script configured daemon A, toggled interference
avoidance, sent packets, retuned the radio, turned it off and queued one more packet:

```text
[2026-10-13T19:23:46Z INFO  rnode_nomu] loaded config from sim-a.toml
[2026-10-13T19:23:46Z INFO  rnode_nomu] PTY slave: /dev/pts/1
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
- With [`[radio.pa]`](configuration.md#radiopa), the TX power in these lines is the power after the
  PA; a `debug` line gives the modem output.

## Packets at `debug`

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
| `rnode_core::node` | `dropping received LoRa frame: CRC error (RSSI R dBm, SNR S dB)` | The modem received a frame whose payload CRC failed: interference, a collision or a weak signal. The firmware drops these without a trace. Several since the last line read `dropping N received LoRa frames: CRC error (last: …)`; without the signal if the driver could not read it |
| `rnode_core::node` | `dropping N-byte TX packet: queue cleared by reset` | `CMD_RESET` or a modem TX timeout emptied the queue |
| `rnode_core::node` | `dropping N-byte TX packet: shutting down` | The packet was still queued when the daemon stopped |

## Channel access at `debug`

Before every transmission the node waits as the firmware's CSMA/CA does: while the channel is busy,
then for DIFS and a random contention window. Each wait gets two lines, however long it lasts: one
when queued packets first have to wait, with the reason, and one when the node transmits, with how
long the wait took for each reason. If the radio goes offline first, the wait ends without the
second line, and packets still queued wait afresh once it is back. These come from two simulated
daemons on macOS, B with `[radio.sim] crc_error = 0.5`; a test script made A send a 254-byte packet
every 2.5 s and B a 20-byte packet half a second after each. From daemon A, then B:

```text
[2026-10-15T08:01:05Z DEBUG rnode_core::node] TX waiting for channel access: contention window; 1 packet queued
[2026-10-15T08:01:05Z DEBUG rnode_core::node] TX after waiting 360 ms for channel access: contention window 360 ms
[2026-10-15T08:01:05Z DEBUG rnode_core::node] TX packet: 254 bytes, 1 frame, header 0xC0 (seq 12): ok
```

```text
[2026-10-15T08:01:05Z DEBUG rnode_core::node] TX waiting for channel access: carrier detected; 1 packet queued
[2026-10-15T08:01:05Z DEBUG rnode_core::node] dropping received LoRa frame: CRC error (RSSI -70 dBm, SNR 7.00 dB)
[2026-10-15T08:01:06Z DEBUG rnode_core::node] TX after waiting 346 ms for channel access: carrier detected 272 ms, contention window 74 ms
[2026-10-15T08:01:06Z DEBUG rnode_core::node] TX packet: 20 bytes, 1 frame, header 0xE0 (seq 14): ok
[2026-10-15T08:01:08Z DEBUG rnode_core::node] TX waiting for channel access: carrier detected; 1 packet queued
[2026-10-15T08:01:08Z DEBUG rnode_core::node] RX packet: 254 bytes, 1 frame, header 0xF0 (seq 15), RSSI -70 dBm, SNR 7.00 dB
[2026-10-15T08:01:08Z DEBUG rnode_core::node] TX after waiting 451 ms for channel access: carrier detected 161 ms, contention window 290 ms
[2026-10-15T08:01:08Z DEBUG rnode_core::node] TX packet: 20 bytes, 1 frame, header 0xE0 (seq 14): ok
```

B's first packet waited for A's frame, which then failed its CRC at B; the second waited for A's
next frame, which arrived intact. The reasons, in the order the second line lists them:

| Reason | Why the packets wait |
| --- | --- |
| `airtime limit` | The host's airtime limit is reached (`CMD_ST_ALOCK`, `CMD_LT_ALOCK`; Reticulum's `airtime_limit_short` and `airtime_limit_long`) |
| `no channel status` | Reading carrier detect or RSSI failed, so CSMA/CA has nothing to decide on; the warnings say why |
| `carrier detected` | Another LoRa transmission is on the air |
| `interference` | A signal well above the noise floor without a LoRa carrier, with `avoid_interference` on. Until the noise floor is measured, about 0.4 s after the radio starts, any signal counts |
| `contention window` | The channel is free; CSMA/CA waits for DIFS and the random contention window, as before every transmission |

## Payload dumps at `trace`

At `trace`, each `RX packet` and `TX packet` line is followed by a dump of the payload in the style
of `hexdump -C`:

```text
[2026-10-13T19:23:53Z TRACE rnode_core::node] RX payload, 255 bytes:
    0000  00 01 02 03 04 05 06 07  08 09 0A 0B 0C 0D 0E 0F  |................|
    0010  10 11 12 13 14 15 16 17  18 19 1A 1B 1C 1D 1E 1F  |................|
    …
    00F0  F0 F1 F2 F3 F4 F5 F6 F7  F8 F9 FA 00 01 02 03     |...............|
```

### Trace logs contain your traffic

`trace` writes every payload the node sends or receives to the log: Reticulum announces, link
setup, destination hashes and anything else that crosses the radio. Use it for short debugging
sessions only, never leave it on in production, and keep such logs private. Under systemd the dumps
end up in the journal, which keeps them until it is rotated away. For a debugging session, prefer
running the daemon in the foreground with its log in a private file that you delete afterwards:

```sh
(umask 077; RUST_LOG=trace rnode-nomu --config /etc/rnode-nomu.toml 2> trace.log)
```

## Warnings

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
  `radio DCD`, `radio RSSI` and `radio RX service`. A preamble length the modem did not take after
  a retune is logged the same way, as `radio set_preamble_length`: the node keeps the new
  modulation's timing and tries again before each transmission and each return to receive, and
  logs `INFO … recovered` as soon as one works.
- `modem timed out during TX; hard-resetting the node and dropping N queued TX packets`: the modem
  never reported TX done. Like the firmware, the node resets, which clears the queue and reloads
  the saved settings.
- `radio begin failed: …`, `radio operation failed: …`, `radio transmit failed: …`: a driver error,
  such as `SX126x BUSY timeout` or `SX126x not detected`. These are not rate-limited, because they
  happen only on a start, a retune or a transmission. The daemon keeps running. When starting the
  radio or a transmission fails, the host also gets a KISS error (`ERROR_INITRADIO` or
  `ERROR_TXFAILED`).
- `radio set_bandwidth(250 kHz) failed: SX126x BUSY timeout; keeping 125 kHz`: the modem did not
  take a frequency, bandwidth, spreading factor, coding rate or TX power, named by the driver call
  (`set_frequency`, `set_bandwidth`, `set_spreading_factor`, `set_coding_rate`, `set_tx_power`). The
  daemon sets the modem back to the value it last accepted, keeps its timing, and reports that value
  to the host. The host thus sees the mismatch: Reticulum does not bring the interface up when the
  frequency, bandwidth, TX power or spreading factor the radio reports differ from its
  configuration. `radio set_bandwidth(125 kHz) failed while restoring it: …` follows if setting the
  old value back failed too. With [`[radio.pa]`](configuration.md#radiopa), `set_tx_power` shows
  modem outputs, before the PA: `radio set_tx_power(19 dBm) failed: …; keeping 6 dBm` through the
  GC1109 means that a host asked for 28 dBm and the daemon kept 17 dBm.
- `dropping host output because the PTY is not draining`: nobody reads the PTY, which is expected
  while no host is attached. It is logged once per stall, and again only after the PTY has taken a
  whole frame:

  ```text
  [2026-10-13T19:17:58Z WARN  rnode_nomu::pty] dropping host output because the PTY is not draining
  ```

- `leaving <path> in place: it is no longer the symlink to this daemon's PTY …`: something replaced
  the PTY symlink while the daemon ran; see [`[pty]`](configuration.md#pty).
- `failed to save radio settings: …` and `failed to delete radio settings: …`: the settings file or
  its directory is not writable. `ignoring corrupt radio settings file …` means the file could not
  be parsed and the daemon started without saved settings.
- `ignoring saved radio settings: TX power N is out of range …`: the file holds a `tx_power` from
  128 to 254, probably a negative power saved by an earlier build; see
  [Saved radio settings](configuration.md#saved-radio-settings). It is logged on every start and
  `CMD_RESET` until the file is fixed.
- `failed to put the radio to sleep: …`: the modem did not take `SetSleep` when the radio stopped,
  or [`[gpio] txen`](configuration.md#gpio) could not be driven low (`GPIO error: …`). If both
  failed, the line reads
  `failed to put the radio to sleep: TXEN could not be driven low (GPIO error: …), so the PA may still be switched to transmit; SetSleep failed too: …`.
  Check the module's power, the SPI bus and the GPIO line; with `power_off_on_exit`, the daemon
  drives TXEN low again on exit.
- `power_off_on_exit: failed to drive [gpio] reset low: …`, or `rxen`, `txen` or `power`: on exit,
  the daemon could not drive that line low while switching the modem off; see
  [`[gpio]`](configuration.md#gpio). It still drives the other lines low, the power line last, so a
  failed `power` leaves the modem on.
- `refusing CMD_ROM_WRITE to EEPROM address 0x00: the EEPROM is locked; sent ERROR_EEPROM_LOCKED`
  and `refusing to wipe the EEPROM (CMD_UNLOCK_ROM): …`: a host tried to change the EEPROM image,
  for example with `rnodeconf --eeprom-wipe`. Nothing changed; see
  [Using rnodeconf](reticulum.md#using-rnodeconf).
- `ignoring CMD_FW_UPD: …`, `ignoring CMD_FW_HASH: …` and `ignoring CMD_DEV_SIG: …`: a host tried to
  update the firmware or sign the device, which do not apply to rnode-nomu.
- `refusing to lock the EEPROM: …; it stays unprovisioned` and `refusing CMD_ROM_WRITE to EEPROM
  address 0x9C: only the info block, 0x00 to 0x9B, can be written`: `rnodeconf --rom` wrote an info
  block the daemon would not load, such as another product or model, or wrote beyond the info block;
  see [Signing with your own key](reticulum.md#signing-with-your-own-key).
- `ignoring EEPROM file …: …; the EEPROM reads as unprovisioned`: the file in
  [`[storage] eeprom`](configuration.md#storage) is not a valid info block, for example cut short.
  It is logged at every start, `CMD_RESET` and `CMD_ROM_READ` until you delete the file or sign the
  device again.
- `failed to save the EEPROM info block: …` and `failed to load the EEPROM info block: …`: the file
  or its directory is not writable or readable. The EEPROM stays, or reads as, unprovisioned.

Logging never changes what the daemon sends to the host or the radio. Below `debug` the per-packet
code costs only a level check.

## Recording SPI traffic

`--spilog <file>` records every SPI transaction between the `sx1262` driver and the modem in a CSV
file, which helps when bringing up a new board. It works only with the `sx1262` driver; with
`radio.driver = "sim"` the daemon refuses to start:

```sh
rnode-nomu --config /etc/rnode-nomu.toml --spilog /var/lib/rnode/spi.csv
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
