use super::*;

fn poll_until_grant(csma: &mut Csma, now_ms: u64) -> Access {
    let mut rng = Rng::new(1);
    csma.cw_min = 0;
    csma.cw_max = 1;
    assert_eq!(csma.poll_access(0, &mut rng), Access::Wait);
    assert_eq!(csma.poll_access(now_ms, &mut rng), Access::Wait);
    csma.poll_access(now_ms, &mut rng)
}

#[test]
fn reset_clears_channel_stats_and_contention_but_preserves_rssi_calibration() {
    let mut csma = Csma::new();
    for i in 0..NOISE_FLOOR_SAMPLES {
        csma.observe(i as u64 * STATUS_INTERVAL_MS, false, -120);
        csma.status_tick(i as u64 * STATUS_INTERVAL_MS);
    }
    csma.observe(1000, false, -100);
    csma.add_airtime(0, 2000.0);
    csma.longterm_bins[0] = 0.5;
    csma.util_samples[0] = true;
    csma.dcd_sample = 42;
    csma.local_channel_util = 0.1;
    csma.total_channel_util = 0.2;
    csma.longterm_channel_util = 0.3;
    csma.set_st_airtime_limit(1_000);
    csma.update_airtime(0);
    csma.difs_wait_start = Some(1);
    csma.cw_wait_start = Some(2);
    csma.cw_wait_passed = 3;
    csma.csma_cw = Some(4);
    csma.cw_band = 4;
    csma.cw_min = 45;
    csma.cw_max = 59;
    csma.interference_start = Some(5);

    csma.reset();

    assert_eq!(csma.current_rssi, -100);
    assert_eq!(csma.noise_floor, -120);
    assert!(csma.noise_floor_sampled);
    assert!(!csma.carrier);
    assert!(!csma.interference);
    assert!(csma.interference_start.is_none());
    assert!(csma.util_samples.iter().all(|&sample| !sample));
    assert!(csma.airtime_bins.iter().all(|&bin| bin == 0));
    assert!(csma.longterm_bins.iter().all(|&bin| bin == 0.0));
    assert_eq!(csma.dcd_sample, 0);
    assert_eq!(csma.local_channel_util, 0.0);
    assert_eq!(csma.total_channel_util, 0.0);
    assert_eq!(csma.longterm_channel_util, 0.0);
    assert_eq!(csma.airtime, 0.0);
    assert_eq!(csma.longterm_airtime, 0.0);
    assert!(!csma.airtime_locked());
    assert!(csma.difs_wait_start.is_none());
    assert!(csma.cw_wait_start.is_none());
    assert_eq!(csma.cw_wait_passed, 0);
    assert!(csma.csma_cw.is_none());
    assert_eq!(csma.csma_payload(), [1, 0, 15]);
}

#[test]
fn difs_and_contention_window_must_elapse_while_medium_free() {
    let mut csma = Csma::new();
    csma.set_avoid_interference(false);
    csma.cw_min = 2;
    csma.cw_max = 3;
    let mut rng = Rng::new(1);
    csma.observe(0, false, -120);

    assert_eq!(csma.poll_access(0, &mut rng), Access::Wait);
    assert_eq!(csma.poll_access(u64::from(csma.difs_ms) - 1, &mut rng), Access::Wait);
    assert_eq!(csma.poll_access(u64::from(csma.difs_ms), &mut rng), Access::Wait);
    assert_eq!(
        csma.poll_access(u64::from(csma.difs_ms + csma.csma_slot_ms), &mut rng),
        Access::Wait
    );
    assert_eq!(
        csma.poll_access(u64::from(csma.difs_ms + 2 * csma.csma_slot_ms), &mut rng),
        Access::Flush
    );
}

#[test]
fn busy_medium_restarts_difs_without_discarding_elapsed_contention() {
    let mut csma = Csma::new();
    csma.set_avoid_interference(false);
    csma.cw_min = 2;
    csma.cw_max = 3;
    let mut rng = Rng::new(1);

    csma.observe(0, false, -120);
    assert_eq!(csma.poll_access(0, &mut rng), Access::Wait);
    assert_eq!(csma.poll_access(48, &mut rng), Access::Wait);
    assert_eq!(csma.poll_access(72, &mut rng), Access::Wait); // one slot accumulated

    csma.observe(73, true, -80);
    assert_eq!(csma.poll_access(73, &mut rng), Access::Wait);

    csma.observe(80, false, -120);
    assert_eq!(csma.poll_access(80, &mut rng), Access::Wait);
    assert_eq!(csma.poll_access(127, &mut rng), Access::Wait); // restarted DIFS not done
    assert_eq!(csma.poll_access(128, &mut rng), Access::Wait); // CW timing resumes
    assert_eq!(csma.poll_access(152, &mut rng), Access::Flush);
}

#[test]
fn access_grant_uses_flush_unless_limit_or_guard_rate() {
    let mut csma = Csma::new();
    csma.set_avoid_interference(false);
    csma.observe(0, false, -120);
    let difs = u64::from(csma.difs_ms);
    assert_eq!(poll_until_grant(&mut csma, difs), Access::Flush);

    let mut single = Csma::new();
    single.set_avoid_interference(false);
    single.limit_or_guard_rate = true;
    single.observe(0, false, -120);
    let difs = u64::from(single.difs_ms);
    assert_eq!(poll_until_grant(&mut single, difs), Access::Single);
}

#[test]
fn persistent_low_interference_resets_noise_floor_sampling() {
    let mut csma = Csma::new();
    for i in 0..NOISE_FLOOR_SAMPLES {
        csma.observe(i as u64 * STATUS_INTERVAL_MS, false, -120);
        csma.status_tick(i as u64 * STATUS_INTERVAL_MS);
    }
    assert_eq!(csma.channel_stats().noise_floor, -120);
    assert!(csma.noise_floor_sampled);

    csma.observe(5_000, false, CSMA_RFENV_RECAL_LIMIT_DB);
    csma.observe(5_000 + CSMA_RFENV_RECAL_MS, false, CSMA_RFENV_RECAL_LIMIT_DB);
    assert!(csma.channel_stats().interference);
    assert!(csma.interference_start.is_none());
    assert!(csma.noise_floor_sampled);

    csma.observe(10_000, false, -100);
    csma.observe(10_000 + CSMA_RFENV_RECAL_MS, false, -100);
    assert!(csma.channel_stats().interference);
    assert!(!csma.medium_free());
    assert!(!csma.noise_floor_sampled);

    for i in 0..NOISE_FLOOR_SAMPLES {
        csma.observe(13_000 + i as u64 * STATUS_INTERVAL_MS, false, -100);
        assert!(csma.channel_stats().interference);
        assert!(!csma.medium_free());
        csma.status_tick(13_000 + i as u64 * STATUS_INTERVAL_MS);
    }
    assert_eq!(csma.channel_stats().noise_floor, -100);
    csma.observe(14_000, false, -100);
    assert!(!csma.channel_stats().interference);
    assert!(csma.medium_free());
}

#[test]
fn airtime_cost_accumulates_in_u16_bins_with_float_truncation() {
    let mut csma = Csma::new();
    csma.add_airtime(0, 1.9);
    assert_eq!(csma.airtime_bins[0], 1);
    csma.add_airtime(0, 1.9);
    assert_eq!(csma.airtime_bins[0], 2);
    assert_eq!(csma.airtime_bins[1], 0);
}

#[test]
fn poll_access_waits_while_airtime_locked_and_resumes_after_release() {
    let mut csma = Csma::new();
    csma.set_avoid_interference(false);
    csma.cw_min = 2;
    csma.cw_max = 3;
    let mut rng = Rng::new(1);
    csma.observe(0, false, -120);
    assert_eq!(csma.poll_access(0, &mut rng), Access::Wait);
    assert_eq!(csma.poll_access(u64::from(csma.difs_ms), &mut rng), Access::Wait);
    assert_eq!(
        csma.poll_access(u64::from(csma.difs_ms) + u64::from(csma.csma_slot_ms) / 2, &mut rng),
        Access::Wait
    );
    assert!(csma.csma_cw.is_some());
    assert!(csma.cw_wait_start.is_some());

    csma.set_st_airtime_limit(1_000);
    csma.add_airtime(0, 2000.0);
    csma.update_airtime(0);
    assert!(csma.airtime_locked());

    assert_eq!(csma.poll_access(5000, &mut rng), Access::Wait);
    assert!(csma.csma_cw.is_none());
    assert!(csma.difs_wait_start.is_none());
    assert!(csma.cw_wait_start.is_none());
    assert_eq!(csma.cw_wait_passed, 0);

    csma.update_airtime(2 * AIRTIME_BINLEN_MS);
    assert!(!csma.airtime_locked());
    csma.cw_min = 0;
    csma.cw_max = 1;
    assert_eq!(csma.poll_access(2 * AIRTIME_BINLEN_MS, &mut rng), Access::Wait);
    assert_eq!(
        csma.poll_access(2 * AIRTIME_BINLEN_MS + u64::from(csma.difs_ms) - 1, &mut rng),
        Access::Wait
    );
    assert_eq!(
        csma.poll_access(2 * AIRTIME_BINLEN_MS + u64::from(csma.difs_ms), &mut rng),
        Access::Wait
    );
    assert_eq!(
        csma.poll_access(2 * AIRTIME_BINLEN_MS + u64::from(csma.difs_ms), &mut rng),
        Access::Flush
    );
}

#[test]
fn longterm_airtime_lock_engages_and_releases_as_bins_age() {
    let mut csma = Csma::new();
    csma.set_lt_airtime_limit(10);
    for bin in [0, 2, 4, 6] {
        csma.add_airtime(bin * AIRTIME_BINLEN_MS, 1000.0);
    }
    csma.update_airtime(6 * AIRTIME_BINLEN_MS);
    csma.update_airtime(6 * AIRTIME_BINLEN_MS);
    assert!(csma.longterm_airtime >= 0.001);
    assert!(csma.airtime_locked());

    for bin in 7..(AIRTIME_BINS as u64 + 7) {
        csma.update_airtime(bin * AIRTIME_BINLEN_MS);
    }
    assert_eq!(csma.longterm_airtime, 0.0);
    assert!(!csma.airtime_locked());
}

#[test]
fn contention_band_changes_with_airtime_using_firmware_integer_map() {
    let mut csma = Csma::new();
    csma.add_airtime(0, 1200.0); // 8% short-term airtime.
    let update = csma.update_airtime(0);
    assert!(update.csma_changed);
    assert_eq!(csma.csma_payload(), [2, 15, 29]);

    csma.airtime_bins[0] = 1300; // Still band 2.
    let update = csma.update_airtime(0);
    assert!(!update.csma_changed);
    assert_eq!(csma.csma_payload(), [2, 15, 29]);

    csma.airtime_bins[0] = 6500; // 43%.
    let update = csma.update_airtime(0);
    assert!(update.csma_changed);
    assert_eq!(csma.csma_payload(), [3, 30, 44]);

    csma.airtime_bins[0] = 13_000; // 86%, capped at band 4.
    let update = csma.update_airtime(0);
    assert!(update.csma_changed);
    assert_eq!(csma.csma_payload(), [4, 45, 59]);

    csma.airtime_bins[0] = 500; // back below band 1 threshold.
    let update = csma.update_airtime(0);
    assert!(update.csma_changed);
    assert_eq!(csma.csma_payload(), [1, 0, 14]);
}
