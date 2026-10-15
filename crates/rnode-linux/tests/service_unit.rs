//! The shipped systemd unit gives a stopping daemon time to finish the packet on the air (issue
//! #59). A stop request ends a TX burst between packets, never inside one, so the daemon may still
//! have both frames of a split packet to send. At the slowest settings that takes minutes, far
//! beyond systemd's default `TimeoutStopSec=` of 90 s. The bound is computed with rnode-core's
//! framing, PHY and CSMA code, so the unit and the code cannot drift apart.

use std::fs;
use std::path::{Path, PathBuf};

use rnode_core::csma::{CSMA_CW_BANDS, CSMA_CW_PER_BAND_WINDOWS};
use rnode_core::framing::tx_frames;
use rnode_core::kiss::MTU;
use rnode_core::phy::{Modulation, PhyParams, airtime_ms, low_data_rate};
use rnode_core::rng::Rng;

/// The slowest modulation a host can select: SF12, CR 4/8 and 7.8 kHz, the narrowest bandwidth
/// the SX1262 driver programs (it rounds anything narrower up to 7.8 kHz).
const SLOWEST: Modulation = Modulation {
    bandwidth_hz: 7_800,
    spreading_factor: 12,
    coding_rate: 8,
};

/// Semtech's time on air of a 255-byte frame at [`SLOWEST`] (SX1261/2 datasheet, "LoRa
/// Time-on-Air"; explicit header, CRC on, low-data-rate optimisation on, the firmware's 18-symbol
/// preamble): 18 + 4.25 + 8 + ⌈(8 × 255 + 16 − 4 × 12 + 8 + 20) / (4 × (12 − 2))⌉ × 8 = 438.25
/// symbols of 2^12 / 7800 Hz = 525.13 ms each.
const SLOWEST_FRAME_TIME_ON_AIR_MS: f32 = 438.25 * 4096.0 / 7.8;

/// Work around the airtime: the SPI commands before and after each frame, putting the radio to
/// sleep and removing the PTY symlink. Milliseconds normally, and under a second even if every
/// command waits out the 100 ms BUSY timeout.
const OVERHEAD_MS: f32 = 2_000.0;

fn repo_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(name)
}

/// rnode-core's airtime of a `len`-byte frame at [`SLOWEST`], raised to cover the real time on
/// air. rnode-core charges the firmware's estimate (`add_airtime()`), which counts 0.25 instead
/// of 4.25 symbols after the preamble and does not round the payload up to whole blocks of CR+4
/// symbols; add both.
fn slowest_frame_ms(len: usize) -> f32 {
    let phy = PhyParams::new(SLOWEST);
    let ldro = low_data_rate(SLOWEST.spreading_factor, SLOWEST.bandwidth_hz);
    let shortfall_symbols = 4.0 + f32::from(SLOWEST.coding_rate);
    airtime_ms(len, SLOWEST, ldro, phy.preamble_symbols, phy.symbol_time_ms) + shortfall_symbols * phy.symbol_time_ms
}

/// The longest a stop can take, in ms: the largest packet at [`SLOWEST`], plus channel access and
/// [`OVERHEAD_MS`].
fn slowest_stop_ms() -> f32 {
    // The largest packet, MTU bytes, goes out as two full LoRa frames, back to back.
    let frames = tx_frames(&[0; MTU], &mut Rng::new(0));
    let packet_ms: f32 = frames.iter().map(|frame| slowest_frame_ms(frame.len())).sum();
    // DIFS plus the longest contention window. A stop does not wait for channel access, because
    // the daemon checks for shutdown after access is granted, just before each packet; it is in
    // the margin anyway.
    let phy = PhyParams::new(SLOWEST);
    let slots = u32::from(CSMA_CW_BANDS) * u32::from(CSMA_CW_PER_BAND_WINDOWS);
    let access_ms = (phy.difs_ms + slots * phy.csma_slot_ms) as f32;
    packet_ms + access_ms + OVERHEAD_MS
}

/// `TimeoutStopSec=` of the unit's `[Service]` section, in seconds. Accepts the forms systemd reads
/// as one number: plain seconds, or seconds or minutes with a unit.
fn timeout_stop_sec(unit: &str) -> f32 {
    let mut section = "";
    let mut value = None;
    for line in unit.lines().map(str::trim) {
        if line.starts_with('[') {
            section = line;
        } else if let Some(setting) = line.strip_prefix("TimeoutStopSec=") {
            assert_eq!(section, "[Service]", "TimeoutStopSec= outside [Service]");
            assert!(value.is_none(), "TimeoutStopSec= is set twice");
            value = Some(setting.trim());
        }
    }
    let value = value.expect("contrib/rnode-linux.service does not set TimeoutStopSec=");
    let number = value.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let seconds: f32 = number
        .parse()
        .unwrap_or_else(|e| panic!("TimeoutStopSec={value}: {e}; extend this parser"));
    match &value[number.len()..] {
        "" | "s" | "sec" => seconds,
        "m" | "min" => seconds * 60.0,
        unit => panic!("TimeoutStopSec={value}: unit {unit:?} not handled; extend this parser"),
    }
}

#[test]
fn service_waits_for_the_slowest_packet_before_killing_the_daemon() {
    let unit = fs::read_to_string(repo_path("contrib/rnode-linux.service")).unwrap();
    let timeout_s = timeout_stop_sec(&unit);
    let needed_s = slowest_stop_ms() / 1000.0;
    assert!(
        timeout_s >= needed_s,
        "TimeoutStopSec= is {timeout_s} s, but a stop can take {needed_s:.1} s: systemd would kill \
         the daemon in the middle of a packet"
    );
    // It is also how long a hung daemon holds up a stop or a reboot.
    assert!(
        timeout_s <= needed_s * 1.25,
        "TimeoutStopSec= is {timeout_s} s, much longer than the {needed_s:.1} s a stop can take"
    );
}

#[test]
fn slowest_packet_is_two_full_frames_and_covers_the_time_on_air() {
    // Firmware `updateBitrate()`: the 24 ms preamble target is under one symbol, so the minimum of
    // 18 symbols; `handleLowDataRate()`: 4096 / 7 > 16, so low-data-rate optimisation is on.
    let phy = PhyParams::new(SLOWEST);
    assert_eq!(phy.preamble_symbols, 18);
    assert!(low_data_rate(SLOWEST.spreading_factor, SLOWEST.bandwidth_hz));
    assert_eq!((phy.difs_ms, phy.csma_slot_ms), (200, 100));

    let frames = tx_frames(&[0; MTU], &mut Rng::new(0));
    assert_eq!(frames.iter().map(Vec::len).collect::<Vec<_>>(), [255, 255]);

    // The firmware's estimate falls short of the time on air; with the shortfall added it covers it.
    let estimate_ms = airtime_ms(255, SLOWEST, true, phy.preamble_symbols, phy.symbol_time_ms);
    assert!(estimate_ms < SLOWEST_FRAME_TIME_ON_AIR_MS, "{estimate_ms}");
    assert!(
        slowest_frame_ms(255) >= SLOWEST_FRAME_TIME_ON_AIR_MS,
        "{}",
        slowest_frame_ms(255)
    );
    // About 7 min 40 s on the air for the packet; the unit's comment quotes this.
    assert_eq!((2.0 * SLOWEST_FRAME_TIME_ON_AIR_MS / 1000.0).round(), 460.0);
}

#[test]
fn timeout_parser_reads_systemd_seconds_and_minutes() {
    for (setting, seconds) in [
        ("480", 480.0),
        ("480s", 480.0),
        ("480sec", 480.0),
        ("8min", 480.0),
        ("8m", 480.0),
    ] {
        let unit = format!("[Unit]\nDescription=x\n\n[Service]\nTimeoutStopSec={setting}\n");
        assert_eq!(timeout_stop_sec(&unit), seconds, "{setting}");
    }
}
