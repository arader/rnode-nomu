// Copyright Sandeep Mistry, Mark Qvist and Jacob Eva.
// Ported to Rust for Linux by RNode contributors.
// Licensed under the MIT license.

//! SX1262 LoRa radio driver for Linux (spidev + GPIO sysfs).
//!
//! This is a direct port of the Arduino sx126x.cpp driver, preserving
//! the same opcode sequences, register addresses, and initialization flow.

#![allow(dead_code)]

use std::thread;
use std::time::{Duration, Instant};

use log::{debug, warn};

use crate::error::{RNodeError, Result};
use crate::gpio::RadioGpio;
use crate::radio::types::{BandwidthCode, TcxoVoltage};
use crate::radio::Radio;
use crate::spi::SpiDevice;

// --- SX1262 Opcodes ---
const OP_RF_FREQ: u8 = 0x86;
const OP_SLEEP: u8 = 0x84;
const OP_STANDBY: u8 = 0x80;
const OP_TX: u8 = 0x83;
const OP_RX: u8 = 0x82;
const OP_PA_CONFIG: u8 = 0x95;
const OP_SET_IRQ_FLAGS: u8 = 0x08;
const OP_CLEAR_IRQ_STATUS: u8 = 0x02;
const OP_GET_IRQ_STATUS: u8 = 0x12;
const OP_RX_BUFFER_STATUS: u8 = 0x13;
const OP_PACKET_STATUS: u8 = 0x14;
const OP_CURRENT_RSSI: u8 = 0x15;
const OP_MODULATION_PARAMS: u8 = 0x8B;
const OP_PACKET_PARAMS: u8 = 0x8C;
const OP_TX_PARAMS: u8 = 0x8E;
const OP_PACKET_TYPE: u8 = 0x8A;
const OP_BUFFER_BASE_ADDR: u8 = 0x8F;
const OP_READ_REGISTER: u8 = 0x1D;
const OP_WRITE_REGISTER: u8 = 0x0D;
const OP_DIO3_TCXO_CTRL: u8 = 0x97;
const OP_DIO2_RF_CTRL: u8 = 0x9D;
const OP_CALIBRATE: u8 = 0x89;
const OP_RX_TX_FALLBACK_MODE: u8 = 0x93;
const OP_REGULATOR_MODE: u8 = 0x96;
const OP_CALIBRATE_IMAGE: u8 = 0x98;
const OP_FIFO_WRITE: u8 = 0x0E;
const OP_FIFO_READ: u8 = 0x1E;

// --- IRQ Masks ---
const IRQ_TX_DONE_MASK: u8 = 0x01;
const IRQ_RX_DONE_MASK: u8 = 0x02;
const IRQ_PREAMBLE_DET_MASK: u8 = 0x04;
const IRQ_HEADER_DET_MASK: u8 = 0x10;
const IRQ_PAYLOAD_CRC_ERROR_MASK: u8 = 0x40;

// --- Registers ---
const REG_OCP: u16 = 0x08E7;
const REG_LNA: u16 = 0x08AC;
const REG_SYNC_WORD_MSB: u16 = 0x0740;
const REG_SYNC_WORD_LSB: u16 = 0x0741;
const REG_RANDOM_GEN: u16 = 0x0819;
const REG_TX_CLAMP_CONFIG: u16 = 0x08D8;

// --- Constants ---
const MASK_CALIBRATE_ALL: u8 = 0x7F;
const MODE_LONG_RANGE: u8 = 0x01;
const MODE_STDBY_RC: u8 = 0x00;
const MODE_STDBY_XOSC: u8 = 0x01;
const MODE_IMPLICIT_HEADER: u8 = 0x01;
const MODE_EXPLICIT_HEADER: u8 = 0x00;
const SYNC_WORD: u16 = 0x1424;
const MAX_PKT_LENGTH: usize = 255;
const MODEM_TIMEOUT_MS: u64 = 20_000;

const XTAL_FREQ: f64 = 32_000_000.0;
const FREQ_DIV: f64 = 33_554_432.0; // 2^25
const FREQ_STEP: f64 = XTAL_FREQ / FREQ_DIV;

/// Default OCP value — override via config if board-specific tuning is needed.
const OCP_DEFAULT: u8 = 0x38;

pub struct Sx126x {
    spi: SpiDevice,
    gpio: RadioGpio,
    frequency: u32,
    txp: i8,
    sf: u8,
    bw: u8,
    cr: u8,
    ldro: u8,
    preamble_length: u16,
    implicit_header_mode: u8,
    payload_length: u8,
    crc_mode: u8,
    fifo_tx_addr_ptr: u8,
    fifo_rx_addr_ptr: u8,
    packet_buf: [u8; MAX_PKT_LENGTH],
    packet_index: usize,
    preinit_done: bool,
    enable_tcxo: bool,
    tcxo_voltage: TcxoVoltage,
    dio2_as_rf_switch: bool,
    ocp_value: u8,
}

impl Sx126x {
    pub fn new(
        spi: SpiDevice,
        gpio: RadioGpio,
        enable_tcxo: bool,
        tcxo_voltage: TcxoVoltage,
        dio2_as_rf_switch: bool,
    ) -> Self {
        Self {
            spi,
            gpio,
            frequency: 0,
            txp: 0,
            sf: 0x07,
            bw: 0x04,
            cr: 0x01,
            ldro: 0x00,
            preamble_length: 18,
            implicit_header_mode: 0,
            payload_length: 255,
            crc_mode: 1,
            fifo_tx_addr_ptr: 0,
            fifo_rx_addr_ptr: 0,
            packet_buf: [0u8; MAX_PKT_LENGTH],
            packet_index: 0,
            preinit_done: false,
            enable_tcxo,
            tcxo_voltage,
            dio2_as_rf_switch,
            ocp_value: OCP_DEFAULT,
        }
    }

    /// Set a board-specific OCP tuning value.
    pub fn set_ocp_value(&mut self, ocp: u8) {
        self.ocp_value = ocp;
    }

    // --- Low-level SPI helpers ---

    fn wait_on_busy(&self) -> Result<()> {
        self.gpio.wait_on_busy()
    }

    fn execute_opcode(&mut self, opcode: u8, buffer: &[u8]) -> Result<()> {
        self.wait_on_busy()?;
        self.spi.execute_opcode(opcode, buffer)
    }

    fn execute_opcode_read(&mut self, opcode: u8, size: usize) -> Result<Vec<u8>> {
        self.wait_on_busy()?;
        self.spi.execute_opcode_read(opcode, size)
    }

    fn read_register(&mut self, address: u16) -> Result<u8> {
        self.wait_on_busy()?;
        self.spi.read_register(OP_READ_REGISTER, address)
    }

    fn write_register(&mut self, address: u16, value: u8) -> Result<()> {
        self.wait_on_busy()?;
        self.spi.write_register(OP_WRITE_REGISTER, address, value)
    }

    fn write_buffer(&mut self, data: &[u8]) -> Result<()> {
        self.wait_on_busy()?;
        let offset = self.fifo_tx_addr_ptr;
        self.spi.write_fifo(OP_FIFO_WRITE, offset, data)?;
        self.fifo_tx_addr_ptr = self.fifo_tx_addr_ptr.wrapping_add(data.len() as u8);
        Ok(())
    }

    fn read_buffer(&mut self, size: usize) -> Result<Vec<u8>> {
        self.wait_on_busy()?;
        let offset = self.fifo_rx_addr_ptr;
        self.spi.read_fifo(OP_FIFO_READ, offset, size)
    }

    // --- Radio configuration helpers ---

    fn lora_mode(&mut self) -> Result<()> {
        let mode = [MODE_LONG_RANGE];
        self.execute_opcode(OP_PACKET_TYPE, &mode)
    }

    fn set_modulation_params(&mut self) -> Result<()> {
        let buf = [
            self.sf, self.bw, self.cr, self.ldro, 0x00, 0x00, 0x00, 0x00,
        ];
        self.execute_opcode(OP_MODULATION_PARAMS, &buf)
    }

    fn set_packet_params(&mut self) -> Result<()> {
        let buf = [
            (self.preamble_length >> 8) as u8,
            (self.preamble_length & 0xFF) as u8,
            self.implicit_header_mode,
            self.payload_length,
            self.crc_mode,
            0x00, // standard IQ
            0x00,
            0x00,
            0x00,
        ];
        self.execute_opcode(OP_PACKET_PARAMS, &buf)
    }

    fn calibrate(&mut self) -> Result<()> {
        let mode = [MODE_STDBY_RC];
        self.execute_opcode(OP_STANDBY, &mode)?;
        let cal = [MASK_CALIBRATE_ALL];
        self.execute_opcode(OP_CALIBRATE, &cal)?;
        thread::sleep(Duration::from_millis(5));
        self.wait_on_busy()
    }

    fn calibrate_image(&mut self, frequency: u32) -> Result<()> {
        let freq = frequency as f64;
        let image_freq = if freq >= 430e6 && freq <= 440e6 {
            [0x6B, 0x6F]
        } else if freq >= 470e6 && freq <= 510e6 {
            [0x75, 0x81]
        } else if freq >= 779e6 && freq <= 787e6 {
            [0xC1, 0xC5]
        } else if freq >= 863e6 && freq <= 870e6 {
            [0xD7, 0xDB]
        } else if freq >= 902e6 && freq <= 928e6 {
            [0xE1, 0xE9]
        } else {
            [0x00, 0x00]
        };
        self.execute_opcode(OP_CALIBRATE_IMAGE, &image_freq)?;
        self.wait_on_busy()
    }

    fn enable_tcxo_internal(&mut self) -> Result<()> {
        if self.enable_tcxo {
            let buf = [self.tcxo_voltage.register_value(), 0x00, 0x00, 0xFF];
            self.execute_opcode(OP_DIO3_TCXO_CTRL, &buf)?;
        }
        Ok(())
    }

    fn handle_low_data_rate(&mut self) {
        let bw_hz = BandwidthCode::from_hz(
            match self.bw {
                0x00 => 7_800,
                0x01 => 15_600,
                0x02 => 31_250,
                0x03 => 62_500,
                0x04 => 125_000,
                0x05 => 250_000,
                0x06 => 500_000,
                0x08 => 10_400,
                0x09 => 20_800,
                0x0A => 41_700,
                _ => 125_000,
            }
        ).to_hz();

        let symbol_duration = (1u32 << self.sf) as f64 / (bw_hz as f64 / 1000.0);
        if symbol_duration > 16.0 {
            self.ldro = 0x01;
        } else {
            self.ldro = 0x00;
        }
    }

    fn pre_init(&mut self) -> Result<bool> {
        let start = Instant::now();
        loop {
            let msb = self.read_register(REG_SYNC_WORD_MSB)?;
            let lsb = self.read_register(REG_SYNC_WORD_LSB)?;
            let sw = (msb as u16) << 8 | lsb as u16;
            if sw == 0x1424 || sw == 0x4434 {
                self.preinit_done = true;
                return Ok(true);
            }
            if start.elapsed() > Duration::from_secs(2) {
                return Ok(false);
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn set_sync_word(&mut self) -> Result<()> {
        self.write_register(REG_SYNC_WORD_MSB, 0x14)?;
        self.write_register(REG_SYNC_WORD_LSB, 0x24)
    }

    fn get_irq_status(&mut self) -> Result<[u8; 2]> {
        let buf = self.execute_opcode_read(OP_GET_IRQ_STATUS, 2)?;
        Ok([buf[0], buf[1]])
    }

    fn clear_irq_status(&mut self, mask: [u8; 2]) -> Result<()> {
        self.execute_opcode(OP_CLEAR_IRQ_STATUS, &mask)
    }
}

impl Radio for Sx126x {
    fn begin(&mut self, frequency: u32) -> Result<()> {
        self.gpio.reset_radio()?;

        if !self.preinit_done {
            if !self.pre_init()? {
                return Err(RNodeError::Radio("SX1262 not detected".to_string()));
            }
        }

        self.calibrate()?;
        self.calibrate_image(frequency)?;
        self.enable_tcxo_internal()?;
        self.lora_mode()?;
        self.standby()?;
        self.set_sync_word()?;

        if self.dio2_as_rf_switch {
            let byte = [0x01u8];
            self.execute_opcode(OP_DIO2_RF_CTRL, &byte)?;
        }

        self.gpio.rx_ant_enable()?;
        self.set_frequency(frequency)?;
        self.set_tx_power(2)?;
        self.enable_crc()?;

        // Set LNA boost
        self.write_register(REG_LNA, 0x96)?;

        // Set base addresses to 0
        let basebuf = [0x00u8, 0x00];
        self.execute_opcode(OP_BUFFER_BASE_ADDR, &basebuf)?;

        self.set_modulation_params()?;
        self.set_packet_params()?;

        // Set up IRQ: enable all IRQs, DIO0 = RX done
        let irq_buf = [
            0xFF, 0xFF, // IRQ mask (all enabled)
            0x00, IRQ_RX_DONE_MASK, // DIO0 mask
            0x00, 0x00, // DIO1 mask
            0x00, 0x00, // DIO2 mask
        ];
        self.execute_opcode(OP_SET_IRQ_FLAGS, &irq_buf)?;

        debug!("SX1262 initialized at {} Hz", frequency);
        Ok(())
    }

    fn end(&mut self) -> Result<()> {
        self.sleep()?;
        self.preinit_done = false;
        Ok(())
    }

    fn begin_packet(&mut self, implicit_header: bool) -> Result<()> {
        self.standby()?;
        if implicit_header {
            self.implicit_header_mode = MODE_IMPLICIT_HEADER;
        } else {
            self.implicit_header_mode = MODE_EXPLICIT_HEADER;
        }
        self.payload_length = 0;
        self.fifo_tx_addr_ptr = 0;
        self.set_packet_params()
    }

    fn write_packet(&mut self, data: &[u8]) -> Result<usize> {
        let remaining = MAX_PKT_LENGTH - self.payload_length as usize;
        let to_write = data.len().min(remaining);
        if to_write == 0 {
            return Ok(0);
        }
        self.write_buffer(&data[..to_write])?;
        self.payload_length += to_write as u8;
        Ok(to_write)
    }

    fn end_packet(&mut self) -> Result<bool> {
        self.set_packet_params()?;
        let timeout = [0x00u8, 0x00, 0x00]; // Single TX mode
        self.execute_opcode(OP_TX, &timeout)?;

        // Wait for TX done
        let start = Instant::now();
        let deadline = Duration::from_millis(MODEM_TIMEOUT_MS);
        loop {
            let irq = self.get_irq_status()?;
            if (irq[1] & IRQ_TX_DONE_MASK) != 0 {
                // Clear TX done IRQ
                self.clear_irq_status([0x00, IRQ_TX_DONE_MASK])?;
                return Ok(true);
            }
            if start.elapsed() > deadline {
                warn!("SX1262 TX timeout");
                return Ok(false);
            }
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn receive(&mut self) -> Result<()> {
        self.implicit_header_mode = MODE_EXPLICIT_HEADER;
        self.set_packet_params()?;
        self.gpio.rx_ant_enable()?;
        let mode = [0xFF, 0xFF, 0xFF]; // Continuous RX
        self.execute_opcode(OP_RX, &mode)
    }

    fn read_packet(&mut self, buf: &mut [u8]) -> Result<usize> {
        let rxbuf = self.execute_opcode_read(OP_RX_BUFFER_STATUS, 2)?;
        let size = rxbuf[0] as usize;
        self.fifo_rx_addr_ptr = rxbuf[1];
        let data = self.read_buffer(size)?;
        let to_copy = size.min(buf.len());
        buf[..to_copy].copy_from_slice(&data[..to_copy]);
        Ok(to_copy)
    }

    fn available(&mut self) -> Result<usize> {
        let rxbuf = self.execute_opcode_read(OP_RX_BUFFER_STATUS, 2)?;
        let size = rxbuf[0] as usize;
        Ok(size.saturating_sub(self.packet_index))
    }

    fn handle_dio0_rise(&mut self) -> Result<Option<usize>> {
        let irq = self.get_irq_status()?;
        self.clear_irq_status(irq)?;

        if (irq[1] & IRQ_PAYLOAD_CRC_ERROR_MASK) != 0 {
            return Ok(None);
        }

        self.packet_index = 0;
        let rxbuf = self.execute_opcode_read(OP_RX_BUFFER_STATUS, 2)?;
        let packet_length = rxbuf[0] as usize;
        self.fifo_rx_addr_ptr = rxbuf[1];

        // Pre-read the packet into our internal buffer
        let data = self.read_buffer(packet_length)?;
        let to_copy = packet_length.min(MAX_PKT_LENGTH);
        self.packet_buf[..to_copy].copy_from_slice(&data[..to_copy]);

        Ok(Some(packet_length))
    }

    fn set_frequency(&mut self, freq: u32) -> Result<()> {
        self.frequency = freq;
        let frf = (freq as f64 / FREQ_STEP) as u32;
        let buf = [
            ((frf >> 24) & 0xFF) as u8,
            ((frf >> 16) & 0xFF) as u8,
            ((frf >> 8) & 0xFF) as u8,
            (frf & 0xFF) as u8,
        ];
        self.execute_opcode(OP_RF_FREQ, &buf)
    }

    fn get_frequency(&self) -> u32 {
        self.frequency
    }

    fn set_bandwidth(&mut self, bw: u32) -> Result<()> {
        let code = BandwidthCode::from_hz(bw);
        self.bw = code.code();
        self.handle_low_data_rate();
        self.set_modulation_params()
    }

    fn get_signal_bandwidth(&self) -> u32 {
        match self.bw {
            0x00 => 7_800,
            0x01 => 15_600,
            0x02 => 31_250,
            0x03 => 62_500,
            0x04 => 125_000,
            0x05 => 250_000,
            0x06 => 500_000,
            0x08 => 10_400,
            0x09 => 20_800,
            0x0A => 41_700,
            _ => 0,
        }
    }

    fn set_spreading_factor(&mut self, sf: u8) -> Result<()> {
        let sf = sf.clamp(5, 12);
        self.sf = sf;
        self.handle_low_data_rate();
        self.set_modulation_params()
    }

    fn get_spreading_factor(&self) -> u8 {
        self.sf
    }

    fn set_coding_rate(&mut self, cr: u8) -> Result<()> {
        let denom = cr.clamp(5, 8);
        self.cr = denom - 4;
        self.set_modulation_params()
    }

    fn get_coding_rate(&self) -> u8 {
        self.cr + 4
    }

    fn set_tx_power(&mut self, level: i8) -> Result<()> {
        // TX clamp workaround per SX1262 datasheet §15.2
        let clamp = self.read_register(REG_TX_CLAMP_CONFIG)?;
        self.write_register(REG_TX_CLAMP_CONFIG, clamp | (0x0F << 1))?;

        // PA config for maximum power (SX1262)
        let pa_buf = [
            0x04, // PADutyCycle
            0x07, // HPMax
            0x00, // DeviceSel (0x00 = SX1262)
            0x01, // PALut
        ];
        self.execute_opcode(OP_PA_CONFIG, &pa_buf)?;

        let level = level.clamp(-9, 22);
        self.write_register(REG_OCP, self.ocp_value)?;

        let tx_buf = [level as u8, 0x02]; // 40µs PA ramp
        self.execute_opcode(OP_TX_PARAMS, &tx_buf)?;

        self.txp = level;
        Ok(())
    }

    fn get_tx_power(&self) -> i8 {
        self.txp
    }

    fn set_preamble_length(&mut self, symbols: u16) -> Result<()> {
        self.preamble_length = symbols;
        self.set_packet_params()
    }

    fn enable_crc(&mut self) -> Result<()> {
        self.crc_mode = 1;
        self.set_packet_params()
    }

    fn disable_crc(&mut self) -> Result<()> {
        self.crc_mode = 0;
        self.set_packet_params()
    }

    fn set_implicit_header_mode(&mut self, payload_length: u8) -> Result<()> {
        self.implicit_header_mode = MODE_IMPLICIT_HEADER;
        self.payload_length = payload_length;
        self.set_packet_params()
    }

    fn set_explicit_header_mode(&mut self) -> Result<()> {
        self.implicit_header_mode = MODE_EXPLICIT_HEADER;
        self.set_packet_params()
    }

    fn current_rssi(&mut self) -> Result<i16> {
        let buf = self.execute_opcode_read(OP_CURRENT_RSSI, 1)?;
        Ok(-(buf[0] as i16) / 2)
    }

    fn current_rssi_raw(&mut self) -> Result<u8> {
        let buf = self.execute_opcode_read(OP_CURRENT_RSSI, 1)?;
        Ok(buf[0])
    }

    fn packet_rssi(&mut self) -> Result<i16> {
        let buf = self.execute_opcode_read(OP_PACKET_STATUS, 3)?;
        Ok(-(buf[0] as i16) / 2)
    }

    fn packet_rssi_raw(&mut self) -> Result<u8> {
        let buf = self.execute_opcode_read(OP_PACKET_STATUS, 3)?;
        Ok(buf[2])
    }

    fn packet_snr_raw(&mut self) -> Result<u8> {
        let buf = self.execute_opcode_read(OP_PACKET_STATUS, 3)?;
        Ok(buf[1])
    }

    fn packet_snr(&mut self) -> Result<f32> {
        let buf = self.execute_opcode_read(OP_PACKET_STATUS, 3)?;
        Ok(buf[1] as f32 * 0.25)
    }

    fn random(&mut self) -> Result<u8> {
        self.read_register(REG_RANDOM_GEN)
    }

    fn dcd(&mut self) -> Result<bool> {
        let irq = self.get_irq_status()?;
        let header_detected = (irq[1] & IRQ_HEADER_DET_MASK) != 0;
        let preamble_detected = (irq[1] & IRQ_PREAMBLE_DET_MASK) != 0;
        Ok(header_detected || preamble_detected)
    }

    fn standby(&mut self) -> Result<()> {
        let byte = [MODE_STDBY_XOSC];
        self.execute_opcode(OP_STANDBY, &byte)
    }

    fn sleep(&mut self) -> Result<()> {
        let byte = [0x00u8];
        self.execute_opcode(OP_SLEEP, &byte)
    }
}
