use rnode_core::phy::{self, Modulation, PhyParams};

use super::{Args, ToaParams, semtech_time_on_air_ms, validate_regulatory};

fn args(freq_hz: u32, power_dbm: i8) -> Args {
    Args {
        freq_hz,
        power_dbm,
        ..Args::default()
    }
}

#[test]
fn the_defaults_are_within_the_regulatory_limits() {
    validate_regulatory(&Args::default()).unwrap();
}

#[test]
fn regulatory_limits_accept_the_us_ism_band_up_to_10_dbm() {
    for freq_hz in [902_000_000, 914_875_000, 928_000_000] {
        for power_dbm in [i8::MIN, -9, 0, 10] {
            assert!(
                validate_regulatory(&args(freq_hz, power_dbm)).is_ok(),
                "{freq_hz} Hz, {power_dbm} dBm"
            );
        }
    }
}

#[test]
fn regulatory_limits_refuse_frequencies_outside_the_us_ism_band() {
    for freq_hz in [0, 868_100_000, 901_999_999, 928_000_001, u32::MAX] {
        let error = validate_regulatory(&args(freq_hz, 2)).unwrap_err().to_string();
        assert_eq!(
            error,
            format!("refusing to transmit or configure outside US 902-928 MHz ISM band: {freq_hz} Hz")
        );
    }
}

#[test]
fn regulatory_limits_refuse_more_than_10_dbm() {
    for power_dbm in [11, 22, i8::MAX] {
        let error = validate_regulatory(&args(914_875_000, power_dbm))
            .unwrap_err()
            .to_string();
        assert_eq!(error, format!("refusing power above 10 dBm; requested {power_dbm} dBm"));
    }
}

/// Explicit header and CRC on, as `transmit_one()` sends unless `--implicit` is given.
fn toa(payload_len: usize, sf: u8, bw_hz: u32, cr_denominator: u8, ldro: bool, preamble_symbols: u16) -> f64 {
    semtech_time_on_air_ms(ToaParams {
        payload_len,
        sf,
        bw_hz,
        cr_denominator,
        ldro,
        preamble_symbols,
        implicit_header: false,
        crc: true,
    })
}

fn assert_ms(actual: f64, expected: f64) {
    assert!((actual - expected).abs() < 1e-9, "{actual} ms, expected {expected} ms");
}

/// The time on air this tool printed on the reference board (tools/hwtest/README.md, "Bring-up
/// results"), which the SDR captures confirm: they measured 2, 50 and 255-byte SF7 frames
/// 0.333-0.335 ms longer than these, the same for every length.
#[test]
fn time_on_air_of_the_bring_up_frames() {
    for (payload_len, expected) in [(1, 42.240), (2, 47.360), (50, 113.920), (255, 416.000)] {
        assert_ms(toa(payload_len, 7, 125_000, 5, false, 24), expected);
    }
    assert_ms(toa(50, 12, 125_000, 5, true, 18), 2629.632);
    assert_ms(toa(50, 9, 31_250, 5, false, 18), 1478.656);
}

/// `transmit_one()` takes the preamble from rnode-core's `PhyParams` and LDRO from the driver, which
/// follow the firmware: 24 symbols at SF7/125 kHz, 18 at SF12/125 kHz and SF9/31.25 kHz, and LDRO
/// only at SF12/125 kHz.
#[test]
fn time_on_air_with_the_firmware_preamble_and_ldro() {
    for (sf, bw_hz, payload_len, expected) in [
        (7, 125_000, 255, 416.000),
        (12, 125_000, 50, 2629.632),
        (9, 31_250, 50, 1478.656),
    ] {
        let phy = PhyParams::new(Modulation {
            bandwidth_hz: bw_hz,
            spreading_factor: sf,
            coding_rate: 5,
        });
        let ldro = phy::low_data_rate(sf, bw_hz);
        assert_ms(toa(payload_len, sf, bw_hz, 5, ldro, phy.preamble_symbols), expected);
    }
}

/// Derived by hand from the Semtech formula.
#[test]
fn time_on_air_header_crc_and_coding_rate_terms() {
    // 8 bytes, implicit header, no CRC: ceil((64 - 28 + 28 - 20) / 28) = 2 blocks of 5 symbols,
    // so (8 + 4.25 + 8 + 10) symbols of 1.024 ms; the header's or the CRC's bits would make it 3.
    let implicit = ToaParams {
        payload_len: 8,
        sf: 7,
        bw_hz: 125_000,
        cr_denominator: 5,
        ldro: false,
        preamble_symbols: 8,
        implicit_header: true,
        crc: false,
    };
    assert_ms(semtech_time_on_air_ms(implicit), 30.976);
    // 255 bytes at CR 4/8: ceil(2056 / 28) = 74 blocks of 8 symbols, (24 + 4.25 + 8 + 592) symbols.
    assert_ms(toa(255, 7, 125_000, 8, false, 24), 643.328);
    // An empty frame without header or CRC at SF12 with LDRO: ceil(-40 / 40) = -1 blocks, clamped
    // to none, leaving (8 + 4.25 + 8) symbols of 32.768 ms.
    let empty = ToaParams {
        payload_len: 0,
        sf: 12,
        ldro: true,
        ..implicit
    };
    assert_ms(semtech_time_on_air_ms(empty), 663.552);
}

/// Below SF7 the SX1262 sends 2 more symbols and its block count has no 8 extra bits, unlike the
/// SX127x (#79). The values of Semtech's LoRaMac-node, `RadioGetLoRaTimeOnAirNumerator()` in
/// `src/radio/sx126x/radio.c`, compiled and run for these frames (it reproduces the SF7 to SF12
/// values above too). 94 and 47 preamble symbols are rnode-core's `PhyParams` at SF5 and SF6, the
/// firmware's; LoRaMac-node needs at least 12.
#[test]
fn time_on_air_at_sf5_and_sf6() {
    assert_ms(toa(255, 5, 125_000, 5, false, 94), 159.552);
    assert_ms(toa(100, 6, 125_000, 5, false, 47), 118.400);
    assert_ms(toa(10, 6, 125_000, 5, false, 12), 23.680);
    assert_ms(toa(50, 5, 500_000, 5, false, 94), 13.648);
    // 6 bytes at SF6 without header or CRC: ceil((48 - 24 + 20 - 20) / 24) = 1 block, which 8 more
    // bits, the header or the CRC would make 2.
    let implicit = ToaParams {
        payload_len: 6,
        sf: 6,
        bw_hz: 125_000,
        cr_denominator: 5,
        ldro: false,
        preamble_symbols: 12,
        implicit_header: true,
        crc: false,
    };
    assert_ms(semtech_time_on_air_ms(implicit), 16.000);
    // An empty frame without header or CRC at SF5: no blocks, (12 + 6.25 + 8) symbols.
    let empty = ToaParams {
        payload_len: 0,
        sf: 5,
        cr_denominator: 8,
        ..implicit
    };
    assert_ms(semtech_time_on_air_ms(empty), 6.720);
    // LDRO does not apply below SF7.
    assert_ms(toa(100, 6, 125_000, 5, true, 47), 118.400);
}

/// rnode-core's `phy::airtime_ms()`, the firmware's own estimate (`add_airtime()`), divides the same
/// bits into blocks as the SX1262 does, below SF7 as from SF7 on. It just neither rounds the blocks
/// up nor counts 4 of the symbols between preamble and header; so the time on air exceeds it by
/// exactly those 4 symbols and the rounding.
#[test]
fn time_on_air_exceeds_the_firmware_estimate_by_4_symbols_and_the_rounding() {
    // 100 bytes at 125 kHz and CR 4/5, as in rnode-core's airtime test, with the firmware's
    // preamble: 816 bits in blocks of 20 at SF5, 812 in blocks of 24 at SF6, 816 in blocks of 28 at SF7.
    for (sf, blocks) in [(5, 816.0 / 20.0), (6, 812.0 / 24.0), (7, 816.0 / 28.0)] {
        let modulation = Modulation {
            bandwidth_hz: 125_000,
            spreading_factor: sf,
            coding_rate: 5,
        };
        let phy = PhyParams::new(modulation);
        let firmware = phy::airtime_ms(100, modulation, false, phy.preamble_symbols, phy.symbol_time_ms);
        let gap_symbols = 4.0 + (f64::ceil(blocks) - blocks) * 5.0;
        let gap_ms = toa(100, sf, 125_000, 5, false, phy.preamble_symbols) - f64::from(firmware);
        assert!(
            (gap_ms - gap_symbols * f64::from(phy.symbol_time_ms)).abs() < 1e-3,
            "SF{sf}: {gap_ms} ms over the firmware estimate, expected {gap_symbols} symbols"
        );
    }
}
