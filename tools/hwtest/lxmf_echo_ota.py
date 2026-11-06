#!/usr/bin/env python3
"""Over-the-air LXMF echo round trip through an RNode, for real-RNode interoperability tests.

The tool starts a Reticulum instance whose only interface is an RNodeInterface on --port (the
PTY of an rnode-nomu daemon, or a hardware RNode), with transport and instance sharing off, so
every packet it sends or receives crosses the radio. It then:

1. waits for the interface to come online;
2. requests a path to the LXMF echo destination (--echo), with retries, unless an announce heard
   since start-up already gave one, and recalls the destination's identity;
3. registers an LXMF delivery destination for --identity and announces it once;
4. sends one message per requested content size and delivery method, one at a time, and waits
   for each echo, checking that its title and content come back unchanged; unless --keep-going is
   given, it stops at the first message that fails;
5. prints a summary with delivery and round-trip times and the RSSI/SNR of each reply, and exits
   with status 1 if any step failed (2 for invalid arguments).

The echo peer must answer every LXMF message with the same title and content, and must already
be known to the network (announced) so that the path request is answered.

Every packet that crosses the RNodeInterface is printed as a WIRE line: UTC time, direction,
length and Reticulum packet type, plus RSSI/SNR for received packets. With --daemon-log, the tool
also checks that each of these packets appears in the rnode-nomu debug log behind --port as a
TX or RX packet line, which also shows how many LoRa frames it took; the log must be on the same
host, at log_level = "debug".

Safety: like tools/hwtest/sx126x-bringup, the tool refuses frequencies outside the US 902-928 MHz
ISM band and TX power above 10 dBm. Keep sessions short on a shared channel.

Requires the rns and lxmf packages (pip install rns lxmf); otherwise only the standard library.
"""

from __future__ import annotations

import argparse
import os
import re
import secrets
import shutil
import signal
import statistics
import string
import sys
import tempfile
import threading
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import NamedTuple

US_ISM_HZ = (902_000_000, 928_000_000)
MAX_TX_POWER_DBM = 10
METHODS = ("opportunistic", "direct")
MAX_CONTENT_BYTES = 32_768
CONTENT_ALPHABET = string.ascii_letters + string.digits
# Time for packets queued just before shutdown (link teardown) to get on the air before the
# interface is detached and the radio switched off.
DRAIN_SECONDS = 3.0
GAP_SECONDS = 2.0
# How long a run waits for an identity file that another run has just created to hold a whole key.
IDENTITY_WAIT_SECONDS = 2.0
PROOF_GRACE_SECONDS = 5.0
# Daemon log timestamps have one-second resolution; RX lines are logged just before the packet
# reaches the host, TX lines after the packet's airtime (plus any CSMA wait and queue ahead of it).
RX_MATCH_WINDOW = (-2.0, 1.0)
TX_MATCH_WINDOW = (-1.5, 90.0)

PACKET_TYPES = {0: "DATA", 1: "ANNOUNCE", 2: "LINKREQUEST", 3: "PROOF"}
DESTINATION_TYPES = {0: "single", 1: "group", 2: "plain", 3: "link"}
CONTEXTS = {
    0x01: "RESOURCE",
    0x02: "RESOURCE_ADV",
    0x03: "RESOURCE_REQ",
    0x04: "RESOURCE_HMU",
    0x05: "RESOURCE_PRF",
    0x06: "RESOURCE_ICL",
    0x07: "RESOURCE_RCL",
    0x08: "CACHE_REQUEST",
    0x09: "REQUEST",
    0x0A: "RESPONSE",
    0x0B: "PATH_RESPONSE",
    0x0C: "COMMAND",
    0x0D: "COMMAND_STATUS",
    0x0E: "CHANNEL",
    0xFA: "KEEPALIVE",
    0xFB: "LINKIDENTIFY",
    0xFC: "LINKCLOSE",
    0xFD: "LINKPROOF",
    0xFE: "LRRTT",
    0xFF: "LRPROOF",
}
DAEMON_PACKET_LINE = re.compile(
    r"^\[(?P<time>\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d)(?:\.\d+)?Z\s+\w+\s+[\w:]+\]\s+"
    r"(?P<dir>TX|RX) packet: (?P<len>\d+) bytes?, (?P<frames>\d+) frames?(?P<rest>.*)$"
)
DAEMON_RSSI_SNR = re.compile(r"RSSI (?P<rssi>-?\d+) dBm, SNR (?P<snr>-?[\d.]+) dB")


def fail_usage(message: str) -> None:
    print(f"lxmf_echo_ota: {message}", file=sys.stderr, flush=True)
    raise SystemExit(2)


def parse_hash(text: str) -> bytes:
    text = text.strip().strip("<>").replace(":", "")
    if len(text) != 32 or any(c not in string.hexdigits for c in text):
        raise argparse.ArgumentTypeError("expected a destination hash of 32 hex digits")
    return bytes.fromhex(text)


def parse_sizes(text: str) -> list[int]:
    sizes = []
    for part in text.split(","):
        part = part.strip()
        if not part:
            continue
        try:
            size = int(part)
        except ValueError:
            raise argparse.ArgumentTypeError(f"invalid size {part!r}") from None
        if not 1 <= size <= MAX_CONTENT_BYTES:
            raise argparse.ArgumentTypeError(f"sizes must be in 1..{MAX_CONTENT_BYTES}")
        sizes.append(size)
    if not sizes:
        raise argparse.ArgumentTypeError("no sizes given")
    return sizes


def parse_methods(text: str) -> list[str]:
    methods = [part.strip().lower() for part in text.split(",") if part.strip()]
    for method in methods:
        if method not in METHODS:
            raise argparse.ArgumentTypeError(f"unknown method {method!r}; expected {' or '.join(METHODS)}")
    if not methods:
        raise argparse.ArgumentTypeError("no methods given")
    return methods


def parse_args(argv: list[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=__doc__.split("\n\n")[0],
        epilog=__doc__.split("\n\n", 1)[1],
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--port", required=True, help="serial port or PTY of the RNode, e.g. the rnode-nomu symlink")
    parser.add_argument("--frequency", type=int, required=True, help="carrier in Hz, within 902-928 MHz")
    parser.add_argument("--bandwidth", type=int, required=True, help="LoRa bandwidth in Hz")
    parser.add_argument("--sf", type=int, required=True, help="spreading factor, 5-12")
    parser.add_argument("--cr", type=int, required=True, help="coding rate denominator, 5-8 (4/5 to 4/8)")
    parser.add_argument(
        "--txpower", type=int, default=2, help=f"TX power in dBm, 0-{MAX_TX_POWER_DBM} (default: 2)"
    )
    parser.add_argument(
        "--echo", type=parse_hash, required=True, help="LXMF delivery destination hash of the echo peer"
    )
    parser.add_argument(
        "--identity",
        required=True,
        help="identity file of the test sender, created if missing, so repeated runs announce the same "
        "identity; its LXMF state (ratchet keys) is kept in the directory IDENTITY.lxmf",
    )
    parser.add_argument(
        "--sizes", type=parse_sizes, default=[200], help="comma-separated content sizes in bytes (default: 200)"
    )
    parser.add_argument(
        "--methods",
        type=parse_methods,
        default=["opportunistic"],
        help="comma-separated delivery methods, opportunistic or direct: one for all sizes, or one per size "
        "(default: opportunistic)",
    )
    parser.add_argument("--timeout", type=float, default=120.0, help="seconds to wait for each echo (default: 120)")
    parser.add_argument(
        "--keep-going",
        action="store_true",
        help="send the remaining messages after one fails; by default the run stops transmitting at the "
        "first failure",
    )
    parser.add_argument("--path-attempts", type=int, default=3, help="path requests before giving up (default: 3)")
    parser.add_argument(
        "--path-wait", type=float, default=60.0, help="seconds to wait for a path after each request (default: 60)"
    )
    parser.add_argument(
        "--settle",
        type=float,
        default=10.0,
        help="seconds between the announce and the first message, so the echo peer can learn the "
        "sender's keys (default: 10)",
    )
    parser.add_argument(
        "--online-timeout", type=float, default=30.0, help="seconds to wait for the interface (default: 30)"
    )
    parser.add_argument("--display-name", default="lxmf_echo_ota", help="display name in the sender's announce")
    parser.add_argument(
        "--workdir",
        help="directory in which to create the temporary Reticulum config directory "
        "(default: the system temporary directory)",
    )
    parser.add_argument(
        "--daemon-log",
        help="rnode-nomu log (log_level = \"debug\") of the daemon behind --port, on this host: every "
        "packet the tool sends or receives must appear there as a TX or RX packet line",
    )
    parser.add_argument(
        "--rns-loglevel",
        type=int,
        default=4,
        help="Reticulum log level, 0-7 (default: 4); 6 adds LXMF and link details",
    )
    args = parser.parse_args(argv)

    if not US_ISM_HZ[0] <= args.frequency <= US_ISM_HZ[1]:
        fail_usage(f"refusing to transmit or configure outside US 902-928 MHz ISM band: {args.frequency} Hz")
    if args.txpower > MAX_TX_POWER_DBM:
        fail_usage(f"refusing TX power above {MAX_TX_POWER_DBM} dBm; requested {args.txpower} dBm")
    if args.txpower < 0:
        fail_usage(f"TX power must be at least 0 dBm; requested {args.txpower} dBm")
    if not 7_800 <= args.bandwidth <= 1_625_000:
        fail_usage(f"bandwidth must be within 7800-1625000 Hz: {args.bandwidth}")
    if not 5 <= args.sf <= 12:
        fail_usage(f"spreading factor must be 5-12: {args.sf}")
    if not 5 <= args.cr <= 8:
        fail_usage(f"coding rate denominator must be 5-8: {args.cr}")
    if len(args.methods) not in (1, len(args.sizes)):
        fail_usage(f"--methods needs one method, or one per size ({len(args.sizes)}); got {len(args.methods)}")
    if args.path_attempts < 1:
        fail_usage("--path-attempts must be at least 1")
    for name in ("timeout", "path_wait", "online_timeout"):
        if getattr(args, name) <= 0:
            fail_usage(f"--{name.replace('_', '-')} must be positive")
    if args.settle < 0:
        fail_usage("--settle must not be negative")
    return args


def utc_clock(t: float) -> str:
    return time.strftime("%H:%M:%S", time.gmtime(t)) + f".{int(t * 1000) % 1000:03d}"


def say(text: str) -> None:
    print(f"OTA {utc_clock(time.time())} {text}", flush=True)


def opt(value, fmt: str = "{}", missing: str = "-") -> str:
    return missing if value is None else fmt.format(value)


class Header(NamedTuple):
    """The parts of a Reticulum packet header the tool looks at."""

    packet_type: str
    destination_type: str
    destination: bytes
    context: str | None
    header_2: bool
    hops: int


def parse_header(raw: bytes) -> Header | None:
    """The header of a Reticulum packet; None if the packet is too short to have one, or carries an
    interface access code (which masks the header)."""
    if len(raw) < 19 or raw[0] & 0x80:
        return None
    flags, hops = raw[0], raw[1]
    header_2 = bool((flags >> 6) & 1)
    if header_2:
        if len(raw) < 35:
            return None
        destination, context = raw[18:34], raw[34]
    else:
        destination, context = raw[2:18], raw[18]
    return Header(
        packet_type=PACKET_TYPES[flags & 0x03],
        destination_type=DESTINATION_TYPES[(flags >> 2) & 0x03],
        destination=bytes(destination),
        context=CONTEXTS.get(context),
        header_2=header_2,
        hops=hops,
    )


@dataclass
class WirePacket:
    """One packet handed to the RNode (tx) or delivered by it (rx)."""

    t: float
    direction: str
    length: int
    description: str
    rssi: float | None = None
    snr: float | None = None
    daemon: DaemonPacket | None = None
    header: Header | None = None


@dataclass
class DaemonPacket:
    """A TX/RX packet line from the rnode-nomu debug log."""

    t: float
    direction: str
    length: int
    frames: int
    rssi: int | None
    snr: float | None
    tx_outcome: str | None
    line: str


class WireLog:
    """Prints and records every packet that crosses the RNodeInterface."""

    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.packets: list[WirePacket] = []
        self.aliases: dict[bytes, str] = {}

    def describe(self, raw: bytes) -> str:
        """A one-line description; destinations with an alias are shown by it, others by the first
        four bytes of their hash. Only for display: lookups use the parsed header."""
        if len(raw) >= 19 and raw[0] & 0x80:
            return f"IFAC-authenticated hops={raw[1]}"
        header = parse_header(raw)
        if header is None:
            return f"malformed ({len(raw)} B)"
        name = self.aliases.get(header.destination, header.destination[:4].hex())
        parts = [header.packet_type, header.destination_type, name]
        if header.context:
            parts.append(header.context)
        parts.append(f"{'hdr2' if header.header_2 else 'hdr1'} hops={header.hops}")
        return " ".join(parts)

    def record(self, direction: str, raw: bytes, rssi=None, snr=None) -> None:
        packet = WirePacket(time.time(), direction, len(raw), self.describe(raw), rssi, snr, header=parse_header(raw))
        with self.lock:
            self.packets.append(packet)
            phy = f" RSSI {opt(rssi)} dBm SNR {opt(snr)} dB" if direction == "rx" else ""
            print(f"WIRE {utc_clock(packet.t)} {direction} {packet.length:3d} B {packet.description}{phy}", flush=True)

    def last_rx(self, predicate=lambda packet: True) -> WirePacket | None:
        with self.lock:
            for packet in reversed(self.packets):
                if packet.direction == "rx" and predicate(packet):
                    return packet
        return None


def instrument_rnode_interface(wire: WireLog):
    """Record every packet through RNodeInterface. Patching the class before Reticulum creates
    the interface catches the first packets too. process_incoming() runs right after the RNode's
    CMD_STAT_RSSI and CMD_STAT_SNR frames, so the interface's r_stat_* are the packet's values.
    The generated config leaves flow control off, so each outgoing packet passes here once."""
    from RNS.Interfaces.RNodeInterface import RNodeInterface

    original_incoming = RNodeInterface.process_incoming
    original_outgoing = RNodeInterface.process_outgoing

    def process_incoming(self, data):
        wire.record("rx", bytes(data), self.r_stat_rssi, self.r_stat_snr)
        return original_incoming(self, data)

    def process_outgoing(self, data):
        if self.online:
            wire.record("tx", bytes(data))
        return original_outgoing(self, data)

    RNodeInterface.process_incoming = process_incoming
    RNodeInterface.process_outgoing = process_outgoing
    return RNodeInterface


def parse_daemon_log(lines, start: float, end: float) -> list[DaemonPacket]:
    """The TX/RX packet lines of an rnode-nomu debug log with timestamps within [start, end],
    widened by 5 s for the log's one-second resolution."""
    packets = []
    for line in lines:
        match = DAEMON_PACKET_LINE.match(line.strip())
        if not match:
            continue
        t = datetime.strptime(match["time"], "%Y-%m-%dT%H:%M:%S").replace(tzinfo=timezone.utc).timestamp()
        if not start - 5 <= t <= end + 5:
            continue
        phy = DAEMON_RSSI_SNR.search(match["rest"])
        # A TX line ends with its outcome: ": ok", or the error that stopped the packet.
        outcome = match["rest"].rsplit(": ", 1)[-1] if match["dir"] == "TX" else None
        packets.append(
            DaemonPacket(
                t=t,
                direction=match["dir"].lower(),
                length=int(match["len"]),
                frames=int(match["frames"]),
                rssi=int(phy["rssi"]) if phy else None,
                snr=float(phy["snr"]) if phy else None,
                tx_outcome=outcome,
                line=line.strip(),
            )
        )
    return packets


def read_daemon_log(path: str, start: float, end: float) -> list[DaemonPacket]:
    with open(path, encoding="utf-8", errors="replace") as log:
        return parse_daemon_log(log, start, end)


def cross_check(wire_packets: list[WirePacket], daemon_packets: list[DaemonPacket]) -> list[DaemonPacket]:
    """Pair each wire packet with a daemon log line of the same direction and length, in order,
    within the timing window of its direction. Returns the daemon lines left unpaired."""
    used: set[int] = set()
    for packet in wire_packets:
        before, after = RX_MATCH_WINDOW if packet.direction == "rx" else TX_MATCH_WINDOW
        for index, candidate in enumerate(daemon_packets):
            if index in used or candidate.direction != packet.direction or candidate.length != packet.length:
                continue
            if packet.t + before <= candidate.t <= packet.t + after:
                packet.daemon = candidate
                used.add(index)
                break
    return [candidate for index, candidate in enumerate(daemon_packets) if index not in used]


@dataclass
class Case:
    index: int
    method: str
    size: int
    title: str
    content: str
    sent_at: float | None = None
    representation: str | None = None
    packed_size: int | None = None
    delivered_at: float | None = None
    failed_at: float | None = None
    echo_at: float | None = None
    echo_method: str | None = None
    echo_rssi: float | None = None
    echo_snr: float | None = None
    echo_signature: bool | None = None
    content_ok: bool | None = None
    echo_event: threading.Event = field(default_factory=threading.Event)

    @property
    def ok(self) -> bool:
        return self.echo_at is not None and bool(self.content_ok)

    @property
    def result(self) -> str:
        """The case's verdict in the summary table."""
        if self.sent_at is None:
            return "not sent"
        if self.ok:
            return "ok"
        return "MISMATCH" if self.echo_at is not None else "NO ECHO"


class Session:
    def __init__(self, RNS, LXMF, args: argparse.Namespace, workdir: str, wire: WireLog, cases: list[Case]) -> None:
        self.RNS = RNS
        self.LXMF = LXMF
        self.args = args
        self.workdir = workdir
        self.wire = wire
        self.cases = cases
        self.by_title = {case.title: case for case in cases}
        self.lock = threading.Lock()
        self.started = time.time()
        self.finished: float | None = None
        self.interface = None
        self.router = None
        self.source = None
        self.path_found_at: float | None = None
        self.path_attempts_used = 0
        self.path_known_before_request = False
        self.path_hops = None
        self.path_packet: WirePacket | None = None
        self.firmware = None
        self.channel_start = None
        self.channel_end = None
        self.unmatched_daemon: list[DaemonPacket] | None = None
        self.daemon_error: str | None = None
        self.failure: str | None = None
        self.interrupted = False

    # Reticulum ---------------------------------------------------------------------------------

    def write_config(self) -> None:
        args = self.args
        config = f"""# Written by lxmf_echo_ota.py: one RNodeInterface, nothing else, so all traffic is on the air.
[reticulum]
  enable_transport = No
  share_instance = No
  panic_on_interface_error = No

[logging]
  loglevel = {args.rns_loglevel}

[interfaces]
  [[RNode under test]]
    type = RNodeInterface
    enabled = Yes
    port = {args.port}
    frequency = {args.frequency}
    bandwidth = {args.bandwidth}
    txpower = {args.txpower}
    spreadingfactor = {args.sf}
    codingrate = {args.cr}
"""
        with open(os.path.join(self.workdir, "config"), "w", encoding="utf-8") as handle:
            handle.write(config)

    def channel_stats(self):
        i = self.interface
        return {
            "noise_floor": i.r_noise_floor,
            "current_rssi": i.r_current_rssi,
            "airtime_short": i.r_airtime_short,
            "airtime_long": i.r_airtime_long,
            "channel_load_short": i.r_channel_load_short,
            "channel_load_long": i.r_channel_load_long,
        }

    def start_reticulum(self, rnode_class) -> bool:
        RNS, args = self.RNS, self.args
        say(
            f"Reticulum config in {self.workdir}: only an RNodeInterface on {args.port}, "
            f"{args.frequency} Hz, {args.bandwidth} Hz, SF{args.sf}, CR 4/{args.cr}, {args.txpower} dBm"
        )
        RNS.Reticulum(configdir=self.workdir, loglevel=args.rns_loglevel)
        install_interrupt_handlers()
        interfaces = [i for i in RNS.Transport.interfaces if isinstance(i, rnode_class)]
        others = [str(i) for i in RNS.Transport.interfaces if not isinstance(i, rnode_class)]
        if others:
            self.failure = f"unexpected interfaces besides the RNode: {', '.join(others)}"
            return False
        if len(interfaces) != 1:
            self.failure = f"expected one RNodeInterface, found {len(interfaces)}"
            return False
        self.interface = interfaces[0]
        deadline = time.time() + args.online_timeout
        while not self.interface.online and time.time() < deadline:
            time.sleep(0.1)
        if not self.interface.online:
            self.failure = f"the RNodeInterface did not come online within {args.online_timeout:g} s"
            return False
        i = self.interface
        self.firmware = f"{i.maj_version}.{i.min_version}"
        say(
            f"interface online: firmware {self.firmware}, platform {opt(i.platform, '0x{:02x}')}, "
            f"MCU {opt(i.mcu, '0x{:02x}')}; radio reports {i.r_frequency} Hz, {i.r_bandwidth} Hz, "
            f"SF{i.r_sf}, CR 4/{i.r_cr}, {i.r_txpower} dBm, {i.bitrate_kbps} kbps"
        )
        time.sleep(2)
        self.channel_start = self.channel_stats()
        return True

    def find_echo(self) -> bool:
        RNS, args = self.RNS, self.args
        echo = args.echo
        if RNS.Transport.has_path(echo):
            # The instance starts with empty storage, so only an announce heard since start-up
            # can have taught it this path.
            self.path_known_before_request = True
            self.path_found_at = time.time()
            say("path already known (heard an announce); no path request sent")
        for attempt in range(1, args.path_attempts + 1):
            # Also stops when a late answer to the previous request arrived after its wait.
            if RNS.Transport.has_path(echo):
                break
            self.path_attempts_used = attempt
            say(f"path request {attempt}/{args.path_attempts} for {RNS.prettyhexrep(echo)}")
            requested = time.time()
            RNS.Transport.request_path(echo)
            while time.time() < requested + args.path_wait:
                if RNS.Transport.has_path(echo):
                    self.path_found_at = time.time()
                    say(f"path found {self.path_found_at - requested:.1f} s after request {attempt}")
                    break
                time.sleep(0.2)
            else:
                heard = sum(1 for p in self.wire.packets if p.direction == "rx")
                say(f"no path after {args.path_wait:g} s; {heard} packets received so far")
        if not RNS.Transport.has_path(echo):
            self.failure = f"no path to the echo destination after {args.path_attempts} requests"
            return False
        self.path_hops = RNS.Transport.hops_to(echo)
        self.path_packet = self.wire.last_rx(
            lambda p: p.header is not None and p.header.packet_type == "ANNOUNCE" and p.header.destination == echo
        )
        return True

    # LXMF --------------------------------------------------------------------------------------

    def on_message(self, message) -> None:
        RNS, LXMF = self.RNS, self.LXMF
        now = time.time()
        title = message.title.decode("utf-8", "replace") if isinstance(message.title, bytes) else str(message.title)
        with self.lock:
            case = self.by_title.get(title)
            if message.source_hash != self.args.echo or case is None:
                say(f"ignoring LXMF message from {RNS.prettyhexrep(message.source_hash)} titled {title!r}")
                return
            if case.echo_at is not None:
                say(f"duplicate echo for case {case.index}, ignored")
                return
            case.echo_at = now
            case.content_ok = message.content == case.content.encode("utf-8")
            method = getattr(message, "method", None)
            methods = {LXMF.LXMessage.OPPORTUNISTIC: "opportunistic", LXMF.LXMessage.DIRECT: "direct"}
            case.echo_method = methods.get(method, opt(method))
            case.echo_signature = bool(message.signature_validated)
            case.echo_rssi = getattr(message, "rssi", None)
            case.echo_snr = getattr(message, "snr", None)
            if case.echo_rssi is None:
                last = self.wire.last_rx()
                if last is not None:
                    case.echo_rssi, case.echo_snr = last.rssi, last.snr
        say(
            f"echo {case.index} ({case.title}) after {now - case.sent_at:.1f} s by {case.echo_method}: "
            f"{'content and title match' if case.content_ok else 'CONTENT MISMATCH'}, "
            f"signature {'valid' if case.echo_signature else 'not validated'}, "
            f"RSSI {opt(case.echo_rssi)} dBm, SNR {opt(case.echo_snr)} dB"
        )
        case.echo_event.set()

    def on_delivered(self, case: Case) -> None:
        if case.delivered_at is None:
            case.delivered_at = time.time()
            say(f"case {case.index} delivered (proof received) after {case.delivered_at - case.sent_at:.1f} s")

    def on_failed(self, case: Case) -> None:
        if case.failed_at is None:
            case.failed_at = time.time()
            say(f"case {case.index}: LXMF gave up delivering after {case.failed_at - case.sent_at:.1f} s")

    def start_lxmf(self, identity) -> None:
        RNS, LXMF = self.RNS, self.LXMF
        state_dir = self.args.identity + ".lxmf"
        os.makedirs(state_dir, mode=0o700, exist_ok=True)
        self.router = LXMF.LXMRouter(identity=identity, storagepath=state_dir)
        # LXMRouter installs SIGINT/SIGTERM handlers that exit at once with status 0; take them back
        # so an interrupted run still closes its links, switches the radio off and reports failure.
        install_interrupt_handlers()
        self.source = self.router.register_delivery_identity(identity, display_name=self.args.display_name)
        self.router.register_delivery_callback(self.on_message)
        self.wire.aliases[self.source.hash] = "self"
        say(
            f"sender {RNS.prettyhexrep(self.source.hash)} (identity {RNS.prettyhexrep(identity.hash)}); "
            "announcing once"
        )
        self.router.announce(self.source.hash)

    def run_case(self, echo_destination, case: Case) -> None:
        LXMF = self.LXMF
        method = LXMF.LXMessage.OPPORTUNISTIC if case.method == "opportunistic" else LXMF.LXMessage.DIRECT
        message = LXMF.LXMessage(echo_destination, self.source, case.content, case.title, desired_method=method)
        message.register_delivery_callback(lambda _message: self.on_delivered(case))
        message.register_failed_callback(lambda _message: self.on_failed(case))
        case.sent_at = time.time()
        self.router.handle_outbound(message)
        representations = {LXMF.LXMessage.PACKET: "packet", LXMF.LXMessage.RESOURCE: "resource"}
        case.representation = representations.get(message.representation, opt(message.representation))
        case.packed_size = message.packed_size
        say(
            f"case {case.index}: sent {case.method} {case.title}, {case.size} B content, "
            f"LXMF {case.packed_size} B as a {case.representation}"
        )
        if not case.echo_event.wait(self.args.timeout):
            say(f"case {case.index}: no echo within {self.args.timeout:g} s")
            return
        deadline = time.time() + PROOF_GRACE_SECONDS
        while case.delivered_at is None and time.time() < deadline:
            time.sleep(0.1)

    def register_aliases(self) -> None:
        """Name the echo and path-request destinations in WIRE lines; before start-up, so that
        packets heard while the interface comes up are named too."""
        path_request = self.RNS.Destination.hash_from_name_and_identity("rnstransport.path.request", None)
        self.wire.aliases[self.args.echo] = "echo"
        self.wire.aliases[path_request] = "path-request"

    def run(self, rnode_class, identity) -> None:
        RNS = self.RNS
        self.register_aliases()
        if not self.start_reticulum(rnode_class):
            return
        if not self.find_echo():
            return
        echo_identity = RNS.Identity.recall(self.args.echo)
        if echo_identity is None:
            self.failure = "path known, but the echo destination's identity was not recalled"
            return
        echo_destination = RNS.Destination(
            echo_identity, RNS.Destination.OUT, RNS.Destination.SINGLE, "lxmf", "delivery"
        )
        if echo_destination.hash != self.args.echo:
            self.failure = "the echo hash is not the lxmf.delivery destination of the identity that announced it"
            return
        say(f"echo destination {RNS.prettyhexrep(self.args.echo)} is {self.path_hops} hops away")
        self.start_lxmf(identity)
        if self.args.settle:
            say(f"waiting {self.args.settle:g} s for the announce to reach the echo peer")
            time.sleep(self.args.settle)
        for number, case in enumerate(self.cases):
            if number:
                if not (self.cases[number - 1].ok or self.args.keep_going):
                    say(f"case {number} failed; not sending the remaining messages (see --keep-going)")
                    break
                time.sleep(GAP_SECONDS)
            self.run_case(echo_destination, case)

    def shutdown(self) -> None:
        """Close links politely, let the last packets go out, then switch the radio off."""
        RNS = self.RNS
        if self.router is not None:
            for link in list(self.router.direct_links.values()):
                try:
                    if link.status == RNS.Link.ACTIVE:
                        link.teardown()
                except Exception as error:  # noqa: BLE001 - shutdown must go on
                    say(f"link teardown failed: {error}")
            try:
                self.router.exit_handler()
            except Exception as error:  # noqa: BLE001
                say(f"LXMF shutdown failed: {error}")
            time.sleep(DRAIN_SECONDS)
        if self.interface is not None:
            self.channel_end = self.channel_stats()
        try:
            # Detaching the RNodeInterface turns the radio off and sends CMD_LEAVE.
            RNS.Reticulum.exit_handler()
        except Exception as error:  # noqa: BLE001
            say(f"Reticulum shutdown failed: {error}")
        self.finished = time.time()
        if self.args.daemon_log:
            try:
                daemon_packets = read_daemon_log(self.args.daemon_log, self.started, self.finished + 2)
                self.unmatched_daemon = cross_check(self.wire.packets, daemon_packets)
            except OSError as error:
                self.daemon_error = str(error)

    # Report ------------------------------------------------------------------------------------

    def report(self) -> bool:
        print("\nSUMMARY", flush=True)
        ok = self.failure is None and not self.interrupted
        if self.interrupted:
            print("  interrupted")
        if self.firmware:
            print(f"  RNode          : firmware {self.firmware}")
        if self.path_hops is not None:
            phy = ""
            if self.path_packet is not None:
                header = self.path_packet.header
                kind = "path response" if header is not None and header.context == "PATH_RESPONSE" else "announce"
                phy = f", {kind} RSSI {opt(self.path_packet.rssi)} dBm SNR {opt(self.path_packet.snr)} dB"
            if self.path_known_before_request:
                how = "already known (heard an announce), no request sent"
            else:
                how = f"after request {self.path_attempts_used}/{self.args.path_attempts}"
            print(f"  path           : {how}, {self.path_hops} hops{phy}")
        elif self.path_attempts_used:
            print(f"  path           : not found after {self.path_attempts_used} requests")

        if any(case.sent_at is not None for case in self.cases):
            rows = self.case_rows()
            widths = [max(len(row[i]) for row in rows) for i in range(len(rows[0]))]
            for row in rows:
                print("  " + "  ".join(cell.ljust(width) for cell, width in zip(row, widths)).rstrip())
            if not all(case.ok for case in self.cases):
                ok = False
        elif self.cases and self.failure is None:
            ok = False

        tx = [p for p in self.wire.packets if p.direction == "tx"]
        rx = [p for p in self.wire.packets if p.direction == "rx"]
        rssi = [p.rssi for p in rx if p.rssi is not None]
        snr = [p.snr for p in rx if p.snr is not None]
        line = (
            f"  wire           : tx {len(tx)} packets / {sum(p.length for p in tx)} B, "
            f"rx {len(rx)} packets / {sum(p.length for p in rx)} B"
        )
        if rssi:
            line += f"; rx RSSI {min(rssi)}..{max(rssi)} dBm (median {statistics.median(rssi):g})"
        if snr:
            line += f", SNR {min(snr)}..{max(snr)} dB (median {statistics.median(snr):g})"
        print(line)
        for label, stats in (("channel, start", self.channel_start), ("channel, end", self.channel_end)):
            if stats:
                print(
                    f"  {label:<15}: noise floor {opt(stats['noise_floor'])} dBm, "
                    f"RSSI {opt(stats['current_rssi'])} dBm, "
                    f"airtime {stats['airtime_short']}% (15 s) {stats['airtime_long']}% (1 h), "
                    f"channel load {stats['channel_load_short']}% (15 s) {stats['channel_load_long']}% (1 h)"
                )

        if self.args.daemon_log:
            if self.daemon_error:
                print(f"  daemon log     : could not read {self.args.daemon_log}: {self.daemon_error}")
                ok = False
            elif self.unmatched_daemon is not None:
                ok = self.report_daemon_log() and ok

        if self.failure:
            print(f"  failure        : {self.failure}")
        passed = sum(1 for case in self.cases if case.ok)
        verdict = "PASS" if ok else "FAIL"
        print(f"RESULT: {verdict} ({passed}/{len(self.cases)} echoes matched)", flush=True)
        return ok

    def case_rows(self) -> list[tuple[str, ...]]:
        """The summary table: a header, then one row per case, ending with its result."""
        rows = [("case", "method", "content", "sent as", "proof", "echo RTT", "echo via", "RSSI", "SNR", "result")]
        for case in self.cases:
            if case.sent_at is None:
                rows.append((str(case.index), case.method, f"{case.size} B", *["-"] * 6, case.result))
                continue
            rows.append(
                (
                    str(case.index),
                    case.method,
                    f"{case.size} B",
                    f"{case.representation} {case.packed_size} B",
                    opt(case.delivered_at and case.delivered_at - case.sent_at, "{:.1f} s"),
                    opt(case.echo_at and case.echo_at - case.sent_at, "{:.1f} s"),
                    opt(case.echo_method),
                    opt(case.echo_rssi),
                    opt(case.echo_snr),
                    case.result,
                )
            )
        return rows

    def exit_status(self) -> int:
        """Print the report; the process exit status is 0 only if everything passed."""
        return 0 if self.report() else 1

    def report_daemon_log(self) -> bool:
        print("\nPACKETS (tool WIRE packets matched to rnode-nomu TX/RX lines)")
        ok = True
        for packet in self.wire.packets:
            if packet.daemon is None:
                ok = False
                daemon = "NOT IN DAEMON LOG"
            else:
                d = packet.daemon
                daemon = f"daemon {d.frames} frame{'s' if d.frames != 1 else ''}"
                if d.direction == "rx":
                    daemon += f", RSSI {d.rssi} dBm SNR {d.snr} dB"
                elif d.tx_outcome != "ok":
                    ok = False
                    daemon += f", TX FAILED: {d.tx_outcome}"
            print(f"  {utc_clock(packet.t)} {packet.direction} {packet.length:3d} B {packet.description:<46} {daemon}")
        for direction in ("tx", "rx"):
            packets = [p for p in self.wire.packets if p.direction == direction]
            matched = [p for p in packets if p.daemon is not None]
            frames: dict[int, int] = {}
            for packet in matched:
                frames[packet.daemon.frames] = frames.get(packet.daemon.frames, 0) + 1
            by_frames = ", ".join(f"{count} x {n} frame{'s' if n != 1 else ''}" for n, count in sorted(frames.items()))
            extra = [d for d in self.unmatched_daemon if d.direction == direction]
            print(
                f"  daemon log {direction}  : {len(matched)}/{len(packets)} matched ({by_frames or 'none'}); "
                f"{len(extra)} daemon {direction.upper()} lines without a tool packet"
            )
            for d in extra:
                print(f"    unmatched: {d.line}")
            if len(matched) != len(packets):
                ok = False
        return ok


def raise_interrupt(signum, frame):
    raise KeyboardInterrupt


def install_interrupt_handlers() -> None:
    """Reticulum and LXMF install SIGINT/SIGTERM handlers that exit at once; turn both signals into
    KeyboardInterrupt in the main thread instead, so the session can clean up."""
    for signum in (signal.SIGINT, signal.SIGTERM):
        signal.signal(signum, raise_interrupt)


def build_cases(args: argparse.Namespace) -> list[Case]:
    run = secrets.token_hex(2)
    methods = args.methods * len(args.sizes) if len(args.methods) == 1 else args.methods
    rng = secrets.SystemRandom()
    cases = []
    for index, (method, size) in enumerate(zip(methods, args.sizes), start=1):
        # Letters and digits only: the content survives UTF-8 decoding and whitespace stripping.
        content = "".join(rng.choice(CONTENT_ALPHABET) for _ in range(size))
        cases.append(Case(index=index, method=method, size=size, title=f"ota-{run}-{index}", content=content))
    return cases


def check_case_sizes(cases: list[Case], LXMF) -> None:
    """LXMF silently sends an opportunistic message that does not fit one packet as a direct one;
    refuse such cases so the requested method is the one tested."""
    limit = LXMF.LXMessage.ENCRYPTED_PACKET_MAX_CONTENT
    for case in cases:
        needed = len(case.title.encode()) + case.size
        if case.method == "opportunistic" and needed > limit:
            fail_usage(
                f"opportunistic content of {case.size} B does not fit one packet (title and content may "
                f"take {limit} B, this message needs {needed}); use direct"
            )


def load_identity(RNS, path: str, wait: float = IDENTITY_WAIT_SECONDS):
    """The identity in the file at `path`, which is created (mode 0600) if it does not exist.

    The exclusive create decides which of two runs racing for a new file writes it; the other gets
    FileExistsError and loads the file instead, waiting up to `wait` seconds for the key to land."""
    os.makedirs(os.path.dirname(os.path.abspath(path)), mode=0o700, exist_ok=True)
    # Generated before the create, so that a single write fills the file right after it appears.
    identity = RNS.Identity()
    try:
        descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    except FileExistsError:
        return read_identity(RNS, path, wait)
    try:
        with os.fdopen(descriptor, "wb") as handle:
            handle.write(identity.get_private_key())
    except OSError as error:
        os.unlink(path)
        fail_usage(f"could not write the identity to {path}: {error}")
    say(f"created identity {RNS.prettyhexrep(identity.hash)} in {path}")
    return identity


def read_identity(RNS, path: str, wait: float):
    key_size = RNS.Identity.KEYSIZE // 8
    deadline = time.monotonic() + wait
    while True:
        try:
            with open(path, "rb") as handle:
                key = handle.read()
        except OSError as error:
            fail_usage(f"could not read the identity in {path}: {error}")
        # A short file can be a key that the run which just created it is still writing.
        if len(key) >= key_size or time.monotonic() >= deadline:
            break
        time.sleep(0.05)
    identity = None
    if len(key) == key_size:
        try:
            identity = RNS.Identity.from_bytes(key)
        except Exception:  # noqa: BLE001 - RNS raises on malformed key bytes
            identity = None
    if identity is None:
        fail_usage(f"could not load the identity in {path}: not a {key_size}-byte Reticulum identity key")
    say(f"loaded identity {RNS.prettyhexrep(identity.hash)} from {path}")
    return identity


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    cases = build_cases(args)
    try:
        import LXMF
        import RNS
    except ImportError as error:
        fail_usage(f"needs the rns and lxmf packages ({error}); install them with pip")
    check_case_sizes(cases, LXMF)

    wire = WireLog()
    rnode_class = instrument_rnode_interface(wire)
    say(f"rns {RNS.__version__}, lxmf {getattr(LXMF, '__version__', '?')}; OTA and WIRE times are UTC")
    identity = load_identity(RNS, args.identity)
    workdir = tempfile.mkdtemp(prefix="lxmf-echo-ota-", dir=args.workdir)
    session = Session(RNS, LXMF, args, workdir, wire, cases)
    session.write_config()
    try:
        session.run(rnode_class, identity)
    except KeyboardInterrupt:
        session.interrupted = True
        say("interrupted; shutting down")
    except Exception as error:  # noqa: BLE001 - always switch the radio off and report
        session.failure = f"{type(error).__name__}: {error}"
        say(f"error: {session.failure}")
    finally:
        try:
            session.shutdown()
        finally:
            shutil.rmtree(workdir, ignore_errors=True)
    return session.exit_status()


if __name__ == "__main__":
    code = main()
    sys.stdout.flush()
    sys.stderr.flush()
    # Reticulum and LXMF leave non-daemon state behind; exit without waiting for it, like RNS.exit().
    os._exit(code)
