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
//! This module currently implements the API contract with simplified behaviour (access whenever
//! carrier detect is clear, no airtime accounting); the firmware algorithm is tracked in issues
//! #6, #11 and #12.

use crate::phy::PhyParams;
use crate::rng::Rng;

/// Firmware `CSMA_CW_PER_BAND_WINDOWS`.
pub const CSMA_CW_PER_BAND_WINDOWS: u8 = 15;

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
    carrier: bool,
    current_rssi: i16,
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
            carrier: false,
            current_rssi: -292,
            cw_band: 1,
            cw_min: 0,
            cw_max: CSMA_CW_PER_BAND_WINDOWS,
        }
    }

    /// Reset the channel statistics when the radio comes online (firmware
    /// `init_channel_stats()`).
    pub fn reset(&mut self) {
        self.carrier = false;
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
    }

    /// The short-term airtime limit in `CMD_ST_ALOCK` units.
    pub fn st_airtime_limit(&self) -> u16 {
        self.st_airtime_limit
    }

    /// Set the long-term airtime limit as sent with `CMD_LT_ALOCK`: percent × 100, 0 for none.
    pub fn set_lt_airtime_limit(&mut self, raw: u16) {
        self.lt_airtime_limit = if raw >= 10_000 { 0 } else { raw };
    }

    /// The long-term airtime limit in `CMD_LT_ALOCK` units.
    pub fn lt_airtime_limit(&self) -> u16 {
        self.lt_airtime_limit
    }

    /// Record a fresh carrier-detect and current-RSSI reading (firmware
    /// `update_modem_status()`).
    pub fn observe(&mut self, now_ms: u64, carrier: bool, rssi_dbm: i16) {
        let _ = now_ms;
        self.carrier = carrier;
        self.current_rssi = rssi_dbm;
    }

    /// Periodic status work after [`Csma::observe`] (firmware `check_modem_status()`): noise
    /// floor, channel utilisation and the once-a-second airtime update.
    pub fn status_tick(&mut self, now_ms: u64) -> StatusUpdate {
        let _ = now_ms;
        StatusUpdate::default()
    }

    /// Account the airtime of a transmitted frame (firmware `add_airtime()`).
    pub fn add_airtime(&mut self, now_ms: u64, cost_ms: f32) {
        let _ = (now_ms, cost_ms);
    }

    /// Recompute airtime figures and contention bands after a send burst (firmware
    /// `update_airtime()`).
    pub fn update_airtime(&mut self, now_ms: u64) -> StatusUpdate {
        let _ = now_ms;
        StatusUpdate {
            channel_stats_due: true,
            csma_changed: false,
        }
    }

    /// Whether the airtime limits currently block transmission (firmware `airtime_lock`).
    pub fn airtime_locked(&self) -> bool {
        false
    }

    /// Whether the medium is free (firmware `medium_free()`), from the last observation.
    pub fn medium_free(&self) -> bool {
        !self.carrier
    }

    /// Channel access decision while packets are queued (firmware `tx_queue_handler()`).
    /// Call after a fresh [`Csma::observe`].
    pub fn poll_access(&mut self, now_ms: u64, rng: &mut Rng) -> Access {
        let _ = (now_ms, rng);
        if !self.medium_free() {
            Access::Wait
        } else if self.limit_or_guard_rate {
            Access::Single
        } else {
            Access::Flush
        }
    }

    /// The current channel statistics.
    pub fn channel_stats(&self) -> ChannelStats {
        ChannelStats {
            airtime: 0.0,
            longterm_airtime: 0.0,
            total_channel_util: 0.0,
            longterm_channel_util: 0.0,
            current_rssi: self.current_rssi,
            noise_floor: -292,
            interference: false,
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
