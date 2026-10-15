//! The modem abstraction used by the node logic.

use core::fmt;

/// A LoRa frame received by the radio, with its signal metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RxFrame {
    /// The raw LoRa payload as received over the air (at most 255 bytes). In normal operation
    /// this starts with the RNode header byte; in promiscuous mode it is passed to the host as is.
    pub data: Vec<u8>,
    /// Packet RSSI in dBm (firmware `packetRssi()`).
    pub rssi_dbm: i16,
    /// Raw packet SNR byte: two's complement, in units of 0.25 dB (firmware `packetSnrRaw()`).
    pub snr_raw: u8,
}

/// Outcome of a blocking transmission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxOutcome {
    /// The modem reported TX done.
    Done,
    /// The modem did not report TX done before the driver's timeout (firmware
    /// `LORA_MODEM_TIMEOUT_MS`); the frame may or may not have been sent.
    Timeout,
}

/// A LoRa modem as seen by the RNode logic.
///
/// The methods mirror the subset of the firmware's modem drivers (`sx126x.cpp`, `sx127x.cpp`)
/// that `RNode_Firmware.ino` and `Utilities.h` use, with the interrupt-driven receive path
/// expressed as [`Radio::poll_rx`]. Implementations must reproduce the firmware's on-air
/// behaviour exactly (register values, low-data-rate optimisation, sync word, CRC, IQ) so that
/// they interoperate with real RNodes.
///
/// Configuration setters take effect immediately on the chip and keep the driver's cached state
/// in sync; getters return the cached state.
pub trait Radio {
    /// Error type of the underlying bus/driver.
    type Error: fmt::Debug + fmt::Display;

    /// Reset and fully initialise the modem for LoRa operation at `frequency_hz`
    /// (firmware `begin()`): chip detection, calibration, TCXO, packet type, sync word,
    /// RF switch, default TX power, CRC, LNA boost, buffer bases, modulation and packet
    /// parameters, and interrupt routing. Leaves the modem in standby.
    fn begin(&mut self, frequency_hz: u32) -> Result<(), Self::Error>;

    /// Put the modem to sleep (firmware `end()`). [`Radio::begin`] must be called again
    /// before further use.
    fn end(&mut self) -> Result<(), Self::Error>;

    /// Set the carrier frequency in Hz.
    fn set_frequency(&mut self, hz: u32) -> Result<(), Self::Error>;

    /// The carrier frequency in Hz last set.
    fn frequency(&self) -> u32;

    /// Set the signal bandwidth. The modem quantises the request to the nearest supported
    /// bandwidth at or above it (firmware `setSignalBandwidth()`).
    fn set_bandwidth(&mut self, hz: u32) -> Result<(), Self::Error>;

    /// The bandwidth actually in use, in Hz (firmware `getSignalBandwidth()`).
    fn bandwidth(&self) -> u32;

    /// Set the spreading factor (clamped to the modem's range, 5–12 for the SX1262).
    fn set_spreading_factor(&mut self, sf: u8) -> Result<(), Self::Error>;

    /// The spreading factor in use.
    fn spreading_factor(&self) -> u8;

    /// Set the coding rate as the denominator of 4/x, clamped to 5–8.
    fn set_coding_rate(&mut self, denominator: u8) -> Result<(), Self::Error>;

    /// The coding rate denominator in use (5–8).
    fn coding_rate(&self) -> u8;

    /// Set the TX power in dBm (clamped to the modem's range). `Node` passes 0 dBm up to
    /// `NodeConfig::max_tx_power`, as RNode hosts cannot ask for less; the type is signed because
    /// modems go below 0 dBm.
    fn set_tx_power(&mut self, dbm: i8) -> Result<(), Self::Error>;

    /// The TX power in dBm in use.
    fn tx_power(&self) -> i8;

    /// Set the preamble length in symbols.
    fn set_preamble_length(&mut self, symbols: u16) -> Result<(), Self::Error>;

    /// Enable or disable the payload CRC.
    fn set_crc(&mut self, enabled: bool) -> Result<(), Self::Error>;

    /// Whether low-data-rate optimisation is active for the current modulation (firmware
    /// global `lora_low_datarate`, decided by the driver's `handleLowDataRate()`).
    fn low_data_rate(&self) -> bool;

    /// Enter continuous receive. `implicit_len` selects implicit-header mode with that payload
    /// length; `None` selects explicit-header mode.
    fn receive(&mut self, implicit_len: Option<u8>) -> Result<(), Self::Error>;

    /// Transmit one LoRa frame of at most 255 bytes and block until the modem reports TX done
    /// or the driver's timeout expires (firmware `beginPacket()`/`write()`/`endPacket()`).
    /// `implicit` selects implicit-header mode with the frame length as payload length.
    /// The modem is left in standby; the caller re-enters receive.
    fn transmit(&mut self, frame: &[u8], implicit: bool) -> Result<TxOutcome, Self::Error>;

    /// Service the modem's receive interrupt (firmware `handleDio0Rise()`): if a packet has
    /// been received, clear the interrupt flags and return it with its RSSI/SNR. Packets that
    /// failed the CRC are dropped (`Ok(None)`). Returns `Ok(None)` without side effects when
    /// nothing was received, so it is safe to call on every poll.
    fn poll_rx(&mut self) -> Result<Option<RxFrame>, Self::Error>;

    /// Carrier detect (firmware `dcd()`): whether a LoRa preamble or header is currently being
    /// received. `now_ms` is the monotonic clock used to expire stale preamble detections
    /// (see [`Radio::set_dcd_timing`]).
    fn dcd(&mut self, now_ms: u64) -> Result<bool, Self::Error>;

    /// The current channel RSSI in dBm (firmware `currentRssi()`).
    fn current_rssi(&mut self) -> Result<i16, Self::Error>;

    /// Timing from the PHY layer for expiring a preamble detection that is not followed by a
    /// header: firmware `lora_preamble_time_ms` and `lora_header_time_ms`. Called whenever
    /// the modulation changes. Modems without that mechanism ignore it.
    fn set_dcd_timing(&mut self, preamble_ms: u32, header_ms: u32);
}
