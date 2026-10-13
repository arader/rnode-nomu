//! LoRa PHY timing derived from the modulation, and airtime.

use rnode_core::phy::*;

fn m(bw: u32, sf: u8, cr: u8) -> Modulation {
    Modulation {
        bandwidth_hz: bw,
        spreading_factor: sf,
        coding_rate: cr,
    }
}

const BANDWIDTHS: [u32; 10] = [
    7_800, 10_400, 15_600, 20_800, 31_250, 41_700, 62_500, 125_000, 250_000, 500_000,
];

#[test]
fn low_data_rate_matches_firmware_integer_expression() {
    for sf in 5u8..=12 {
        for bw in BANDWIDTHS {
            let firmware = ((1i64 << sf) / i64::from(bw / 1000)) > 16;
            assert_eq!(low_data_rate(sf, bw), firmware, "sf={sf} bw={bw}");
        }
    }
}

#[test]
fn low_data_rate_boundary_cases_are_off() {
    // Symbol time is 16.384 ms here; the float comparison would enable LDRO, RNodes do not.
    assert!(!low_data_rate(9, 31_250));
    assert!(!low_data_rate(10, 62_500));
    assert!(!low_data_rate(11, 125_000));
    assert!(!low_data_rate(12, 250_000));
    assert!(low_data_rate(12, 125_000));
    assert!(!low_data_rate(7, 125_000));
}

#[test]
fn sf7_125k_cr5() {
    // symbol time 1.024 ms; bitrate 5468 bps; preamble ceil(24 / 1.024) = 24 symbols.
    let p = PhyParams::new(m(125_000, 7, 5));
    assert_eq!(p.symbol_rate, 976.5625);
    assert!((p.symbol_time_ms - 1.024).abs() < 1e-6);
    assert_eq!(p.bitrate_bps, 5468);
    assert!(!p.fast_rate && !p.limit_rate && !p.guard_rate);
    assert_eq!(p.csma_slot_ms, 24); // 12.288 ms truncated to 12, raised to the 24 ms minimum
    assert_eq!(p.difs_ms, 48);
    assert_eq!(p.preamble_symbols, 24);
    assert_eq!(p.preamble_time_ms, 25); // ceil(24 * 1.024)
    assert_eq!(p.header_time_ms, 21); // ceil(20 * 1.024)
}

#[test]
fn sf8_125k_uses_minimum_preamble() {
    // symbol time 2.048 ms → 24 / 2.048 = 11.7 symbols, below the 18-symbol minimum.
    let p = PhyParams::new(m(125_000, 8, 5));
    assert_eq!(p.preamble_symbols, 18);
    assert_eq!(p.csma_slot_ms, 24);
    assert_eq!(p.preamble_time_ms, 37); // ceil(18 * 2.048 = 36.864)
}

#[test]
fn sf7_250k_long_preamble() {
    let p = PhyParams::new(m(250_000, 7, 5));
    assert_eq!(p.preamble_symbols, 47); // ceil(24 / 0.512 = 46.875)
}

#[test]
fn sf12_125k_slot_capped() {
    // symbol time 32.768 ms → slot 393 ms capped to 100 ms.
    let p = PhyParams::new(m(125_000, 12, 8));
    assert_eq!(p.csma_slot_ms, 100);
    assert_eq!(p.difs_ms, 200);
    assert_eq!(p.preamble_symbols, 18);
}

#[test]
fn fast_rate_shortens_targets() {
    // SF5/500 kHz/CR5: bitrate 62500 bps → fast and limit rate; preamble target 6 ms.
    let p = PhyParams::new(m(500_000, 5, 5));
    assert_eq!(p.bitrate_bps, 62_500);
    assert!(p.fast_rate && p.limit_rate && !p.guard_rate);
    assert_eq!(p.csma_slot_ms, 6); // 0.768 ms truncated to 0, below 24 → fast minimum 24 - 18
    assert_eq!(p.preamble_symbols, 94); // ceil(6 / 0.064 = 93.75)
}

#[test]
fn guard_rate_band() {
    // SF7/500 kHz/CR5: 21875 bps → guard rate, not fast.
    let p = PhyParams::new(m(500_000, 7, 5));
    assert_eq!(p.bitrate_bps, 21_875);
    assert!(p.guard_rate && !p.limit_rate && !p.fast_rate);
}

#[test]
fn stats_payload_layout() {
    let p = PhyParams::new(m(125_000, 7, 5));
    let payload = p.stats_payload();
    assert_eq!(u16::from_be_bytes([payload[0], payload[1]]), 1024); // symbol time in µs
    assert_eq!(u16::from_be_bytes([payload[2], payload[3]]), 976);
    assert_eq!(u16::from_be_bytes([payload[4], payload[5]]), 24);
    assert_eq!(u16::from_be_bytes([payload[6], payload[7]]), 25);
    assert_eq!(u16::from_be_bytes([payload[8], payload[9]]), 24);
    assert_eq!(u16::from_be_bytes([payload[10], payload[11]]), 48);
}

#[test]
fn airtime_sf7_and_sf_below_7_branches() {
    let p = PhyParams::new(m(125_000, 7, 5));
    // (8*100 + 16 - 4*7 + 8 + 20) / (4*7) * 5 + 24 + 0.25 + 8 = 816/28*5 + 32.25 symbols
    let expected = (816.0f32 / 28.0 * 5.0 + 32.25) * 1.024;
    let cost = airtime_ms(100, m(125_000, 7, 5), false, p.preamble_symbols, p.symbol_time_ms);
    assert!((cost - expected).abs() < 1e-3, "{cost} vs {expected}");

    let p6 = PhyParams::new(m(125_000, 6, 5));
    // (8*100 + 16 - 24 + 20) / 24 * 5 + preamble + 2.25 + 8
    let expected = (812.0f32 / 24.0 * 5.0 + f32::from(p6.preamble_symbols) + 10.25) * p6.symbol_time_ms;
    let cost6 = airtime_ms(100, m(125_000, 6, 5), false, p6.preamble_symbols, p6.symbol_time_ms);
    assert!((cost6 - expected).abs() < 1e-3, "{cost6} vs {expected}");
}

#[test]
fn airtime_low_data_rate_reduces_denominator() {
    let p = PhyParams::new(m(125_000, 12, 5));
    let without = airtime_ms(50, m(125_000, 12, 5), false, p.preamble_symbols, p.symbol_time_ms);
    let with = airtime_ms(50, m(125_000, 12, 5), true, p.preamble_symbols, p.symbol_time_ms);
    assert!(with > without);
}
