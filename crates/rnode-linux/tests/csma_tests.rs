use rnode_linux::csma::{calculate_airtime_ms, CsmaState};

#[test]
fn csma_disabled_always_can_transmit() {
    let csma = CsmaState::new(false, 24, 100);
    assert!(csma.can_transmit());
}

#[test]
fn csma_enabled_idle_can_transmit() {
    let csma = CsmaState::new(true, 24, 100);
    // No DCD detected, no airtime lock
    assert!(csma.can_transmit());
}

#[test]
fn csma_airtime_lock_blocks_transmit() {
    let mut csma = CsmaState::new(true, 24, 100);
    // Set a very low airtime limit then exceed it
    csma.set_st_airtime_limit(0.0001); // 0.01%
    csma.add_airtime_ms(100_000); // way over limit
    assert!(!csma.can_transmit());
}

#[test]
fn csma_airtime_limits_set_and_query() {
    let mut csma = CsmaState::new(true, 24, 100);

    csma.set_st_airtime_limit(0.10); // 10%
    assert_eq!(csma.st_airtime_limit_raw(), 1000);

    csma.set_lt_airtime_limit(0.05); // 5%
    assert_eq!(csma.lt_airtime_limit_raw(), 500);
}

#[test]
fn csma_limit_at_100_percent_disables() {
    let mut csma = CsmaState::new(true, 24, 100);
    csma.set_st_airtime_limit(1.0);
    assert_eq!(csma.st_airtime_limit_raw(), 0); // >= 1.0 resets to 0
}

#[test]
fn csma_initial_state_not_locked() {
    let csma = CsmaState::new(true, 24, 100);
    assert!(!csma.is_airtime_locked());
}

#[test]
fn csma_add_small_airtime_no_lock() {
    let mut csma = CsmaState::new(true, 24, 100);
    csma.set_st_airtime_limit(0.5); // 50%
    csma.add_airtime_ms(1); // tiny amount
    assert!(!csma.is_airtime_locked());
    assert!(csma.can_transmit());
}

// --- Airtime calculation tests ---

#[test]
fn airtime_sf7_125khz_10bytes() {
    // Known reference: SF7, 125kHz, CR4/5, 18 preamble, explicit header, CRC on
    let ms = calculate_airtime_ms(10, 7, 125_000, 1, 18, false, true, false);
    // Should be roughly 36ms for a small packet at SF7/125kHz
    assert!(ms > 20.0, "Airtime too low: {} ms", ms);
    assert!(ms < 80.0, "Airtime too high: {} ms", ms);
}

#[test]
fn airtime_sf12_125khz_50bytes() {
    let ms = calculate_airtime_ms(50, 12, 125_000, 1, 18, false, true, true);
    // SF12 is much slower, expect >1 second
    assert!(ms > 1000.0, "Airtime too low for SF12: {} ms", ms);
    assert!(ms < 10000.0, "Airtime too high for SF12: {} ms", ms);
}

#[test]
fn airtime_increases_with_sf() {
    let ms_sf7 = calculate_airtime_ms(20, 7, 125_000, 1, 18, false, true, false);
    let ms_sf12 = calculate_airtime_ms(20, 12, 125_000, 1, 18, false, true, true);
    assert!(ms_sf12 > ms_sf7 * 10.0, "SF12 should be >10x slower than SF7");
}

#[test]
fn airtime_decreases_with_bandwidth() {
    let ms_125k = calculate_airtime_ms(20, 7, 125_000, 1, 18, false, true, false);
    let ms_500k = calculate_airtime_ms(20, 7, 500_000, 1, 18, false, true, false);
    assert!(ms_125k > ms_500k * 3.0, "125kHz should be ~4x slower than 500kHz");
}

#[test]
fn airtime_increases_with_payload() {
    let ms_10 = calculate_airtime_ms(10, 7, 125_000, 1, 18, false, true, false);
    let ms_200 = calculate_airtime_ms(200, 7, 125_000, 1, 18, false, true, false);
    assert!(ms_200 > ms_10, "Larger payload should take longer");
}

#[test]
fn airtime_zero_payload() {
    let ms = calculate_airtime_ms(0, 7, 125_000, 1, 18, false, true, false);
    // Even 0 payload has preamble + header overhead
    assert!(ms > 0.0, "Zero-payload airtime should still be positive");
}

#[test]
fn airtime_implicit_header_shorter() {
    let ms_explicit = calculate_airtime_ms(20, 7, 125_000, 1, 18, false, true, false);
    let ms_implicit = calculate_airtime_ms(20, 7, 125_000, 1, 18, true, true, false);
    assert!(ms_implicit <= ms_explicit, "Implicit header should be <= explicit");
}
