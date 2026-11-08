# rnode-noµ

rnode-noµ (written `rnode-nomu`, pronounced "R-node no-mew") is an RNode with no µC: it turns a
Linux computer with a LoRa modem on its SPI bus into an [RNode](https://unsigned.io/rnode/), with
no microcontroller in between. The daemon drives the modem directly and exposes a pseudo-terminal
(PTY) that speaks the RNode KISS host protocol. Reticulum's `RNodeInterface`, or any other RNode
host, uses that PTY exactly like the serial port of a USB-attached RNode running RNode_Firmware. On
the air, rnode-nomu interoperates with real RNodes.

The supported modem is the Semtech SX1262, optionally with a power amplifier after it (not yet
tested on such hardware). The reference platform is the Clockwork uConsole (CM5) with the
HackerGadgets AIO V2 extension board. A simulated radio lets you run everything, including
Reticulum end-to-end tests, on any Linux or macOS machine without radio hardware.

Behaviour follows [RNode_Firmware](https://github.com/markqvist/RNode_Firmware) v1.86 (commit
`56775c5`) wherever a host or another radio can observe it. The deliberate differences are listed in
[Differences from RNode_Firmware](docs/how-it-works.md#differences-from-rnode_firmware).

## Quick start

These commands set up the reference uConsole with the AIO V2 on 64-bit Raspberry Pi OS or Debian 13.
rnode-nomu comes from the Debian package of the latest
[release](https://github.com/arader/rnode-nomu/releases). It and Reticulum's `rnsd` run as systemd
services under their own accounts, rnode-nomu starts first, and Nomad Network runs under your own
account. [Running as a systemd service](docs/running.md#running-as-a-systemd-service) explains each
step, and its subsection
[Using rnsd from your own account](docs/running.md#using-rnsd-from-your-own-account) explains
step 5. On an AIO V1, install `config.uconsole-aio-v1.toml` in step 2 instead of the V2 file (see
[the AIO V1 notes](docs/hardware.md#uconsole-with-the-hackergadgets-aio-v1)). To build rnode-nomu
from source instead, see [Building from source](docs/installing.md#building-from-source). For
another board, adapt the config as in [Setting up the LoRa hardware](docs/hardware.md); without
hardware, see [Quick start without hardware](docs/development.md#quick-start-without-hardware).

First enable SPI1 as in
[step 1 of the uConsole setup](docs/hardware.md#uconsole-cm5-with-the-hackergadgets-aio-v2) and
check that `/dev/spidev1.0` and `/dev/gpiochip15` exist.

**1. Install `rnsd`** into its own Python environment:

```sh
sudo apt update
sudo apt install -y python3-venv
sudo python3 -m venv /opt/reticulum
sudo /opt/reticulum/bin/pip install rns
```

**2. Install rnode-nomu** and its config for the AIO V2. The package installs the daemon and its
service, neither enabled nor started yet, and creates the `rnode` account that runs the daemon, in
the groups `spi` and `gpio` for the devices and `dialout` for the PTY. The config's `power = 16`
makes the daemon switch on the AIO V2's LoRa power rail, GPIO16, when it starts; `config.txt`
switches it off at boot.

```sh
sudo apt install -y curl
curl -fL -o /tmp/rnode-nomu_arm64.deb \
  https://github.com/arader/rnode-nomu/releases/latest/download/rnode-nomu_arm64.deb
sudo apt install -y /tmp/rnode-nomu_arm64.deb
sudo install -m 0644 /usr/share/doc/rnode-nomu/examples/config.uconsole-aio-v2.toml /etc/rnode-nomu.toml
id rnode
```

`id rnode` lists the groups `spi`, `gpio` and `dialout`. The package adds the account only to
groups that exist; if `spi` or `gpio` is missing, give it access to the devices as in
[Service account and device access](docs/running.md#service-account-and-device-access).

**3. Create the `rnsd` account,** which runs Reticulum. It and `rnode` are both in `dialout`, and
the daemon hands its PTY to that group:

```sh
sudo useradd --system --create-home --home-dir /var/lib/rnsd --shell /usr/sbin/nologin rnsd
sudo usermod -aG dialout rnsd
sudo sed -i -e 's/^# group = "dialout"/group = "dialout"/' -e 's/^# mode = "0660"/mode = "0660"/' /etc/rnode-nomu.toml
grep -E '^(power|group|mode) =' /etc/rnode-nomu.toml
```

The last command prints `power = 16`, `group = "dialout"` and `mode = "0660"`. Then give `rnsd` a
Reticulum config with the PTY as its RNode. The radio settings are lab examples for the US
902–928 MHz band: use values that are legal where you are and that match the RNodes you want to
reach (see [Running with Reticulum](docs/reticulum.md)).

```sh
sudo -u rnsd mkdir -p /var/lib/rnsd/.reticulum
sudo -u rnsd tee /var/lib/rnsd/.reticulum/config >/dev/null <<'EOF'
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
EOF
```

**4. Configure systemd** to start rnode-nomu first, then `rnsd`. The `rnsd` unit follows
[Reticulum's manual](https://reticulum.network/manual/using.html#systemwide-service) but logs to
the journal. The ordering is a drop-in, a separate file, so it stays when you change `rnsd.service`:

```sh
sudo tee /etc/systemd/system/rnsd.service >/dev/null <<'EOF'
[Unit]
Description=Reticulum Network Stack Daemon
After=multi-user.target

[Service]
Type=simple
Restart=always
RestartSec=3
User=rnsd
Environment=PYTHONUNBUFFERED=1
ExecStart=/opt/reticulum/bin/rnsd --config /var/lib/rnsd/.reticulum

[Install]
WantedBy=multi-user.target
EOF
sudo mkdir -p /etc/systemd/system/rnsd.service.d
printf '[Unit]\nWants=rnode-nomu.service\nAfter=rnode-nomu.service\n' | sudo tee /etc/systemd/system/rnsd.service.d/rnode-nomu.conf >/dev/null
sudo systemctl daemon-reload
sudo systemctl enable --now rnode-nomu rnsd
```

**Check it:**

```sh
pinctrl get 16
sudo journalctl -b -u rnode-nomu -u rnsd --no-pager
sudo -u rnsd /opt/reticulum/bin/rnstatus --config /var/lib/rnsd/.reticulum
```

`pinctrl get 16` reports `op dh` once the daemon has switched the rail on. In the journal,
rnode-nomu logs `radio online (host request): 914.875 MHz, 125 kHz, SF8, CR 4/5, 2 dBm` and `rnsd`
logs `RNodeInterface[rnode-nomu] is configured and powered up`. `rnstatus` lists the interface as
up.

**5. Run Nomad Network under your own account.** It reaches the radio through `rnsd`. To query
`rnsd` as well, which Nomad Network's interface does at startup, a program needs `rnsd`'s RPC key.
These commands keep the key in a file that only group `rns` can read, and give it to `rnsd` and to
your account. They replace `~/.reticulum/config`, and expect `rnsd`'s config to have no
`[reticulum]` section yet, as step 3 left it.

```sh
sudo /opt/reticulum/bin/pip install nomadnet
sudo groupadd --system rns
sudo install -m 0640 -o root -g rns /dev/null /etc/reticulum-rpc.key
python3 -c 'import secrets; print(secrets.token_hex(32))' | sudo tee /etc/reticulum-rpc.key >/dev/null
{ printf '\n[reticulum]\n  rpc_key = '; sudo cat /etc/reticulum-rpc.key; } | sudo -u rnsd tee -a /var/lib/rnsd/.reticulum/config >/dev/null
sudo systemctl restart rnsd
sudo usermod -aG rns "$USER"
install -d -m 0700 ~/.reticulum
install -m 0600 /dev/null ~/.reticulum/config
{ printf '[reticulum]\n  rpc_key = '; sudo cat /etc/reticulum-rpc.key; printf '\n[interfaces]\n'; } > ~/.reticulum/config
until /opt/reticulum/bin/rnstatus; do sleep 2; done
/opt/reticulum/bin/nomadnet
```

The `until` line runs `rnstatus` as you until `rnsd` answers, a few seconds after its restart; until
then `rnstatus` reports `Could not get RNS status`. Then it lists `RNodeInterface[rnode-nomu]`. In
Nomad Network, `Ctrl-P` shows your own address and `Ctrl-N` starts a conversation. Start it only
while `rnsd` runs; see
[Using rnsd from your own account](docs/running.md#using-rnsd-from-your-own-account) for this and
for giving other accounts access.

## Documentation

- [Installing](docs/installing.md): the release files, the Debian package, updates, and building
  from source.
- [Setting up the LoRa hardware](docs/hardware.md): what the daemon needs from a board, the uConsole
  with the AIO V2 or V1, and mapping your own board.
- [Configuration reference](docs/configuration.md): every key of the config file, the checks at
  startup, and the saved radio settings.
- [Running the daemon](docs/running.md): the command line, stopping, and running rnode-nomu and
  `rnsd` as systemd services.
- [Running with Reticulum](docs/reticulum.md): the `RNodeInterface` settings, and `rnodeconf`.
- [Logging](docs/logging.md): what each log level shows, the warnings, and recording SPI traffic.
- [Troubleshooting](docs/troubleshooting.md): error messages and what to do about them.
- [How it works](docs/how-it-works.md): the layers, a packet's way through them, and the deliberate
  differences from RNode_Firmware.
- [Development](docs/development.md): trying rnode-nomu without hardware, the repository's layout,
  the tests, coverage and the hardware test tools.
- [Radio drivers](docs/drivers.md): the `Radio` trait, and how to add support for a modem.
- [Releases and versions](docs/releases.md): what the version numbers mean, and how releases are
  made.

## Why rnode-nomu

An RNode is usually a microcontroller board with a LoRa modem, connected to the computer running
Reticulum over USB serial or Bluetooth. Many single-board computers and handhelds already have a
LoRa modem wired straight to their SPI bus, with no microcontroller in between. rnode-nomu lets
that modem act as an RNode:

- **No extra hardware.** The Linux host drives the modem itself; there is no microcontroller, no
  firmware to flash and no USB serial link.
- **Unchanged host software.** Reticulum's `RNodeInterface` opens the PTY as if it were a serial
  RNode. Reticulum needs no patches or plugins.
- **Interoperability.** On-air framing, modulation parameters, sync word, preamble length,
  low-data-rate optimisation and CSMA/CA follow the firmware, so real RNodes hear rnode-nomu and
  rnode-nomu hears them.
- **Linux tooling.** Configuration lives in a TOML file, the daemon runs under systemd, logs go to
  the journal with optional per-packet detail, and an SPI recorder captures every modem transaction.
- **Testability.** The RNode logic is a pure Rust library tested against values derived from the
  firmware. A simulated radio runs the whole daemon in CI, under real Reticulum too.

rnode-nomu is not a Reticulum interface and not a new protocol: it is the modem side of the
existing RNode protocol, running on Linux instead of a microcontroller.

## Limitations

- **`rnodeconf` calls the device not verifiable,** unless you
  [sign it with your own key](docs/reticulum.md#signing-with-your-own-key): the EEPROM image
  rnode-nomu makes up carries no signature. rnodeconf works anyway;
  [Using rnodeconf](docs/reticulum.md#using-rnodeconf) lists what it can do.
- **Commands for microcontroller boards are ignored:** display, framebuffer, Bluetooth, Wi-Fi, LEDs,
  device signatures, firmware hashes and firmware updates.
- **SX1262 only.** Other modems need a driver; see [Radio drivers](docs/drivers.md).
- **One modem per daemon.** For several modems, run one daemon per modem, each with its own config,
  PTY and settings file.
- **TX power above 22 dBm needs a power amplifier, and that is untested.** Without one, TX power
  tops out at the SX1262's 22 dBm. [`[radio.pa]`](docs/configuration.md#radiopa) maps the host's
  power through a PA's gain table as the firmware does, up to its `max_output_dbm` (28 dBm for the
  Heltec V4's front ends), but no board with a PA has run it yet.
  [`max_tx_power_dbm`](docs/configuration.md#radio) sets a lower ceiling.
- **The firmware's board-specific PA control is not ported:** PA model detection and the Heltec
  V4's enable lines and interference rule; see [`[radio.pa]`](docs/configuration.md#radiopa).
- **One GPIO chip.** All of a modem's GPIO lines must be on the chip named by `[gpio] chip`.
- **Hardware support needs Linux;** other systems can only run the simulator.
- **One host at a time.** The PTY is a serial port, so only one program can use it.

## Safety and radio regulations

- **Transmissions are regulated.** Frequency, bandwidth, TX power, duty cycle and antenna gain must
  be legal where you operate. The examples in this repository use 914.875 MHz, 125 kHz and 2 dBm,
  which suits the 902–928 MHz ISM band of the US and some other countries in ITU region 2. They are
  illegal in other regions: Europe, for example, uses 863–870 MHz with duty-cycle limits.
- **rnode-nomu enforces no regional rules.** The host (Reticulum) chooses the frequency,
  bandwidth, spreading factor, coding rate and TX power over KISS, or they come from the saved
  settings file. The daemon only caps TX power at the SX1262's 22 dBm, or with a power amplifier
  at its [`max_output_dbm`](docs/configuration.md#radiopa), or at a lower ceiling you set with
  [`[radio] max_tx_power_dbm`](docs/configuration.md#radio). Reticulum's
  `airtime_limit_short` and `airtime_limit_long` options can help you stay within a duty cycle.
- **A power amplifier needs `[radio.pa]`.** With an amplifier after the SX1262, the host's TX power
  is the power after the amplifier, and [`[radio.pa]`](docs/configuration.md#radiopa) tells the
  daemon how much the amplifier adds. Without it, or with a wrong gain table, the daemon drives the
  amplifier as if it were the antenna, and the board puts out up to the amplifier's gain more than
  the host asked for. Support for amplifiers is untested on hardware: measure the output into a
  dummy load before you transmit through one.
- **Attach an antenna or a dummy load before transmitting.** Transmitting into an open RF port can
  damage the power amplifier.
- **Start with low power** (the examples use 2 dBm) and raise it only as far as you need and are
  allowed. `max_tx_power_dbm` keeps any host from going higher.

## How this was made

Most of rnode-nomu's code, tests and documentation were written by AI coding agents
(GitHub Copilot), following the rules in [AGENTS.md](AGENTS.md) and using RNode_Firmware and
Reticulum's `RNodeInterface` as references. From October 2026, changes went through pull requests:
CI ran on each, and AI review agents (code, security and QA) reviewed them before they were
merged. The first commits carry `Co-authored-by: Copilot` trailers; the later ones are
squash-merged pull requests whose messages are the agents' own descriptions of the change and how
it was tested.

Throughout development I have tested rnode-nomu on a ClockworkPi uConsole running a Raspberry Pi
CM5 with the HackerGadgets AIO V2 add-on. With this hardware I've been communicating with a 2nd
Raspberry Pi connected to a Heltec v3. Each set of changes was reviewed with an agent's assistance.
Decisions were made to promote sane layering of concerns through Rust crates, such that different
layers can be swapped out. Different drivers, SPI libraries, and more, are all meant to be
replaceable for maximum compatibility.

The project started on a private Forgejo instance, under the name rnode-linux, and moved here as
rnode-nomu in November 2026. Before the move, its history was rewritten to replace private
hostnames, and to give every commit one author and committer: Andrew Rader, with GitHub's no-reply
address. Commit dates, co-author trailers and code are otherwise unchanged, and so are commit
messages, apart from the IDs of the commits they cite. References such as `#3` or `(#94)` in older
commit messages, code comments and documentation point to issues and pull requests on that
instance, not to this repository.

## Contributing

Changes go through pull requests on GitHub, and CI runs the eight
[checks](docs/development.md#tests) on each, the last of them the SF7 Reticulum end-to-end test;
`main` gets every scenario after the merge. [AGENTS.md](AGENTS.md) has the rules: test layout,
firmware references, and tests for every behaviour change. A pull request's title becomes its
changelog entry ([Releases and versions](docs/releases.md)). New radio drivers start with
[Radio drivers](docs/drivers.md).

## License

rnode-nomu is free software under the GNU General Public License, version 3 or (at your option)
any later version: see [LICENSE](LICENSE).

Much of it is a Rust translation of [RNode_Firmware](https://github.com/markqvist/RNode_Firmware):
`rnode-core` follows the firmware's main program and utilities (Copyright (C) 2024, Mark Qvist,
GPL-3.0-or-later), and `rnode-sx126x` follows its SX126x modem driver (Copyright Sandeep Mistry,
Mark Qvist and Jacob Eva, MIT license). [NOTICE.md](NOTICE.md) lists what comes from where, with
the notices those licenses require.
