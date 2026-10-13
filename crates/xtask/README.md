# RNS end-to-end harness

`cargo xtask rns-e2e` checks rnode-linux against Reticulum itself. Run it with:

```sh
cargo xtask rns-e2e --reticulum ../Reticulum
```

If `--reticulum` is omitted, the harness uses `$RNODE_RETICULUM` when set, otherwise `../Reticulum`
relative to the repository root. A relative `--reticulum` or `$RNODE_RETICULUM` is taken from the
directory you run the command in. The harness builds `rnode-linux`, creates an isolated run
directory under `target/rns-e2e`, starts two daemon instances with `radio.driver = "sim"` on a
shared simulated air directory and distinct PTYs, creates a `uv` virtual environment with
`pyserial` and `cryptography`, and runs two separate Reticulum Python processes using
`RNodeInterface`.

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
CHTM/PHYPRM stats the interface exposes. On failure the harness leaves its logs in
`target/rns-e2e`; `--keep-logs` keeps them after a pass too.

A run takes about five minutes, almost all of it SF11 airtime and CSMA backoff. Typical timings
(914.875 MHz, 2 dBm, CR 4/5):

| Scenario | Path      | Link      | Echo RTT (1, 64, 240, 152 B) | 4 KiB resource |
| -------- | --------- | --------- | ---------------------------- | -------------- |
| SF7      | 0.9–1.2 s | 0.9–1.1 s | 0.9–2.3 s                    | 12 s           |
| SF11     | 5–7 s     | 8–10 s    | 9–16.5 s                     | 94–96 s        |

Prerequisites:

- `uv` on `$PATH` (or set `$UV`)
- the Reticulum checkout at `../Reticulum` (or pass `--reticulum PATH`, or set `$RNODE_RETICULUM`)

## In CI

CI has no Python, `uv` or Reticulum, so this harness is run by hand. CI does run its Rust-only
counterpart, `crates/rnode-linux/tests/sim_daemon_smoke.rs`, as part of `cargo test`: two
daemons on the simulated radio carry 1, 254, 255 and 508-byte KISS data frames in both
directions and silently drop a 509-byte frame.

## Standalone KISS probe

`tests/e2e/kiss_probe.py` can be run against a real daemon PTY. Attach an antenna, and use a
frequency and power legal in your region:

```sh
python3 tests/e2e/kiss_probe.py --port /path/to/pty --frequency 914875000 \
  --bandwidth 125000 --txpower 2 --sf 7 --cr 5 --sizes 1,254,255,508,509 --count 2
```

It uses only the Python standard library plus `pyserial`, prints every decoded KISS reply (including FW/platform/MCU, PHYPRM, CHTM and CSMA stats), sends configurable data frame sizes, prints final stats, and turns the radio off before exiting. `--config-commands` also sends `CMD_CONF_SAVE`, `CMD_CONF_DELETE` and `CMD_RESET`; that leaves the daemon without saved radio settings.
