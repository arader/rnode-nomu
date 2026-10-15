"""Unit tests for lxmf_echo_ota.py; no radio, Reticulum or LXMF needed (stand-ins replace them).

Run with: python3 -m unittest discover -s tools/hwtest -p 'test_*.py'
"""

import argparse
import contextlib
import io
import os
import shutil
import stat
import sys
import tempfile
import threading
import time
import unittest
from datetime import datetime, timezone
from unittest import mock

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import lxmf_echo_ota as ota  # noqa: E402

ECHO = bytes.fromhex("00112233445566778899aabbccddeeff")
BASE_ARGS = [
    "--port", "rnode0",
    "--frequency", "914875000",
    "--bandwidth", "125000",
    "--sf", "8",
    "--cr", "5",
    "--echo", ECHO.hex(),
    "--identity", "identity",
]
T0 = datetime(2026, 10, 13, 21, 0, 0, tzinfo=timezone.utc).timestamp()


def args_with(*extra):
    return ota.parse_args(BASE_ARGS + list(extra))


def refusal(*extra):
    """parse_args() must exit with status 2; return what it printed."""
    stderr = io.StringIO()
    with contextlib.redirect_stderr(stderr), unittest.TestCase().assertRaises(SystemExit) as raised:
        args_with(*extra)
    assert raised.exception.code == 2, raised.exception.code
    return stderr.getvalue()


def header_1(flags, destination, context, hops=0, payload=40):
    return bytes([flags, hops]) + destination + bytes([context]) + bytes(payload)


def header_2(flags, transport_id, destination, context, hops=0, payload=40):
    return bytes([flags, hops]) + transport_id + destination + bytes([context]) + bytes(payload)


def daemon_line(seconds, text):
    stamp = datetime.fromtimestamp(T0 + seconds, timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    return f"[{stamp} DEBUG rnode_core::node] {text}"


class FakeTransport:
    """RNS.Transport stand-in: knows the path from the start, after a number of requests, or never."""

    def __init__(self, known=False, after_requests=None):
        self.known = known
        self.after_requests = after_requests
        self.requests = 0

    def has_path(self, destination):
        return self.known or (self.after_requests is not None and self.requests >= self.after_requests)

    def request_path(self, destination):
        self.requests += 1

    def hops_to(self, destination):
        return 2


class FakeIdentity:
    """RNS.Identity stand-in with 64-byte keys; from_bytes() raises on a bad key, as RNS does."""

    KEYSIZE = 512

    def __init__(self, key=None):
        self.key = os.urandom(64) if key is None else key
        self.hash = self.key[:16]

    def get_private_key(self):
        return self.key

    @staticmethod
    def from_bytes(key):
        if len(key) != 64:
            raise ValueError("bad key length")
        return FakeIdentity(key)

    @staticmethod
    def recall(destination_hash):
        identity = FakeIdentity()
        identity.destination_hash = destination_hash
        return identity


class FakeDestination:
    """RNS.Destination stand-in: an OUT destination gets the hash its identity was recalled for."""

    OUT = "out"
    SINGLE = "single"

    def __init__(self, identity, direction, kind, app_name, *aspects):
        self.hash = identity.destination_hash

    @staticmethod
    def hash_from_name_and_identity(name, identity):
        return name.encode()[:16].ljust(16, b"\0")


class FakeReticulum:
    """RNS.Reticulum stand-in: shutdown() detaches the RNodeInterface through exit_handler()."""

    @staticmethod
    def exit_handler():
        pass


class FakeRNS:
    Identity = FakeIdentity
    Destination = FakeDestination
    Reticulum = FakeReticulum

    def __init__(self, transport=None):
        self.Transport = transport

    @staticmethod
    def prettyhexrep(data):
        return f"<{data.hex()}>"


class FakeLXMessage:
    """LXMF.LXMessage stand-in for an outgoing message, as run_case() builds it."""

    OPPORTUNISTIC = 0x01
    DIRECT = 0x02
    PACKET = 0x01
    RESOURCE = 0x02

    def __init__(self, destination, source, content, title, desired_method=None):
        self.title = title.encode()
        self.content = content.encode()
        self.desired_method = desired_method
        self.representation = FakeLXMessage.PACKET
        self.packed_size = 112 + len(self.title) + len(self.content)
        self.delivered = None
        self.failed = None

    def register_delivery_callback(self, callback):
        self.delivered = callback

    def register_failed_callback(self, callback):
        self.failed = callback


class FakeLXMF:
    LXMessage = FakeLXMessage


class Reply:
    """An incoming LXMF message as LXMRouter hands it to the delivery callback."""

    def __init__(self, title, content, source=ECHO, method=FakeLXMessage.OPPORTUNISTIC, rssi=-51, snr=13.5):
        self.title = title if isinstance(title, bytes) else title.encode()
        self.content = content if isinstance(content, bytes) else content.encode()
        self.source_hash = source
        self.method = method
        self.signature_validated = True
        self.rssi = rssi
        self.snr = snr


class FakeRouter:
    """LXMRouter stand-in: proves each outgoing message, then delivers the replies `answer` makes
    for it to the session, as the echo peer and LXMRouter would."""

    def __init__(self, session, answer=None):
        self.session = session
        self.answer = answer

    def handle_outbound(self, message):
        message.delivered(message)
        for reply in self.answer(message) if self.answer else []:
            self.session.on_message(reply)


def quietly(function, *args, **kwargs):
    """Call function, returning its result and what it printed."""
    stdout = io.StringIO()
    with contextlib.redirect_stdout(stdout):
        result = function(*args, **kwargs)
    return result, stdout.getvalue()


class SafetyLimits(unittest.TestCase):
    def test_refuses_frequencies_outside_the_us_ism_band(self):
        for frequency in ("868100000", "901999999", "928000001"):
            message = refusal("--frequency", frequency)
            self.assertIn("refusing to transmit or configure outside US 902-928 MHz ISM band", message)

    def test_accepts_the_band_edges(self):
        self.assertEqual(args_with("--frequency", "902000000").frequency, 902_000_000)
        self.assertEqual(args_with("--frequency", "928000000").frequency, 928_000_000)

    def test_refuses_tx_power_above_10_dbm(self):
        self.assertIn("refusing TX power above 10 dBm; requested 11 dBm", refusal("--txpower", "11"))
        self.assertEqual(args_with("--txpower", "10").txpower, 10)
        self.assertEqual(args_with().txpower, 2)

    def test_refuses_negative_power_and_invalid_modulation(self):
        self.assertIn("at least 0 dBm", refusal("--txpower", "-1"))
        self.assertIn("spreading factor", refusal("--sf", "13"))
        self.assertIn("coding rate", refusal("--cr", "9"))
        self.assertIn("bandwidth", refusal("--bandwidth", "5000"))


class Arguments(unittest.TestCase):
    def test_refuses_no_path_attempts_and_waits_that_are_not_positive(self):
        # Review 5940: each of these refusals had no test.
        for extra, message in (
            (("--path-attempts", "0"), "--path-attempts must be at least 1"),
            (("--timeout", "0"), "--timeout must be positive"),
            (("--path-wait", "-1"), "--path-wait must be positive"),
            (("--online-timeout", "0"), "--online-timeout must be positive"),
            (("--settle", "-1"), "--settle must not be negative"),
        ):
            with self.subTest(extra=extra):
                self.assertIn(message, refusal(*extra))
        args = args_with(
            "--path-attempts", "1", "--timeout", "0.5", "--path-wait", "0.5", "--online-timeout", "0.5", "--settle", "0"
        )
        self.assertEqual(
            (args.path_attempts, args.timeout, args.path_wait, args.online_timeout, args.settle), (1, 0.5, 0.5, 0.5, 0)
        )
    def test_echo_hash_accepts_plain_and_bracketed_hex(self):
        self.assertEqual(ota.parse_hash(ECHO.hex()), ECHO)
        self.assertEqual(ota.parse_hash(f"<{ECHO.hex()}>"), ECHO)
        for bad in ("0a81f2", ECHO.hex() + "00", "zz" * 16):
            with self.assertRaises(argparse.ArgumentTypeError):
                ota.parse_hash(bad)

    def test_sizes_and_methods(self):
        self.assertEqual(ota.parse_sizes("200, 300,2000"), [200, 300, 2000])
        self.assertEqual(ota.parse_methods("Opportunistic,direct"), ["opportunistic", "direct"])
        for bad in ("0", "abc", str(ota.MAX_CONTENT_BYTES + 1), ","):
            with self.assertRaises(argparse.ArgumentTypeError):
                ota.parse_sizes(bad)
        with self.assertRaises(argparse.ArgumentTypeError):
            ota.parse_methods("propagated")

    def test_methods_are_one_for_all_sizes_or_one_per_size(self):
        self.assertIn("one per size (3); got 2", refusal("--sizes", "1,2,3", "--methods", "direct,direct"))
        cases = ota.build_cases(args_with("--sizes", "200,300", "--methods", "direct"))
        self.assertEqual([case.method for case in cases], ["direct", "direct"])
        cases = ota.build_cases(args_with("--sizes", "200,300", "--methods", "opportunistic,direct"))
        self.assertEqual([(case.method, case.size) for case in cases], [("opportunistic", 200), ("direct", 300)])

    def test_cases_have_exact_alphanumeric_content_and_unique_titles(self):
        cases = ota.build_cases(args_with("--sizes", "1,200,2000", "--methods", "direct"))
        self.assertEqual([len(case.content) for case in cases], [1, 200, 2000])
        self.assertTrue(all(case.content.isascii() and case.content.isalnum() for case in cases))
        self.assertEqual(len({case.title for case in cases}), 3)

    def test_opportunistic_messages_must_fit_one_packet(self):
        class LXMessage:
            ENCRYPTED_PACKET_MAX_CONTENT = 295

        class LXMF:
            pass

        LXMF.LXMessage = LXMessage
        fits = ota.build_cases(args_with("--sizes", "200", "--methods", "opportunistic"))
        ota.check_case_sizes(fits, LXMF)
        too_big = ota.build_cases(args_with("--sizes", "290", "--methods", "opportunistic"))
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr), self.assertRaises(SystemExit):
            ota.check_case_sizes(too_big, LXMF)
        self.assertIn("does not fit one packet", stderr.getvalue())
        ota.check_case_sizes(ota.build_cases(args_with("--sizes", "2000", "--methods", "direct")), LXMF)


class PacketDescriptions(unittest.TestCase):
    def setUp(self):
        self.wire = ota.WireLog()
        self.wire.aliases[ECHO] = "echo"

    def test_header_2_data_to_a_known_destination(self):
        raw = header_2(0x50, bytes(16), ECHO, 0x00)
        self.assertEqual(self.wire.describe(raw), "DATA single echo hdr2 hops=0")

    def test_path_response_announce(self):
        raw = header_2(0x51, bytes(16), ECHO, 0x0B, hops=1)
        self.assertEqual(self.wire.describe(raw), "ANNOUNCE single echo PATH_RESPONSE hdr2 hops=1")

    def test_link_request_proof_to_an_unknown_link(self):
        raw = header_1(0x0F, bytes.fromhex("11" * 16), 0xFF)
        self.assertEqual(self.wire.describe(raw), "PROOF link 11111111 LRPROOF hdr1 hops=0")

    def test_short_and_ifac_packets(self):
        self.assertEqual(self.wire.describe(bytes(5)), "malformed (5 B)")
        self.assertEqual(self.wire.describe(bytes([0x50, 0]) + bytes(20)), "malformed (22 B)")
        self.assertEqual(self.wire.describe(bytes([0x80, 2]) + bytes(40)), "IFAC-authenticated hops=2")


class DaemonLogCrossCheck(unittest.TestCase):
    LINES = (
        daemon_line(-60, "TX packet: 51 bytes, 1 frame, header 0x10 (seq 1): ok"),
        daemon_line(1, "TX packet: 51 bytes, 1 frame, header 0x10 (seq 1): ok"),
        daemon_line(3, "RX packet: 223 bytes, 1 frame, header 0x20 (seq 2), RSSI -51 dBm, SNR 14.00 dB"),
        daemon_line(9, "TX packet: 435 bytes, 2 frames, header 0x31 (seq 3, split): ok"),
        daemon_line(12, "RX packet: 419 bytes, 2 frames, header 0x41 (seq 4, split), RSSI -51 dBm, SNR 13.50 dB"),
        daemon_line(
            13,
            "TX packet: 83 bytes, 1 frame, header 0x50 (seq 5): modem timeout on frame 1/1; "
            "sent ERROR_MODEM_TIMEOUT, ERROR_TXFAILED; hard reset",
        ),
        daemon_line(14, "RX packet: 99 bytes, 1 frame, header 0x60 (seq 6), RSSI -50 dBm, SNR 12.75 dB"),
        daemon_line(15, "INFO line that is not a packet"),
        "[2026-10-13T21:00:15Z INFO  rnode_core::node] radio offline (host request): 914.875 MHz",
    )

    def test_parses_packet_lines_within_the_window(self):
        packets = ota.parse_daemon_log(self.LINES, T0, T0 + 20)
        self.assertEqual(
            [(p.direction, p.length, p.frames, p.t - T0) for p in packets],
            [
                ("tx", 51, 1, 1),
                ("rx", 223, 1, 3),
                ("tx", 435, 2, 9),
                ("rx", 419, 2, 12),
                ("tx", 83, 1, 13),
                ("rx", 99, 1, 14),
            ],
        )
        self.assertEqual((packets[1].rssi, packets[1].snr, packets[1].tx_outcome), (-51, 14.0, None))
        self.assertEqual(packets[0].tx_outcome, "ok")
        self.assertTrue(packets[4].tx_outcome.startswith("modem timeout on frame 1/1"))

    def test_matches_wire_packets_in_order_within_their_windows(self):
        daemon = ota.parse_daemon_log(self.LINES, T0, T0 + 20)
        wire = [
            ota.WirePacket(T0 + 0.5, "tx", 51, ""),
            ota.WirePacket(T0 + 3.4, "rx", 223, ""),
            ota.WirePacket(T0 + 7.9, "tx", 435, ""),
            ota.WirePacket(T0 + 12.2, "rx", 419, ""),
            ota.WirePacket(T0 + 12.9, "tx", 83, ""),
            # Logged at 14 s but seen by the host 3.5 s later: outside the RX window.
            ota.WirePacket(T0 + 17.5, "rx", 99, ""),
        ]
        unmatched = ota.cross_check(wire, daemon)
        self.assertEqual([p.daemon is not None for p in wire], [True, True, True, True, True, False])
        self.assertEqual([p.daemon.frames for p in wire[:5]], [1, 1, 2, 2, 1])
        self.assertEqual([(d.direction, d.length) for d in unmatched], [("rx", 99)])

    def test_equal_lengths_pair_in_order(self):
        daemon = [
            ota.DaemonPacket(T0 + 2, "tx", 99, 1, None, None, "ok", "first"),
            ota.DaemonPacket(T0 + 3, "tx", 99, 1, None, None, "ok", "second"),
        ]
        wire = [ota.WirePacket(T0 + 1.0, "tx", 99, ""), ota.WirePacket(T0 + 1.1, "tx", 99, "")]
        self.assertEqual(ota.cross_check(wire, daemon), [])
        self.assertEqual([p.daemon.line for p in wire], ["first", "second"])


class DaemonLogVerdict(unittest.TestCase):
    """With --daemon-log, shutdown() matches the tool's packets to the daemon log and report() fails
    the run when a packet is missing from it or a transmission failed (review 5939)."""

    LINES = (
        daemon_line(-60, "TX packet: 51 bytes, 1 frame, header 0x10 (seq 1): ok"),
        daemon_line(1, "TX packet: 51 bytes, 1 frame, header 0x10 (seq 1): ok"),
        daemon_line(3, "RX packet: 223 bytes, 1 frame, header 0x20 (seq 2), RSSI -51 dBm, SNR 14.00 dB"),
        daemon_line(9, "TX packet: 435 bytes, 2 frames, header 0x31 (seq 3, split): ok"),
        daemon_line(12, "RX packet: 419 bytes, 2 frames, header 0x41 (seq 4, split), RSSI -51 dBm, SNR 13.50 dB"),
    )
    WIRE = ((0.5, "tx", 51), (3.4, "rx", 223), (7.9, "tx", 435), (12.2, "rx", 419))

    def setUp(self):
        self.scratch = tempfile.mkdtemp(prefix=".scratch-", dir=HERE)
        self.addCleanup(shutil.rmtree, self.scratch)
        self.log = os.path.join(self.scratch, "rnode-linux.log")

    def shut_down(self, lines):
        """A session that saw the WIRE packets and shut down with `lines` in the daemon log, or with
        no daemon log if `lines` is None."""
        if lines is not None:
            with open(self.log, "w", encoding="utf-8") as handle:
                handle.write("\n".join(lines) + "\n")
        session = ota.Session(FakeRNS(), None, args_with("--daemon-log", self.log), ".", ota.WireLog(), [])
        session.started = T0
        for seconds, direction, length in self.WIRE:
            session.wire.packets.append(ota.WirePacket(T0 + seconds, direction, length, f"{direction} packet"))
        _, printed = quietly(session.shutdown)
        self.assertNotIn("failed", printed)
        return session

    def verdict(self, session):
        """report_daemon_log()'s and report()'s results, and what report() printed."""
        daemon_ok, _ = quietly(session.report_daemon_log)
        ok, report = quietly(session.report)
        return daemon_ok, ok, report

    def test_every_packet_in_the_log_passes(self):
        # A daemon line without a tool packet is listed but does not fail the run; the line from
        # before the session is not even read.
        extra = daemon_line(14, "RX packet: 99 bytes, 1 frame, header 0x60 (seq 6), RSSI -50 dBm, SNR 12.75 dB")
        daemon_ok, ok, report = self.verdict(self.shut_down(self.LINES + (extra,)))
        self.assertEqual((daemon_ok, ok), (True, True), report)
        self.assertNotIn("NOT IN DAEMON LOG", report)
        self.assertNotIn("TX FAILED", report)
        self.assertIn(
            "daemon log tx  : 2/2 matched (1 x 1 frame, 1 x 2 frames); 0 daemon TX lines without a tool packet", report
        )
        self.assertIn(
            "daemon log rx  : 2/2 matched (1 x 1 frame, 1 x 2 frames); 1 daemon RX lines without a tool packet", report
        )
        self.assertIn(f"    unmatched: {extra}", report)
        self.assertIn("RESULT: PASS", report)

    def test_a_packet_missing_from_the_log_fails(self):
        for missing, direction, summary in (
            (3, "tx", "daemon log tx  : 1/2 matched (1 x 1 frame); 0 daemon TX lines"),
            (4, "rx", "daemon log rx  : 1/2 matched (1 x 1 frame); 0 daemon RX lines"),
        ):
            with self.subTest(direction=direction):
                lines = self.LINES[:missing] + self.LINES[missing + 1 :]
                daemon_ok, ok, report = self.verdict(self.shut_down(lines))
                self.assertEqual((daemon_ok, ok), (False, False), report)
                flagged = [line for line in report.splitlines() if "NOT IN DAEMON LOG" in line]
                self.assertEqual(len(flagged), 1, report)
                self.assertIn(f"{direction} {self.WIRE[missing - 1][2]} B", flagged[0])
                self.assertIn(summary, report)
                self.assertIn("RESULT: FAIL", report)

    def test_a_failed_transmission_fails(self):
        failure = "modem timeout on frame 2/2; sent ERROR_MODEM_TIMEOUT, ERROR_TXFAILED; hard reset"
        lines = self.LINES[:3] + (self.LINES[3].replace(": ok", f": {failure}"),) + self.LINES[4:]
        session = self.shut_down(lines)
        daemon_ok, ok, report = self.verdict(session)
        self.assertEqual((daemon_ok, ok), (False, False), report)
        self.assertNotIn("NOT IN DAEMON LOG", report)
        flagged = [line for line in report.splitlines() if "TX FAILED" in line]
        self.assertEqual(len(flagged), 1, report)
        self.assertIn("tx 435 B", flagged[0])
        self.assertTrue(flagged[0].endswith(f"daemon 2 frames, TX FAILED: {failure}"), flagged[0])
        self.assertIn("daemon log tx  : 2/2 matched", report)
        self.assertEqual(quietly(session.exit_status)[0], 1)

    def test_an_unreadable_log_fails(self):
        ok, report = quietly(self.shut_down(None).report)
        self.assertFalse(ok, report)
        self.assertIn(f"daemon log     : could not read {self.log}: ", report)
        self.assertIn("RESULT: FAIL", report)



class PathDiscovery(unittest.TestCase):
    def session(self, transport, *extra):
        args = args_with("--path-attempts", "2", "--path-wait", "0.05", *extra)
        return ota.Session(FakeRNS(transport), None, args, ".", ota.WireLog(), [])

    def path_line(self, session):
        ok, report = quietly(session.report)
        lines = [line for line in report.splitlines() if line.lstrip().startswith("path ")]
        self.assertEqual(len(lines), 1, report)
        return ok, lines[0]

    def test_path_already_known_sends_no_request_and_says_so(self):
        transport = FakeTransport(known=True)
        session = self.session(transport)
        quietly(session.wire.record, "rx", header_2(0x51, bytes(16), ECHO, 0x00, hops=1), -60, 9.5)
        found, log = quietly(session.find_echo)
        self.assertTrue(found)
        self.assertEqual(transport.requests, 0)
        self.assertTrue(session.path_known_before_request)
        self.assertEqual(session.path_attempts_used, 0)
        self.assertIn("path already known (heard an announce); no path request sent", log)
        ok, line = self.path_line(session)
        self.assertTrue(ok)
        self.assertEqual(
            line.strip(),
            "path           : already known (heard an announce), no request sent, 2 hops, "
            "announce RSSI -60 dBm SNR 9.5 dB",
        )
        self.assertNotIn("request 0/", line)

    def test_path_found_after_the_first_request(self):
        transport = FakeTransport(after_requests=1)
        session = self.session(transport)
        quietly(session.wire.record, "rx", header_2(0x51, bytes(16), ECHO, 0x0B, hops=1), -51, 14.0)
        found, _ = quietly(session.find_echo)
        self.assertTrue(found)
        self.assertEqual((transport.requests, session.path_attempts_used), (1, 1))
        self.assertFalse(session.path_known_before_request)
        _, line = self.path_line(session)
        self.assertEqual(
            line.strip(),
            "path           : after request 1/2, 2 hops, path response RSSI -51 dBm SNR 14.0 dB",
        )

    def test_announce_heard_during_start_up_keeps_its_name_and_rssi(self):
        # Review 5926: an echo announce recorded while the interface comes up must be named and found.
        transport = FakeTransport(known=True)
        session = self.session(transport, "--settle", "0")
        announce = header_2(0x51, bytes(16), ECHO, 0x00, hops=1)

        def start_reticulum(rnode_class):
            session.wire.record("rx", announce, -60, 9.5)
            return True

        session.start_reticulum = start_reticulum
        session.start_lxmf = lambda identity: None
        quietly(session.run, None, None)
        self.assertIsNone(session.failure)
        self.assertEqual(transport.requests, 0)
        self.assertEqual(session.wire.packets[0].description, "ANNOUNCE single echo hdr2 hops=1")
        self.assertIs(session.path_packet, session.wire.packets[0])
        ok, line = self.path_line(session)
        self.assertTrue(ok)
        self.assertEqual(
            line.strip(),
            "path           : already known (heard an announce), no request sent, 2 hops, "
            "announce RSSI -60 dBm SNR 9.5 dB",
        )

    def test_path_packet_is_found_without_aliases(self):
        # The lookup uses the parsed header, so a packet described by its raw hash still counts.
        transport = FakeTransport(known=True)
        session = self.session(transport)
        quietly(session.wire.record, "rx", header_2(0x51, bytes(16), ECHO, 0x00, hops=1), -60, 9.5)
        self.assertEqual(session.wire.packets[0].description, "ANNOUNCE single 00112233 hdr2 hops=1")
        quietly(session.find_echo)
        self.assertIs(session.path_packet, session.wire.packets[0])

    def test_no_path_after_every_request(self):
        transport = FakeTransport()
        session = self.session(transport)
        found, log = quietly(session.find_echo)
        self.assertFalse(found)
        self.assertEqual((transport.requests, session.path_attempts_used), (2, 2))
        self.assertEqual(log.count("no path after 0.05 s"), 2)
        self.assertEqual(session.failure, "no path to the echo destination after 2 requests")
        ok, line = self.path_line(session)
        self.assertFalse(ok)
        self.assertEqual(line.strip(), "path           : not found after 2 requests")


class IdentityFile(unittest.TestCase):
    def setUp(self):
        # A scratch directory beside the tests rather than in the system temporary directory.
        self.scratch = tempfile.mkdtemp(prefix=".scratch-", dir=HERE)
        self.addCleanup(shutil.rmtree, self.scratch)
        self.path = os.path.join(self.scratch, "ota", "identity")

    def load(self, wait=ota.IDENTITY_WAIT_SECONDS):
        identity, _ = quietly(ota.load_identity, FakeRNS(), self.path, wait)
        return identity

    def read_file(self):
        with open(self.path, "rb") as handle:
            return handle.read()

    def test_creates_a_private_file_and_reuses_it(self):
        created = self.load()
        self.assertEqual(stat.S_IMODE(os.stat(self.path).st_mode), 0o600)
        self.assertEqual(stat.S_IMODE(os.stat(os.path.dirname(self.path)).st_mode) & 0o077, 0)
        self.assertEqual(self.read_file(), created.key)
        again = self.load()
        self.assertEqual(again.key, created.key)
        self.assertEqual(self.read_file(), created.key)

    def test_loads_a_file_another_run_created_first(self):
        # The race from review: an existence check would still say "absent" while another run has
        # already created the file. The exclusive create must fail over to loading that run's key,
        # not crash and not overwrite it.
        os.makedirs(os.path.dirname(self.path))
        other = os.urandom(64)
        with open(self.path, "wb") as handle:
            handle.write(other)
        exists = os.path.exists
        with mock.patch("os.path.exists", side_effect=lambda path: path != self.path and exists(path)):
            self.assertEqual(self.load().key, other)
        self.assertEqual(self.read_file(), other)

    def test_waits_for_a_key_that_lands_after_the_exclusive_create(self):
        os.makedirs(os.path.dirname(self.path))
        os.close(os.open(self.path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600))
        late = os.urandom(64)

        def other_run_writes_its_key():
            time.sleep(0.2)
            with open(self.path, "r+b") as handle:
                handle.write(late)

        writer = threading.Thread(target=other_run_writes_its_key)
        writer.start()
        self.addCleanup(writer.join)
        self.assertEqual(self.load(wait=5.0).key, late)

    def test_refuses_a_file_that_never_holds_a_key(self):
        os.makedirs(os.path.dirname(self.path))
        with open(self.path, "wb") as handle:
            handle.write(b"not a key")
        stderr = io.StringIO()
        started = time.monotonic()
        with contextlib.redirect_stderr(stderr), self.assertRaises(SystemExit) as raised:
            self.load(wait=0.2)
        self.assertEqual(raised.exception.code, 2)
        self.assertGreaterEqual(time.monotonic() - started, 0.2)
        self.assertIn("could not load the identity in", stderr.getvalue())
        self.assertEqual(self.read_file(), b"not a key")



class EchoVerdict(unittest.TestCase):
    """run_case() -> LXMRouter -> on_message(): the comparison that decides PASS or FAIL."""

    def session(self, sizes="200"):
        args = args_with("--sizes", sizes, "--methods", "opportunistic", "--timeout", "0.1")
        cases = ota.build_cases(args)
        session = ota.Session(FakeRNS(FakeTransport(known=True)), FakeLXMF, args, ".", ota.WireLog(), cases)
        session.source = object()
        return session

    def send(self, session, answer=None):
        """Send the first case through a stand-in router whose echo peer answers with answer(message)."""
        session.router = FakeRouter(session, answer)
        return quietly(session.run_case, object(), session.cases[0])[1]

    def report(self, session):
        """The exit status main() would return, and the printed summary row of each case."""
        status, printed = quietly(session.exit_status)
        rows = {}
        for line in printed.splitlines():
            cells = line.split()
            if cells and cells[0].isdigit():
                rows[int(cells[0])] = line.rstrip()
        return status, rows

    def test_matching_echo_passes(self):
        session = self.session()
        case = session.cases[0]

        def echo(message):
            return [Reply(message.title, message.content)]

        log = self.send(session, echo)
        self.assertTrue(case.content_ok)
        self.assertTrue(case.ok)
        self.assertEqual((case.echo_method, case.echo_rssi, case.echo_snr), ("opportunistic", -51, 13.5))
        self.assertIn(f"echo 1 ({case.title})", log)
        self.assertIn("content and title match", log)
        self.assertEqual(session.case_rows()[1][-1], "ok")
        status, rows = self.report(session)
        self.assertEqual(status, 0)
        self.assertTrue(rows[1].endswith("  ok"), rows[1])

    def test_mismatched_content_fails(self):
        session = self.session()
        case = session.cases[0]

        def echo_changed(message):
            return [Reply(message.title, message.content[:-1] + b"!")]

        log = self.send(session, echo_changed)
        self.assertIsNotNone(case.echo_at)
        self.assertFalse(case.content_ok)
        self.assertFalse(case.ok)
        self.assertIn("CONTENT MISMATCH", log)
        self.assertEqual(session.case_rows()[1][-1], "MISMATCH")
        status, rows = self.report(session)
        self.assertEqual(status, 1)
        self.assertTrue(rows[1].endswith("  MISMATCH"), rows[1])

    def test_unknown_title_or_other_source_is_ignored(self):
        session = self.session()
        case = session.cases[0]
        other = bytes.fromhex("ffeeddccbbaa99887766554433221100")

        def strays(message):
            return [Reply("not-a-case-title", message.content), Reply(message.title, message.content, source=other)]

        log = self.send(session, strays)
        self.assertIn(f"ignoring LXMF message from <{ECHO.hex()}> titled 'not-a-case-title'", log)
        self.assertIn(f"ignoring LXMF message from <{other.hex()}> titled '{case.title}'", log)
        self.assertIsNone(case.echo_at)
        self.assertIsNone(case.content_ok)
        self.assertIn("case 1: no echo within 0.1 s", log)
        status, rows = self.report(session)
        self.assertEqual(status, 1)
        self.assertTrue(rows[1].endswith("  NO ECHO"), rows[1])

    def test_no_echo_before_the_timeout_fails(self):
        session = self.session()
        case = session.cases[0]
        started = time.monotonic()
        log = self.send(session)
        self.assertGreaterEqual(time.monotonic() - started, 0.1)
        self.assertIsNotNone(case.delivered_at)
        self.assertIsNone(case.echo_at)
        self.assertFalse(case.ok)
        self.assertIn("case 1: no echo within 0.1 s", log)
        self.assertEqual(session.case_rows()[1][-1], "NO ECHO")
        status, rows = self.report(session)
        self.assertEqual(status, 1)
        self.assertTrue(rows[1].endswith("  NO ECHO"), rows[1])

    def test_duplicate_echo_keeps_the_first_verdict(self):
        session = self.session()
        case = session.cases[0]

        def echo_twice(message):
            return [Reply(message.title, message.content, rssi=-51), Reply(message.title, b"different", rssi=-90)]

        log = self.send(session, echo_twice)
        self.assertIn("duplicate echo for case 1, ignored", log)
        self.assertTrue(case.ok)
        self.assertEqual(case.echo_rssi, -51)
        self.assertEqual(self.report(session)[0], 0)

    def test_reply_without_phy_stats_takes_the_last_received_packet(self):
        session = self.session()
        case = session.cases[0]
        quietly(session.wire.record, "rx", header_1(0x00, bytes(16), 0x00), -48, 12.25)

        def echo_without_phy(message):
            return [Reply(message.title, message.content, rssi=None, snr=None)]

        self.send(session, echo_without_phy)
        self.assertTrue(case.ok)
        self.assertEqual((case.echo_rssi, case.echo_snr), (-48, 12.25))

    def test_unsent_cases_fail_the_run(self):
        session = self.session("200,300")

        def echo(message):
            return [Reply(message.title, message.content)]

        self.send(session, echo)
        self.assertEqual([case.result for case in session.cases], ["ok", "not sent"])
        status, rows = self.report(session)
        self.assertEqual(status, 1)
        self.assertTrue(rows[1].endswith("  ok"), rows[1])
        self.assertTrue(rows[2].endswith("  not sent"), rows[2])

if __name__ == "__main__":
    unittest.main()
