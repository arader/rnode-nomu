#!/usr/bin/env python3
"""Probe an rnode-nomu daemon PTY with the RNode KISS host protocol.

Requires pyserial. Example:
  python3 tests/e2e/kiss_probe.py --port /dev/rnode0 --frequency 914875000 \
    --bandwidth 125000 --txpower 2 --sf 7 --cr 5 --sizes 1,254,255,508,509 --count 2

Attach an antenna; use a frequency and power legal in your region. With --config-commands it
also sends CMD_CONF_SAVE, CMD_CONF_DELETE and CMD_RESET, which leaves the daemon without saved
radio settings.
"""

import argparse
import struct
import sys
import time
from dataclasses import dataclass

try:
    import serial
except ImportError as exc:
    raise SystemExit("pyserial is required: python3 -m pip install pyserial") from exc

FEND = 0xC0
FESC = 0xDB
TFEND = 0xDC
TFESC = 0xDD

CMD_DATA = 0x00
CMD_FREQUENCY = 0x01
CMD_BANDWIDTH = 0x02
CMD_TXPOWER = 0x03
CMD_SF = 0x04
CMD_CR = 0x05
CMD_RADIO_STATE = 0x06
CMD_RADIO_LOCK = 0x07
CMD_DETECT = 0x08
CMD_READY = 0x0F
CMD_STAT_RX = 0x21
CMD_STAT_TX = 0x22
CMD_STAT_RSSI = 0x23
CMD_STAT_SNR = 0x24
CMD_STAT_CHTM = 0x25
CMD_STAT_PHYPRM = 0x26
CMD_STAT_CSMA = 0x28
CMD_RANDOM = 0x40
CMD_BOARD = 0x47
CMD_PLATFORM = 0x48
CMD_MCU = 0x49
CMD_FW_VERSION = 0x50
CMD_CONF_SAVE = 0x53
CMD_CONF_DELETE = 0x54
CMD_RESET = 0x55
CMD_DIS_IA = 0x69
CMD_ERROR = 0x90

DETECT_REQ = 0x73
CMD_RESET_BYTE = 0xF8
RADIO_STATE_ON = 0x01
RADIO_STATE_OFF = 0x00
RSSI_OFFSET = 157

NAMES = {
    CMD_DATA: "DATA",
    CMD_FREQUENCY: "FREQUENCY",
    CMD_BANDWIDTH: "BANDWIDTH",
    CMD_TXPOWER: "TXPOWER",
    CMD_SF: "SF",
    CMD_CR: "CR",
    CMD_RADIO_STATE: "RADIO_STATE",
    CMD_RADIO_LOCK: "RADIO_LOCK",
    CMD_DETECT: "DETECT",
    CMD_READY: "READY",
    CMD_STAT_RX: "STAT_RX",
    CMD_STAT_TX: "STAT_TX",
    CMD_STAT_RSSI: "STAT_RSSI",
    CMD_STAT_SNR: "STAT_SNR",
    CMD_STAT_CHTM: "STAT_CHTM",
    CMD_STAT_PHYPRM: "STAT_PHYPRM",
    CMD_STAT_CSMA: "STAT_CSMA",
    CMD_RANDOM: "RANDOM",
    CMD_BOARD: "BOARD",
    CMD_PLATFORM: "PLATFORM",
    CMD_MCU: "MCU",
    CMD_FW_VERSION: "FW_VERSION",
    CMD_CONF_SAVE: "CONF_SAVE",
    CMD_CONF_DELETE: "CONF_DELETE",
    CMD_RESET: "RESET",
    CMD_DIS_IA: "DIS_IA",
    CMD_ERROR: "ERROR",
}

def escape(data: bytes) -> bytes:
    out = bytearray()
    for b in data:
        if b == FEND:
            out += bytes([FESC, TFEND])
        elif b == FESC:
            out += bytes([FESC, TFESC])
        else:
            out.append(b)
    return bytes(out)


def frame(cmd: int, payload: bytes = b"") -> bytes:
    return bytes([FEND, cmd]) + escape(payload) + bytes([FEND])


@dataclass
class DecodedFrame:
    cmd: int
    payload: bytes


class KissDecoder:
    def __init__(self):
        self.in_frame = False
        self.cmd = None
        self.data = bytearray()
        self.escape = False

    def feed(self, chunk: bytes):
        frames = []
        for b in chunk:
            if b == FEND:
                if self.in_frame and self.cmd is not None:
                    frames.append(DecodedFrame(self.cmd, bytes(self.data)))
                    self.in_frame = False
                    self.cmd = None
                    self.data.clear()
                    self.escape = False
                else:
                    self.in_frame = True
                    self.cmd = None
                    self.data.clear()
                    self.escape = False
                continue
            if not self.in_frame:
                continue
            if self.cmd is None:
                self.cmd = b
                continue
            if b == FESC:
                self.escape = True
                continue
            if self.escape:
                if b == TFEND:
                    b = FEND
                elif b == TFESC:
                    b = FESC
                self.escape = False
            self.data.append(b)
        return frames


def decode_payload(cmd: int, payload: bytes) -> str:
    if cmd in (CMD_FREQUENCY, CMD_BANDWIDTH, CMD_STAT_RX, CMD_STAT_TX) and len(payload) == 4:
        return str(struct.unpack(">I", payload)[0])
    if cmd == CMD_FW_VERSION and len(payload) >= 2:
        return f"{payload[0]}.{payload[1]}"
    if cmd == CMD_STAT_RSSI and payload:
        return f"{payload[0] - RSSI_OFFSET} dBm raw={payload.hex()}"
    if cmd == CMD_STAT_SNR and payload:
        return f"{int.from_bytes(payload[:1], 'big', signed=True) * 0.25:.2f} dB raw={payload.hex()}"
    if cmd == CMD_STAT_CHTM and len(payload) == 11:
        ats, atl, cls, cll = struct.unpack(">HHHH", payload[:8])
        current_rssi = payload[8] - RSSI_OFFSET
        noise_floor = payload[9] - RSSI_OFFSET
        interference = payload[10] != 0xFF
        return (
            f"airtime_short={ats / 100:.2f}% airtime_long={atl / 100:.2f}% "
            f"channel_short={cls / 100:.2f}% channel_long={cll / 100:.2f}% "
            f"current_rssi={current_rssi} dBm noise_floor={noise_floor} dBm "
            f"interference={interference} raw={payload.hex()}"
        )
    if cmd == CMD_STAT_PHYPRM and len(payload) == 12:
        values = struct.unpack(">HHHHHH", payload)
        return "symbol_time_ms={} symbol_rate={} preamble_symbols={} preamble_time_ms={} csma_slot_ms={} csma_difs_ms={} raw={}".format(*values, payload.hex())
    if cmd == CMD_STAT_CSMA and len(payload) == 3:
        return "cw_band={} cw_min={} cw_max={} raw={}".format(payload[0], payload[1], payload[2], payload.hex())
    if cmd == CMD_DATA:
        return f"len={len(payload)} sha256-ish={sum(payload) & 0xffff:04x} first={payload[:16].hex()}"
    return payload.hex()


def drain(ser, decoder, until):
    frames = []
    while time.time() < until:
        chunk = ser.read(4096)
        if chunk:
            frames.extend(decoder.feed(chunk))
        else:
            time.sleep(0.02)
    return frames


def print_frames(frames):
    for item in frames:
        name = NAMES.get(item.cmd, f"0x{item.cmd:02x}")
        print(f"<- {name}: {decode_payload(item.cmd, item.payload)}")


def send_cmd(ser, cmd, payload=b""):
    name = NAMES.get(cmd, f"0x{cmd:02x}")
    print(f"-> {name}: {payload.hex()}")
    ser.write(frame(cmd, payload))
    ser.flush()


def parse_sizes(text):
    sizes = []
    for part in text.split(','):
        part = part.strip()
        if part:
            size = int(part)
            if size < 0 or size > 509:
                raise argparse.ArgumentTypeError("sizes must be in 0..509")
            sizes.append(size)
    return sizes


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", required=True, help="PTY/serial device exposed by rnode-nomu")
    parser.add_argument("--frequency", type=int, required=True)
    parser.add_argument("--bandwidth", type=int, default=125000)
    parser.add_argument("--txpower", type=int, default=2)
    parser.add_argument("--sf", type=int, default=7)
    parser.add_argument("--cr", type=int, default=5)
    parser.add_argument("--sizes", type=parse_sizes, default=parse_sizes("1,254,255,508,509"))
    parser.add_argument("--count", type=int, default=1)
    parser.add_argument("--wait", type=float, default=5.0, help="seconds to listen after sending frames")
    parser.add_argument("--baud", type=int, default=115200)
    parser.add_argument(
        "--config-commands",
        action="store_true",
        help="also send CMD_CONF_SAVE, CMD_CONF_DELETE and CMD_RESET; "
        "this deletes any radio settings the daemon has saved",
    )
    args = parser.parse_args()

    decoder = KissDecoder()
    with serial.Serial(args.port, args.baud, timeout=0, write_timeout=2) as ser:
        commands = [
            (CMD_DETECT, bytes([DETECT_REQ])),
            (CMD_FW_VERSION, b"\x00"),
            (CMD_PLATFORM, b"\x00"),
            (CMD_MCU, b"\x00"),
            (CMD_BOARD, b"\x00"),
            (CMD_FREQUENCY, struct.pack(">I", args.frequency)),
            (CMD_BANDWIDTH, struct.pack(">I", args.bandwidth)),
            (CMD_TXPOWER, bytes([args.txpower])),
            (CMD_SF, bytes([args.sf])),
            (CMD_CR, bytes([args.cr])),
            (CMD_RADIO_STATE, bytes([RADIO_STATE_ON])),
            (CMD_RADIO_LOCK, b"\xff"),
        ]
        for cmd, payload in commands:
            send_cmd(ser, cmd, payload)
            print_frames(drain(ser, decoder, time.time() + 0.25))

        stats = [CMD_STAT_RX, CMD_STAT_TX, CMD_STAT_RSSI, CMD_STAT_SNR, CMD_STAT_CHTM, CMD_STAT_PHYPRM, CMD_STAT_CSMA, CMD_READY]
        for cmd in stats:
            send_cmd(ser, cmd, b"\xff")
        print_frames(drain(ser, decoder, time.time() + 1.0))

        for idx in range(args.count):
            for size in args.sizes:
                payload = bytes((idx + x) % 251 for x in range(size))
                send_cmd(ser, CMD_DATA, payload)
                time.sleep(0.05)

        print(f"listening for {args.wait:.1f}s")
        print_frames(drain(ser, decoder, time.time() + args.wait))

        for cmd in stats:
            send_cmd(ser, cmd, b"\xff")
        print_frames(drain(ser, decoder, time.time() + 1.0))
        if args.config_commands:
            for cmd, payload in [(CMD_CONF_SAVE, b"\x00"), (CMD_CONF_DELETE, b"\x00"), (CMD_RESET, bytes([CMD_RESET_BYTE]))]:
                send_cmd(ser, cmd, payload)
                print_frames(drain(ser, decoder, time.time() + 0.5))
        send_cmd(ser, CMD_RADIO_STATE, bytes([RADIO_STATE_OFF]))
        print_frames(drain(ser, decoder, time.time() + 0.5))


if __name__ == "__main__":
    main()
