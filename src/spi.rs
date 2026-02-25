// Copyright (C) 2024, Mark Qvist
// Ported to Rust for Linux by RNode contributors.
// Licensed under the GNU General Public License v3.0 or later.

use spidev::{SpiModeFlags, Spidev, SpidevOptions, SpidevTransfer};
use std::io::Write;
use std::path::Path;

use crate::error::Result;

/// Thin wrapper around Linux spidev for LoRa radio communication.
#[allow(dead_code)]
pub struct SpiDevice {
    dev: Spidev,
}

impl SpiDevice {
    pub fn open(device: &str, speed_hz: u32) -> Result<Self> {
        let path = Path::new(device);
        let mut dev = Spidev::open(path)?;
        let options = SpidevOptions::new()
            .bits_per_word(8)
            .max_speed_hz(speed_hz)
            .mode(SpiModeFlags::SPI_MODE_0)
            .build();
        dev.configure(&options)?;
        Ok(Self { dev })
    }

    /// Full-duplex SPI transfer: writes `tx` and reads into `rx` simultaneously.
    pub fn transfer(&mut self, tx: &[u8], rx: &mut [u8]) -> Result<()> {
        let mut transfer = SpidevTransfer::read_write(tx, rx);
        self.dev.transfer(&mut transfer)?;
        Ok(())
    }

    /// Write-only SPI transfer.
    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        self.dev.write_all(data)?;
        Ok(())
    }

    /// Execute an opcode with payload bytes (write-only, no response read).
    /// Caller is responsible for CS assertion if not using kernel CS.
    pub fn execute_opcode(&mut self, opcode: u8, buffer: &[u8]) -> Result<()> {
        let mut cmd = Vec::with_capacity(1 + buffer.len());
        cmd.push(opcode);
        cmd.extend_from_slice(buffer);
        self.write(&cmd)
    }

    /// Execute an opcode and read response bytes.
    /// Sends opcode + NOP byte, then reads `size` response bytes.
    pub fn execute_opcode_read(&mut self, opcode: u8, size: usize) -> Result<Vec<u8>> {
        // Opcode + 1 NOP + N read bytes
        let tx_len = 2 + size;
        let tx: Vec<u8> = {
            let mut v = Vec::with_capacity(tx_len);
            v.push(opcode);
            v.resize(tx_len, 0x00);
            v
        };
        let mut rx = vec![0u8; tx_len];
        self.transfer(&tx, &mut rx)?;
        // Response starts at byte 2 (after opcode echo + NOP)
        Ok(rx[2..].to_vec())
    }

    /// Read a register at a 16-bit address (SX126x protocol).
    pub fn read_register(&mut self, opcode: u8, address: u16) -> Result<u8> {
        let tx = [
            opcode,
            (address >> 8) as u8,
            (address & 0xFF) as u8,
            0x00, // NOP for read
            0x00, // value read here
        ];
        let mut rx = [0u8; 5];
        self.transfer(&tx, &mut rx)?;
        Ok(rx[4])
    }

    /// Write a register at a 16-bit address (SX126x protocol).
    pub fn write_register(&mut self, opcode: u8, address: u16, value: u8) -> Result<()> {
        let tx = [
            opcode,
            (address >> 8) as u8,
            (address & 0xFF) as u8,
            value,
        ];
        self.write(&tx)
    }

    /// Write to the FIFO buffer (SX126x protocol).
    pub fn write_fifo(&mut self, opcode: u8, offset: u8, data: &[u8]) -> Result<()> {
        let mut tx = Vec::with_capacity(2 + data.len());
        tx.push(opcode);
        tx.push(offset);
        tx.extend_from_slice(data);
        self.write(&tx)
    }

    /// Read from the FIFO buffer (SX126x protocol).
    pub fn read_fifo(&mut self, opcode: u8, offset: u8, size: usize) -> Result<Vec<u8>> {
        let tx_len = 3 + size; // opcode + offset + NOP + data
        let mut tx = Vec::with_capacity(tx_len);
        tx.push(opcode);
        tx.push(offset);
        tx.resize(tx_len, 0x00);
        let mut rx = vec![0u8; tx_len];
        self.transfer(&tx, &mut rx)?;
        Ok(rx[3..].to_vec())
    }

    /// Get a mutable reference to the underlying spidev for advanced use.
    pub fn inner_mut(&mut self) -> &mut Spidev {
        &mut self.dev
    }
}
