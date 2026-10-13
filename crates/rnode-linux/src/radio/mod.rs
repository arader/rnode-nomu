// Copyright (C) 2024, Mark Qvist
// Ported to Rust for Linux by RNode contributors.
// Licensed under the GNU General Public License v3.0 or later.

pub mod sx126x;
pub mod types;

use crate::error::Result;

/// Trait abstracting a LoRa radio transceiver.
///
/// Implement this trait for each supported radio chip (SX1262, SX1276, SX1280, etc.).
/// The rest of the daemon is generic over `impl Radio`.
#[allow(dead_code)]
pub trait Radio {
    /// Initialize the radio at the given frequency (Hz).
    fn begin(&mut self, frequency: u32) -> Result<()>;

    /// Shut down the radio.
    fn end(&mut self) -> Result<()>;

    /// Begin composing a packet for transmission.
    fn begin_packet(&mut self, implicit_header: bool) -> Result<()>;

    /// Write data into the current TX packet buffer.
    fn write_packet(&mut self, data: &[u8]) -> Result<usize>;

    /// Finalize and transmit the packet. Returns true on success.
    fn end_packet(&mut self) -> Result<bool>;

    /// Enter continuous receive mode.
    fn receive(&mut self) -> Result<()>;

    /// Read a received packet into `buf`. Returns the number of bytes read.
    fn read_packet(&mut self, buf: &mut [u8]) -> Result<usize>;

    /// Get the number of bytes available to read from the last received packet.
    fn available(&mut self) -> Result<usize>;

    /// Handle a DIO0 rising-edge interrupt. Returns Some(packet_length) if a valid
    /// packet was received, None otherwise.
    fn handle_dio0_rise(&mut self) -> Result<Option<usize>>;

    // --- Configuration ---

    fn set_frequency(&mut self, freq: u32) -> Result<()>;
    fn get_frequency(&self) -> u32;
    fn set_bandwidth(&mut self, bw: u32) -> Result<()>;
    fn get_signal_bandwidth(&self) -> u32;
    fn set_spreading_factor(&mut self, sf: u8) -> Result<()>;
    fn get_spreading_factor(&self) -> u8;
    fn set_coding_rate(&mut self, cr: u8) -> Result<()>;
    fn get_coding_rate(&self) -> u8;
    fn set_tx_power(&mut self, dbm: i8) -> Result<()>;
    fn get_tx_power(&self) -> i8;
    fn set_preamble_length(&mut self, symbols: u16) -> Result<()>;
    fn enable_crc(&mut self) -> Result<()>;
    fn disable_crc(&mut self) -> Result<()>;
    fn set_implicit_header_mode(&mut self, payload_length: u8) -> Result<()>;
    fn set_explicit_header_mode(&mut self) -> Result<()>;

    // --- Status ---

    fn current_rssi(&mut self) -> Result<i16>;
    fn packet_rssi(&mut self) -> Result<i16>;
    fn packet_rssi_raw(&mut self) -> Result<u8>;
    fn current_rssi_raw(&mut self) -> Result<u8>;
    fn packet_snr_raw(&mut self) -> Result<u8>;
    fn packet_snr(&mut self) -> Result<f32>;
    fn random(&mut self) -> Result<u8>;

    /// Data Carrier Detect: returns true if a LoRa signal is currently detected.
    fn dcd(&mut self) -> Result<bool>;

    fn standby(&mut self) -> Result<()>;
    fn sleep(&mut self) -> Result<()>;
}
