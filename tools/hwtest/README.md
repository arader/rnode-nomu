# uConsole AIO V2 hardware tests

This directory contains repeatable hardware checks for the uConsole CM5 with the
HackerGadgets AIO V2 board: SX1262 on `/dev/spidev1.0` and the on-board RTL-SDR.
The tests are intentionally outside the main Cargo workspace so they can depend
on Linux-only GPIO/SPI crates without affecting normal CI.

## Safety

The procedure is for the US 902-928 MHz ISM band only.  The bring-up tool refuses
frequencies outside that range and refuses TX power above 10 dBm.  Use short test
frames only; the commands below use 914.875 MHz at 2 dBm.

## SX1262 bring-up

Build on the uConsole (from an rsynced worktree):

```sh
rsync -a --delete --exclude target ./ uconsole.local:~/rnode-hw/
ssh uconsole.local 'cd ~/rnode-hw && cargo build --manifest-path tools/hwtest/sx126x-bringup/Cargo.toml'
```

Run the default bring-up against the AIO V2 wiring:

```sh
ssh uconsole.local 'pinctrl get 16; cd ~/rnode-hw && tools/hwtest/sx126x-bringup/target/debug/sx126x-bringup --mode bringup'
```

The tool:

- opens `/dev/spidev1.0` at 16 MHz, mode 0;
- requests `gpiochip15` lines RESET=25, BUSY=24 and DIO1=26;
- constructs `rnode_sx126x::Sx126x` with 1.8 V TCXO on DIO3, DIO2 RF switch,
  no RXEN, OCP `0x28`, and the driver timeouts;
- calls `begin(914_875_000)`;
- configures 125 kHz / SF7 / CR 4/5 / 2 dBm, firmware-derived preamble, CRC on;
- samples `current_rssi()` and `dcd()` while idle;
- transmits 1, 50 and 255 byte frames and prints elapsed time vs the Semtech
  time-on-air formula;
- checks SF12 low-data-rate optimisation and the SF9/31.25 kHz LDRO boundary;
- enters receive mode and confirms `poll_rx() == Ok(None)` and DIO1 is low.

Useful focused modes:

```sh
# Repeated SF7 packets for SDR capture.
ssh uconsole.local 'cd ~/rnode-hw && tools/hwtest/sx126x-bringup/target/debug/sx126x-bringup \
  --mode tx --freq-hz 914875000 --bw-hz 125000 --sf 7 --cr 5 \
  --power-dbm 2 --payload-len 50 --count 3 --interval-ms 1000'

# Idle receive polling.
ssh uconsole.local 'cd ~/rnode-hw && tools/hwtest/sx126x-bringup/target/debug/sx126x-bringup --mode rx-idle --count 10'
```

### Bring-up results, 2026-10-12

Hardware: uConsole CM5 / AIO V2, LoRa rail on (`GPIO16 = dh`), RTL-SDR rail off
for driver-only bring-up (`GPIO7 = dl`).  Driver branch: `feat/sx126x-driver`
commit `0219741a8d77`.

| Check | Result |
| --- | --- |
| `begin(914.875 MHz)` | OK |
| Configured PHY | 125000 Hz, SF7, CR 4/5, 2 dBm, CRC on |
| Firmware-derived preamble | 24 symbols at SF7/125 kHz |
| LDRO at SF7/125 kHz | false |
| Current RSSI idle | -109, -109, -109, -109, -110, -109, -110, -127, -107, -109 dBm; average -110.8 dBm |
| DCD idle | false for all ten samples |
| TX 1 byte | Done, 46.862 ms elapsed vs 42.240 ms Semtech ToA (+4.622 ms) |
| TX 50 bytes | Done, 122.682 ms elapsed vs 113.920 ms Semtech ToA (+8.762 ms) |
| TX 255 bytes | Done, 424.475 ms elapsed vs 416.000 ms Semtech ToA (+8.475 ms) |
| SF12/125 kHz | LDRO true, preamble 18 symbols, 50 B TX Done, 2649.479 ms vs 2629.632 ms (+19.847 ms) |
| SF9/31.25 kHz | LDRO false (firmware integer boundary), preamble 18 symbols, 50 B TX Done, 1498.393 ms vs 1478.656 ms (+19.737 ms) |
| RX idle | `poll_rx()` returned `Ok(None)`; DIO1 low |

A later repeated-TX run for SDR capture measured 50 byte SF7 packets at 117.648
ms and 127.150 ms elapsed vs 113.920 ms ToA.

## RTL-SDR capture and LoRa analysis

Power the SDR only for capture and turn it off afterwards:

```sh
ssh uconsole.local 'pinctrl set 7 op dh'
ssh uconsole.local 'SoapySDRUtil --find=driver=rtlsdr'
ssh uconsole.local 'cd ~/rnode-hw/tools/hwtest && gcc -O2 -Wall -Wextra -o soapysdr_capture \
  bin/soapysdr_capture.c $(pkg-config --cflags --libs SoapySDR)'
```

Capture while transmitting.  Center the SDR 200 kHz above the carrier to avoid
the RTL-SDR DC spike:

```sh
ssh uconsole.local 'cd ~/rnode-hw/tools/hwtest; mkdir -p captures; \
  ./soapysdr_capture captures/sf7_125k_914875_g0.cfile 915075000 1000000 4 0 \
    > captures/sf7_g0.stdout 2> captures/sf7_g0.stderr & cap=$!; \
  sleep 1; cd ~/rnode-hw; \
  tools/hwtest/sx126x-bringup/target/debug/sx126x-bringup --mode tx \
    --freq-hz 914875000 --bw-hz 125000 --sf 7 --cr 5 --power-dbm 2 \
    --payload-len 50 --count 2 --interval-ms 1000; \
  wait $cap'
rsync -a uconsole.local:~/rnode-hw/tools/hwtest/captures/sf7_125k_914875_g0.cfile tools/hwtest/captures/
/usr/local/bin/uv run --with numpy python tools/hwtest/lora_capture_analyze.py \
  tools/hwtest/captures/sf7_125k_914875_g0.cfile \
  --sample-rate 1000000 --center-hz 915075000 --carrier-hz 914875000 \
  --bw-hz 125000 --sf 7 --cr 5 --payload-len 50 --preamble 24
ssh uconsole.local 'pinctrl set 7 op dl'
```

`tools/hwtest/lora_capture_analyze.py` accepts SoapySDR CF32 captures and uses
only numpy.  It detects bursts, mixes the configured carrier to baseband,
dechirps against an ideal LoRa up-chirp, estimates frequency offset, reports
99% occupied bandwidth, checks preamble up-chirps, derives the expected sync
symbol bins from LoRa sync word `0x12` / SX126x registers `0x1424`, and verifies
the following down-chirps.

### On-air result, 2026-10-12

Capture: RTL-SDR at 1.000000 MS/s, center 915.075 MHz, gain 0 dB, carrier
914.875 MHz, 50 byte SF7/125 kHz/CR 4/5/CRC frames at 2 dBm.

Analyzer output for the clean burst:

- burst duration 114.253 ms vs 113.920 ms Semtech ToA (`+0.333 ms`);
- frequency offset -2929.7 Hz, -3.202 ppm relative to 914.875 MHz;
- 99% occupied bandwidth 125793 Hz;
- 24/24 expected preamble up-chirps detected, median dechirp SNR 61.3 dB;
- sync bins `[8, 16]`, matching LoRa sync word `0x12` at SF7 (`0x1 * 128/16`,
  `0x2 * 128/16`);
- two following down-chirps found with dechirp SNR 61.5 dB and 60.9 dB.

The SDR rail was turned off after capture (`pinctrl set 7 op dl`).

## Integrated daemon follow-up

Stage 3 uses a scratch, unpushed integration branch layered as:

```text
origin/main + origin/feat/core-csma + origin/fix/tx-timeout-scaling
+ origin/feat/sim-radio + origin/feat/core-protocol + origin/feat/linux-daemon
```

On 2026-10-13 that branch built on the uConsole with:

```sh
cargo build --release -p rnode-linux
```

The daemon started with `/dev/spidev1.0`, `gpiochip15` RESET=25/BUSY=24/DIO1=26,
1.8 V DIO3 TCXO, DIO2 RF switch, OCP `0x28`, and a home-directory PTY/storage
path.  PR #30 did not yet expose the LoRa rail enable GPIO, so GPIO16 was left in
its existing enabled state (`pinctrl get 16` reported high); power-GPIO support
is tracked in #35.  The SDR rail was
enabled only for capture and returned to low afterwards.

KISS and Reticulum checks passed for detect/version/platform/MCU, radio
configuration at 914.875 MHz / 125 kHz / SF7 / CR 4/5 / 2 dBm, READY,
`CMD_RANDOM`, `CMD_CONF_SAVE`, daemon restart with persisted settings, and
`CMD_RESET`.  Reticulum's `RNodeInterface` came up in full mode over the daemon
PTY and transmitted an `rnid` announce.

Known integration gaps found during hardware validation:

- `CMD_STAT_RSSI` before any RX returns the firmware's `last_rssi = -292`
  sentinel wrapped to `0x79` (-36 dBm); real RNodes do the same
  (`Utilities.h:871-877`).
- `CMD_STAT_CHTM`, `CMD_STAT_PHYPRM` and `CMD_STAT_CSMA` are push-only in the
  firmware, so host queries for them are ignored.
- The `CMD_DIS_IA` runtime toggle is tracked in #36.  The firmware itself
  persists the setting and resets with no reply.
- RX packet validation is still limited by the lack of a second LoRa transmitter;
  current evidence covers RX/idle noise floor, DCD-idle behaviour and DIO1 low in
  the direct bring-up tool, plus daemon channel telemetry.  Over-the-air RX
  remains an open question.

The split-frame SDR summary is kept in `captures/stage3_daemon_split_2026-10-13.txt`;
IQ files are intentionally not committed.
