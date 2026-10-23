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
/// Firmware `LORA_LNA_GVT` (`Boards.h`, the Heltec V4, the firmware's one board with
/// `HAS_LORA_LNA`): with an external LNA, how far in dB below the noise floor an RSSI sample may
/// be before `update_noise_floor()` discards it. The LNA's gain varies while it re-calibrates, and
/// such samples come from that variance, not from the channel.
pub const LORA_LNA_GVT: i16 = 12;
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
    external_lna: bool,
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
            external_lna: false,
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

    /// Configure whether detected non-LoRa interference makes the medium busy (firmware
    /// `avoid_interference`). [`Csma::new`] starts with `true`, the firmware's `Config.h`
    /// initialiser, but the firmware's `setup()` then replaces it from EEPROM: only `0x00` at
    /// `ADDR_CONF_DIA` turns it on, so erased EEPROM turns it off. [`crate::Node`] applies
    /// [`crate::NodeConfig::avoid_interference`] instead.
    pub fn set_avoid_interference(&mut self, avoid: bool) {
        self.avoid_interference = avoid;
    }

    /// Whether detected non-LoRa interference makes the medium busy.
    pub fn avoid_interference(&self) -> bool {
        self.avoid_interference
    }

    /// Configure whether an external LNA amplifies the signal before the modem (firmware
    /// `HAS_LORA_LNA`). With one, [`Csma::status_tick`] leaves RSSI samples more than
    /// [`LORA_LNA_GVT`] dB below the noise floor out of it. Off by default, as for the firmware's
    /// boards without an LNA; [`crate::Node`] applies [`crate::NodeConfig::external_lna`].
    pub fn set_external_lna(&mut self, external_lna: bool) {
        self.external_lna = external_lna;
    }

    /// Whether an external LNA amplifies the signal before the modem.
    pub fn external_lna(&self) -> bool {
        self.external_lna
    }

    /// Record a fresh carrier-detect and current-RSSI reading (firmware
    /// `update_modem_status()`). The Heltec V4's own interference rule, which compares the RSSI
    /// with the fixed `LNA_GD_LIMIT` of −89 dBm instead of the noise floor while the noise floor
    /// is at or below `LNA_GD_THRSHLD` (−109 dBm), is deliberately left out, like that board's
    /// matching noise-floor branch: it is tuned to one board's LNA, and this core is
    /// board-independent. The general `HAS_LORA_LNA` rule of `update_noise_floor()` is ported (see
    /// [`Csma::set_external_lna`]).
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
            // Firmware `HAS_LORA_LNA`: discard invalid samples due to gain variance during LNA
            // re-calibration.
            if self.external_lna && self.current_rssi < self.noise_floor - LORA_LNA_GVT {
                return;
            }
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
mod tests;
