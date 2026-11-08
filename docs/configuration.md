# Configuration reference

The daemon reads one TOML file. It contains only how the hardware is wired and how the daemon
behaves. Radio link parameters (frequency, bandwidth, spreading factor, coding rate, TX power) are
not in it: the host sets them over KISS, as with any RNode, and `CMD_CONF_SAVE` stores them in the
[saved settings file](#saved-radio-settings). The config can only cap the TX power, with
[`max_tx_power_dbm`](#radio), and describe a power amplifier after the modem, with
[`[radio.pa]`](#radiopa), whose gain the host's TX power then includes.

- Unknown sections and keys are errors, so typos fail at startup instead of being ignored.
- Relative paths resolve against the daemon's working directory, not the config file's location.
- Integers may use TOML's `_` separators and `0x` prefixes (`8_000_000`, `0x28`).

[`config.example.toml`](../config.example.toml) lists every key with a comment.

## `[spi]`

Required when `radio.driver = "sx1262"`; ignored otherwise.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `device` | path | required | spidev device, e.g. `"/dev/spidev1.0"` |
| `speed_hz` | integer | `8_000_000` | SPI clock in Hz; must be greater than 0. The SX1262 accepts up to 16 MHz; lower it if long wires cause errors |

The mode (SPI mode 0) and word size (8 bits) are fixed.

## `[gpio]`

Required when `radio.driver = "sx1262"`; ignored otherwise. Line numbers are offsets on `chip`.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `chip` | string | `"gpiochip0"` | GPIO chip: a name such as `"gpiochip15"` (opened as `/dev/gpiochip15`), an absolute path, or a chip label such as `"pinctrl-rp1"`. A label is looked up in `/dev/gpiochip*` in name order. Chips whose label cannot be read are skipped with a `debug` line, and listed in the error if no chip matches |
| `reset` | integer | required | NRESET line, an output held high |
| `busy` | integer | required | BUSY line, an input |
| `dio1` | integer | required | DIO1 line, an input watched for rising edges (RX done) |
| `rxen` | integer | unset | Optional RX-enable output, driven high on receive |
| `txen` | integer | unset | Optional TX-enable output of a PA or RF switch, driven high before each transmission and low before receive and when the radio stops; with `rxen`, never high together with it |
| `power` | integer | unset | Optional power-rail enable output, driven high before the other lines are requested |
| `power_on_delay_ms` | integer | `100` | Wait after driving `power` high, before resetting the modem; only used when `power` is set |
| `power_off_on_exit` | bool | `false` | Switch the modem off when the daemon exits: drive `reset`, then `rxen`, then `txen`, then `power` low. Needs `power`. The default leaves the rail on and the lines as they are, so restarts do not power-cycle the modem; see below |

The daemon holds the lines for as long as it runs and releases them when it exits. Raspberry Pi
kernels keep a released output at its level (`persist_gpio_outputs`, on by default); other GPIO
drivers may switch released lines back to inputs. When the daemon stops on SIGINT or SIGTERM, or
exits on a fatal error, it leaves the lines like this:

| Line | `power_off_on_exit = false` | `power_off_on_exit = true` |
| --- | --- | --- |
| `reset` | high | low, driven first |
| `rxen` | low until the radio first starts, then high; with `txen`, low after a transmission that ended in a reset | low, driven next |
| `txen` | low: it is high only during transmissions, and the radio stops first | low, driven after `rxen` |
| `power` | high: the modem stays on | low, driven last |
| `busy`, `dio1` | inputs | inputs |

With `power_off_on_exit = true`, a stop still puts the radio to sleep first. Then `reset`, `rxen`
and `txen` go low before `power`: a line left high would hold 3.3 V on a pin of the unpowered
SX1262, whose datasheet allows at most VDD + 0.3 V on a digital input, and could feed the modem
through its input protection. If a line cannot be driven low, the daemon logs a
[warning](logging.md#warnings) and goes on with the next. Without `power`, the setting does nothing
and the lines stay as in the first column.

SIGKILL skips this: the lines stay as they were while the daemon ran, `power` and `reset` high; see
[When the daemon is killed](running.md#when-the-daemon-is-killed).

The SPI chip select is not a `[gpio]` line: the kernel's SPI driver holds it high while the bus is
idle, from boot on, whether or not the modem has power. On the uConsole that is GPIO18, which stays
high while `config.txt` or `power_off_on_exit` holds the rail off. The daemon cannot change that; it
is a matter for the device-tree overlay or the board.

## `[radio]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `driver` | string | `"sx1262"` | `"sx1262"` for the hardware driver, `"sim"` for the simulator |
| `avoid_interference` | bool | `false` | Interference avoidance at startup; see below |
| `max_tx_power_dbm` | integer | `22`, or `max_output_dbm` with [`[radio.pa]`](#radiopa) | Highest TX power in dBm a host may set, from 0 to 22, or to `max_output_dbm` with `[radio.pa]`; see below |

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
- The host can change the running value with `CMD_DIS_IA`, as `rnodeconf --ia-enable` and
  `--ia-disable` do. Unlike the firmware, which stores the value in EEPROM and reboots, rnode-nomu
  keeps it until the daemon exits (including across `CMD_RESET`) and logs it at `INFO`. The config
  file stays the persistent setting.

**`max_tx_power_dbm`** caps the TX power below the SX1262's 22 dBm, or below a power amplifier's
`max_output_dbm`, for a regional limit, a bench setup, or a module or antenna that cannot take full
power:

- A host's `CMD_TXPOWER` above the ceiling sets the ceiling, and the daemon reports the clamped
  value back, as RNode_Firmware's `kiss_indicate_txpower()` reports the power it applied. Settings
  saved with `CMD_CONF_SAVE` are clamped too when they are loaded, so a power saved before you
  lowered the ceiling is capped as well.
- Reticulum compares the reported power with its `txpower` and refuses the interface if they
  differ (`TX power mismatch`, then "the reported radio parameters did not match your
  configuration"). Set `txpower` to at most the ceiling.
- The range is 0 to 22 dBm. RNode hosts only ask for 0 dBm or more: Reticulum rejects a negative
  `txpower`, and the power byte is unsigned, as in RNode_Firmware. 22 dBm is the most an SX1262
  gives without an external amplifier. The `sim` driver has the same range. Values outside it stop
  the daemon at startup:

  ```text
  fatal: load /etc/rnode-nomu.toml: invalid config: radio.max_tx_power_dbm = 30 is out of range; the sx1262 driver allows 0 to 22 dBm
  ```

- With [`[radio.pa]`](#radiopa), the TX power is the power after the amplifier, and the range and
  the default become its `max_output_dbm`, as the firmware clamps `CMD_TXPOWER` to `PA_MAX_OUTPUT`
  on a board with an amplifier:

  ```text
  fatal: load /etc/rnode-nomu.toml: invalid config: radio.max_tx_power_dbm = 30 is out of range; with the gc1109 PA of [radio.pa] it allows 0 to 28 dBm, its max_output_dbm
  ```

- The ceiling limits the power the modem is set to. Antenna gain comes on top of it, so check the
  radiated power against your limit.

## `[radio.sx1262]`

Board options of the SX1262 driver. All of them are optional.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `tcxo_voltage` | string | unset (crystal) | Voltage DIO3 supplies to the TCXO: one of `"1.6V"`, `"1.7V"`, `"1.8V"`, `"2.2V"`, `"2.4V"`, `"2.7V"`, `"3.0V"`, `"3.3V"`. The trailing `V` is optional and `"3"` means 3.0 V. Validated when `driver = "sx1262"` |
| `dio2_as_rf_switch` | bool | `true` | Let the SX1262 drive the RF switch from DIO2 (firmware `setDio2AsRfSwitchCtrl()`) |
| `ocp` | integer | `0x28` | Over-current protection register value, in 2.5 mA steps (`0x28` = 100 mA); firmware `OCP_TUNED` |
| `busy_timeout_ms` | integer | `100` | How long to wait for BUSY to go low before a command fails with `SX126x BUSY timeout` (firmware `waitOnBusy()` waits 100 ms) |
| `tx_timeout_ms` | integer | `20000` | Minimum TX-done wait (firmware `LORA_MODEM_TIMEOUT_MS`). The driver waits for `max(tx_timeout_ms, 2 × airtime + 1000 ms)`, so long frames at SF12 are not cut short |

## `[radio.pa]`

A power amplifier (PA) after the SX1262, as on boards and modules that put out more than the
SX1262's 22 dBm, and the low-noise amplifier (LNA) that such front ends put before it on receive.
Leave the section out for a modem that drives the antenna itself.

> **Untested on PA hardware.** The mapping below is checked against RNode_Firmware's own functions,
> but no board with an amplifier has run it yet, and no measurement backs a `custom` table. Check
> the output power into a dummy load before you transmit through an amplifier.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `model` | string | required | `"gc1109"` or `"kct8103l"`, the two front ends of the Heltec WiFi LoRa 32 V4, with the firmware's gain tables (`Boards.h`); `"custom"` for your own table in `gain_db` |
| `gain_db` | array of integers | none | For `"custom"`, and required there: the PA's gain in dB at each modem output from 0 dBm up, one entry per dB. 1 to 23 entries (0 to 22 dBm), of 0 to 40 dB, each equal to the one before or 1 dB lower |
| `max_output_dbm` | integer | `28` for the named models; required for `"custom"` | Highest TX power in dBm a host may set through the PA (firmware `PA_MAX_OUTPUT`), from 0 up to the table's highest output: the last entry's modem output plus its gain, 28 dBm for both named models. Also the default and the upper bound of [`max_tx_power_dbm`](#radio) |
| `lna_gain_db` | integer | `17` for `"gc1109"`, `21` for `"kct8103l"`, `0` for `"custom"` | Gain in dB of the LNA before the SX1262 (firmware `LORA_LNA_GAIN`, `LORA_LNA_KCT8103L_GAIN`), 0 to 30. Set 0 if the receive path has none |

```toml
[radio.pa]
model = "gc1109"
# max_output_dbm = 28 # lower it to cap what hosts may set
# lna_gain_db = 17    # 0 if the receive path has no LNA
```

A `custom` table, with made-up values for illustration only:

```toml
[radio.pa]
model = "custom"
# Gain in dB at modem outputs of 0, 1, 2, ... 15 dBm.
gain_db = [12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 11, 11, 10, 10, 9, 8]
max_output_dbm = 23 # 15 + 8, the highest output of the table
lna_gain_db = 0
```

**How the TX power maps.** With `[radio.pa]`, the TX power a host sets (Reticulum's `txpower`,
`CMD_TXPOWER`) is the power after the PA. The daemon follows RNode_Firmware's `setTXPower()` on a
board with a PA (`HAS_LORA_PA`):

1. It clamps the request to `max_output_dbm`, or to [`max_tx_power_dbm`](#radio) if that is lower.
2. While the radio is on, it sets the SX1262 to the modem output whose power after the PA, the
   modem output plus its gain, reaches the request (`map_target_power_to_modem_output()`).
3. It reports to the host the power that this modem output gives after the PA
   (`map_modem_output_to_target_power()`), and `CMD_CONF_SAVE` saves that power.

Through the GC1109, for example:

| Host asks for | SX1262 set to | Host is told |
| --- | --- | --- |
| 0 dBm | −9 dBm, its minimum (the table gives −11 dBm) | 11 dBm |
| 10 dBm | −1 dBm | 11 dBm |
| 11 dBm | 0 dBm | 11 dBm |
| 17 dBm | 6 dBm | 17 dBm |
| 22 dBm | 11 dBm | 22 dBm |
| 27 dBm | 17 dBm | 27 dBm |
| 28 dBm or more | 19 dBm | 28 dBm |

**What the host is told** is the power the modem output gives after the PA, as the firmware's
`kiss_indicate_txpower()` reports it, not always the power the host asked for:

- Below the PA's minimum, its output at a modem output of 0 dBm (11 dBm through the GC1109, 13 dBm
  through the KCT8103L), the SX1262 is set below 0 dBm, down to its −9 dBm minimum, and the host
  is told the minimum. Reticulum then sees a different power than its `txpower` and does not bring
  the interface up: give it at least the PA's minimum.
- While the radio is off, the daemon reports the request as clamped, before the mapping, as the
  firmware maps only when it programs the modem. Once the radio is on, it reports the mapped power.
- If the SX1262 does not take a TX power, the daemon keeps the modem output it last took and
  reports the power that gives; the `radio set_tx_power(…)` warning names modem outputs (see
  [Warnings](logging.md#warnings)).

The `radio online` and `radio settings changed` lines show the power after the PA. At `debug`,
each TX power the daemon programs gets a line with the modem output. From a simulated daemon with
`model = "gc1109"`, whose host turned the radio on at 17 dBm and then asked for 0 dBm:

```text
[2026-10-23T06:37:13Z DEBUG rnode_core::node] TX power 17 dBm through the external PA: modem output 6 dBm, reported as 17 dBm
[2026-10-23T06:37:13Z INFO  rnode_core::node] radio online (host request): 914.875 MHz, 125 kHz, SF7, CR 4/5, 17 dBm
[2026-10-23T06:37:13Z DEBUG rnode_core::node] TX power 0 dBm through the external PA: modem output -11 dBm, -9 dBm in use, reported as 11 dBm
[2026-10-23T06:37:13Z INFO  rnode_core::node] radio settings changed: 914.875 MHz, 125 kHz, SF7, CR 4/5, 11 dBm
```

**A `custom` table** gives the gain at modem outputs 0, 1, 2, … dBm. To measure one, leave
`[radio.pa]` out, so that the host's TX power is the SX1262's output, and set
[`max_tx_power_dbm`](#radio) to the highest output you want to measure: the board then puts out up
to the PA's gain more than that. Measure the power after the PA at each TX power from 0 dBm up with
a power meter or a calibrated SDR, into a dummy load and within the limits that apply to you. Each
entry is the measured power minus the TX power. The rule that each gain equals the one before or is
1 dB lower keeps the PA's output rising by 1 or 0 dB per step, as in the firmware's tables: the
firmware's mapping, which the daemon follows, gets some powers wrong otherwise.

**The LNA.** `lna_gain_db` is subtracted from every RSSI the SX1262 driver reads, as firmware
`currentRssi()` and `packetRssi()` subtract `lora_lna_gain`, so that RSSI, noise floor and the
packet RSSI the host gets refer to the antenna. With a gain above 0, the daemon also leaves RSSI
samples more than 12 dB below the noise floor out of it (firmware `LORA_LNA_GVT`), as the firmware
does because the LNA's gain varies while it re-calibrates. The firmware's ESP32 builds, the Heltec
V4 among them, report the packet RSSI without subtracting the gain; see
[Differences from RNode_Firmware](how-it-works.md#differences-from-rnode_firmware).

**A PA's TX-enable input**, such as the KCT8103L's CTX (firmware `LORA_PA_CTX`) or the TXEN pin of
an Ebyte E22-900M30S, goes to [`[gpio] txen`](#gpio): high for each transmission, low for
receive, as the firmware drives CTX.

**With `driver = "sim"`** the daemon maps the TX power the same way, so you can try a configuration
without hardware. `lna_gain_db` has no effect there: the simulated radio reports `rssi_dbm` as
configured.

**Not ported** from the firmware's Heltec V4 support, because it is specific to that board:

- telling the GC1109 from the KCT8103L by reading a pin (`LORA_PA_CSD`): set `model` instead;
- the PA's enable lines `LORA_PA_PWR_EN`, `LORA_PA_CSD` and `LORA_PA_CPS`, which the firmware
  drives high once the radio starts and low when the board goes to sleep: [`[gpio] power`](#gpio)
  can hold one supply line on while the daemon runs;
- the V4's interference rule, which compares the RSSI with −89 dBm (`LNA_GD_LIMIT`) instead of the
  noise floor while the noise floor is at or below −109 dBm (`LNA_GD_THRSHLD`).

## `[radio.sim]`

Options of the simulated radio, used when `driver = "sim"`.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `air_dir` | path | `"target/rnode-air"` | Directory shared by all simulated radios that should hear each other. It must be private: mode `0700`, owned by the daemon's user and not a symlink. The daemon creates it if it is missing |
| `node_name` | string | `"rnode-nomu"` | Unique name of this radio; its socket is `<air_dir>/<node_name>.sock`. No `/`, not empty, not `.` or `..` |
| `rssi_dbm` | integer | `-70` | RSSI reported for received packets, and as the channel RSSI while a matching transmission is on the air. The idle channel reads −110 dBm |
| `snr_db` | float | `7.0` | SNR reported for received packets, in 0.25 dB steps |
| `loss` | float | `0.0` | Probability, from 0.0 to 1.0, that this radio drops an otherwise good frame. Validated when `driver = "sim"` |
| `crc_error` | float | `0.0` | Probability, from 0.0 to 1.0, that a good frame this radio did not lose fails its CRC instead: like a modem, the radio signals it but drops it, and the daemon logs it at `debug`. Validated when `driver = "sim"` |

The socket path `<air_dir>/<node_name>.sock` must fit in 103 bytes, so keep `air_dir` short; macOS
in particular limits Unix socket paths.

## `[pty]`

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
[2026-10-13T19:24:02Z WARN  rnode_nomu::pty] leaving /run/rnode/rnode0 in place: it is no longer the symlink to this daemon's PTY /dev/pts/0
```

## `[storage]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `radio_settings` | path | unset | File for the settings the host saves with `CMD_CONF_SAVE` (the firmware's EEPROM) |
| `eeprom` | path | unset | File for the EEPROM info block that [`rnodeconf --rom`](reticulum.md#signing-with-your-own-key) writes and signs with your own key. While the file does not exist, the EEPROM is unprovisioned. Unset, the daemon reports its built-in, unsigned info block |

Without `radio_settings`, `CMD_CONF_SAVE` stores nothing, and the daemon says so at startup:

```text
[2026-10-13T19:24:02Z WARN  rnode_nomu] storage.radio_settings is not set: radio settings the host saves with CMD_CONF_SAVE are not kept, so they are lost on restart and on CMD_RESET
```

The radio then stays off after a restart or a `CMD_RESET` until a host configures it again.

## `[eeprom]`

What the EEPROM image that [`rnodeconf`](reticulum.md#using-rnodeconf) reads reports.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `serial_number` | integer | derived | Serial number of the built-in info block, from 0 to 4294967295 (`0xFFFFFFFF`). Unset, the daemon derives one from the machine ID and [`[pty] symlink`](#pty). With [`[storage] eeprom`](#storage), the serial number is the one `rnodeconf --rom` writes, and this key has no effect |

```toml
[eeprom]
serial_number = 0x0000002A
```

## `[logging]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `log_level` | string | `"info"` | Log filter in `RUST_LOG` syntax; see [Logging](logging.md). `RUST_LOG`, when set, replaces it |

## Validation and startup warnings

`rnode-nomu` refuses to start, with `fatal: load <file>: …`, when:

- the file is not valid TOML, or has an unknown section or key;
- `radio.driver` is neither `"sx1262"` nor `"sim"`;
- `radio.driver = "sx1262"` and `[spi]` or `[gpio]` is missing, or `tcxo_voltage` is not one of the
  listed voltages;
- `radio.driver = "sim"` and `loss` is outside 0.0–1.0;
- `radio.max_tx_power_dbm` is outside 0–22, or with `[radio.pa]` outside 0 to its `max_output_dbm`;
- `[radio.pa]` names no or an unknown `model`, has `gain_db` with a named model, lacks `gain_db` or
  `max_output_dbm` with `"custom"`, or has a `gain_db`, `max_output_dbm` or `lna_gain_db` outside
  the rules of [`[radio.pa]`](#radiopa). These come as `failed to parse …`, with the line of
  `[radio.pa]` and the reason;
- `spi.speed_hz` is 0;
- `pty.mode` is not an octal permission string, sets setuid, setgid or sticky, or takes read or
  write access away from the owner;
- `pty.group` does not exist.

Devices, GPIO lines and directories are only opened afterwards; failures there are also fatal (see
[Troubleshooting](troubleshooting.md)). So is `--spilog` with the `sim` driver. The daemon starts
but logs a `WARN` line when:

- `storage.radio_settings` is not set, so saved settings are lost on restart and on `CMD_RESET`;
- `eeprom.serial_number` is set together with `storage.eeprom`, which replaces it;
- `pty.mode` gives other users read or write access (prefer `group` with `"0660"`);
- `pty.group` is set without `pty.mode`, since the PTY's default mode, usually `0620`, gives the
  group no read access.

## Saved radio settings

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

`tx_power` is in dBm, 0 or more. A value above 22 dBm, or above [`max_tx_power_dbm`](#radio), is
lowered to that limit when the file is loaded; with [`[radio.pa]`](#radiopa) the limit is its
`max_output_dbm`. With `[radio.pa]`, `tx_power` is the power after the PA, and `CMD_CONF_SAVE`
saves the power the daemon reported, as the firmware's EEPROM gets the power its `setTXPower()`
mapped. Builds before the fix for issue #75 read the host's
power as signed, and saved −9…−2 dBm as 247…254; read as unsigned, those would mean the highest
power allowed. So the daemon ignores a file whose `tx_power` is 128 to 254, logs a `WARN` line, and
leaves the radio off until a host configures it:

```text
[2026-10-15T08:12:30Z WARN  rnode_core::node] ignoring saved radio settings: TX power 247 is out of range (earlier builds saved -9 to -2 dBm as 247 to 254); the radio stays off until a host configures it
```

Set `tx_power` to the power you want, delete the file, or have a host save new settings with
`CMD_CONF_SAVE`. RNode_Firmware never meets such a value: its EEPROM only ever holds the power its
host path clamped to 22 dBm.

To provision TNC mode, write this file yourself, run
[`rnodeconf --tnc`](reticulum.md#using-rnodeconf), or have another host configure the radio and send
`CMD_CONF_SAVE`.
