//! LoRa PHY timing derived from the modulation, ported from RNode_Firmware v1.86
//! (`updateBitrate()` and `add_airtime()` in `RNode_Firmware.ino`/`Utilities.h`,
//! `handleLowDataRate()` in `sx126x.cpp`).
//!
//! The firmware computes these values with a mix of `float`, `double` and integer arithmetic
//! (including truncating conversions). This module mirrors those types step by step, because the
//! integer results — preamble length, CSMA slot, DIFS, the low-data-rate decision — must match a
//! real RNode exactly: the preamble length and low-data-rate optimisation are on-air parameters,
//! and the timing values are reported to the host.

/// Firmware `PHY_HEADER_LORA_SYMBOLS`.
pub const PHY_HEADER_LORA_SYMBOLS: i32 = 20;
/// Firmware `PHY_CRC_LORA_BITS`.
pub const PHY_CRC_LORA_BITS: i32 = 16;
/// Firmware `LORA_PREAMBLE_SYMBOLS_MIN`.
pub const LORA_PREAMBLE_SYMBOLS_MIN: u16 = 18;
/// Firmware `LORA_PREAMBLE_TARGET_MS`.
pub const LORA_PREAMBLE_TARGET_MS: f32 = 24.0;
/// Firmware `LORA_PREAMBLE_FAST_DELTA`.
pub const LORA_PREAMBLE_FAST_DELTA: f32 = 18.0;
/// Firmware `LORA_FAST_THRESHOLD_BPS`.
pub const LORA_FAST_THRESHOLD_BPS: u32 = 30_000;
/// Firmware `LORA_LIMIT_THRESHOLD_BPS`.
pub const LORA_LIMIT_THRESHOLD_BPS: u32 = 60_000;
/// Firmware `LORA_GUARD_THRESHOLD_BPS`.
pub const LORA_GUARD_THRESHOLD_BPS: u32 = 14_000;
/// Firmware `CSMA_SIFS_MS`.
pub const CSMA_SIFS_MS: u32 = 0;
/// Firmware `CSMA_SLOT_MAX_MS`.
pub const CSMA_SLOT_MAX_MS: u32 = 100;
/// Firmware `CSMA_SLOT_MIN_MS`.
pub const CSMA_SLOT_MIN_MS: u32 = 24;
/// Firmware `CSMA_SLOT_MIN_FAST_DELTA`.
pub const CSMA_SLOT_MIN_FAST_DELTA: u32 = 18;
/// Firmware `CSMA_SLOT_SYMBOLS`.
pub const CSMA_SLOT_SYMBOLS: i32 = 12;

/// The modulation the host configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Modulation {
    /// Bandwidth in Hz (the value in use by the modem, i.e. after quantisation).
    pub bandwidth_hz: u32,
    /// Spreading factor, 5–12.
    pub spreading_factor: u8,
    /// Coding rate denominator, 5–8.
    pub coding_rate: u8,
}

/// Timing derived from a [`Modulation`] (firmware `updateBitrate()`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PhyParams {
    /// Symbols per second (`lora_symbol_rate`).
    pub symbol_rate: f32,
    /// Symbol duration in ms (`lora_symbol_time_ms`).
    pub symbol_time_ms: f32,
    /// Raw bitrate in bit/s (`lora_bitrate`).
    pub bitrate_bps: u32,
    /// Microseconds per payload byte (`lora_us_per_byte`).
    pub us_per_byte: f32,
    /// Bitrate above [`LORA_FAST_THRESHOLD_BPS`]: shorter preamble target and minimum slot.
    pub fast_rate: bool,
    /// Bitrate above [`LORA_LIMIT_THRESHOLD_BPS`] (`lora_limit_rate`).
    pub limit_rate: bool,
    /// Bitrate above [`LORA_GUARD_THRESHOLD_BPS`] but not limit rate (`lora_guard_rate`).
    pub guard_rate: bool,
    /// CSMA slot time in ms (`csma_slot_ms`).
    pub csma_slot_ms: u32,
    /// DIFS in ms (`difs_ms`).
    pub difs_ms: u32,
    /// Preamble length in symbols (`lora_preamble_symbols`), applied to the modem.
    pub preamble_symbols: u16,
    /// Preamble duration in ms (`lora_preamble_time_ms`).
    pub preamble_time_ms: u32,
    /// Explicit-header duration in ms (`lora_header_time_ms`).
    pub header_time_ms: u32,
}

impl PhyParams {
    /// Derive the timing for `m`, exactly as the firmware's `updateBitrate()` does.
    pub fn new(m: Modulation) -> Self {
        let sf = i32::from(m.spreading_factor);
        let cr = i32::from(m.coding_rate);
        let chips = 2f64.powi(sf);

        // lora_symbol_rate = (float)lora_bw/(float)(pow(2, lora_sf));
        let symbol_rate = m.bandwidth_hz as f32 / chips as f32;
        // lora_symbol_time_ms = (1.0/lora_symbol_rate)*1000.0;  (double maths, stored as float)
        let symbol_time_ms = ((1.0 / f64::from(symbol_rate)) * 1000.0) as f32;
        // lora_bitrate = (uint32_t)(lora_sf * ((4.0/(float)lora_cr) / ((float)(pow(2, lora_sf))/((float)lora_bw/1000.0))) * 1000.0);
        let bw_khz = f64::from(m.bandwidth_hz as f32) / 1000.0;
        let bitrate = f64::from(sf) * ((4.0 / f64::from(cr as f32)) / (f64::from(chips as f32) / bw_khz)) * 1000.0;
        let bitrate_bps = bitrate as u32;
        // lora_us_per_byte = 1000000.0/((float)lora_bitrate/8.0);
        let us_per_byte = (1_000_000.0 / (f64::from(bitrate_bps as f32) / 8.0)) as f32;

        let fast_rate = bitrate_bps > LORA_FAST_THRESHOLD_BPS;
        let limit_rate = bitrate_bps > LORA_LIMIT_THRESHOLD_BPS;
        let guard_rate = !limit_rate && bitrate_bps > LORA_GUARD_THRESHOLD_BPS;

        let mut csma_slot_min_ms = CSMA_SLOT_MIN_MS;
        let mut preamble_target_ms = LORA_PREAMBLE_TARGET_MS;
        if fast_rate {
            csma_slot_min_ms -= CSMA_SLOT_MIN_FAST_DELTA;
            preamble_target_ms -= LORA_PREAMBLE_FAST_DELTA;
        }

        // csma_slot_ms = lora_symbol_time_ms*CSMA_SLOT_SYMBOLS;  (float truncated into int)
        let mut csma_slot_ms = (symbol_time_ms * CSMA_SLOT_SYMBOLS as f32) as i64;
        if csma_slot_ms > i64::from(CSMA_SLOT_MAX_MS) {
            csma_slot_ms = i64::from(CSMA_SLOT_MAX_MS);
        }
        if csma_slot_ms < i64::from(CSMA_SLOT_MIN_MS) {
            csma_slot_ms = i64::from(csma_slot_min_ms);
        }
        let csma_slot_ms = csma_slot_ms as u32;
        let difs_ms = CSMA_SIFS_MS + 2 * csma_slot_ms;

        // float target_preamble_symbols = lora_preamble_target_ms/lora_symbol_time_ms;
        let target = preamble_target_ms / symbol_time_ms;
        let preamble_symbols = if target < f32::from(LORA_PREAMBLE_SYMBOLS_MIN) {
            LORA_PREAMBLE_SYMBOLS_MIN
        } else {
            f64::from(target).ceil() as u16
        };
        // lora_preamble_time_ms = (ceil)(lora_preamble_symbols * lora_symbol_time_ms);
        let preamble_time_ms = f64::from(f32::from(preamble_symbols) * symbol_time_ms).ceil() as u32;
        // lora_header_time_ms = (ceil)(PHY_HEADER_LORA_SYMBOLS * lora_symbol_time_ms);
        let header_time_ms = f64::from(PHY_HEADER_LORA_SYMBOLS as f32 * symbol_time_ms).ceil() as u32;

        Self {
            symbol_rate,
            symbol_time_ms,
            bitrate_bps,
            us_per_byte,
            fast_rate,
            limit_rate,
            guard_rate,
            csma_slot_ms,
            difs_ms,
            preamble_symbols,
            preamble_time_ms,
            header_time_ms,
        }
    }

    /// The 12-byte payload of `CMD_STAT_PHYPRM` (firmware `kiss_indicate_phy_stats()`), before
    /// KISS escaping: symbol time (µs), symbol rate, preamble symbols, preamble time (ms),
    /// CSMA slot (ms) and DIFS (ms), each big-endian `u16`.
    pub fn stats_payload(&self) -> [u8; 12] {
        let fields = [
            (self.symbol_time_ms * 1000.0) as u16,
            self.symbol_rate as u16,
            self.preamble_symbols,
            self.preamble_time_ms as u16,
            self.csma_slot_ms as u16,
            self.difs_ms as u16,
        ];
        let mut out = [0u8; 12];
        for (chunk, value) in out.chunks_exact_mut(2).zip(fields) {
            chunk.copy_from_slice(&value.to_be_bytes());
        }
        out
    }
}

/// Whether low-data-rate optimisation is used (firmware `handleLowDataRate()` in both
/// `sx126x.cpp` and `sx127x.cpp`): `long((1 << sf) / (bandwidth / 1000)) > 16`, in integer
/// arithmetic. This differs from the "symbol time ≥ 16 ms" float comparison at SF9/31.25 kHz,
/// SF10/62.5 kHz, SF11/125 kHz and SF12/250 kHz, where RNodes leave it off.
pub fn low_data_rate(spreading_factor: u8, bandwidth_hz: u32) -> bool {
    let bw_khz = i64::from(bandwidth_hz / 1000);
    if bw_khz == 0 {
        return true;
    }
    (1i64 << spreading_factor) / bw_khz > 16
}

/// Airtime in ms charged for a transmitted frame of `len` bytes (firmware `add_airtime()`,
/// SX1262 branch). This is the firmware's own estimate used for airtime accounting, not the
/// Semtech time-on-air formula.
pub fn airtime_ms(len: usize, m: Modulation, low_data_rate: bool, preamble_symbols: u16, symbol_time_ms: f32) -> f32 {
    let written = len as i32;
    let sf = i32::from(m.spreading_factor);
    let cr = i32::from(m.coding_rate);
    let ldr_opt = i32::from(low_data_rate);
    let mut symbols: f32;
    if sf < 7 {
        symbols = (8 * written + PHY_CRC_LORA_BITS - 4 * sf + PHY_HEADER_LORA_SYMBOLS) as f32;
        symbols /= (4 * sf) as f32;
        symbols *= cr as f32;
        symbols = (f64::from(symbols) + f64::from(preamble_symbols) + 2.25 + 8.0) as f32;
    } else {
        symbols = (8 * written + PHY_CRC_LORA_BITS - 4 * sf + 8 + PHY_HEADER_LORA_SYMBOLS) as f32;
        symbols /= (4 * (sf - 2 * ldr_opt)) as f32;
        symbols *= cr as f32;
        symbols = (f64::from(symbols) + f64::from(preamble_symbols) + 0.25 + 8.0) as f32;
    }
    symbols * symbol_time_ms
}
