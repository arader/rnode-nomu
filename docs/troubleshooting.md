# Troubleshooting

| Symptom | Cause and fix |
| --- | --- |
| `fatal: load config.toml: failed to read config.toml: No such file or directory (os error 2)` | No config in the working directory. Pass `--config /etc/rnode-nomu.toml` |
| ``fatal: load …: … unknown field `frequency`, expected one of `driver`, `avoid_interference`, `max_tx_power_dbm`, `sx1262`, `sim`, `pa` `` | Link parameters do not belong in the config; the host sets them. Other unknown fields are usually typos |
| `fatal: load …: invalid config: [spi] is required when radio.driver = "sx1262"` | `driver` defaults to `"sx1262"`. Add `[spi]` and `[gpio]`, or set `driver = "sim"` |
| `fatal: load …: invalid config: pty.group "…" is not a known group` | Create the group, or fix the name |
| `fatal: sx1262 hardware daemon runs only on Linux` | Use the `sim` driver on other systems |
| `fatal: --spilog records SPI transactions, but radio.driver = "sim" has no SPI bus` | `--spilog` only works with the `sx1262` driver |
| `fatal: open SPI device /dev/spidev1.0: spi io: No such file or directory (os error 2)` | SPI is not enabled (see [step 1 of the uConsole setup](hardware.md#uconsole-cm5-with-the-hackergadgets-aio-v2)), or the daemon's user lacks access to the device |
| `fatal: open GPIO lines: …` | Wrong chip or line number, missing permission on `/dev/gpiochipN`, or another program (another daemon, a Meshtastic or LoRa service) holds the lines |
| `fatal: open GPIO lines: gpio chip "…" not found` | No chip has that label. Chips whose label could not be read are listed after it, with the reason |
| `fatal: open PTY: symlink … failed: No such file or directory (os error 2)` | The symlink's directory does not exist. Under systemd it is created by `RuntimeDirectory=`; for a manual run create it |
| `fatal: open PTY: refusing to replace … with the PTY symlink: it is a regular file, not a symlink` | `[pty] symlink` points at an existing file that is not a symlink. Fix the path; the daemon never deletes such a file |
| `fatal: open simulated radio: simulated air_dir must be private to its owner (mode 0700)` | Run `chmod 700` on the directory, or let the daemon create it |
| `WARN … radio begin failed: SX126x not detected` | The modem does not answer on SPI: no power (on the AIO V2, set `power = 16`), wrong `[spi] device`, wrong reset line, or bad wiring. Record a `--spilog` and look at the sync-word reads |
| `WARN … radio begin failed: SX126x BUSY timeout` | BUSY stays high: wrong `busy` line, no power, or a TCXO the modem cannot start (check `tcxo_voltage`) |
| `WARN … radio DCD failed: …`, `radio RSSI failed: …`, `radio RX service failed: …` | The modem stopped answering while the radio was on: power, wiring or SPI. Repeats are summarised every 10 s; an `INFO … recovered after …` line follows once the calls work again |
| `WARN … modem timed out during TX` | The modem never reported TX done. Check `tcxo_voltage`, the power supply and `tx_timeout_ms`. The node resets after each timeout |
| `WARN … ignoring saved radio settings: TX power 247 is out of range …` | An earlier build saved a negative power. Set `tx_power` in the settings file to the power you want, or delete the file (see [Saved radio settings](configuration.md#saved-radio-settings)) |
| Reticulum: "the reported radio parameters did not match your configuration" | Use an SX1262 bandwidth and at most 22 dBm, or at most `max_tx_power_dbm` if you set it (see [Running with Reticulum](reticulum.md)). With `[radio.pa]`, use a `txpower` from the PA's minimum to its `max_output_dbm` (see [`[radio.pa]`](configuration.md#radiopa)). If the daemon logged `radio set_… failed`, the modem rejected the setting; see [Warnings](logging.md#warnings) |
| Reticulum: `Invalid spreading factor configured`, or `Skipping disabled interface` | Use the keys `spreadingfactor` and `codingrate`, and add `enabled = yes` (see [Running with Reticulum](reticulum.md)) |
| Reticulum: "Could not open serial port", or permission denied | The daemon is not running, `port` is not the symlink, or the PTY permissions do not let the `rnsd` user in (see [PTY permissions for rnsd](running.md#pty-permissions-for-rnsd)) |
| `rnodeconf`: "Could not download EEPROM from device" | Another program, such as `rnsd`, has the PTY open and took the reply: stop it first. rnode-nomu builds without EEPROM emulation never reply; update. See [Using rnodeconf](reticulum.md#using-rnodeconf) |
| `rnodeconf`: "WARNING! This device is NOT verifiable" | Expected: the EEPROM image carries no signature. rnodeconf works anyway; [sign the device with your own key](reticulum.md#signing-with-your-own-key) to remove the warning |
| `rnodeconf`: "EEPROM is invalid, no further information available", or "This device contains a valid firmware, but EEPROM is invalid" | [`[storage] eeprom`](configuration.md#storage) is set and the file does not exist, or is not a valid info block: sign the device with `rnodeconf --rom` (see [Signing with your own key](reticulum.md#signing-with-your-own-key)), or unset `eeprom` |
| `rnodeconf --rom`: "a valid EEPROM was already present" | The device is signed already, or `[storage] eeprom` is unset. To sign it again, delete the EEPROM file |
| `rnodeconf --rom`: "EEPROM was written, but validation failed" | The daemon refused the info block; its `WARN` line says why. Use `--product f0 --model fe --hwrev 1` |
| `rnodeconf --eeprom-wipe`: "hardware error (code 03)" | rnode-nomu refuses the wipe and keeps the saved settings. `rnodeconf --normal` deletes them |
| Two RNodes do not hear each other | They must share frequency, bandwidth and spreading factor. With `RUST_LOG=debug`, a `TX packet` line on one side and no `RX packet` line on the other points at RF: antenna, distance or power. A `CRC error` line instead means the frame was heard but corrupted: interference, a collision or a weak signal |
| `WARN … dropping host output because the PTY is not draining` | No host is reading the PTY. Harmless while none is attached |
| The daemon takes long to stop | It finishes the packet on the air, which takes seconds at high spreading factors and minutes at SF12 with the narrowest bandwidths; see [How long a stop takes](running.md#how-long-a-stop-takes) |
| systemd: `State 'stop-sigterm' timed out. Killing.` | The unit's `TimeoutStopSec=` is shorter than the packet on the air took. Use the shipped unit's `8min`, or at least the time in [How long a stop takes](running.md#how-long-a-stop-takes) for your settings; [When the daemon is killed](running.md#when-the-daemon-is-killed) says what was skipped |

For a closer look, run the daemon in the foreground with `RUST_LOG=trace` and `--spilog`, and
compare the SPI log with the sequence `begin()` sends in
[`crates/rnode-sx126x/src/lib.rs`](../crates/rnode-sx126x/src/lib.rs).
