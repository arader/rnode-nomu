#!/usr/bin/env python3
"""Analyze a LoRa IQ capture for RNode/SX126x hardware bring-up.

Input samples are interleaved little-endian float32 IQ (SoapySDR CF32).  The
script intentionally depends only on numpy so it can be run with `uv run --with
numpy` on a development machine after capturing IQ on the uConsole.
"""
from __future__ import annotations

import argparse
import math
from pathlib import Path

try:
    import numpy as np
except ModuleNotFoundError:
    # Only the analysis needs numpy. Without it, the pure helpers (semtech_toa_ms(),
    # low_data_rate()) still import, so their unit tests run on the standard library alone.
    np = None


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("capture", type=Path)
    p.add_argument("--sample-rate", type=float, required=True)
    p.add_argument("--center-hz", type=float, required=True, help="SDR center frequency")
    p.add_argument("--carrier-hz", type=float, required=True, help="configured LoRa carrier")
    p.add_argument("--bw-hz", type=float, default=125_000)
    p.add_argument("--sf", type=int, default=7)
    p.add_argument("--cr", type=int, default=5, help="coding-rate denominator, 5..8")
    p.add_argument("--payload-len", type=int, default=50)
    p.add_argument("--preamble", type=int, default=24)
    p.add_argument("--sync-word", type=lambda s: int(s, 0), default=0x12)
    p.add_argument("--max-samples", type=int, default=0, help="optional sample cap for quick checks")
    return p.parse_args()


def load_cf32(path: Path, max_samples: int = 0) -> np.ndarray:
    count = max_samples * 2 if max_samples else -1
    raw = np.fromfile(path, dtype="<f4", count=count)
    if raw.size % 2:
        raw = raw[:-1]
    return raw[0::2].astype(np.float32) + 1j * raw[1::2].astype(np.float32)


def mix(iq: np.ndarray, fs: float, freq_hz: float) -> np.ndarray:
    n = np.arange(iq.size, dtype=np.float64)
    return iq * np.exp(-2j * np.pi * freq_hz * n / fs)


def reference_chirp(sps: int, bw: float, fs: float, up: bool = True) -> np.ndarray:
    t = np.arange(sps, dtype=np.float64) / fs
    tsym = sps / fs
    phase = 2 * np.pi * ((-bw / 2.0) * t + (bw / (2.0 * tsym)) * t * t)
    chirp = np.exp(1j * phase)
    return chirp if up else np.conj(chirp)


def robust_threshold(power: np.ndarray) -> float:
    med = float(np.median(power))
    mad = float(np.median(np.abs(power - med)))
    return med + max(8.0 * mad, med * 4.0)


def find_bursts(iq: np.ndarray, sps: int) -> list[tuple[int, int]]:
    win = max(64, sps // 4)
    kernel = np.ones(win, dtype=np.float32) / win
    pwr = np.convolve(np.abs(iq) ** 2, kernel, mode="same")
    thr = robust_threshold(pwr)
    active = pwr > thr
    bursts: list[tuple[int, int]] = []
    i = 0
    min_len = max(sps * 8, 1)
    pad = sps
    while i < active.size:
        if not active[i]:
            i += 1
            continue
        j = i + 1
        while j < active.size and active[j]:
            j += 1
        if j - i >= min_len:
            bursts.append((max(0, i - pad), min(active.size, j + pad)))
        i = j
    # Merge groups separated by less than one symbol.
    merged: list[tuple[int, int]] = []
    for start, end in bursts:
        if merged and start - merged[-1][1] < sps:
            merged[-1] = (merged[-1][0], end)
        else:
            merged.append((start, end))
    return merged


def peak_freq(segment: np.ndarray, ref: np.ndarray, fs: float, search_hz: float | None = None) -> tuple[float, float]:
    z = segment * np.conj(ref)
    spec = np.fft.fftshift(np.fft.fft(z * np.hanning(z.size)))
    freqs = np.fft.fftshift(np.fft.fftfreq(z.size, d=1 / fs))
    mag = np.abs(spec)
    if search_hz is not None:
        mask = np.abs(freqs) <= search_hz
        freqs2 = freqs[mask]
        mag2 = mag[mask]
    else:
        freqs2 = freqs
        mag2 = mag
    idx = int(np.argmax(mag2))
    peak = float(freqs2[idx])
    snr = float(20 * np.log10((mag2[idx] + 1e-12) / (np.median(mag2) + 1e-12)))
    return peak, snr


def refine_start(iq: np.ndarray, rough: int, sps: int, fs: float, bw: float) -> tuple[int, float, float]:
    up = reference_chirp(sps, bw, fs, up=True)
    lo = max(0, rough - 2 * sps)
    hi = min(iq.size - 10 * sps, rough + 2 * sps)
    best = (rough, -1e9, 0.0)
    for off in range(lo, hi, max(1, sps // 64)):
        peaks = []
        snrs = []
        for k in range(8):
            seg = iq[off + k * sps : off + (k + 1) * sps]
            f, snr = peak_freq(seg, up, fs, search_hz=bw / 2)
            peaks.append(f)
            snrs.append(snr)
        score = float(np.mean(snrs) - np.std(peaks) / 50.0)
        if score > best[1]:
            best = (off, score, float(np.median(peaks)))
    return best


# Below this dechirp SNR the SFD's down-chirps count as absent, for example because --preamble does
# not match the frame, and the carrier offset as unknown.
MIN_DOWNCHIRP_SNR_DB = 15.0


def fine_peak_freq(
    segment: np.ndarray, ref: np.ndarray, fs: float, search_hz: float, oversample: int = 16
) -> tuple[float, float]:
    """peak_freq() to a small fraction of a bin: the peak of a zero-padded spectrum, refined by a
    parabola through the log magnitudes around it. Returns the frequency and the SNR in dB."""
    z = segment * np.conj(ref) * np.hanning(segment.size)
    n = segment.size * oversample
    mag = np.abs(np.fft.fftshift(np.fft.fft(z, n=n)))
    freqs = np.fft.fftshift(np.fft.fftfreq(n, d=1 / fs))
    inband = np.flatnonzero(np.abs(freqs) <= search_hz)
    k = int(inband[np.argmax(mag[inband])])
    shift = 0.0
    if 0 < k < n - 1:
        a, b, c = np.log(mag[k - 1 : k + 2] + 1e-300)
        if a - 2 * b + c < 0:
            shift = 0.5 * (a - c) / (a - 2 * b + c)
    snr = float(20 * np.log10((mag[k] + 1e-12) / (np.median(mag[inband]) + 1e-12)))
    return float(freqs[k] + shift * fs / n), snr


def down_chirp_tone(
    iq: np.ndarray, start: int, sps: int, fs: float, bw: float, preamble: int
) -> tuple[float, float] | None:
    """The dechirped tone of the SFD's two down-chirps, symbols preamble + 2 and + 3 of the frame
    whose preamble starts at `start`: their mean frequency and the weaker one's SNR. None if the
    frame does not fit in `iq`."""
    if start < 0 or start + (preamble + 4) * sps > iq.size:
        return None
    down = reference_chirp(sps, bw, fs, up=False)
    symbols = (iq[start + k * sps : start + (k + 1) * sps] for k in (preamble + 2, preamble + 3))
    tones = [fine_peak_freq(symbol, down, fs, bw / 2) for symbol in symbols]
    return float(np.mean([f for f, _ in tones])), min(snr for _, snr in tones)


def up_chirp_tone(iq: np.ndarray, start: int, sps: int, fs: float, bw: float, preamble: int) -> float:
    """The median dechirped tone of the up-chirps of the preamble starting at `start`, but its first
    and last."""
    up = reference_chirp(sps, bw, fs, up=True)
    symbols = (iq[start + k * sps : start + (k + 1) * sps] for k in range(1, max(2, preamble - 1)))
    return float(np.median([fine_peak_freq(symbol, up, fs, bw / 2)[0] for symbol in symbols]))


def align_frame(
    iq: np.ndarray, start: int, up_hz: float, sps: int, fs: float, bw: float, preamble: int
) -> tuple[int, float, float]:
    """Separate the carrier offset from the timing error of the frame whose preamble refine_start()
    found at `start`, with up-chirp tone `up_hz` there (#78).

    Dechirping turns a start t samples late into a tone t * bw / sps Hz higher for an up-chirp and as
    much lower for a down-chirp, while a carrier offset moves both the same way. So the preamble's
    up-chirps and the SFD's down-chirps give the carrier offset, (up + down) / 2, and the timing
    error, (up - down) / 2 * sps / bw samples, as long as the tones stay within the bandwidth: for
    starts up to about 0.4 symbol from a symbol boundary, which refine_start() always finds. A start
    a whole symbol out reads as no timing error, so the start first moves by whole symbols to where
    the two down-chirps are strongest.

    Returns the corrected start, the carrier offset and the up-chirp tone there, against which
    symbols are read; or `start`, nan and `up_hz` when the down-chirps are not where `preamble`
    puts them."""
    aligned = start
    for _ in range(2):
        tones = {c: down_chirp_tone(iq, c, sps, fs, bw, preamble) for c in (aligned - sps, aligned, aligned + sps)}
        found = {c: tone for c, tone in tones.items() if tone is not None and tone[1] >= MIN_DOWNCHIRP_SNR_DB}
        if not found:
            return start, math.nan, up_hz
        aligned = max(found, key=lambda c: found[c][1])
        timing = (up_chirp_tone(iq, aligned, sps, fs, bw, preamble) - found[aligned][0]) / 2 * sps / bw
        aligned -= int(round(timing))
    tone = down_chirp_tone(iq, aligned, sps, fs, bw, preamble)
    if tone is None or tone[1] < MIN_DOWNCHIRP_SNR_DB:
        return start, math.nan, up_hz
    up = up_chirp_tone(iq, aligned, sps, fs, bw, preamble)
    return aligned, (up + tone[0]) / 2, up


def low_data_rate(sf: int, bw_hz: float) -> bool:
    """Whether the transmitter uses low-data-rate optimisation, decided as RNode firmware does in
    `sx126x::handleLowDataRate()`: `(1 << sf) / (bandwidth / 1000) > 16` in integer arithmetic, so
    it is off at SF11/125 kHz although the symbol time there is 16.384 ms."""
    return int((1 << sf) / (int(bw_hz) // 1000)) > 16


def semtech_toa_ms(payload_len: int, sf: int, bw: float, cr_den: int, ldro: bool, preamble: int, implicit: bool = False, crc: bool = True) -> float:
    """Time on air in ms by the SX126x formula, as Semtech's LoRaMac-node computes it
    (`RadioGetLoRaTimeOnAirNumerator()` in src/radio/sx126x/radio.c). Below SF7 a frame has 2 more
    symbols of overhead and no 8 extra bits in its block count, and LDRO does not apply."""
    tsym_ms = (2**sf / bw) * 1000.0
    de = 1 if ldro else 0
    ih = 1 if implicit else 0
    crc_i = 1 if crc else 0
    if sf < 7:
        numerator = 8 * payload_len - 4 * sf + 20 + 16 * crc_i - 20 * ih
        denominator = 4 * sf
        overhead = 6.25
    else:
        numerator = 8 * payload_len - 4 * sf + 28 + 16 * crc_i - 20 * ih
        denominator = 4 * (sf - 2 * de)
        overhead = 4.25
    payload_symbols = 8 + max(math.ceil(numerator / denominator) * (cr_den - 4 + 4), 0)
    return (preamble + overhead + payload_symbols) * tsym_ms


def occupied_bandwidth(iq: np.ndarray, fs: float) -> tuple[float, float]:
    nfft = 1 << min(20, max(10, int(math.ceil(math.log2(iq.size)))))
    spec = np.fft.fftshift(np.fft.fft(iq[:nfft] * np.hanning(min(iq.size, nfft)), n=nfft))
    p = np.abs(spec) ** 2
    freqs = np.fft.fftshift(np.fft.fftfreq(nfft, d=1 / fs))
    cdf = np.cumsum(p)
    total = cdf[-1]
    lo = freqs[int(np.searchsorted(cdf, total * 0.005))]
    hi = freqs[int(np.searchsorted(cdf, total * 0.995))]
    peak = freqs[int(np.argmax(p))]
    return float(hi - lo), float(peak)


def main() -> None:
    if np is None:
        raise SystemExit("lora_capture_analyze.py needs numpy: run it with `uv run --with numpy python ...`")
    args = parse_args()
    iq = load_cf32(args.capture, args.max_samples)
    expected_offset = args.carrier_hz - args.center_hz
    baseband = mix(iq, args.sample_rate, expected_offset)
    chips = 1 << args.sf
    sps = int(round(args.sample_rate * chips / args.bw_hz))
    actual_bw = args.sample_rate * chips / sps
    ldro = low_data_rate(args.sf, args.bw_hz)
    expected_ms = semtech_toa_ms(args.payload_len, args.sf, args.bw_hz, args.cr, ldro, args.preamble)
    print(f"CAPTURE samples={iq.size} fs_hz={args.sample_rate:.3f} center_hz={args.center_hz:.0f} carrier_hz={args.carrier_hz:.0f} expected_offset_hz={expected_offset:.1f}")
    print(f"LORA sf={args.sf} bw_hz={args.bw_hz:.0f} samples_per_symbol={sps} actual_bw_from_sps_hz={actual_bw:.3f} ldro={bool(ldro)} expected_toa_ms={expected_ms:.3f}")

    bursts = find_bursts(baseband, sps)
    print(f"BURSTS detected={len(bursts)}")
    up = reference_chirp(sps, args.bw_hz, args.sample_rate, up=True)
    down = reference_chirp(sps, args.bw_hz, args.sample_rate, up=False)
    expected_sync = [((args.sync_word >> 4) & 0x0F) * chips / 16.0, (args.sync_word & 0x0F) * chips / 16.0]
    bin_hz = args.bw_hz / chips

    for idx, (start, end) in enumerate(bursts, 1):
        searched, score, search_up_hz = refine_start(baseband, start, sps, args.sample_rate, args.bw_hz)
        # The search's start and up-chirp tone mix timing error into the frequency (#78).
        refined, offset_hz, up_hz = align_frame(
            baseband, searched, search_up_hz, sps, args.sample_rate, args.bw_hz, args.preamble
        )
        # Estimate duration from the contiguous active region, trimming the one-symbol padding.
        duration_ms = max(0.0, (end - start - 2 * sps) / args.sample_rate * 1000.0)
        ppm = offset_hz / args.carrier_hz * 1e6
        burst = baseband[max(0, refined) : min(baseband.size, refined + int(math.ceil(expected_ms / 1000 * args.sample_rate)))]
        obw, peak = occupied_bandwidth(burst, args.sample_rate)
        preamble_peaks = []
        preamble_snrs = []
        for k in range(args.preamble):
            seg = baseband[refined + k * sps : refined + (k + 1) * sps]
            if seg.size != sps:
                break
            f, snr = peak_freq(seg, up, args.sample_rate, search_hz=args.bw_hz / 2)
            preamble_peaks.append(f)
            preamble_snrs.append(snr)
        preamble_count = sum(abs(f - up_hz) < bin_hz * 1.5 for f in preamble_peaks)
        sync_bins = []
        for k in range(2):
            sym_idx = args.preamble + k
            seg = baseband[refined + sym_idx * sps : refined + (sym_idx + 1) * sps]
            f, snr = peak_freq(seg, up, args.sample_rate, search_hz=args.bw_hz / 2)
            sync_bins.append(round((f - up_hz) / bin_hz) % chips)
        down_snrs = []
        for k in range(2):
            sym_idx = args.preamble + 2 + k
            seg = baseband[refined + sym_idx * sps : refined + (sym_idx + 1) * sps]
            f, snr = peak_freq(seg, down, args.sample_rate, search_hz=args.bw_hz / 2)
            down_snrs.append(snr)
        print(
            "BURST"
            f" index={idx} rough_start_sample={start} start_sample={refined}"
            f" start_correction_samples={refined - searched} refine_score_db={score:.1f}"
            f" duration_ms={duration_ms:.3f} toa_delta_ms={duration_ms - expected_ms:.3f}"
            f" freq_offset_hz={offset_hz:.1f} freq_offset_ppm={ppm:.3f}"
            f" occupied_bw_99pct_hz={obw:.0f} psd_peak_hz={peak:.1f}"
            f" preamble_upchirps={preamble_count}/{args.preamble} preamble_snr_db={np.median(preamble_snrs):.1f}"
            f" sync_bins={sync_bins} expected_sync_bins={[round(v) for v in expected_sync]}"
            f" downchirp_snr_db={[round(v, 1) for v in down_snrs]}"
        )


if __name__ == "__main__":
    main()
