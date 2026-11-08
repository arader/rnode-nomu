# Running with Reticulum

Add an `RNodeInterface` to Reticulum's config (`~/.reticulum/config`, or the config directory of
your `rnsd`) with the PTY symlink as its port:

```text
[interfaces]
  [[rnode-nomu]]
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
Without `enabled = yes`, `rnsd` skips the interface (`Skipping disabled interface "rnode-nomu"`);
with `spreading_factor` or `coding_rate` it logs `Invalid spreading factor configured …` and
`Invalid coding rate configured …`, and exits. This block, with only `port` changed, brings the
interface up with `rnsd` 1.5.5 and the sim driver:

```text
[Notice]   RNodeInterface[rnode-nomu] is configured and powered up
```

When Reticulum opens the port it detects the RNode (rnode-nomu reports firmware version 1.86),
sends the frequency, bandwidth, TX power, spreading factor and coding rate, turns the radio on and
checks the values the daemon reports back. The daemon logs `radio online (host request): …` with
the settings it applied. RNodes that should hear each other need the same frequency, bandwidth and
spreading factor.

- `bandwidth` must be one the SX1262 supports: 7800, 10400, 15600, 20800, 31250, 41700, 62500,
  125000, 250000 or 500000 Hz. The modem rounds other values up, Reticulum sees a different value
  and aborts with "the reported radio parameters did not match your configuration".
- `txpower` may be at most 22 dBm, the SX1262 limit, or the daemon's
  [`max_tx_power_dbm`](configuration.md#radio) if that is lower. Higher values are clamped and abort
  the same way. With [`[radio.pa]`](configuration.md#radiopa), `txpower` is the power after the PA:
  from the PA's minimum (11 dBm through the GC1109, 13 dBm through the KCT8103L) up to its
  `max_output_dbm`. Below the minimum, the daemon reports the minimum, as the firmware does, and
  Reticulum aborts the same way.
- `airtime_limit_short` and `airtime_limit_long` work as with any RNode.
- Start the daemon before Reticulum, or let Reticulum retry: it tries to reopen the port every
  5 seconds.
- A restarted daemon has a new PTY, and the symlink follows it. On Linux, Reticulum sees the old PTY
  hang up, reopens `port` and configures the radio again. On macOS it does not notice, so restart
  Reticulum after restarting a simulated daemon.
- Several Reticulum programs cannot share one PTY. Run a shared `rnsd` instance and let the other
  programs connect to it.
- rnode-nomu has not been tried with `rnsd` in a container. The symlink in `/run/rnode` points
  into the daemon's `/dev/pts`, which a container does not see by default, and the PTY gets a new
  number every time the daemon starts.

## Using rnodeconf

`rnodeconf`, Reticulum's tool for setting up RNodes, works with rnode-nomu for the jobs that apply
to it: device info, TNC and normal mode, and interference avoidance. Before almost anything else it
reads the RNode's EEPROM with `CMD_ROM_READ`. rnode-nomu has no EEPROM, so it answers with an image
in the firmware's layout (`ROM.h`), built from what the daemon keeps:

- the device information of a homebrew RNode: product `0xF0` and model `0xFE`, the firmware's codes
  for homebrew boards, hardware revision 1, a serial number, and 2026-10-22 00:00 UTC as the
  manufacturing time, the same on every installation;
- the MD5 checksum of that information, an empty signature (128 bytes of `0xFF`) and the lock byte
  of a provisioned device; or, once you have [signed it](#signing-with-your-own-key), the info block
  that rnodeconf wrote, with its own serial number, time and signature;
- the [saved settings](configuration.md#saved-radio-settings) as TNC settings, if the file holds
  settings the radio starts with;
- interference avoidance as the daemon runs it;
- erased bytes, `0xFF`, for everything else, so Bluetooth and Wi-Fi read as off.

The serial number stays the same across restarts: the daemon derives it from the machine ID
(`/etc/machine-id`, or `/var/lib/dbus/machine-id` if that is missing) and the
[`[pty] symlink`](configuration.md#pty) path, so that daemons on one machine differ. Without a
machine ID it depends on the symlink path alone. [`[eeprom] serial_number`](configuration.md#eeprom)
sets it instead.

Stop Reticulum first, and anything else that has the PTY open. As with a USB RNode, `rnsd` and
rnodeconf would both send commands and take each other's replies. Then give rnodeconf the PTY
symlink as its port. This is a simulated daemon with `serial_number = 42`, after
`rnodeconf /run/rnode/rnode0 --tnc --freq 914875000 --bw 125000 --txp 2 --sf 8 --cr 5`:

```text
$ rnodeconf /run/rnode/rnode0 --info
[23:20:48] Opening serial port /run/rnode/rnode0...
[23:20:51] Device connected
[23:20:51] Current firmware version: 1.86
[23:20:51] Reading EEPROM...
[23:20:51] EEPROM checksum correct
[23:20:51] Device signature validation failed

     WARNING! This device is NOT verifiable and should NOT be trusted.
     Someone could have added privacy-breaking or malicious code to it.
     …
              rnodeconf /run/rnode/rnode0 --autoinstall

[23:20:51]
[23:20:51] Device info:
[23:20:51]      Product            : Hombrew RNode (Band capabilities unknown) (f0:fe:60)
[23:20:51]      Device signature   : Unverified
[23:20:51]      Firmware version   : 1.86
[23:20:51]      Hardware revision  : 1
[23:20:51]      Serial number      : 00:00:00:2a
[23:20:51]      Modem chip         : Unknown
[23:20:51]      Frequency range    : 100.0 MHz - 1100.0 MHz
[23:20:51]      Max TX power       : 17 dBm
[23:20:51]      Manufactured       : 2026-10-21 17:00:00
[23:20:51]
[23:20:51]      Device mode        : TNC
[23:20:51]        Frequency        : 914.875 MHz
[23:20:51]        Bandwidth        : 125.0 KHz
[23:20:51]        TX power         : 2 dBm (1.585 mW)
[23:20:51]        Spreading factor : 8
[23:20:51]        Coding rate      : 5
[23:20:51]        On-air bitrate   : 3.12 kbps
```

`f0:fe:60` are the product, the model and the board rnode-nomu reports; "Hombrew" is rnodeconf's
spelling. The modem chip, frequency range and maximum TX power are what rnodeconf lists for model
`0xFE`. They limit nothing: the daemon accepts up to 22 dBm, or what
[`max_tx_power_dbm`](configuration.md#radio) and [`[radio.pa]`](configuration.md#radiopa) allow.
rnodeconf shows the manufacturing time in local time.

| `rnodeconf` option | With rnode-nomu |
| --- | --- |
| `--info` (`-i`) | The device info above. Without saved settings, `Device mode : Normal (host-controlled)` |
| `--config` (`-c`) | Bluetooth and Wi-Fi disabled, interference avoidance as the daemon runs it, display defaults |
| `--tnc` (`-T`) with `--freq`, `--bw`, `--txp`, `--sf` and `--cr` | Configures and starts the radio, then saves its settings with `CMD_CONF_SAVE` to [`storage.radio_settings`](configuration.md#storage), if that is set: from then on the radio starts without a host. rnodeconf asks for the TX power if `--txp` is missing or above 17 dBm; type it at the prompt |
| `--normal` (`-N`) | Deletes the saved settings (`CMD_CONF_DELETE`). The radio stays on until the daemon restarts or a host sends `CMD_RESET`, then waits for a host |
| `--ia-enable` (`-x`), `--ia-disable` (`-X`) | Interference avoidance with `CMD_DIS_IA`, until the daemon exits; [`[radio] avoid_interference`](configuration.md#radio) applies again after a restart |
| `--eeprom-dump`, `--eeprom-backup` | Print the image, or save it under `~/.config/rnodeconf/` |
| `--eeprom-wipe` | Refused. rnodeconf prints `hardware error (code 03)` and sends `CMD_RESET` 13 s later, which resets the node; the saved settings and a signed info block stay. Use `--normal` to delete the settings |
| `--update` (`-u`) | Fetches the firmware release list, finds no firmware for model `0xFE` and stops without flashing anything. Update rnode-nomu by installing a new build |
| `--rom` (`-r`) | Signs the device with your own key, if [`[storage] eeprom`](configuration.md#storage) is set and the EEPROM is unprovisioned; see [Signing with your own key](#signing-with-your-own-key). Otherwise rnodeconf refuses: "a valid EEPROM was already present" |
| `--sign` (`-S`), `--firmware-hash` (`-H`) | Change nothing: `--sign` finds no device hash to sign, and the daemon ignores the hash `--firmware-hash` sends, with a `WARN` line, although rnodeconf prints "Firmware hash set" |
| `--get-target-firmware-hash` (`-K`), `--get-firmware-hash` (`-L`) | rnodeconf stops with an `AttributeError`: it does not handle a device without firmware hashes |
| Bluetooth, display, NeoPixel and Wi-Fi options | Ignored |

Never run `rnodeconf --flash` (`-f`) on the PTY. Given `--fw-version` and a downloaded firmware, it
starts a flasher, avrdude unless `--platform` names another, on the port before it talks KISS; there
is no bootloader behind the PTY, and the flasher's bytes go to the daemon. `--autoinstall` looks the
port up among the serial ports, where a PTY never appears, and stops with "Could not find specified
port"; `--extract` fails the same lookup with an `UnboundLocalError`. No other option starts a
flasher: rnode-nomu reports its own platform, `0x60`, for which rnodeconf has none.

**The "NOT verifiable" warning.** rnodeconf checks the signature in the EEPROM against the RNode
vendor's keys and the user's own, and prints the warning above on every run when none verifies. An
image rnode-nomu makes up can never carry the vendor's signature. rnodeconf refuses nothing because
of it: every option above works the same. The `--autoinstall` it suggests does not apply; signing
the device with your own key, as homebrew RNodes are, removes the warning.

Reticulum's `RNodeInterface` (also the Android one) and `RNodeMultiInterface` never read the EEPROM,
so none of this affects using the radio. rnodeconf could recognise platform `0x60` and skip its
firmware and flashing steps for it; that would be a change to Reticulum.

### Signing with your own key

With [`[storage] eeprom`](configuration.md#storage) set, the daemon keeps an EEPROM info block in
that file, which `rnodeconf --rom` writes and signs with a key of yours. Until the file exists, the
EEPROM is unprovisioned: rnodeconf says "EEPROM is invalid, no further information available" for
`--info`, and refuses `--tnc` and `--normal` until you have signed it. To sign it:

1. Add the file to `/etc/rnode-nomu.toml`, in `/var/lib/rnode`, which the
   [systemd unit](running.md#running-as-a-systemd-service) lets the daemon write, and restart the
   daemon:

   ```toml
   [storage]
   radio_settings = "/var/lib/rnode/radio_settings.toml"
   eeprom = "/var/lib/rnode/eeprom.bin"
   ```

2. Make a signing key, unless you have one: `rnodeconf --key`. rnodeconf keeps it in
   `~/.config/rnodeconf/firmware/signing.key` of the user who runs it.
3. Stop Reticulum, then write and sign the info block, with the product, model and hardware revision
   the daemon accepts:

   ```sh
   rnodeconf /run/rnode/rnode0 --rom --product f0 --model fe --hwrev 1
   ```

   rnodeconf takes the serial number from its own counter, writes the 156 bytes of the info block
   and the lock byte with `CMD_ROM_WRITE`, resets the device and reads the EEPROM back. It ends with
   "EEPROM Bootstrapping successful!", and the daemon logs
   `host provisioned the EEPROM: product 0xF0, model 0xFE, hardware revision 1, serial number 0x00000001`.
   The daemon writes the file as it writes the settings, with mode `0600`, to a temporary file it
   renames into place.
4. `rnodeconf /run/rnode/rnode0 --info` now shows `Device signature   : Validated - Local signature`
   and no warning.

On other machines, rnodeconf verifies the device only with your public key: print it with
`rnodeconf --public` and add it there with `rnodeconf --trust-key <hex>`.

The daemon locks only an info block it would load: product `0xF0` with model `0xFE`, which name no
firmware in rnodeconf's tables, a hardware revision other than 0 and 255, and a correct checksum.
For any other, it refuses the lock byte with `ERROR_EEPROM_LOCKED` and logs why; rnodeconf then
reports "EEPROM was written, but validation failed", and you can run `--rom` again with the right
values. Writes to the settings beyond the info block are refused, since they come from the
settings file and the config.

To sign the device again, with another key or serial number, delete the file and run `--rom`
again; the daemon reads the file for each `CMD_ROM_READ`, so it needs no restart. `--eeprom-wipe`
is refused here too. A file that is not a valid info block, for example one cut short, is ignored
with a `WARN` line, and the EEPROM reads as unprovisioned.
