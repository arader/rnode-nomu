# Development

How to try rnode-nomu without radio hardware, how the repository is laid out, and how it is tested:
the unit tests, the Reticulum end-to-end test, coverage and the hardware test tools.
[AGENTS.md](../AGENTS.md) has the rules for changes, and [Radio drivers](drivers.md) explains how
to add support for a modem.

## Quick start without hardware

The `sim` driver replaces the modem with a simulated one, so you can try the daemon on any Linux
or macOS machine. Build it:

```sh
cargo build --release --locked -p rnode-nomu
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
target/release/rnode-nomu --config sim-a.toml &
target/release/rnode-nomu --config sim-b.toml &
```

Each daemon logs its PTY and creates the symlink:

```text
[2026-10-13T19:23:46Z INFO  rnode_nomu] loaded config from sim-a.toml
[2026-10-13T19:23:46Z INFO  rnode_nomu] PTY slave: /dev/pts/1
```

Now point two Reticulum instances at `target/rnode-a` and `target/rnode-b`, as described in
[Running with Reticulum](reticulum.md), with the same radio parameters on both. Give each instance
its own config directory (`rnsd --config ./rns-a`) with `share_instance = No`, so that the second
one does not attach to the first. The daemons log when Reticulum turns the radio on, and every
packet at `debug`; here daemon A sends and daemon B receives:

```text
[2026-10-13T19:23:48Z INFO  rnode_core::node] radio online (host request): 914.875 MHz, 125 kHz, SF7, CR 4/5, 2 dBm
[2026-10-13T19:23:53Z DEBUG rnode_core::node] TX packet: 255 bytes, 2 frames, header 0xB1 (seq 11, split): ok
[2026-10-13T19:23:53Z DEBUG rnode_core::node] RX packet: 255 bytes, 2 frames, header 0xB1 (seq 11, split), RSSI -70 dBm, SNR 7.00 dB
```

Instead of Reticulum, [`tests/e2e/kiss_probe.py`](../tests/e2e/kiss_probe.py) can configure a daemon
and send test packets; see [Simulation and tests](#simulation-and-tests). Stop the daemons with
`kill %1 %2`: each puts its radio to sleep and removes its symlink and socket.

## Workspace layout

| Path | Contents |
| --- | --- |
| [`crates/rnode-core`](../crates/rnode-core) | Hardware-independent RNode logic: KISS host protocol, TX queue, split framing, PHY timing, CSMA/CA, the `Node<R, S>` state machine, and the `Radio` and `Storage` traits. No I/O; time and randomness are passed in, so tests are deterministic. |
| [`crates/rnode-sx126x`](../crates/rnode-sx126x) | SX1262 driver over `embedded-hal` 1.0 that mirrors RNode_Firmware `sx126x.cpp`. |
| [`crates/rnode-sim`](../crates/rnode-sim) | Simulated LoRa radio. Daemons on one host share an "air" directory of Unix datagram sockets. |
| [`crates/rnode-nomu`](../crates/rnode-nomu) | The daemon: config, GPIO character device, spidev, PTY, saved settings and event loop. Its [`debian`](../crates/rnode-nomu/debian) directory holds the Debian package's unit, account and maintainer script. |
| [`crates/xtask`](../crates/xtask) | Repository automation: `cargo xtask rns-e2e` runs two simulated daemons under two Reticulum instances. |
| [`tests/e2e/kiss_probe.py`](../tests/e2e/kiss_probe.py) | Standalone KISS probe for a daemon PTY. |
| [`tools/hwtest`](../tools/hwtest) | Hardware bring-up for the uConsole AIO V2, RTL-SDR capture, LoRa burst analysis and an over-the-air LXMF echo test against a real RNode. Outside the Cargo workspace. |
| [`config.example.toml`](../config.example.toml), [`config.uconsole-aio-v2.toml`](../config.uconsole-aio-v2.toml), [`config.uconsole-aio-v1.toml`](../config.uconsole-aio-v1.toml) | Example daemon configurations. |
| [`contrib/rnode-nomu.service`](../contrib/rnode-nomu.service) | Hardened systemd unit. |
| [`release-plz.toml`](../release-plz.toml), [`.github/workflows/release.yml`](../.github/workflows/release.yml), `crates/*/CHANGELOG.md` | Versions, changelogs and releases: see [Releases and versions](releases.md). |
| [`README.md`](../README.md) and `docs/` | The summary and quick start, and the guides it links. |
| [`docs/drivers.md`](drivers.md) | How radio drivers work and how to add one. |
| [`AGENTS.md`](../AGENTS.md) | Rules for changing this repository. |

## Simulation and tests

### The simulated radio

`radio.driver = "sim"` replaces the SX1262 with [`rnode-sim`](../crates/rnode-sim). Each daemon
binds a Unix datagram socket `<node_name>.sock` in `air_dir`. A transmission is sent to every other
socket in that directory as two datagrams: a carrier-start record and, after the LoRa time on air,
the frame itself. A simulated receiver gets the frame only if it was listening for the whole frame,
was not transmitting, and uses the same frequency, bandwidth, spreading factor and low-data-rate
optimisation as the sender. Overlapping transmissions collide and are lost. So the simulator catches
the timing, CSMA/CA and modulation mismatches that a plain loopback would hide. `loss` and
`crc_error` in [`[radio.sim]`](configuration.md#radiosim) make a radio lose good frames, or receive
them with a failed CRC, at random.

The simulator works without root or radio hardware, on Linux and macOS, and in containers. Every
daemon pointed at the same `air_dir` hears the others.

### Tests

CI runs these eight checks on every pull request, in this order:

```sh
cargo fmt --all --check
cargo build --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
python3 -m unittest discover -s tools/hwtest -p 'test_*.py' -v
cargo test --manifest-path tools/hwtest/sx126x-bringup/Cargo.toml --locked
uv venv --clear --python python3 target/hwtest-venv &&
  uv pip install --python target/hwtest-venv/bin/python --require-hashes --no-build \
    -r tools/hwtest/requirements-test.txt &&
  HWTEST_REQUIRE_NUMPY=1 target/hwtest-venv/bin/python -m unittest discover -s tools/hwtest -p 'test_*.py' -v
cargo xtask rns-e2e --fetch-reticulum --scenario sf7
```

The three `uv` lines are one check: CI makes the venv in a fresh temporary directory, while
`--clear` here replaces the one an earlier run left. `--no-build` makes a missing numpy wheel fail
the install instead of compiling numpy from source.
The last one is the SF7 scenario of the [Reticulum end-to-end test](#reticulum-end-to-end-test).
After every merge, CI on `main` ([`.github/workflows/ci.yml`](../.github/workflows/ci.yml)) runs the
same steps with every scenario of that test, so every commit on `main` is checked with all of them.

- `rnode-core` checks the KISS protocol, framing, CSMA/CA, PHY timing and logging against values
  derived from the firmware, and the EEPROM image `rnodeconf` reads against Python's MD5. It also
  replays the bytes recorded from `rnodeconf` 1.5.5 sessions, `--rom` signing an unprovisioned
  EEPROM included, and checks the replies.
- `rnode-sx126x` checks the exact SPI bytes the driver sends against a mocked bus.
- `rnode-sim` runs simulated radios against each other.
- `rnode-nomu` covers config parsing, storage, the PTY and its symlink, the GPIO chip lookup, the
  SPI recorder, the command line and the event loop, including shutdown during a TX burst, and
  checks that the systemd unit's `TimeoutStopSec=` covers the slowest packet, computed with
  rnode-core's airtime. Its `docs_examples` test parses every daemon config in the README and the
  guides in `docs/` with the real config loader, and checks the option names of their Reticulum
  examples, their relative links, and that the README links every guide, so the documentation
  cannot drift from the code.
- The smoke test
  [`crates/rnode-nomu/tests/sim_daemon_smoke.rs`](../crates/rnode-nomu/tests/sim_daemon_smoke.rs)
  starts two daemon processes on the simulated radio. It checks that KISS data frames of 1, 254, 255
  and 508 bytes travel between their PTYs in both directions, that a 509-byte frame is dropped, and
  that each daemon's EEPROM image has the serial number derived for it. It is part of `cargo test`,
  so it runs in CI.
- The unit tests in [`tools/hwtest`](../tools/hwtest) check the hardware test tools without
  hardware or Reticulum. For the over-the-air LXMF echo test they cover the frequency and power
  limits, the arguments, how it describes packets, matches them to the daemon's debug log and
  fails the run when one is missing there or failed to transmit, and, with stand-ins for Reticulum
  and LXMF, path discovery, the identity file and the echo verdicts. For the capture analyzer they
  check the Semtech time on air and the low-data-rate rule against the recorded hardware results,
  and, on synthetic captures, burst detection, dechirping, the frequency offset and the report.
  For the bring-up tool they check the frequency and power limits and the time on air. The Python
  tests need only the standard library, except the analyzer's signal-processing tests, which need
  numpy: CI runs the Python tests once without numpy, which skips those, and once with the numpy
  release that `tools/hwtest/requirements-test.txt` pins. The bring-up tool's tests build only on
  Linux. [`tools/hwtest/README.md`](../tools/hwtest/README.md) says how to run them all.

### Reticulum end-to-end test

`cargo xtask rns-e2e` runs real Reticulum against simulated daemons. CI runs it: every pull
request runs its SF7 scenario, about a minute, and `main` runs every scenario, about six minutes,
after each merge. It needs [uv](https://docs.astral.sh/uv/) and git. To run what a pull request
runs:

```sh
cargo xtask rns-e2e --fetch-reticulum --scenario sf7
```

- `--fetch-reticulum` tests the Reticulum commit CI tests, pinned in `crates/xtask` (1.5.5,
  `e40191b`): it fetches that commit into `target/rns-e2e` and checks it. Without it, the harness
  uses the checkout given with `--reticulum PATH`, else `$RNODE_RETICULUM`, else `../Reticulum`
  next to the repository, and warns if that checkout is not at the pinned commit.
- Without `--scenario` it runs every scenario, as `main` does. `--scenario sf11` runs the slow one
  alone; `--keep-logs` keeps the logs of a passing run.
- It builds the daemon, starts two instances on a private air directory under `target/rns-e2e`, and
  installs `pyserial` and `cryptography` into a virtual environment, at the exact versions and
  hashes in [`tests/e2e/requirements.txt`](../tests/e2e/requirements.txt).
- At 125 kHz with SF7, and with SF11, where the firmware's low-data-rate rule and the textbook
  one disagree, it first probes the KISS boundary sizes (1, 254, 255, 508 and 509 bytes) in both
  directions. Then two Reticulum processes discover each other, establish a link, exchange packets
  and transfer a 4 KiB resource.
- The `rnodeconf` scenario, about 40 s, runs Reticulum's [`rnodeconf`](reticulum.md#using-rnodeconf)
  against the daemons, with its `HOME` in the run directory, so your own `~/.config/rnodeconf` is
  never read or changed: `--info`, `--tnc`, which must save the settings file, and `--normal`, which
  must delete it, against the built-in EEPROM image; then `--key` and `--rom` against a daemon with
  `[storage] eeprom`, after which `--info` must show "Validated - Local signature".
- When a scenario fails, it prints the last 100 lines of each daemon, Reticulum and rnodeconf log,
  so a CI log shows the cause. The logs stay in `target/rns-e2e`, and with `--keep-logs` also after
  a pass.

[`crates/xtask/README.md`](../crates/xtask/README.md) has the details, typical timings, and how to
move the pinned Reticulum, Python packages and uv.
[`tests/e2e/kiss_probe.py`](../tests/e2e/kiss_probe.py) also works on its own against any daemon
PTY, real hardware included:

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

The workspace defines three aliases in [`.cargo/config.toml`](../.cargo/config.toml):

| Command | Runs | Output |
| --- | --- | --- |
| `cargo coverage-summary` | `cargo llvm-cov` | Per-file summary in the terminal |
| `cargo coverage` | `cargo llvm-cov --html` | HTML report in `target/llvm-cov/html/` |
| `cargo coverage-lcov` | `cargo llvm-cov --lcov --output-path lcov.info` | `lcov.info` for editors and CI tools (git-ignored) |

The aliases accept further `cargo llvm-cov` options, e.g. `cargo coverage -p rnode-core --open`.

## Hardware test tools

[`tools/hwtest`](../tools/hwtest) holds the checks used to validate the SX1262 driver on the
uConsole AIO V2, outside the Cargo workspace (CI runs only their [unit tests](#tests)):

- `sx126x-bringup` runs the driver directly, without the daemon: it resets and detects the modem,
  configures it, samples RSSI and carrier detect, transmits test frames and times them against the
  Semtech time-on-air formula, and checks the low-data-rate boundaries. It refuses frequencies
  outside 902–928 MHz and powers above 10 dBm.
- `bin/soapysdr_capture.c` records IQ samples from the AIO V2's RTL-SDR.
- `lora_capture_analyze.py` finds LoRa bursts in a capture and checks duration, occupied bandwidth,
  preamble, sync word and frequency offset.
- `lxmf_echo_ota.py` sends LXMF messages over the air through the daemon to an echo peer behind a
  real RNode, checks the echoes, and matches every packet to the daemon's debug log. It applies
  the same frequency and power limits.

Build the bring-up tool on the target:

```sh
cargo build --manifest-path tools/hwtest/sx126x-bringup/Cargo.toml
tools/hwtest/sx126x-bringup/target/debug/sx126x-bringup --mode bringup
```

[`tools/hwtest/README.md`](../tools/hwtest/README.md) has the full procedure and the recorded
results.
