# Running the daemon

The daemon's command line, what happens when it stops, and how to run it and Reticulum's `rnsd` as
systemd services. The [quick start](../README.md#quick-start) sets up both services on the
uConsole.

## Command line

```text
$ rnode-nomu --help
Linux userspace RNode daemon

Usage: rnode-nomu [OPTIONS]

Options:
  -c, --config <CONFIG>  Path to the TOML configuration file [default: config.toml]
      --spilog <SPILOG>  Record SPI transactions to a CSV file (radio.driver = "sx1262" only)
  -h, --help             Print help
  -V, --version          Print version

$ rnode-nomu --version
rnode-nomu 0.3.0 (reports RNode firmware 1.86 to hosts)
```

- `--config` defaults to `config.toml` in the working directory.
- `--spilog` records every SPI transaction of the `sx1262` driver; see
  [Recording SPI traffic](logging.md#recording-spi-traffic). With `radio.driver = "sim"` the daemon
  refuses to start, because the simulator has no SPI bus.
- `--version` prints the daemon's version ([Releases and versions](releases.md)) and the RNode
  firmware version it reports to hosts.

On a fatal error, such as an invalid config or a device that cannot be opened, the daemon prints
one `fatal:` line to stderr, naming each cause once, and exits with status 1:

```text
fatal: load nope.toml: failed to read nope.toml: No such file or directory (os error 2)
```

## Stopping the daemon

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
the [deliberate differences](how-it-works.md#differences-from-rnode_firmware).

With `power_off_on_exit = true`, the daemon switches the modem off last: it drives NRESET low, then
RXEN and TXEN if the board has them, and only then the power line, so that no line is left driving
the unpowered modem. [`[gpio]`](configuration.md#gpio) lists the state each line is left in, with
and without the setting.

### How long a stop takes

A stop waits for at most one packet on the air, up to two 255-byte LoRa frames, and then takes a
few milliseconds. The worst case is a 508-byte packet, whose time on air depends on the modulation
the host chose (Semtech's formula, with the firmware's preamble and low-data-rate rules):

| Modulation | 508-byte packet, CR 4/5 | 508-byte packet, CR 4/8 |
| --- | --- | --- |
| SF7, 125 kHz | 0.8 s | 1.3 s |
| SF9, 125 kHz | 2.6 s | 4.0 s |
| SF11, 125 kHz | 8.7 s | 13 s |
| SF12, 125 kHz | 19 s | 29 s |
| SF12, 31.25 kHz | 75 s | 115 s |
| SF12, 15.6 kHz | 150 s | 230 s |
| SF12, 7.8 kHz | 300 s | 460 s (7 min 40 s) |

A packet of up to 254 bytes is one frame and takes half as long. The
[systemd unit](#installing-the-unit) allows 8 minutes, enough for every row. Other supervisors
must allow as long for the slowest settings your hosts use, or they kill the daemon in the middle
of a packet:

- Docker waits 10 s by default: stop the container with `docker stop -t 480 <container>`, or
  start it with `docker run --stop-timeout 480` to make that its default.
- Docker Compose: set `stop_grace_period: 8m` on the service.
- Other supervisors have an equivalent, for example `stopwaitsecs` in supervisord or
  `terminationGracePeriodSeconds` in Kubernetes.

### When the daemon is killed

If a supervisor sends SIGKILL during a transmission, after a stop timeout that was too short:

- The frame on the air is still sent to the end. The driver starts each frame with the SX1262's
  `SetTx` in single mode (timeout 0), so the modem sends the frame from its buffer on its own and
  then falls back to standby (`STDBY_RC`; SX1261/2 datasheet, `SetTx` and
  `SetRxTxFallbackMode`). It does not keep transmitting.
- A split packet killed during its first frame loses its second frame: receivers get a lone first
  half, which a normal stop never leaves behind.
- The modem then idles in standby, neither receiving nor transmitting, until the daemon starts
  again and resets it.
- The daemon's cleanup does not run: the queued packets are dropped without log lines, the radio is
  not put to sleep, `power_off_on_exit` does not switch the modem off, and the PTY symlink stays
  behind, pointing to a PTY that no longer exists. The next start replaces it; under the systemd
  unit, systemd removes `/run/rnode`, symlink included, when the unit stops.
- The kernel releases the GPIO lines, as on any exit. Raspberry Pi kernels keep released outputs as
  they were (the `persist_gpio_outputs` option of their RP1 and BCM2835 GPIO drivers, on by
  default), so on the uConsole the modem keeps power and is not reset: the power line and NRESET
  stay high, even with `power_off_on_exit = true`. A GPIO driver that switches released lines back
  to inputs can instead cut the modem's power or hold it in reset, which ends the frame early. On a
  board with [`[gpio] txen`](configuration.md#gpio), TXEN stays high as well, so the PA stays
  switched to transmit while the modem idles, until the daemon starts again.
- systemd logs `State 'stop-sigterm' timed out. Killing.` and marks the unit failed, with result
  `timeout`.

The daemon could end a frame early instead, by putting the modem in standby. It deliberately does
not. A frame cut short is lost to every receiver, and cutting a split packet's second frame leaves
the same lone first half, so aborting would only trade a slow stop for a lost packet; it would also
need a change to the `Radio` trait for every driver. RNode_Firmware has no abort either: an RNode
that loses power simply stops. The cost is a stop that can take minutes at the slowest settings,
which the stop timeout has to allow for.

## Running as a systemd service

rnode-nomu is meant to run all the time, like a USB RNode that stays plugged in. It creates the PTY
when it starts, so a host cannot start it by opening the port.
[`contrib/rnode-nomu.service`](../contrib/rnode-nomu.service) runs the daemon as an unprivileged
`rnode` user, with a read-only file system, a private `/tmp` and access to only the listed devices.
systemd creates `/run/rnode` (`RuntimeDirectory=`) for the PTY symlink, and removes it when the
daemon stops, and `/var/lib/rnode` (`StateDirectory=`) for the saved settings. The
[Debian package](installing.md#debian-package) installs the binary and the unit, and creates the
account; to build it from source instead, see
[Building from source](installing.md#building-from-source). Install the config at
`/etc/rnode-nomu.toml`, as in the [uConsole steps](hardware.md#uconsole-cm5-with-the-hackergadgets-aio-v2).
The [Quick start](../README.md#quick-start) does all of this, and runs `rnsd` as a service too.

### Service account and device access

The daemon has an account of its own so that it can use what it needs and nothing else: the SPI
device and the GPIO chip, its two directories and its config. It reads whatever arrives on its PTY
and over the air, and it drives a transmitter and a power rail, so a fault in it should not reach
the rest of the system. On top of that, the unit sets `NoNewPrivileges=`, `ProtectSystem=strict`,
`ProtectHome=` and `PrivateTmp=`, and allows only the devices in its `DeviceAllow=` lines.

Create the account the unit runs as (the Debian package creates it, and gives it the groups below
where they exist):

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

On other distributions, add a udev rule such as `/etc/udev/rules.d/60-rnode-nomu.rules`, adjusted
to your devices, and reload the rules with `sudo udevadm control --reload && sudo udevadm trigger`:

```text
SUBSYSTEM=="spidev", KERNEL=="spidev1.0", GROUP="rnode", MODE="0660"
SUBSYSTEM=="gpio", KERNEL=="gpiochip15", GROUP="rnode", MODE="0660"
```

Also check the unit's `DeviceAllow=` lines. They cover the example configs: `/dev/spidev0.0` and
`/dev/gpiochip0` for the generic one, and `/dev/spidev1.0` with `/dev/gpiochip15` for the uConsole
ones on the reference image, or with `/dev/gpiochip0`, where a CM4's `pinctrl-bcm2711` usually is.
With `DevicePolicy=closed` the daemon cannot open any other spidev or GPIO chip.

GPIO permissions apply to a whole chip, not to single lines: the account can drive every line of
a chip it can open, not only the lines in its config. On the uConsole, `/dev/gpiochip15` carries
all of the RP1's lines, the AIO V2's other power rails (GPIO 7, 23 and 27) included.

### PTY permissions for rnsd

The PTY belongs to the daemon's user and, by default, only that user can read it (issue #47).
Whoever can open it can transmit, retune the radio and read everything it receives. If `rnsd` runs
as a different user, share a group with it. This example uses `dialout`, the usual group for serial
ports, which RNode users are often in already:

1. In `/etc/rnode-nomu.toml`, set `group = "dialout"` and `mode = "0660"` under `[pty]`.
2. Let the daemon hand the PTY to that group: add its account with
   `sudo usermod -aG dialout rnode`, or set `SupplementaryGroups=dialout` in a drop-in
   (`sudo systemctl edit rnode-nomu`; see
   [Keeping local changes across updates](#keeping-local-changes-across-updates)).
3. Add the user that runs `rnsd` to the group with `sudo usermod -aG dialout <rnsd-user>`, then log
   in again or restart its service.

Avoid modes such as `"0666"`, which let every local user send packets through your radio; the
daemon warns about them.

Running `rnsd` and the daemon under one account instead, such as your own, needs no group or mode:
the PTY's owner can always open it. But the daemon then has everything that account has, and every
program running under that account can use the SPI and GPIO devices.

### Installing the unit

The Debian package installs the unit as `/usr/lib/systemd/system/rnode-nomu.service`; with it,
skip the `install` command below. Otherwise:

```sh
sudo install -m 0644 contrib/rnode-nomu.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now rnode-nomu
journalctl -u rnode-nomu -f
```

To start `rnsd` after the daemon, see [Starting rnsd after the daemon](#starting-rnsd-after-the-daemon).

The unit sets `TimeoutStopSec=8min` instead of systemd's default 90 s. On stop, the daemon first
finishes the packet on the air, which takes up to 7 min 40 s at SF12, 7.8 kHz and CR 4/8; 90 s
would kill it in the middle of such a packet (see [How long a stop takes](#how-long-a-stop-takes)
and [When the daemon is killed](#when-the-daemon-is-killed)). At common settings the daemon still
stops within seconds: the timeout only limits how long a daemon that does not exit can hold up a
stop or a reboot. If your hosts never use such slow settings, you can lower it in a drop-in
(`sudo systemctl edit rnode-nomu`) to a little more than the table's time for the slowest
modulation they do use.

The unit's file system is read-only apart from `/run/rnode` and `/var/lib/rnode`, so keep the
symlink, the settings file, the EEPROM file and any `--spilog` file in those directories, for example
`ExecStart=/usr/local/bin/rnode-nomu --config /etc/rnode-nomu.toml --spilog /var/lib/rnode/spi.csv`.

### Running rnsd as a service

Reticulum ships no unit file;
[its manual](https://reticulum.network/manual/using.html#systemwide-service) shows one to write
yourself. The [Quick start](../README.md#quick-start) follows it, with these choices:

- Reticulum goes into a virtual environment of its own, `/opt/reticulum`, owned by root:
  Raspberry Pi OS and Debian refuse `pip install` into the system's Python, and `rnsd` cannot
  change its own code.
- `rnsd` runs as its own account, `rnsd`. Its config, identity and caches live in
  `/var/lib/rnsd/.reticulum`, which `--config` names, so `rnsd` depends neither on `$HOME` nor on
  whether `/etc/reticulum` exists.
- Reticulum's manual starts `rnsd --service`, which logs to `logfile` in the config directory.
  Without `--service`, `rnsd` logs to standard output, which systemd sends to the journal, and
  `PYTHONUNBUFFERED=1` makes each line arrive as it is logged.
- Reticulum's tools need `rnsd`'s RPC key to query it. Run them as `rnsd` with the same
  `--config`, for example
  `sudo -u rnsd /opt/reticulum/bin/rnstatus --config /var/lib/rnsd/.reticulum`, or give your
  account the key as in [Using rnsd from your own account](#using-rnsd-from-your-own-account).

Upgrade Reticulum with `sudo /opt/reticulum/bin/pip install --upgrade rns`, then
`sudo systemctl restart rnsd`. `pip` never touches `rnsd.service`, which is a file you wrote.

### Starting rnsd after the daemon

The order is not essential. If `rnsd` starts first, `RNodeInterface` logs
`Could not open serial port` and tries again every 5 seconds until the PTY appears. With the unit
from Reticulum's manual (`After=multi-user.target`) and rnode-nomu enabled, `rnsd` already starts
after the daemon at boot, because `multi-user.target` waits for the units it starts.

To make the order explicit, give `rnsd` a drop-in, as the [Quick start](../README.md#quick-start)
does. `systemctl edit` opens an editor for one:

```sh
sudo systemctl edit rnsd
```

```ini
[Unit]
Wants=rnode-nomu.service
After=rnode-nomu.service
```

systemd saves these lines as `/etc/systemd/system/rnsd.service.d/override.conf` and reloads
itself; `rnsd.service` stays as it was. `Wants=` starts the daemon with `rnsd`, and `After=` makes
`rnsd` wait for it.

The drop-in does not stop the daemon when `rnsd` stops. To run the daemon only while `rnsd` runs,
tie it to `rnsd` from its own side instead, and take it out of the boot targets:

```sh
sudo systemctl edit rnode-nomu
```

```ini
[Unit]
Before=rnsd.service
PartOf=rnsd.service
```

```sh
sudo systemctl disable rnode-nomu
sudo systemctl add-wants rnsd.service rnode-nomu.service
```

Starting `rnsd` then starts the daemon first, and stopping or restarting `rnsd` stops or restarts
the daemon too; `rnsd` needs no drop-in of its own. Run `disable` before `add-wants`:
`systemctl disable rnode-nomu` removes every link to the unit, the one `add-wants` makes included.
With [`power_off_on_exit = true`](configuration.md#gpio), the AIO V2's LoRa rail is then off while
`rnsd` is not running, and every restart of `rnsd` power-cycles the modem.

This only works if `rnsd` is a system service. A user service (`systemctl --user`) cannot be
ordered against a system unit or tied to one; only `RNodeInterface`'s retry applies.

### Using rnsd from your own account

`rnsd` is Reticulum's shared instance: it opens the interfaces, and every other Reticulum program
on the machine, such as Nomad Network, connects to it over a local socket and uses them. Each
program has identities and addresses of its own; Nomad Network keeps its identity in
`~/.nomadnetwork/storage/identity`, so back that file up. `rnsd` only relays the programs'
packets, which are encrypted to their addresses.

- **Any local account can use the radio.** On Linux the shared instance listens on an abstract
  UNIX socket (`rns/default`), which has no file permissions, so every program on the machine can
  connect and send and receive through `rnsd`. With `shared_instance_type = tcp` it listens on
  `127.0.0.1` instead, which is just as open.
- **Querying `rnsd` needs its RPC key.** Status, interface statistics and path tables go over RPC,
  authenticated with a key that each instance derives from its own config directory unless `rpc_key`
  is set. A program under another account gets `Could not get RNS status` from `rnstatus`. Nomad
  Network 1.4.4's interface exits at startup with `digest sent was rejected`, while its daemon mode
  (`nomadnet --daemon`) runs without the key. Reticulum reads `rpc_key` only from the `[reticulum]`
  section of each instance's own config, so step 5 of the [Quick start](../README.md#quick-start)
  writes the same key into `rnsd`'s config and yours. If `rnsd`'s config already has a `[reticulum]`
  section, set `rpc_key` there instead of appending a second section, with which `rnsd` does not
  start.
- **Group `rns` controls who can read the key.** `/etc/reticulum-rpc.key` is readable by root and
  group `rns`. To let another account use `rnsd`, add it with `sudo usermod -aG rns <account>`;
  after logging in again, that account runs the three lines of step 5 that set up `~/.reticulum`,
  with `cat` instead of `sudo cat`. Taking an account out of the group does not revoke a key it
  has already copied: to revoke access, write a new key to the file, `rnsd`'s config and every
  account's config, then restart `rnsd`. The commands pass the key through pipes, never as a
  command argument, which other local users could read in the process list.
- **Your config needs an `[interfaces]` section,** even an empty one: without it Nomad Network
  1.4.4's interface exits with `KeyError: 'interfaces'`. Your programs use `rnsd`'s interfaces, so
  the section can stay empty.
- **Start programs only while `rnsd` runs.** The first Reticulum program to start becomes the
  shared instance. If one starts before `rnsd`, `rnsd` logs
  `connected to another shared local instance, this is probably NOT what you want!` and does not
  open the radio. Quit the program, then run `sudo systemctl restart rnsd`. `rnsd` also answers
  queries only once its interfaces are up, a few seconds after it starts: until then `rnstatus`
  reports `Could not get RNS status`, and Nomad Network's interface exits with
  `Connection refused`.

### Keeping local changes across updates

Installing `contrib/rnode-nomu.service` from a newer version replaces the file in
`/etc/systemd/system`, and every change made in it, as an update of the Debian package replaces its
unit in `/usr/lib/systemd/system`. Drop-ins, in `/etc/systemd/system/rnode-nomu.service.d/`, and the
links that `systemctl enable` and `systemctl add-wants` make are separate files and stay. So keep
local changes, such as `SupplementaryGroups=` or a lower `TimeoutStopSec=`, in a drop-in. To update
a Debian package installation, install the newer package
([Debian package](installing.md#debian-package)). To update an installation from source:

```sh
cd rnode-nomu
git pull
cargo build --release --locked -p rnode-nomu
sudo install -m 0755 target/release/rnode-nomu /usr/local/bin/rnode-nomu
sudo install -m 0644 contrib/rnode-nomu.service /etc/systemd/system/rnode-nomu.service
sudo systemctl daemon-reload
sudo systemctl restart rnode-nomu
```

`install` replaces the binary while the old one still runs, and the restart switches to the new one.
The restart first finishes the packet on the air ([How long a stop takes](#how-long-a-stop-takes)).
`rnsd` then logs a serial port error for the old PTY (`[Errno 5] Input/output error`, and that the
interface is offline), and about 5 seconds later `Reconnected port for RNodeInterface[…]`, once it
has reopened the new PTY and configured the radio again ([Running with Reticulum](reticulum.md)).
`/etc/rnode-nomu.toml` is your copy: compare it with the example configs when you update.
