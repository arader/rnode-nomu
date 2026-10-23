//! CSMA/CA, airtime accounting and channel statistics through `Csma`'s public API. Tests that
//! need its private state are unit tests in `src/csma/tests.rs`.

use rnode_core::csma::*;

#[test]
fn airtime_limits_of_100_percent_disable_the_limit() {
    let mut csma = Csma::new();
    csma.set_st_airtime_limit(2_500);
    assert_eq!(csma.st_airtime_limit(), 2_500);
    csma.set_st_airtime_limit(10_000);
    assert_eq!(csma.st_airtime_limit(), 0);
    csma.set_lt_airtime_limit(65_535);
    assert_eq!(csma.lt_airtime_limit(), 0);
}

#[test]
fn csma_payload_reports_initial_band() {
    assert_eq!(Csma::new().csma_payload(), [1, 0, 15]);
}

#[test]
fn interference_blocks_medium_only_when_avoidance_is_enabled() {
    let mut csma = Csma::new();
    assert!(csma.avoid_interference());
    for i in 0..NOISE_FLOOR_SAMPLES {
        csma.observe(i as u64 * STATUS_INTERVAL_MS, false, -120);
        csma.status_tick(i as u64 * STATUS_INTERVAL_MS);
    }

    csma.observe(1000, false, -100);
    assert!(csma.channel_stats().interference);
    assert!(!csma.medium_free());
    csma.set_avoid_interference(false);
    assert!(!csma.avoid_interference());
    assert!(csma.medium_free());
}

#[test]
fn boot_noise_floor_sentinel_reports_interference_before_sampling() {
    let mut csma = Csma::new();
    csma.observe(0, false, -120);
    assert!(csma.channel_stats().interference);
    assert!(!csma.medium_free());

    for i in 0..NOISE_FLOOR_SAMPLES {
        csma.observe(i as u64 * STATUS_INTERVAL_MS, false, -120);
        csma.status_tick(i as u64 * STATUS_INTERVAL_MS);
    }
    csma.observe(1000, false, -121);
    assert!(!csma.channel_stats().interference);
    assert!(csma.medium_free());
}

#[test]
fn interference_threshold_is_strictly_greater_than_noise_floor_plus_threshold() {
    let mut csma = Csma::new();
    for i in 0..NOISE_FLOOR_SAMPLES {
        csma.observe(i as u64 * STATUS_INTERVAL_MS, false, -120);
        csma.status_tick(i as u64 * STATUS_INTERVAL_MS);
    }

    csma.observe(1000, false, -109);
    assert!(!csma.channel_stats().interference);
    csma.observe(1001, false, -108);
    assert!(csma.channel_stats().interference);
}

#[test]
fn noise_floor_converges_from_idle_samples_only_below_threshold() {
    let mut csma = Csma::new();
    for i in 0..NOISE_FLOOR_SAMPLES {
        csma.observe(i as u64 * STATUS_INTERVAL_MS, false, -120);
        csma.status_tick(i as u64 * STATUS_INTERVAL_MS);
    }
    assert_eq!(csma.channel_stats().noise_floor, -120);

    for i in 0..NOISE_FLOOR_SAMPLES {
        csma.observe(1000 + i as u64 * STATUS_INTERVAL_MS, false, -108);
        csma.status_tick(1000 + i as u64 * STATUS_INTERVAL_MS);
    }
    assert_eq!(csma.channel_stats().noise_floor, -120);

    for i in 0..NOISE_FLOOR_SAMPLES {
        csma.observe(2000 + i as u64 * STATUS_INTERVAL_MS, true, -130);
        csma.status_tick(2000 + i as u64 * STATUS_INTERVAL_MS);
    }
    assert_eq!(csma.channel_stats().noise_floor, -120);
}

#[test]
fn status_tick_updates_channel_utilisation_and_airtime_once_per_firmware_interval() {
    let mut csma = Csma::new();
    for i in 0..(UTIL_UPDATE_INTERVAL - 1) {
        csma.observe(i as u64 * STATUS_INTERVAL_MS, i % 2 == 0, -120);
        assert_eq!(csma.status_tick(i as u64 * STATUS_INTERVAL_MS), StatusUpdate::default());
    }

    csma.observe(999, true, -120);
    let update = csma.status_tick(999);
    assert!(update.channel_stats_due);
    let stats = csma.channel_stats();
    let expected_total = 167.0 / DCD_SAMPLES as f32;
    assert_eq!(stats.total_channel_util, expected_total);
    assert!((stats.longterm_channel_util - expected_total / AIRTIME_BINS as f32).abs() < f32::EPSILON);

    let payload = csma.channel_stats_payload();
    assert_eq!(&payload[4..6], &668u16.to_be_bytes());
    assert_eq!(&payload[6..8], &1u16.to_be_bytes());
}

#[test]
fn status_tick_saturates_total_channel_utilisation_at_one() {
    let mut csma = Csma::new();
    csma.add_airtime(0, 30_000.0);
    csma.update_airtime(0);
    assert!(csma.channel_stats().airtime > 1.0);
    assert_eq!(csma.csma_payload(), [4, 45, 59]);

    for i in 0..UTIL_UPDATE_INTERVAL {
        csma.observe(i as u64 * STATUS_INTERVAL_MS, true, -80);
        csma.status_tick(i as u64 * STATUS_INTERVAL_MS);
    }

    assert_eq!(csma.channel_stats().total_channel_util, 1.0);
    let payload = csma.channel_stats_payload();
    assert_eq!(&payload[4..6], &10_000u16.to_be_bytes());
}

#[test]
fn airtime_lock_engages_and_releases_as_bins_age() {
    let mut csma = Csma::new();
    csma.set_st_airtime_limit(1_000);
    csma.add_airtime(0, 2000.0);

    let update = csma.update_airtime(0);
    assert!(update.channel_stats_due);
    assert!(csma.airtime_locked());
    assert!((csma.channel_stats().airtime - (2000.0 / 15_000.0)).abs() < f32::EPSILON);

    csma.update_airtime(2 * AIRTIME_BINLEN_MS);
    assert!(!csma.airtime_locked());
    assert_eq!(csma.channel_stats().airtime, 0.0);
}

#[test]
fn tightening_airtime_limits_rechecks_lock_immediately() {
    let mut csma = Csma::new();
    csma.add_airtime(0, 2000.0);
    csma.update_airtime(0);
    assert!(!csma.airtime_locked());
    csma.set_st_airtime_limit(1_000);
    assert!(csma.airtime_locked());
    csma.set_st_airtime_limit(0);
    assert!(!csma.airtime_locked());

    let mut longterm = Csma::new();
    for bin in [0, 2, 4, 6] {
        longterm.add_airtime(bin * AIRTIME_BINLEN_MS, 1000.0);
    }
    longterm.update_airtime(6 * AIRTIME_BINLEN_MS);
    assert!(!longterm.airtime_locked());
    longterm.set_lt_airtime_limit(10);
    assert!(longterm.airtime_locked());
}

#[test]
fn longterm_airtime_wraps_after_one_hour() {
    let mut csma = Csma::new();
    csma.add_airtime(AIRTIME_LONGTERM_MS - 1, 1000.0);
    csma.add_airtime(AIRTIME_LONGTERM_MS, 2000.0);
    csma.update_airtime(AIRTIME_LONGTERM_MS);

    let stats = csma.channel_stats();
    assert!((stats.airtime - (3000.0 / 15_000.0)).abs() < f32::EPSILON);
    assert!((stats.longterm_airtime - (3000.0 / AIRTIME_LONGTERM_MS as f32)).abs() < f32::EPSILON);
}

#[test]
fn channel_stats_payload_uses_firmware_offsets_and_no_interference_marker() {
    let mut csma = Csma::new();
    for i in 0..NOISE_FLOOR_SAMPLES {
        csma.observe(i as u64 * STATUS_INTERVAL_MS, false, -120);
        csma.status_tick(i as u64 * STATUS_INTERVAL_MS);
    }
    csma.observe(1000, false, -121);
    csma.add_airtime(0, 750.0);
    csma.update_airtime(0);

    let payload = csma.channel_stats_payload();
    assert_eq!(&payload[0..2], &500u16.to_be_bytes()); // 5.00%.
    assert_eq!(&payload[2..4], &2u16.to_be_bytes()); // 0.02% of one hour.
    assert_eq!(payload[8], 36); // -121 + RSSI_OFFSET(157).
    assert_eq!(payload[9], 37); // -120 + RSSI_OFFSET(157).
    assert_eq!(payload[10], 0xFF);

    csma.observe(1001, false, -100);
    let payload = csma.channel_stats_payload();
    assert_eq!(payload[8], 57);
    assert_eq!(payload[10], 57);
}

/// Firmware `update_noise_floor()` with `HAS_LORA_LNA`: a sample more than `LORA_LNA_GVT` (12 dB)
/// below the noise floor is discarded before it reaches the 128-sample buffer, so a floor of
/// -100 dBm ignores -113 dBm and takes -112 dBm. Without an LNA both count, as before.
#[test]
fn an_external_lna_discards_samples_more_than_12_db_below_the_noise_floor() {
    fn sample_floor(csma: &mut Csma, start_ms: u64, rssi_dbm: i16) -> i16 {
        for i in 0..NOISE_FLOOR_SAMPLES as u64 {
            csma.observe(start_ms + i * STATUS_INTERVAL_MS, false, rssi_dbm);
            csma.status_tick(start_ms + i * STATUS_INTERVAL_MS);
        }
        csma.channel_stats().noise_floor
    }
    assert_eq!(LORA_LNA_GVT, 12);

    let mut lna = Csma::new();
    assert!(!lna.external_lna());
    lna.set_external_lna(true);
    assert!(lna.external_lna());
    let mut no_lna = Csma::new();

    // The -292 dBm boot sentinel discards nothing.
    assert_eq!(sample_floor(&mut lna, 0, -100), -100);
    assert_eq!(sample_floor(&mut no_lna, 0, -100), -100);

    assert_eq!(sample_floor(&mut lna, 1_000, -113), -100);
    assert_eq!(sample_floor(&mut no_lna, 1_000, -113), -113);

    assert_eq!(sample_floor(&mut lna, 2_000, -112), -112);
    // The Heltec V4's own interference rule, which at a floor of -109 dBm or less compares with
    // -89 dBm instead, is not ported: interference still starts more than 11 dB above the floor.
    lna.observe(3_000, false, -101);
    assert!(!lna.channel_stats().interference);
    lna.observe(3_003, false, -100);
    assert!(lna.channel_stats().interference);
}
