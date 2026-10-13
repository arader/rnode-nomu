// Copyright (C) 2024, Mark Qvist
// Ported to Rust for Linux by RNode contributors.
// Licensed under the GNU General Public License v3.0 or later.

//! KISS protocol frame parser and encoder.
//!
//! Implements the same byte-by-byte state machine as the firmware's
//! `serial_callback()`, ensuring wire-compatibility with RNS host software.

#![allow(dead_code)]

// --- KISS framing bytes ---
pub const FEND: u8 = 0xC0;
pub const FESC: u8 = 0xDB;
pub const TFEND: u8 = 0xDC;
pub const TFESC: u8 = 0xDD;

// --- Command bytes ---
pub const CMD_UNKNOWN: u8 = 0xFE;
pub const CMD_DATA: u8 = 0x00;
pub const CMD_FREQUENCY: u8 = 0x01;
pub const CMD_BANDWIDTH: u8 = 0x02;
pub const CMD_TXPOWER: u8 = 0x03;
pub const CMD_SF: u8 = 0x04;
pub const CMD_CR: u8 = 0x05;
pub const CMD_RADIO_STATE: u8 = 0x06;
pub const CMD_RADIO_LOCK: u8 = 0x07;
pub const CMD_DETECT: u8 = 0x08;
pub const CMD_IMPLICIT: u8 = 0x09;
pub const CMD_LEAVE: u8 = 0x0A;
pub const CMD_ST_ALOCK: u8 = 0x0B;
pub const CMD_LT_ALOCK: u8 = 0x0C;
pub const CMD_PROMISC: u8 = 0x0E;
pub const CMD_READY: u8 = 0x0F;

pub const CMD_STAT_RX: u8 = 0x21;
pub const CMD_STAT_TX: u8 = 0x22;
pub const CMD_STAT_RSSI: u8 = 0x23;
pub const CMD_STAT_SNR: u8 = 0x24;
pub const CMD_STAT_CHTM: u8 = 0x25;
pub const CMD_STAT_PHYPRM: u8 = 0x26;
pub const CMD_STAT_BAT: u8 = 0x27;
pub const CMD_BLINK: u8 = 0x30;
pub const CMD_RANDOM: u8 = 0x40;

pub const CMD_BOARD: u8 = 0x47;
pub const CMD_PLATFORM: u8 = 0x48;
pub const CMD_MCU: u8 = 0x49;
pub const CMD_FW_VERSION: u8 = 0x50;
pub const CMD_CONF_SAVE: u8 = 0x53;
pub const CMD_CONF_DELETE: u8 = 0x54;
pub const CMD_RESET: u8 = 0x55;
pub const CMD_RESET_BYTE: u8 = 0xF8;

pub const CMD_ERROR: u8 = 0x90;
pub const ERROR_INITRADIO: u8 = 0x01;
pub const ERROR_TXFAILED: u8 = 0x02;
pub const ERROR_QUEUE_FULL: u8 = 0x04;

pub const DETECT_REQ: u8 = 0x73;
pub const DETECT_RESP: u8 = 0x46;

pub const RADIO_STATE_OFF: u8 = 0x00;
pub const RADIO_STATE_ON: u8 = 0x01;

// Packet header bits
pub const NIBBLE_SEQ: u8 = 0xF0;
pub const NIBBLE_FLAGS: u8 = 0x0F;
pub const FLAG_SPLIT: u8 = 0x01;
pub const SEQ_UNSET: u8 = 0xFF;

/// Firmware version we report to the host.
pub const MAJ_VERS: u8 = 0x01;
pub const MIN_VERS: u8 = 0x55;

/// Platform identifier for Linux.
pub const PLATFORM_LINUX: u8 = 0x90;
/// Board model: generic Linux SPI.
pub const BOARD_LINUX_SPI: u8 = 0xF0;

/// Maximum command buffer length.
const CMD_L: usize = 64;

/// Parsed KISS command ready for dispatch.
#[derive(Debug)]
pub enum KissCommand {
    /// Packet data to transmit.
    Data(Vec<u8>),
    /// Set frequency (Hz), or query if 0.
    SetFrequency(u32),
    /// Set bandwidth (Hz), or query if 0.
    SetBandwidth(u32),
    /// Set TX power (dBm), or query if 0xFF.
    SetTxPower(u8),
    /// Set spreading factor, or query if 0xFF.
    SetSf(u8),
    /// Set coding rate, or query if 0xFF.
    SetCr(u8),
    /// Set radio state: 0=off, 1=on, 0xFF=query.
    RadioState(u8),
    /// Query radio lock.
    RadioLock,
    /// Device detect handshake.
    Detect,
    /// Set implicit header length.
    SetImplicit(u8),
    /// Host is leaving (disconnect signal).
    Leave,
    /// Set short-term airtime lock (percentage * 100).
    StAirlock(u16),
    /// Set long-term airtime lock (percentage * 100).
    LtAirlock(u16),
    /// Enable/disable promiscuous mode.
    Promisc(u8),
    /// Query if ready to accept data.
    Ready,
    /// Query RX stats.
    StatRx,
    /// Query TX stats.
    StatTx,
    /// Query current RSSI.
    StatRssi,
    /// Query last packet SNR.
    StatSnr,
    /// Blink LED.
    Blink(u8),
    /// Get random byte.
    Random,
    /// Query firmware version.
    FwVersion,
    /// Query platform.
    Platform,
    /// Query board.
    Board,
    /// Save radio config to persistent settings (like EEPROM).
    ConfSave,
    /// Delete saved radio settings.
    ConfDelete,
    /// Reset device.
    Reset,
}

/// KISS frame parser state machine.
///
/// Feed bytes one at a time via `feed()`. When a complete command is parsed,
/// it is returned. This matches the firmware's `serial_callback()` logic.
pub struct KissParser {
    in_frame: bool,
    escape: bool,
    command: u8,
    frame_len: usize,
    cmdbuf: [u8; CMD_L],
    data_buf: Vec<u8>,
}

impl KissParser {
    pub fn new() -> Self {
        Self {
            in_frame: false,
            escape: false,
            command: CMD_UNKNOWN,
            frame_len: 0,
            cmdbuf: [0u8; CMD_L],
            data_buf: Vec::new(),
        }
    }

    /// Feed a single byte into the parser. Returns `Some(command)` when a
    /// complete KISS command has been parsed, `None` otherwise.
    pub fn feed(&mut self, byte: u8) -> Option<KissCommand> {
        // End of data frame
        if self.in_frame && byte == FEND && self.command == CMD_DATA {
            self.in_frame = false;
            if !self.data_buf.is_empty() {
                let data = std::mem::take(&mut self.data_buf);
                return Some(KissCommand::Data(data));
            }
            return None;
        }

        // Start of frame
        if byte == FEND {
            self.in_frame = true;
            self.command = CMD_UNKNOWN;
            self.frame_len = 0;
            self.data_buf.clear();
            return None;
        }

        if !self.in_frame {
            return None;
        }

        // First byte after FEND is the command
        if self.frame_len == 0 && self.command == CMD_UNKNOWN {
            self.command = byte;
            return None;
        }

        // Route by command type
        match self.command {
            CMD_DATA => {
                let b = self.unescape(byte)?;
                self.data_buf.push(b);
                None
            }
            CMD_FREQUENCY => {
                let b = self.unescape(byte)?;
                self.push_cmdbuf(b);
                if self.frame_len == 4 {
                    let freq = u32::from_be_bytes([
                        self.cmdbuf[0],
                        self.cmdbuf[1],
                        self.cmdbuf[2],
                        self.cmdbuf[3],
                    ]);
                    Some(KissCommand::SetFrequency(freq))
                } else {
                    None
                }
            }
            CMD_BANDWIDTH => {
                let b = self.unescape(byte)?;
                self.push_cmdbuf(b);
                if self.frame_len == 4 {
                    let bw = u32::from_be_bytes([
                        self.cmdbuf[0],
                        self.cmdbuf[1],
                        self.cmdbuf[2],
                        self.cmdbuf[3],
                    ]);
                    Some(KissCommand::SetBandwidth(bw))
                } else {
                    None
                }
            }
            CMD_TXPOWER => Some(KissCommand::SetTxPower(byte)),
            CMD_SF => Some(KissCommand::SetSf(byte)),
            CMD_CR => Some(KissCommand::SetCr(byte)),
            CMD_RADIO_STATE => Some(KissCommand::RadioState(byte)),
            CMD_RADIO_LOCK => Some(KissCommand::RadioLock),
            CMD_DETECT => {
                if byte == DETECT_REQ {
                    Some(KissCommand::Detect)
                } else {
                    None
                }
            }
            CMD_IMPLICIT => Some(KissCommand::SetImplicit(byte)),
            CMD_LEAVE => {
                if byte == 0xFF {
                    Some(KissCommand::Leave)
                } else {
                    None
                }
            }
            CMD_ST_ALOCK => {
                let b = self.unescape(byte)?;
                self.push_cmdbuf(b);
                if self.frame_len == 2 {
                    let at = u16::from_be_bytes([self.cmdbuf[0], self.cmdbuf[1]]);
                    Some(KissCommand::StAirlock(at))
                } else {
                    None
                }
            }
            CMD_LT_ALOCK => {
                let b = self.unescape(byte)?;
                self.push_cmdbuf(b);
                if self.frame_len == 2 {
                    let at = u16::from_be_bytes([self.cmdbuf[0], self.cmdbuf[1]]);
                    Some(KissCommand::LtAirlock(at))
                } else {
                    None
                }
            }
            CMD_PROMISC => Some(KissCommand::Promisc(byte)),
            CMD_READY => Some(KissCommand::Ready),
            CMD_STAT_RX => Some(KissCommand::StatRx),
            CMD_STAT_TX => Some(KissCommand::StatTx),
            CMD_STAT_RSSI => Some(KissCommand::StatRssi),
            CMD_STAT_SNR => Some(KissCommand::StatSnr),
            CMD_BLINK => Some(KissCommand::Blink(byte)),
            CMD_RANDOM => Some(KissCommand::Random),
            CMD_FW_VERSION => Some(KissCommand::FwVersion),
            CMD_PLATFORM => Some(KissCommand::Platform),
            CMD_BOARD => Some(KissCommand::Board),
            CMD_CONF_SAVE => Some(KissCommand::ConfSave),
            CMD_CONF_DELETE => Some(KissCommand::ConfDelete),
            CMD_RESET => {
                if byte == CMD_RESET_BYTE {
                    Some(KissCommand::Reset)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Handle FESC escape sequences. Returns None if we consumed an escape prefix.
    fn unescape(&mut self, byte: u8) -> Option<u8> {
        if byte == FESC {
            self.escape = true;
            return None;
        }
        if self.escape {
            self.escape = false;
            match byte {
                TFEND => Some(FEND),
                TFESC => Some(FESC),
                _ => Some(byte),
            }
        } else {
            Some(byte)
        }
    }

    fn push_cmdbuf(&mut self, byte: u8) {
        if self.frame_len < CMD_L {
            self.cmdbuf[self.frame_len] = byte;
            self.frame_len += 1;
        }
    }
}

// --- KISS frame encoder ---

/// Encode a KISS response frame: FEND + cmd + escaped payload + FEND.
pub fn encode_frame(cmd: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.push(FEND);
    frame.push(cmd);
    for &b in payload {
        match b {
            FEND => {
                frame.push(FESC);
                frame.push(TFEND);
            }
            FESC => {
                frame.push(FESC);
                frame.push(TFESC);
            }
            _ => frame.push(b),
        }
    }
    frame.push(FEND);
    frame
}

// --- KISS indication (response) helpers ---

pub fn indicate_frequency(freq: u32) -> Vec<u8> {
    encode_frame(CMD_FREQUENCY, &freq.to_be_bytes())
}

pub fn indicate_bandwidth(bw: u32) -> Vec<u8> {
    encode_frame(CMD_BANDWIDTH, &bw.to_be_bytes())
}

pub fn indicate_txpower(txp: u8) -> Vec<u8> {
    encode_frame(CMD_TXPOWER, &[txp])
}

pub fn indicate_spreadingfactor(sf: u8) -> Vec<u8> {
    encode_frame(CMD_SF, &[sf])
}

pub fn indicate_codingrate(cr: u8) -> Vec<u8> {
    encode_frame(CMD_CR, &[cr])
}

pub fn indicate_radiostate(state: u8) -> Vec<u8> {
    encode_frame(CMD_RADIO_STATE, &[state])
}

pub fn indicate_radio_lock(locked: bool) -> Vec<u8> {
    encode_frame(CMD_RADIO_LOCK, &[if locked { 0x01 } else { 0x00 }])
}

pub fn indicate_detect() -> Vec<u8> {
    encode_frame(CMD_DETECT, &[DETECT_RESP])
}

pub fn indicate_implicit_length(len: u8) -> Vec<u8> {
    encode_frame(CMD_IMPLICIT, &[len])
}

pub fn indicate_promisc(enabled: bool) -> Vec<u8> {
    encode_frame(CMD_PROMISC, &[if enabled { 0x01 } else { 0x00 }])
}

pub fn indicate_ready() -> Vec<u8> {
    encode_frame(CMD_READY, &[0x01])
}

pub fn indicate_not_ready() -> Vec<u8> {
    encode_frame(CMD_READY, &[0x00])
}

pub fn indicate_stat_rx(count: u32) -> Vec<u8> {
    encode_frame(CMD_STAT_RX, &count.to_be_bytes())
}

pub fn indicate_stat_tx(count: u32) -> Vec<u8> {
    encode_frame(CMD_STAT_TX, &count.to_be_bytes())
}

pub fn indicate_stat_rssi(rssi: u8) -> Vec<u8> {
    encode_frame(CMD_STAT_RSSI, &[rssi])
}

pub fn indicate_stat_snr(snr: u8) -> Vec<u8> {
    encode_frame(CMD_STAT_SNR, &[snr])
}

pub fn indicate_random(val: u8) -> Vec<u8> {
    encode_frame(CMD_RANDOM, &[val])
}

pub fn indicate_version() -> Vec<u8> {
    encode_frame(CMD_FW_VERSION, &[MAJ_VERS, MIN_VERS])
}

pub fn indicate_platform() -> Vec<u8> {
    encode_frame(CMD_PLATFORM, &[PLATFORM_LINUX])
}

pub fn indicate_board() -> Vec<u8> {
    encode_frame(CMD_BOARD, &[BOARD_LINUX_SPI])
}

pub fn indicate_error(error_code: u8) -> Vec<u8> {
    encode_frame(CMD_ERROR, &[error_code])
}

pub fn indicate_st_alock(limit: u16) -> Vec<u8> {
    encode_frame(CMD_ST_ALOCK, &limit.to_be_bytes())
}

pub fn indicate_lt_alock(limit: u16) -> Vec<u8> {
    encode_frame(CMD_LT_ALOCK, &limit.to_be_bytes())
}

/// Encode a received data packet as a KISS frame for the host.
pub fn indicate_data(data: &[u8]) -> Vec<u8> {
    encode_frame(CMD_DATA, data)
}

