# RNS end-to-end harness

`cargo xtask rns-e2e` checks rnode-linux against Reticulum itself. CI runs it: pull requests run
the SF7 scenario and main runs every scenario (see [In CI](#in-ci)). Run what a pull request runs
with:

```sh
cargo xtask rns-e2e --fetch-reticulum --scenario sf7
```

Without `--scenario` it runs every scenario, as CI does on main. The harness builds `rnode-linux`,
creates an isolated run directory under `target/rns-e2e`, starts two daemon instances with
`radio.driver = "sim"` on a shared simulated air directory and distinct PTYs, and runs two separate
Reticulum Python processes using `RNodeInterface`.

Options:

- `--scenario NAME` runs only the named scenario, `sf7` or `sf11`; repeat it to run several. They run
  in that order whatever order they are named in. An unknown name is an error that lists the valid
  ones.
- `--fetch-reticulum` tests the Reticulum commit CI tests (see [Pins](#pins)). It fetches that
  commit into `target/rns-e2e/reticulum-e40191b3d193` with a shallow `git fetch`, retried three
  times with backoff when it fails, and checks that the checkout's `HEAD` is the pinned commit.
  Later runs reuse the checkout after the same check, which also refuses a checkout with local
  changes. It cannot be combined with `--reticulum`.
- `--reticulum PATH` tests a Reticulum checkout of your own. Without it, and without
  `--fetch-reticulum`, the harness uses `$RNODE_RETICULUM` when set, otherwise `../Reticulum`
  relative to the repository root. A relative `--reticulum` or `$RNODE_RETICULUM` is taken from the
  directory you run the command in. When that checkout is not at the pinned commit, or is not a git
  checkout, the harness warns and carries on.
- `--keep-logs` keeps the logs after a pass too.

The harness installs its Python packages, `pyserial` and `cryptography` with their dependencies,
into a `uv` virtual environment in `target/rns-e2e/venv`, with
`uv pip sync --require-hashes tests/e2e/requirements.txt`: exact versions, each file checked
against its sha256 hash, and nothing the file does not list. It syncs the environment again
whenever that file changes.

It runs two scenarios at 125 kHz. SF7 is the common default. SF11 is the slow, long-range case,
still without low data rate optimisation (LDRO): firmware `sx126x::handleLowDataRate()` turns LDRO
on only when `(1 << sf) / (bw / 1000) > 16`, which at SF11/125 kHz is 2048 / 125 = 16 in integer
arithmetic, so LDRO starts at SF12/125 kHz. LDRO itself is covered by unit tests: rnode-core's
`phy` tests check the firmware's rule at its boundaries, and rnode-sx126x's goldens check the LDRO
bit it sends to the modem.

Each scenario first runs a direct KISS boundary probe: 1-byte and 254-byte single-frame payloads
and 255-byte and exactly 508-byte split payloads in both directions, and a silently dropped
509-byte over-MTU payload. It then checks Reticulum announce/path discovery, link establishment,
link packets in both directions, a 4 KiB `Resource` transfer, Reticulum's RSSI/SNR metadata and the
CHTM/PHYPRM stats the interface exposes.

When a scenario fails, the harness prints the last 100 lines of each of its logs to stderr: both
daemons' logs, the Reticulum server's and client's output, and any error file the Python scripts
left. A CI log therefore shows the cause without any artifacts. The logs stay in
`target/rns-e2e/runs`; `--keep-logs` keeps them after a pass too.

A run takes about five minutes, almost all of it SF11 airtime and CSMA backoff; SF7 alone takes
about a minute. Typical timings (914.875 MHz, 2 dBm, CR 4/5):

| Scenario | Path      | Link      | Echo RTT (1, 64, 240, 152 B) | 4 KiB resource |
| -------- | --------- | --------- | ---------------------------- | -------------- |
| SF7      | 0.8–1.3 s | 0.7–1.3 s | 0.8–2.3 s                    | 11.5–12 s      |
| SF11     | 5–7 s     | 8–10 s    | 9–17 s                       | 93–96 s        |

Prerequisites:

- `uv` on `$PATH` (or set `$UV`)
- `git`, for `--fetch-reticulum`; otherwise a Reticulum checkout at `../Reticulum` (or pass
  `--reticulum PATH`, or set `$RNODE_RETICULUM`)

## In CI

- **Pull requests:** `PR / test` (`.forgejo/workflows/pr.yml`) runs
  `cargo xtask rns-e2e --fetch-reticulum --scenario sf7` after `cargo test`. Together with
  installing `uv`, it adds about a minute to the job.
- **main:** `.forgejo/workflows/ci.yml` runs on every push to main, and on any branch from
  Actions → CI → Run workflow. It runs the same checks as `PR / test`, then every scenario with
  `cargo xtask rns-e2e --fetch-reticulum`. Runs on main queue up rather than cancel each other.

The CI image has Python 3.13 but no pip or `uv`. Both workflows install `uv` with the same
"Install uv" step: a pinned release, checked against its sha256 before use. They set
`UV_PYTHON_DOWNLOADS=never`, so the harness uses the image's Python and downloads nothing that is
not pinned, and `UV_LINK_MODE=copy`, because uv's cache and the workspace are on different
filesystems. `cargo test` also runs the harness's Rust-only counterpart,
`crates/rnode-linux/tests/sim_daemon_smoke.rs`: two daemons on the simulated radio carry 1, 254,
255 and 508-byte KISS data frames in both directions and silently drop a 509-byte frame.

## Pins

Everything the end-to-end test downloads is pinned:

- **Reticulum:** `RETICULUM_REPO` and `RETICULUM_COMMIT` in `crates/xtask/src/main.rs`, now
  `e40191b3d193b46b7f2d8a44424a594cd758839b` (1.5.5-1-ge40191b3, `RNS.__version__` 1.5.5). To move
  it, check the new commit out, run every scenario against it with `--reticulum`, then change
  `RETICULUM_COMMIT` and the version noted here. The checkout directory is named after the commit,
  so the next `--fetch-reticulum` run fetches the new one.
- **Python packages:** `tests/e2e/requirements.txt`, generated from `tests/e2e/requirements.in`.
  After changing `requirements.in`, or to move the packages to their newest releases with
  `--upgrade`, regenerate it from the repository root and run the harness:

  ```sh
  uv pip compile --universal --generate-hashes --python-version 3.13 [--upgrade] \
    tests/e2e/requirements.in -o tests/e2e/requirements.txt
  ```

  Without `--upgrade`, uv keeps the current pins where it can. A unit test checks that every
  package in the file is pinned with `==` and has sha256 hashes.
- **uv:** the `version` and the two `sha256` values in the "Install uv" step of
  `.forgejo/workflows/pr.yml` and `.forgejo/workflows/ci.yml`. To move it, take the hashes for
  `uv-x86_64-unknown-linux-gnu.tar.gz` and `uv-aarch64-unknown-linux-gnu.tar.gz` from the release's
  `sha256.sum` (`https://github.com/astral-sh/uv/releases/download/<version>/sha256.sum`), check
  them against `sha256sum` of the downloads, and change both workflows: a unit test checks that
  their steps are identical.

## Standalone KISS probe

`tests/e2e/kiss_probe.py` can be run against a real daemon PTY. Attach an antenna, and use a
frequency and power legal in your region:

```sh
python3 tests/e2e/kiss_probe.py --port /path/to/pty --frequency 914875000 \
  --bandwidth 125000 --txpower 2 --sf 7 --cr 5 --sizes 1,254,255,508,509 --count 2
```

It uses only the Python standard library plus `pyserial`, prints every decoded KISS reply (including FW/platform/MCU, PHYPRM, CHTM and CSMA stats), sends configurable data frame sizes, prints final stats, and turns the radio off before exiting. `--config-commands` also sends `CMD_CONF_SAVE`, `CMD_CONF_DELETE` and `CMD_RESET`; that leaves the daemon without saved radio settings.
