//! Channel access (CSMA/CA), airtime accounting and channel statistics.
//!
//! Firmware counterparts: `tx_queue_handler()`, `medium_free()`, `update_modem_status()`,
//! `update_noise_floor()`, `check_modem_status()`, `add_airtime()`, `update_airtime()`,
//! `update_csma_parameters()` and the airtime-lock check in `loop()`.
//!
//! The node drives it as follows while the radio is online:
//! - every [`crate::node::STATUS_INTERVAL_MS`]: read DCD and RSSI from the radio, call
//!   [`Csma::observe`], then [`Csma::status_tick`];
//! - while packets are queued and [`Csma::airtime_locked`] is false: refresh the observation and
//!   ask [`Csma::poll_access`] whether to send all queued packets, one, or wait;
//! - after each transmitted frame: [`Csma::add_airtime`]; after a send burst:
//!   [`Csma::update_airtime`];
//! - report [`Csma::channel_stats_payload`] / [`Csma::csma_payload`] to the host when the returned
//!   [`StatusUpdate`] says so.
//!
use crate::phy::PhyParams;
use crate::rng::Rng;

/// Firmware `STATUS_INTERVAL_MS`.
pub const STATUS_INTERVAL_MS: u64 = 3;
/// Firmware `DCD_SAMPLES`.
pub const DCD_SAMPLES: usize = 2500;
/// Firmware `UTIL_UPDATE_INTERVAL_MS`.
pub const UTIL_UPDATE_INTERVAL_MS: u64 = 1000;
/// Firmware `UTIL_UPDATE_INTERVAL`.
pub const UTIL_UPDATE_INTERVAL: usize = (UTIL_UPDATE_INTERVAL_MS / STATUS_INTERVAL_MS) as usize;
/// Firmware `AIRTIME_LONGTERM`.
pub const AIRTIME_LONGTERM: u64 = 3600;
/// Firmware `AIRTIME_LONGTERM_MS`.
pub const AIRTIME_LONGTERM_MS: u64 = AIRTIME_LONGTERM * 1000;
/// Firmware `AIRTIME_BINLEN_MS`.
pub const AIRTIME_BINLEN_MS: u64 = STATUS_INTERVAL_MS * DCD_SAMPLES as u64;
/// Firmware `AIRTIME_BINS`.
pub const AIRTIME_BINS: usize = (AIRTIME_LONGTERM_MS / AIRTIME_BINLEN_MS) as usize;
/// Firmware `NOISE_FLOOR_SAMPLES`.
pub const NOISE_FLOOR_SAMPLES: usize = 128;
/// Firmware `CSMA_CW_BANDS`.
pub const CSMA_CW_BANDS: u8 = 4;
/// Firmware `CSMA_CW_MIN`.
pub const CSMA_CW_MIN: u8 = 0;
/// Firmware `CSMA_CW_PER_BAND_WINDOWS`.
pub const CSMA_CW_PER_BAND_WINDOWS: u8 = 15;
/// Firmware `CSMA_BAND_1_MAX_AIRTIME`.
pub const CSMA_BAND_1_MAX_AIRTIME: i32 = 7;
/// Firmware `CSMA_BAND_N_MIN_AIRTIME`.
pub const CSMA_BAND_N_MIN_AIRTIME: i32 = 85;
/// Firmware `CSMA_INFR_THRESHOLD_DB`.
pub const CSMA_INFR_THRESHOLD_DB: i16 = 11;
/// Firmware `CSMA_RFENV_RECAL_MS`.
pub const CSMA_RFENV_RECAL_MS: u64 = 2500;
/// Firmware `CSMA_RFENV_RECAL_LIMIT_DB`.
pub const CSMA_RFENV_RECAL_LIMIT_DB: i16 = -83;
const INITIAL_RSSI_DBM: i16 = -292;

/// What the TX queue handler may do now (firmware `tx_queue_handler()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Keep waiting (medium busy, DIFS or contention window not yet elapsed).
    Wait,
    /// Transmit every queued packet back to back (firmware `flush_queue()`).
    Flush,
    /// Transmit one queued packet (firmware `pop_queue()`, used at limit/guard bitrates).
    Single,
}

/// Channel statistics reported with `CMD_STAT_CHTM`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChannelStats {
    /// Short-term airtime as a fraction (`airtime`).
    pub airtime: f32,
    /// Long-term (one hour) airtime as a fraction (`longterm_airtime`).
    pub longterm_airtime: f32,
    /// Short-term channel utilisation including other nodes (`total_channel_util`).
    pub total_channel_util: f32,
    /// Long-term channel utilisation (`longterm_channel_util`).
    pub longterm_channel_util: f32,
    /// Last current-RSSI reading in dBm (`current_rssi`).
    pub current_rssi: i16,
    /// Noise floor in dBm (`noise_floor`).
    pub noise_floor: i16,
    /// Whether interference is currently detected (`interference_detected`).
    pub interference: bool,
}

/// What the caller should report to the host after a status update.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatusUpdate {
    /// Send `CMD_STAT_CHTM` with [`Csma::channel_stats_payload`].
    pub channel_stats_due: bool,
    /// Send `CMD_STAT_CSMA` with [`Csma::csma_payload`] (the contention band changed).
    pub csma_changed: bool,
}

/// CSMA/CA and airtime state.
#[derive(Debug, Clone)]
pub struct Csma {
    csma_slot_ms: u32,
    difs_ms: u32,
    limit_or_guard_rate: bool,
    st_airtime_limit: u16,
    lt_airtime_limit: u16,
    airtime_lock: bool,
    carrier: bool,
    current_rssi: i16,
    noise_floor: i16,
    noise_floor_sampled: bool,
    noise_floor_sample: usize,
    noise_floor_buffer: [i16; NOISE_FLOOR_SAMPLES],
    interference: bool,
    avoid_interference: bool,
    interference_start: Option<u64>,
    util_samples: [bool; DCD_SAMPLES],
    airtime_bins: [u16; AIRTIME_BINS],
    longterm_bins: [f32; AIRTIME_BINS],
    dcd_sample: usize,
    local_channel_util: f32,
    total_channel_util: f32,
    longterm_channel_util: f32,
    airtime: f32,
    longterm_airtime: f32,
    difs_wait_start: Option<u64>,
    cw_wait_start: Option<u64>,
    cw_wait_target: u64,
    cw_wait_passed: u64,
    csma_cw: Option<u32>,
    cw_band: u8,
    cw_min: u8,
    cw_max: u8,
}

impl Default for Csma {
    fn default() -> Self {
        Self::new()
    }
}

impl Csma {
    /// Fresh state, as at firmware boot.
    pub fn new() -> Self {
        Self {
            csma_slot_ms: crate::phy::CSMA_SLOT_MIN_MS,
            difs_ms: crate::phy::CSMA_SIFS_MS + 2 * crate::phy::CSMA_SLOT_MIN_MS,
            limit_or_guard_rate: false,
            st_airtime_limit: 0,
            lt_airtime_limit: 0,
            airtime_lock: false,
            carrier: false,
            current_rssi: INITIAL_RSSI_DBM,
            noise_floor: INITIAL_RSSI_DBM,
            noise_floor_sampled: false,
            noise_floor_sample: 0,
            noise_floor_buffer: [0; NOISE_FLOOR_SAMPLES],
            interference: false,
            avoid_interference: true,
            interference_start: None,
            util_samples: [false; DCD_SAMPLES],
            airtime_bins: [0; AIRTIME_BINS],
            longterm_bins: [0.0; AIRTIME_BINS],
            dcd_sample: 0,
            local_channel_util: 0.0,
            total_channel_util: 0.0,
            longterm_channel_util: 0.0,
            airtime: 0.0,
            longterm_airtime: 0.0,
            difs_wait_start: None,
            cw_wait_start: None,
            cw_wait_target: 0,
            cw_wait_passed: 0,
            csma_cw: None,
            cw_band: 1,
            cw_min: CSMA_CW_MIN,
            cw_max: CSMA_CW_PER_BAND_WINDOWS,
        }
    }

    /// Reset channel statistics when the radio comes online. This includes firmware
    /// `init_channel_stats()` fields plus Linux-core-only derived state (airtime lock,
    /// contention wait, contention band and interference timer) so reconnects start from a
    /// coherent idle channel-access state while preserving RSSI/noise-floor calibration.
    pub fn reset(&mut self) {
        self.carrier = false;
        self.interference = false;
        self.interference_start = None;
        self.util_samples = [false; DCD_SAMPLES];
        self.airtime_bins = [0; AIRTIME_BINS];
        self.longterm_bins = [0.0; AIRTIME_BINS];
        self.dcd_sample = 0;
        self.local_channel_util = 0.0;
        self.total_channel_util = 0.0;
        self.longterm_channel_util = 0.0;
        self.airtime = 0.0;
        self.longterm_airtime = 0.0;
        self.airtime_lock = false;
        self.difs_wait_start = None;
        self.cw_wait_start = None;
        self.cw_wait_passed = 0;
        self.csma_cw = None;
        self.cw_band = 1;
        self.cw_min = CSMA_CW_MIN;
        self.cw_max = CSMA_CW_PER_BAND_WINDOWS;
    }

    /// Apply the timing derived by `updateBitrate()`: slot, DIFS and the limit/guard flags.
    pub fn set_phy(&mut self, phy: &PhyParams) {
        self.csma_slot_ms = phy.csma_slot_ms;
        self.difs_ms = phy.difs_ms;
        self.limit_or_guard_rate = phy.limit_rate || phy.guard_rate;
    }

    /// Set the short-term airtime limit as sent with `CMD_ST_ALOCK`: percent × 100, 0 for none.
    /// Values of 100 % or more disable the limit, as in the firmware.
    pub fn set_st_airtime_limit(&mut self, raw: u16) {
        self.st_airtime_limit = if raw >= 10_000 { 0 } else { raw };
        self.update_airtime_lock();
    }

    /// The short-term airtime limit in `CMD_ST_ALOCK` units.
    pub fn st_airtime_limit(&self) -> u16 {
        self.st_airtime_limit
    }

    /// Set the long-term airtime limit as sent with `CMD_LT_ALOCK`: percent × 100, 0 for none.
    pub fn set_lt_airtime_limit(&mut self, raw: u16) {
        self.lt_airtime_limit = if raw >= 10_000 { 0 } else { raw };
        self.update_airtime_lock();
    }

    /// The long-term airtime limit in `CMD_LT_ALOCK` units.
    pub fn lt_airtime_limit(&self) -> u16 {
        self.lt_airtime_limit
    }

    /// Configure whether detected non-LoRa interference makes the medium busy. Firmware reads
    /// this from EEPROM (`avoid_interference`); the boot default is `true`.
    pub fn set_avoid_interference(&mut self, avoid: bool) {
        self.avoid_interference = avoid;
    }

    /// Whether detected non-LoRa interference makes the medium busy.
    pub fn avoid_interference(&self) -> bool {
        self.avoid_interference
    }

    /// Record a fresh carrier-detect and current-RSSI reading (firmware
    /// `update_modem_status()`). Board-specific HELTEC32_V4/LNA gain branches are deliberately
    /// omitted: this core is board-independent and the reference SX1262 target does not need
    /// those compensations.
    pub fn observe(&mut self, now_ms: u64, carrier: bool, rssi_dbm: i16) {
        self.carrier = carrier;
        self.current_rssi = rssi_dbm;
        self.interference = !carrier && rssi_dbm > self.noise_floor + CSMA_INFR_THRESHOLD_DB;

        if self.interference && rssi_dbm < CSMA_RFENV_RECAL_LIMIT_DB {
            match self.interference_start {
                None => self.interference_start = Some(now_ms),
                Some(start) if now_ms.saturating_sub(start) >= CSMA_RFENV_RECAL_MS => {
                    self.noise_floor_sampled = false;
                    self.interference_start = None;
                }
                Some(_) => {}
            }
        } else {
            self.interference_start = None;
        }
    }

    /// Periodic status work after [`Csma::observe`] (firmware `check_modem_status()`): noise
    /// floor, channel utilisation and the once-a-second airtime update.
    pub fn status_tick(&mut self, now_ms: u64) -> StatusUpdate {
        self.update_noise_floor();

        self.util_samples[self.dcd_sample] = self.carrier;
        self.dcd_sample = (self.dcd_sample + 1) % DCD_SAMPLES;
        if self.dcd_sample % UTIL_UPDATE_INTERVAL == 0 {
            let util_count = self.util_samples.iter().filter(|&&dcd| dcd).count();
            self.local_channel_util = util_count as f32 / DCD_SAMPLES as f32;
            self.total_channel_util = (self.local_channel_util + self.airtime).min(1.0);

            let cb = current_airtime_bin(now_ms);
            let nb = next_airtime_bin(cb);
            if self.total_channel_util > self.longterm_bins[cb] {
                self.longterm_bins[cb] = self.total_channel_util;
            }
            self.longterm_bins[nb] = 0.0;

            self.update_airtime(now_ms)
        } else {
            StatusUpdate::default()
        }
    }

    /// Account the airtime of a transmitted frame (firmware `add_airtime()`).
    pub fn add_airtime(&mut self, now_ms: u64, cost_ms: f32) {
        let cb = current_airtime_bin(now_ms);
        let nb = next_airtime_bin(cb);
        self.airtime_bins[cb] = (f32::from(self.airtime_bins[cb]) + cost_ms) as u16;
        self.airtime_bins[nb] = 0;
    }

    /// Recompute airtime figures and contention bands after a send burst (firmware
    /// `update_airtime()`).
    pub fn update_airtime(&mut self, now_ms: u64) -> StatusUpdate {
        let cb = current_airtime_bin(now_ms);
        let pb = previous_airtime_bin(cb);
        let nb = next_airtime_bin(cb);
        self.airtime_bins[nb] = 0;
        let shortterm_airtime_sum = u32::from(self.airtime_bins[cb]) + u32::from(self.airtime_bins[pb]);
        self.airtime = shortterm_airtime_sum as f32 / (2.0 * AIRTIME_BINLEN_MS as f32);

        let longterm_airtime_sum: u32 = self.airtime_bins.iter().map(|&bin| u32::from(bin)).sum();
        self.longterm_airtime = longterm_airtime_sum as f32 / AIRTIME_LONGTERM_MS as f32;

        let longterm_channel_util_sum: f32 = self.longterm_bins.iter().sum();
        self.longterm_channel_util = longterm_channel_util_sum / AIRTIME_BINS as f32;

        let csma_changed = self.update_csma_parameters();
        self.update_airtime_lock();

        StatusUpdate {
            channel_stats_due: true,
            csma_changed,
        }
    }

    /// Whether the airtime limits currently block transmission (firmware `airtime_lock`).
    pub fn airtime_locked(&self) -> bool {
        self.airtime_lock
    }

    /// Whether the medium is free (firmware `medium_free()`), from the last observation.
    pub fn medium_free(&self) -> bool {
        !(self.carrier || (self.avoid_interference && self.interference))
    }

    /// Channel access decision while packets are queued (firmware `tx_queue_handler()`).
    /// Call after a fresh [`Csma::observe`].
    pub fn poll_access(&mut self, now_ms: u64, rng: &mut Rng) -> Access {
        if self.airtime_locked() {
            self.reset_contention_wait();
            return Access::Wait;
        }
        if self.csma_cw.is_none() {
            let cw = rng.range(u32::from(self.cw_min), u32::from(self.cw_max));
            self.csma_cw = Some(cw);
            self.cw_wait_target = cw as u64 * u64::from(self.csma_slot_ms);
        }

        if self.difs_wait_start.is_none() {
            if self.medium_free() {
                self.difs_wait_start = Some(now_ms);
            }
            return Access::Wait;
        }

        if !self.medium_free() {
            self.difs_wait_start = None;
            self.cw_wait_start = None;
            Access::Wait
        } else {
            let difs_wait_start = self.difs_wait_start.expect("checked above");
            if now_ms < difs_wait_start + u64::from(self.difs_ms) {
                return Access::Wait;
            }

            if let Some(cw_wait_start) = self.cw_wait_start {
                self.cw_wait_passed = self.cw_wait_passed.saturating_add(now_ms.saturating_sub(cw_wait_start));
                self.cw_wait_start = Some(now_ms);
                if self.cw_wait_passed < self.cw_wait_target {
                    return Access::Wait;
                }

                self.reset_contention_wait();
                if self.limit_or_guard_rate {
                    Access::Single
                } else {
                    Access::Flush
                }
            } else {
                self.cw_wait_start = Some(now_ms);
                Access::Wait
            }
        }
    }

    /// The current channel statistics.
    pub fn channel_stats(&self) -> ChannelStats {
        ChannelStats {
            airtime: self.airtime,
            longterm_airtime: self.longterm_airtime,
            total_channel_util: self.total_channel_util,
            longterm_channel_util: self.longterm_channel_util,
            current_rssi: self.current_rssi,
            noise_floor: self.noise_floor,
            interference: self.interference,
        }
    }

    /// The 11-byte payload of `CMD_STAT_CHTM` (firmware `kiss_indicate_channel_stats()`),
    /// before KISS escaping.
    pub fn channel_stats_payload(&self) -> [u8; 11] {
        let s = self.channel_stats();
        let ats = (s.airtime * 100.0 * 100.0) as u16;
        let atl = (s.longterm_airtime * 100.0 * 100.0) as u16;
        let cls = (s.total_channel_util * 100.0 * 100.0) as u16;
        let cll = (s.longterm_channel_util * 100.0 * 100.0) as u16;
        let crs = (i32::from(s.current_rssi) + crate::RSSI_OFFSET) as u8;
        let nfl = (i32::from(s.noise_floor) + crate::RSSI_OFFSET) as u8;
        let ntf = if s.interference { crs } else { 0xFF };
        let mut out = [0u8; 11];
        out[0..2].copy_from_slice(&ats.to_be_bytes());
        out[2..4].copy_from_slice(&atl.to_be_bytes());
        out[4..6].copy_from_slice(&cls.to_be_bytes());
        out[6..8].copy_from_slice(&cll.to_be_bytes());
        out[8] = crs;
        out[9] = nfl;
        out[10] = ntf;
        out
    }

    /// The 3-byte payload of `CMD_STAT_CSMA` (firmware `kiss_indicate_csma_stats()`):
    /// contention band, window minimum and maximum.
    pub fn csma_payload(&self) -> [u8; 3] {
        [self.cw_band, self.cw_min, self.cw_max]
    }

    fn update_noise_floor(&mut self) {
        if !self.carrier && (!self.noise_floor_sampled || self.current_rssi < self.noise_floor + CSMA_INFR_THRESHOLD_DB)
        {
            self.noise_floor_buffer[self.noise_floor_sample] = self.current_rssi;
            self.noise_floor_sample += 1;
            let mut sum_noise_floor = false;
            if self.noise_floor_sample >= NOISE_FLOOR_SAMPLES {
                self.noise_floor_sample %= NOISE_FLOOR_SAMPLES;
                self.noise_floor_sampled = true;
                sum_noise_floor = true;
            }

            if self.noise_floor_sampled && sum_noise_floor {
                let sum: i32 = self.noise_floor_buffer.iter().map(|&sample| i32::from(sample)).sum();
                self.noise_floor = (sum / NOISE_FLOOR_SAMPLES as i32) as i16;
            }
        }
    }

    fn update_csma_parameters(&mut self) -> bool {
        let airtime_pct = (self.airtime * 100.0) as i32;
        let mut new_cw_band = if airtime_pct <= CSMA_BAND_1_MAX_AIRTIME {
            1
        } else {
            let at = airtime_pct + CSMA_BAND_1_MAX_AIRTIME;
            arduino_map(
                at,
                CSMA_BAND_1_MAX_AIRTIME,
                CSMA_BAND_N_MIN_AIRTIME,
                2,
                i32::from(CSMA_CW_BANDS),
            )
        };

        if new_cw_band > i32::from(CSMA_CW_BANDS) {
            new_cw_band = i32::from(CSMA_CW_BANDS);
        }

        if new_cw_band as u8 != self.cw_band {
            self.cw_band = new_cw_band as u8;
            self.cw_min = (self.cw_band - 1) * CSMA_CW_PER_BAND_WINDOWS;
            self.cw_max = self.cw_band * CSMA_CW_PER_BAND_WINDOWS - 1;
            true
        } else {
            false
        }
    }

    fn update_airtime_lock(&mut self) {
        self.airtime_lock = false;
        if self.st_airtime_limit != 0 && self.airtime >= f32::from(self.st_airtime_limit) / 10_000.0 {
            self.airtime_lock = true;
        }
        if self.lt_airtime_limit != 0 && self.longterm_airtime >= f32::from(self.lt_airtime_limit) / 10_000.0 {
            self.airtime_lock = true;
        }
    }

    fn reset_contention_wait(&mut self) {
        self.difs_wait_start = None;
        self.cw_wait_start = None;
        self.cw_wait_passed = 0;
        self.csma_cw = None;
    }
}

fn current_airtime_bin(now_ms: u64) -> usize {
    ((now_ms % AIRTIME_LONGTERM_MS) / AIRTIME_BINLEN_MS) as usize
}

fn next_airtime_bin(bin: usize) -> usize {
    (bin + 1) % AIRTIME_BINS
}

fn previous_airtime_bin(bin: usize) -> usize {
    if bin == 0 { AIRTIME_BINS - 1 } else { bin - 1 }
}

fn arduino_map(x: i32, in_min: i32, in_max: i32, out_min: i32, out_max: i32) -> i32 {
    (x - in_min) * (out_max - out_min) / (in_max - in_min) + out_min
}

#[cfg(test)]
mod tests {
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
    fn airtime_cost_accumulates_in_u16_bins_with_float_truncation() {
        let mut csma = Csma::new();
        csma.add_airtime(0, 1.9);
        assert_eq!(csma.airtime_bins[0], 1);
        csma.add_airtime(0, 1.9);
        assert_eq!(csma.airtime_bins[0], 2);
        assert_eq!(csma.airtime_bins[1], 0);
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
}
