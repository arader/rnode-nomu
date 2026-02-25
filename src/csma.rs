// Copyright (C) 2024, Mark Qvist
// Ported to Rust for Linux by RNode contributors.
// Licensed under the GNU General Public License v3.0 or later.

//! CSMA/CA (Carrier Sense Multiple Access with Collision Avoidance) logic.
//!
//! Ported from the firmware's CSMA implementation with airtime tracking.

#![allow(dead_code)]

use std::time::{Duration, Instant};

use crate::radio::Radio;

// --- CSMA constants (matching firmware Config.h) ---
const CSMA_SIFS_MS: u64 = 0;
const CSMA_POST_TX_YIELD_SLOTS: u32 = 3;
const CSMA_SLOT_SYMBOLS: u32 = 12;
const CSMA_CW_BANDS: u8 = 4;
const CSMA_CW_MIN: u8 = 0;
const CSMA_CW_PER_BAND_WINDOWS: u8 = 15;
const CSMA_BAND_1_MAX_AIRTIME: u8 = 7;
const CSMA_BAND_N_MIN_AIRTIME: u8 = 85;

const STATUS_INTERVAL_MS: u64 = 3;
const DCD_SAMPLES: usize = 2500;
const UTIL_UPDATE_INTERVAL_MS: u64 = 1000;
const AIRTIME_LONGTERM_S: u64 = 3600;
const AIRTIME_LONGTERM_MS: u64 = AIRTIME_LONGTERM_S * 1000;
const AIRTIME_BINLEN_MS: u64 = STATUS_INTERVAL_MS * DCD_SAMPLES as u64;
const AIRTIME_BINS: usize = (AIRTIME_LONGTERM_MS / AIRTIME_BINLEN_MS) as usize;

pub struct CsmaState {
    enabled: bool,
    slot_min_ms: u64,
    slot_max_ms: u64,
    csma_slot_ms: u64,

    // Airtime tracking
    airtime_bins: Vec<u16>,
    longterm_bins: Vec<f32>,
    util_samples: Vec<bool>,
    dcd_sample: usize,
    local_channel_util: f32,
    total_channel_util: f32,
    longterm_channel_util: f32,
    airtime: f32,
    longterm_airtime: f32,
    st_airtime_limit: f32,
    lt_airtime_limit: f32,
    airtime_lock: bool,

    // DCD state
    dcd: bool,
    dcd_count: u16,
    dcd_threshold: u16,
    last_status_update: Instant,

    // Contention window
    cw_band: u8,
    cw_min: u8,
    cw_max: u8,
    difs_ms: u64,

    started: Instant,
}

impl CsmaState {
    pub fn new(enabled: bool, slot_min_ms: u64, slot_max_ms: u64) -> Self {
        let csma_slot_ms = slot_min_ms;
        let difs_ms = CSMA_SIFS_MS + 2 * csma_slot_ms;
        let now = Instant::now();
        Self {
            enabled,
            slot_min_ms,
            slot_max_ms,
            csma_slot_ms,
            airtime_bins: vec![0u16; AIRTIME_BINS],
            longterm_bins: vec![0.0f32; AIRTIME_BINS],
            util_samples: vec![false; DCD_SAMPLES],
            dcd_sample: 0,
            local_channel_util: 0.0,
            total_channel_util: 0.0,
            longterm_channel_util: 0.0,
            airtime: 0.0,
            longterm_airtime: 0.0,
            st_airtime_limit: 0.0,
            lt_airtime_limit: 0.0,
            airtime_lock: false,
            dcd: false,
            dcd_count: 0,
            dcd_threshold: 2,
            last_status_update: now,
            cw_band: 1,
            cw_min: 0,
            cw_max: CSMA_CW_PER_BAND_WINDOWS,
            difs_ms,
            started: now,
        }
    }

    pub fn set_st_airtime_limit(&mut self, limit: f32) {
        self.st_airtime_limit = if limit >= 1.0 { 0.0 } else { limit };
    }

    pub fn set_lt_airtime_limit(&mut self, limit: f32) {
        self.lt_airtime_limit = if limit >= 1.0 { 0.0 } else { limit };
    }

    pub fn st_airtime_limit_raw(&self) -> u16 {
        (self.st_airtime_limit * 100.0 * 100.0) as u16
    }

    pub fn lt_airtime_limit_raw(&self) -> u16 {
        (self.lt_airtime_limit * 100.0 * 100.0) as u16
    }

    pub fn is_airtime_locked(&self) -> bool {
        self.airtime_lock
    }

    /// Update DCD (Data Carrier Detect) state by polling the radio.
    pub fn update_dcd(&mut self, radio: &mut dyn Radio) {
        if !self.enabled {
            return;
        }

        let now = Instant::now();
        if now.duration_since(self.last_status_update) < Duration::from_millis(STATUS_INTERVAL_MS) {
            return;
        }
        self.last_status_update = now;

        let carrier = radio.dcd().unwrap_or(false);
        self.util_samples[self.dcd_sample] = carrier;
        self.dcd_sample = (self.dcd_sample + 1) % DCD_SAMPLES;

        if carrier {
            self.dcd_count = self.dcd_count.saturating_add(1);
        } else {
            self.dcd_count = self.dcd_count.saturating_sub(1);
        }

        self.dcd = self.dcd_count >= self.dcd_threshold;
    }

    /// Check whether it's clear to transmit (CSMA/CA).
    /// Returns true if the channel is idle and we've waited enough.
    pub fn can_transmit(&self) -> bool {
        if !self.enabled {
            return true;
        }

        if self.airtime_lock {
            return false;
        }

        !self.dcd
    }

    /// Record airtime usage for a transmitted packet.
    pub fn add_airtime_ms(&mut self, ms: u64) {
        let elapsed = self.started.elapsed().as_millis() as u64;
        let bin = (elapsed % AIRTIME_LONGTERM_MS) / AIRTIME_BINLEN_MS;
        if (bin as usize) < self.airtime_bins.len() {
            self.airtime_bins[bin as usize] = self.airtime_bins[bin as usize].saturating_add(ms as u16);
        }

        // Update short-term airtime
        self.airtime = self.calculate_short_term_airtime();
        self.longterm_airtime = self.calculate_long_term_airtime();

        // Check limits
        if self.st_airtime_limit > 0.0 && self.airtime > self.st_airtime_limit {
            self.airtime_lock = true;
        }
        if self.lt_airtime_limit > 0.0 && self.longterm_airtime > self.lt_airtime_limit {
            self.airtime_lock = true;
        }
        if (self.st_airtime_limit == 0.0 || self.airtime <= self.st_airtime_limit)
            && (self.lt_airtime_limit == 0.0 || self.longterm_airtime <= self.lt_airtime_limit)
        {
            self.airtime_lock = false;
        }
    }

    fn calculate_short_term_airtime(&self) -> f32 {
        let elapsed = self.started.elapsed().as_millis() as u64;
        let current_bin = (elapsed % AIRTIME_LONGTERM_MS) / AIRTIME_BINLEN_MS;

        // Sum the last few bins for short-term
        let window = 4usize;
        let mut total_ms: u64 = 0;
        for i in 0..window {
            let bin = if current_bin as usize >= i {
                current_bin as usize - i
            } else {
                AIRTIME_BINS - (i - current_bin as usize)
            };
            total_ms += self.airtime_bins[bin] as u64;
        }

        let window_ms = window as u64 * AIRTIME_BINLEN_MS;
        if window_ms > 0 {
            total_ms as f32 / window_ms as f32
        } else {
            0.0
        }
    }

    fn calculate_long_term_airtime(&self) -> f32 {
        let total_ms: u64 = self.airtime_bins.iter().map(|&b| b as u64).sum();
        if AIRTIME_LONGTERM_MS > 0 {
            total_ms as f32 / AIRTIME_LONGTERM_MS as f32
        } else {
            0.0
        }
    }
}

/// Calculate the on-air time of a LoRa packet in milliseconds.
pub fn calculate_airtime_ms(
    payload_bytes: usize,
    sf: u8,
    bw_hz: u32,
    cr: u8,
    preamble_symbols: u16,
    implicit_header: bool,
    crc_on: bool,
    low_data_rate: bool,
) -> f64 {
    let sf = sf as f64;
    let bw = bw_hz as f64;
    let cr = cr as f64;

    let t_sym = (2.0_f64.powf(sf)) / bw * 1000.0; // symbol time in ms
    let t_preamble = (preamble_symbols as f64 + 4.25) * t_sym;

    let h = if implicit_header { 1.0 } else { 0.0 };
    let de = if low_data_rate { 1.0 } else { 0.0 };
    let crc_val = if crc_on { 1.0 } else { 0.0 };

    let numerator = 8.0 * payload_bytes as f64 - 4.0 * sf + 28.0 + 16.0 * crc_val - 20.0 * h;
    let denominator = 4.0 * (sf - 2.0 * de);
    let payload_symbols = 8.0 + (numerator / denominator).ceil().max(0.0) * (cr + 4.0);

    let t_payload = payload_symbols * t_sym;
    t_preamble + t_payload
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
