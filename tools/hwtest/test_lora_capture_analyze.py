"""Unit tests for lora_capture_analyze.py.

The time-on-air and low-data-rate tests need only the standard library. The signal-processing tests
build synthetic captures with numpy, which the analyzer needs, and are skipped without it. CI runs
these tests twice: with the standard library alone, which checks that the analyzer's pure helpers
import without numpy, and in a venv with the numpy that requirements-test.txt pins, where
HWTEST_REQUIRE_NUMPY=1 turns a numpy that does not import into an error instead of skipped tests.
tools/hwtest/README.md ("Unit tests") shows how to run them as CI does.
"""

import contextlib
import io
import json
import math
import os
import re
import shutil
import sys
import tempfile
import unittest
from unittest import mock

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import lora_capture_analyze as analyze  # noqa: E402

np = analyze.np
if np is None and os.environ.get("HWTEST_REQUIRE_NUMPY") == "1":
    raise ImportError("HWTEST_REQUIRE_NUMPY=1, but numpy does not import")
needs_numpy = unittest.skipIf(np is None, "needs numpy: see tools/hwtest/README.md, Unit tests")

# The modulation of the README's captures, recorded by an RTL-SDR at 1 MS/s.
FS = 1_000_000.0
BW = 125_000.0
SF = 7
CHIPS = 1 << SF
SPS = 1024  # samples per symbol, FS * CHIPS / BW
WINDOW = SPS // 4  # the moving average of find_bursts()
BIN_HZ = BW / CHIPS  # 976.5625 Hz: one symbol bin, and one FFT bin of a dechirped symbol at FS
OFFSET_HZ = -3 * BIN_HZ  # the carrier offset of the README's on-air result, -2929.7 Hz
PAYLOAD = [(37 * i + 11) % CHIPS for i in range(18)]  # the 18 payload symbols of a 2-byte frame


class TimeOnAir(unittest.TestCase):
    """semtech_toa_ms() against the time on air of the reference board's transmissions."""

    def test_sf7_frames_of_the_bring_up_and_sdr_captures(self):
        # SF7, 125 kHz, CR 4/5, 24 preamble symbols (rnode-core's PhyParams at SF7/125 kHz),
        # explicit header, CRC, no LDRO: the values the bring-up printed (README, "Bring-up
        # results") and the analyzer expected (README, "On-air result";
        # captures/stage3_daemon_split_2026-10-13.txt). The SDR measured 2, 50 and 255-byte frames
        # 0.333-0.335 ms longer than these, the same for every length, so the per-byte slope holds.
        for payload_len, expected_ms in ((1, 42.240), (2, 47.360), (50, 113.920), (255, 416.000)):
            with self.subTest(payload_len=payload_len):
                self.assertAlmostEqual(analyze.semtech_toa_ms(payload_len, 7, BW, 5, False, 24), expected_ms, places=9)

    def test_low_data_rate_optimisation_and_slow_modulations(self):
        # 50 bytes with 18 preamble symbols at SF12/125 kHz (LDRO on) and SF9/31.25 kHz (LDRO
        # off): the bring-up's figures on the reference board (README, "Bring-up results").
        self.assertAlmostEqual(analyze.semtech_toa_ms(50, 12, 125_000, 5, True, 18), 2629.632, places=9)
        self.assertAlmostEqual(analyze.semtech_toa_ms(50, 9, 31_250, 5, False, 18), 1478.656, places=9)

    def test_header_crc_and_coding_rate_terms(self):
        # Derived by hand from the Semtech formula.
        # 8 bytes, implicit header, no CRC: ceil((64 - 28 + 28 - 20) / 28) = 2 blocks of 5
        # symbols, so (8 + 4.25 + 8 + 10) symbols of 1.024 ms; the header's or the CRC's bits
        # would make it 3.
        self.assertAlmostEqual(
            analyze.semtech_toa_ms(8, 7, BW, 5, False, 8, implicit=True, crc=False), 30.976, places=9
        )
        # 255 bytes at CR 4/8: ceil(2056 / 28) = 74 blocks of 8 symbols, (24 + 4.25 + 8 + 592) symbols.
        self.assertAlmostEqual(analyze.semtech_toa_ms(255, 7, BW, 8, False, 24), 643.328, places=9)
        # An empty frame without header or CRC at SF12 with LDRO: ceil(-40 / 40) = -1 blocks,
        # clamped to none, leaving (8 + 4.25 + 8) symbols of 32.768 ms.
        self.assertAlmostEqual(
            analyze.semtech_toa_ms(0, 12, BW, 5, True, 8, implicit=True, crc=False), 663.552, places=9
        )

    def test_sf5_and_sf6_frames(self):
        # Below SF7 the SX1262 sends 2 more symbols and its block count has no 8 extra bits, unlike
        # the SX127x (#79). The values of Semtech's LoRaMac-node, RadioGetLoRaTimeOnAirNumerator()
        # in src/radio/sx126x/radio.c, compiled and run for these frames (it reproduces the SF7 to
        # SF12 values above too). 94 and 47 preamble symbols are rnode-core's PhyParams at SF5 and
        # SF6, the firmware's; LoRaMac-node needs at least 12.
        for args, expected_ms in (
            ((255, 5, 125_000, 5, False, 94), 159.552),
            ((100, 6, 125_000, 5, False, 47), 118.400),
            ((10, 6, 125_000, 5, False, 12), 23.680),
            ((50, 5, 500_000, 5, False, 94), 13.648),
        ):
            with self.subTest(args=args):
                self.assertAlmostEqual(analyze.semtech_toa_ms(*args), expected_ms, places=9)
        # 6 bytes at SF6 without header or CRC: ceil((48 - 24 + 20 - 20) / 24) = 1 block, which
        # 8 more bits, the header or the CRC would make 2.
        self.assertAlmostEqual(
            analyze.semtech_toa_ms(6, 6, BW, 5, False, 12, implicit=True, crc=False), 16.000, places=9
        )
        # An empty frame without header or CRC at SF5: no blocks, (12 + 6.25 + 8) symbols.
        self.assertAlmostEqual(
            analyze.semtech_toa_ms(0, 5, BW, 8, False, 12, implicit=True, crc=False), 6.720, places=9
        )
        # LDRO does not apply below SF7.
        self.assertEqual(
            analyze.semtech_toa_ms(100, 6, BW, 5, True, 47), analyze.semtech_toa_ms(100, 6, BW, 5, False, 47)
        )


class LowDataRate(unittest.TestCase):
    """low_data_rate() decides LDRO as RNode firmware does, which sets the expected time on air."""

    def test_firmware_boundary_cases(self):
        # rnode-core's phy tests: where the symbol time is 16.384 ms, the firmware's integer
        # division leaves LDRO off, unlike the datasheet's "16 ms or more".
        for sf, bw, expected in (
            (7, 125_000, False),
            (11, 125_000, False),
            (12, 125_000, True),
            (9, 31_250, False),
            (10, 62_500, False),
            (12, 250_000, False),
            (10, 31_250, True),
        ):
            with self.subTest(sf=sf, bw=bw):
                self.assertIs(analyze.low_data_rate(sf, bw), expected)

    def test_every_rnode_modulation_uses_integer_division(self):
        # main() passes the bandwidth as a float; the firmware divides integers throughout.
        for sf in range(5, 13):
            for bw in (7_800, 10_400, 15_600, 20_800, 31_250, 41_700, 62_500, 125_000, 250_000, 500_000):
                firmware = (1 << sf) // (bw // 1000) > 16
                with self.subTest(sf=sf, bw=bw):
                    self.assertIs(analyze.low_data_rate(sf, bw), firmware)
                    self.assertIs(analyze.low_data_rate(sf, float(bw)), firmware)


class WithoutNumpy(unittest.TestCase):
    """Without numpy, main() refuses to run with a hint (review 6360), before it parses arguments
    or opens a capture; the pure helpers above still work."""

    def test_main_exits_with_the_uv_hint(self):
        # No arguments at all: argparse would exit with status 2 if it ran first.
        stderr = io.StringIO()
        with (
            mock.patch.object(analyze, "np", None),
            mock.patch.object(sys, "argv", ["lora_capture_analyze.py"]),
            contextlib.redirect_stderr(stderr),
            self.assertRaises(SystemExit) as raised,
        ):
            analyze.main()
        message = raised.exception.code
        self.assertIsInstance(message, str, f"exit status {message!r}, stderr: {stderr.getvalue()!r}")
        self.assertIn("needs numpy", message)
        self.assertIn("uv run --with numpy", message)
        self.assertEqual(stderr.getvalue(), "")


def chirp(value):
    """LoRa symbol `value` at FS: an up-chirp that starts `value` bins above -BW/2 and wraps to
    -BW/2 when it reaches +BW/2."""
    t = np.arange(SPS) / FS
    t_sym = SPS / FS
    phase = 2 * np.pi * ((-BW / 2 + value * BIN_HZ) * t + BW / (2 * t_sym) * t * t)
    t_wrap = t_sym * (1 - value / CHIPS)
    phase -= 2 * np.pi * BW * np.where(t >= t_wrap, t - t_wrap, 0.0)
    return np.exp(1j * phase)


def frame(payload=PAYLOAD):
    """A LoRa frame as the SX1262 sends it: 24 preamble up-chirps, the sync word 0x12 as symbols 8
    and 16, 2.25 down-chirps, then the payload symbols (header included)."""
    down = np.conj(chirp(0))
    symbols = [chirp(0)] * 24 + [chirp(8), chirp(16), down, down, down[: SPS // 4]]
    return np.concatenate(symbols + [chirp(value) for value in payload])


def capture(*parts, shift_hz=OFFSET_HZ, noise_db=-60.0, seed=1):
    """A capture of `parts`, each a number of samples of silence or an array of IQ samples, shifted
    by `shift_hz`, with complex white noise `noise_db` below the unit-power signal. Returns the
    capture and the (start, end) sample of each array."""
    pieces, spans, at = [], [], 0
    for part in parts:
        piece = np.zeros(part, complex) if isinstance(part, int) else part
        if not isinstance(part, int):
            spans.append((at, at + piece.size))
        pieces.append(piece)
        at += piece.size
    iq = np.concatenate(pieces) * np.exp(2j * np.pi * shift_hz * np.arange(at) / FS)
    sigma = math.sqrt(10 ** (noise_db / 10) / 2)
    rng = np.random.default_rng(seed)
    iq = iq + sigma * (rng.standard_normal(at) + 1j * rng.standard_normal(at))
    return iq.astype(np.complex64), spans


@needs_numpy
class BurstDetection(unittest.TestCase):
    """find_bursts() on synthetic captures: each burst spans its frame, padded by one symbol on each
    side, plus at most half the moving average's window."""

    def assertSpans(self, burst, start, end):
        self.assertTrue(start - SPS - WINDOW // 2 <= burst[0] <= start - SPS, (burst, start))
        self.assertTrue(end + SPS <= burst[1] <= end + SPS + WINDOW // 2, (burst, end))

    def test_each_frame_is_one_burst(self):
        iq, spans = capture(60_000, frame(), 50_000, frame(PAYLOAD[::-1]), 70_000)
        bursts = analyze.find_bursts(iq, SPS)
        self.assertEqual(len(bursts), 2, bursts)
        for burst, (start, end) in zip(bursts, spans):
            self.assertSpans(burst, start, end)

    def test_noise_alone_is_no_burst(self):
        iq, _ = capture(200_000)
        self.assertEqual(analyze.find_bursts(iq, SPS), [])

    def test_bursts_shorter_than_8_symbols_are_ignored(self):
        iq, spans = capture(60_000, frame()[: 6 * SPS], 50_000, frame(), 60_000)
        bursts = analyze.find_bursts(iq, SPS)
        self.assertEqual(len(bursts), 1, bursts)
        self.assertSpans(bursts[0], *spans[1])

    def test_frames_close_together_are_one_burst(self):
        # Bursts are padded by a symbol each and merged when less than a symbol apart: frames
        # 2.5 symbols apart are one burst, 4 symbols apart two.
        iq, spans = capture(60_000, frame(), 5 * SPS // 2, frame(), 60_000)
        bursts = analyze.find_bursts(iq, SPS)
        self.assertEqual(len(bursts), 1, bursts)
        self.assertSpans(bursts[0], spans[0][0], spans[1][1])
        iq, spans = capture(60_000, frame(), 4 * SPS, frame(), 60_000)
        bursts = analyze.find_bursts(iq, SPS)
        self.assertEqual(len(bursts), 2, bursts)
        for burst, (start, end) in zip(bursts, spans):
            self.assertSpans(burst, start, end)


@needs_numpy
class FrequencyOffset(unittest.TestCase):
    """Dechirping turns a symbol into a tone at its bin plus the carrier offset."""

    def test_dechirped_up_chirp_peaks_at_the_carrier_offset(self):
        up = analyze.reference_chirp(SPS, BW, FS, up=True)
        t = np.arange(SPS) / FS
        for bins in (-3, 0, 5):
            offset_hz = bins * BIN_HZ
            with self.subTest(bins=bins):
                peak_hz, snr_db = analyze.peak_freq(chirp(0) * np.exp(2j * np.pi * offset_hz * t), up, FS, BW / 2)
                self.assertEqual(peak_hz, offset_hz)
                self.assertGreater(snr_db, 60)

    def test_symbol_value_is_its_bin(self):
        # How main() reads the sync word: symbols 8 and 16 at SF7 are 0x12.
        up = analyze.reference_chirp(SPS, BW, FS, up=True)
        for value in (8, 16, 100):
            with self.subTest(value=value):
                peak_hz, _ = analyze.peak_freq(chirp(value), up, FS, BW / 2)
                self.assertEqual(round(peak_hz / BIN_HZ) % CHIPS, value)

    def test_refined_start_is_the_first_preamble_symbol(self):
        # The analyzer's own flow: find_bursts() gives the rough start, which refine_start()
        # searches around, 16 samples at a time; then all 8 symbols it dechirps peak at the offset.
        iq, spans = capture(60_000, frame(), 60_000)
        bursts = analyze.find_bursts(iq, SPS)
        self.assertEqual(len(bursts), 1, bursts)
        start, score_db, offset_hz = analyze.refine_start(iq, bursts[0][0], SPS, FS, BW)
        self.assertLess(abs(start - spans[0][0]), 8)
        self.assertEqual(offset_hz, OFFSET_HZ)
        self.assertGreater(score_db, 60)

    def test_search_finds_the_exact_start_on_its_grid(self):
        # A start that falls on the search grid is found to the sample, so the offset is exact.
        # (Off the grid, a timing error of 8 samples reads as one more bin of offset.)
        iq, spans = capture(60_000, frame(), 60_000)
        start = spans[0][0]
        for steps in (1, 6):
            with self.subTest(steps=steps):
                refined, _, offset_hz = analyze.refine_start(iq, start - SPS - 16 * steps, SPS, FS, BW)
                self.assertEqual((refined, offset_hz), (start, OFFSET_HZ))


@needs_numpy
class Helpers(unittest.TestCase):
    def test_robust_threshold(self):
        # Median plus the larger of 8 median absolute deviations and 4 medians.
        self.assertEqual(analyze.robust_threshold(np.array([1.0, 1.0, 1.0, 1.0, 100.0])), 5.0)
        self.assertEqual(analyze.robust_threshold(np.array([0.0, 0.0, 1.0, 2.0, 2.0])), 9.0)

    def test_mix_moves_a_carrier_to_zero(self):
        n = np.arange(4096)
        carrier = np.exp(2j * np.pi * -200_000 * n / FS)
        self.assertTrue(np.allclose(analyze.mix(carrier, FS, -200_000), 1.0))

    def test_occupied_bandwidth_of_a_tone_and_of_a_frame(self):
        n = np.arange(50_000)
        obw, peak = analyze.occupied_bandwidth(np.exp(2j * np.pi * 10_000 * n / FS), FS)
        self.assertLess(abs(peak - 10_000), FS / 65_536)
        self.assertLess(obw, 100)
        # Ideal chirps switch frequency abruptly, so 1% of their power spills a little outside the
        # LoRa bandwidth; the SX1262's own signal measured 125.2-125.9 kHz.
        obw, peak = analyze.occupied_bandwidth(frame(), FS)
        self.assertTrue(0.99 * BW <= obw <= 1.1 * BW, obw)
        self.assertLessEqual(abs(peak), BW / 2)


@needs_numpy
class AnalyzerOutput(unittest.TestCase):
    """main() on a synthetic capture of two 2-byte frames, recorded as in the README: the SDR
    centred 200 kHz above the carrier."""

    def analyze(self, iq):
        scratch = tempfile.mkdtemp(prefix=".scratch-", dir=HERE)
        self.addCleanup(shutil.rmtree, scratch)
        path = os.path.join(scratch, "capture.cfile")
        iq.astype("<c8").tofile(path)
        argv = [
            "lora_capture_analyze.py", path,
            "--sample-rate", "1000000",
            "--center-hz", "915075000",
            "--carrier-hz", "914875000",
            "--bw-hz", "125000",
            "--sf", "7",
            "--cr", "5",
            "--payload-len", "2",
            "--preamble", "24",
        ]
        stdout = io.StringIO()
        with mock.patch.object(sys, "argv", argv), contextlib.redirect_stdout(stdout):
            analyze.main()
        return stdout.getvalue().splitlines()

    @staticmethod
    def fields(line):
        return dict(re.findall(r"(\w+)=(\[[^\]]*\]|\S+)", line))

    def test_reports_each_frame(self):
        iq, spans = capture(60_000, frame(), 50_000, frame(PAYLOAD[::-1]), 70_000, shift_hz=OFFSET_HZ - 200_000)
        lines = self.analyze(iq)
        lora = self.fields(next(line for line in lines if line.startswith("LORA ")))
        self.assertEqual(
            (lora["samples_per_symbol"], lora["ldro"], lora["expected_toa_ms"]), ("1024", "False", "47.360")
        )
        self.assertIn("BURSTS detected=2", lines)
        bursts = [self.fields(line) for line in lines if line.startswith("BURST index=")]
        self.assertEqual(len(bursts), 2)
        for burst, (start, _end) in zip(bursts, spans):
            with self.subTest(index=burst["index"]):
                self.assertLess(abs(int(burst["start_sample"]) - start), 8)
                # Padding and the moving average lengthen a burst by at most its window.
                self.assertTrue(0 <= float(burst["toa_delta_ms"]) <= WINDOW / FS * 1000, burst["toa_delta_ms"])
                self.assertEqual((burst["freq_offset_hz"], burst["freq_offset_ppm"]), ("-2929.7", "-3.202"))
                self.assertEqual(burst["preamble_upchirps"], "24/24")
                self.assertEqual((burst["sync_bins"], burst["expected_sync_bins"]), ("[8, 16]", "[8, 16]"))
                self.assertTrue(0.99 * BW <= float(burst["occupied_bw_99pct_hz"]) <= 1.1 * BW, burst)
                self.assertTrue(all(snr > 40 for snr in json.loads(burst["downchirp_snr_db"])), burst)


if __name__ == "__main__":
    unittest.main()
