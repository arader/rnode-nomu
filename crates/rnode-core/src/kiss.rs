//! KISS constants, escaping and host-command decoding.
//!
//! The decoder follows `serial_callback()` in RNode_Firmware v1.86: after `FEND` the first byte
//! is the command, data frames are unescaped until the closing `FEND`, multi-byte commands are
//! unescaped into a 64-byte command buffer and fire as soon as their firmware length is reached,
//! while single-byte commands act on each raw byte until `FEND`.

pub const FEND: u8 = 0xC0;
pub const FESC: u8 = 0xDB;
pub const TFEND: u8 = 0xDC;
pub const TFESC: u8 = 0xDD;

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
pub const CMD_STAT_CSMA: u8 = 0x28;
pub const CMD_BLINK: u8 = 0x30;
pub const CMD_RANDOM: u8 = 0x40;
pub const CMD_FB_EXT: u8 = 0x41;
pub const CMD_FB_READ: u8 = 0x42;
pub const CMD_FB_WRITE: u8 = 0x43;
pub const CMD_DISP_INT: u8 = 0x45;
pub const CMD_BT_CTRL: u8 = 0x46;
pub const CMD_BOARD: u8 = 0x47;
pub const CMD_PLATFORM: u8 = 0x48;
pub const CMD_MCU: u8 = 0x49;
pub const CMD_FW_VERSION: u8 = 0x50;
pub const CMD_ROM_READ: u8 = 0x51;
pub const CMD_ROM_WRITE: u8 = 0x52;
pub const CMD_CONF_SAVE: u8 = 0x53;
pub const CMD_CONF_DELETE: u8 = 0x54;
pub const CMD_RESET: u8 = 0x55;
pub const CMD_DEV_HASH: u8 = 0x56;
pub const CMD_DEV_SIG: u8 = 0x57;
pub const CMD_FW_HASH: u8 = 0x58;
pub const CMD_UNLOCK_ROM: u8 = 0x59;
pub const CMD_HASHES: u8 = 0x60;
pub const CMD_FW_UPD: u8 = 0x61;
pub const CMD_BT_PIN: u8 = 0x62;
pub const CMD_DISP_ADDR: u8 = 0x63;
pub const CMD_DISP_BLNK: u8 = 0x64;
pub const CMD_NP_INT: u8 = 0x65;
pub const CMD_DISP_READ: u8 = 0x66;
pub const CMD_DISP_ROT: u8 = 0x67;
pub const CMD_DISP_RCND: u8 = 0x68;
pub const CMD_DIS_IA: u8 = 0x69;
pub const CMD_WIFI_MODE: u8 = 0x6A;
pub const CMD_WIFI_SSID: u8 = 0x6B;
pub const CMD_WIFI_PSK: u8 = 0x6C;
pub const CMD_CFG_READ: u8 = 0x6D;
pub const CMD_WIFI_CHN: u8 = 0x6E;
pub const CMD_BT_UNPAIR: u8 = 0x70;
pub const CMD_WIFI_IP: u8 = 0x84;
pub const CMD_WIFI_NM: u8 = 0x85;
pub const CMD_ERROR: u8 = 0x90;

pub const DETECT_REQ: u8 = 0x73;
pub const DETECT_RESP: u8 = 0x46;
pub const RADIO_STATE_OFF: u8 = 0x00;
pub const RADIO_STATE_ON: u8 = 0x01;
pub const CMD_RESET_BYTE: u8 = 0xF8;

pub const ERROR_INITRADIO: u8 = 0x01;
pub const ERROR_TXFAILED: u8 = 0x02;
pub const ERROR_EEPROM_LOCKED: u8 = 0x03;
pub const ERROR_QUEUE_FULL: u8 = 0x04;
pub const ERROR_MEMORY_LOW: u8 = 0x05;
pub const ERROR_MODEM_TIMEOUT: u8 = 0x06;

pub const MTU: usize = 508;
pub const SINGLE_MTU: usize = 255;
pub const HEADER_LEN: usize = 1;
pub const FLAG_SPLIT: u8 = 0x01;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEvent {
    Data(Vec<u8>),
    Command { cmd: u8, payload: Vec<u8> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoder {
    in_frame: bool,
    escape: bool,
    command: u8,
    frame_len: usize,
    data: Vec<u8>,
    cmdbuf: Vec<u8>,
    data_over_mtu: bool,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            in_frame: false,
            escape: false,
            command: CMD_UNKNOWN,
            frame_len: 0,
            data: Vec::new(),
            cmdbuf: Vec::new(),
            data_over_mtu: false,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    pub fn push(&mut self, byte: u8, events: &mut Vec<HostEvent>) {
        if self.in_frame && byte == FEND && self.command == CMD_DATA {
            self.in_frame = false;
            self.escape = false;
            if !self.data_over_mtu {
                events.push(HostEvent::Data(core::mem::take(&mut self.data)));
            }
            self.data.clear();
            return;
        }
        if byte == FEND {
            self.in_frame = true;
            self.escape = false;
            self.command = CMD_UNKNOWN;
            self.frame_len = 0;
            self.data.clear();
            self.cmdbuf.clear();
            self.data_over_mtu = false;
            return;
        }
        if !self.in_frame || self.frame_len >= MTU {
            return;
        }
        if self.frame_len == 0 && self.command == CMD_UNKNOWN {
            self.command = byte;
            self.frame_len = 1;
            return;
        }
        if self.command == CMD_DATA {
            if let Some(b) = self.unescape(byte) {
                if self.data.len() < MTU {
                    self.data.push(b);
                } else {
                    self.data_over_mtu = true;
                }
            }
            return;
        }
        if let Some(len) = multi_len(self.command) {
            if let Some(b) = self.unescape(byte) {
                if self.cmdbuf.len() < 64 {
                    self.cmdbuf.push(b);
                    self.frame_len += 1;
                }
                if self.cmdbuf.len() == len {
                    events.push(HostEvent::Command {
                        cmd: self.command,
                        payload: self.cmdbuf.clone(),
                    });
                }
            }
        } else {
            events.push(HostEvent::Command {
                cmd: self.command,
                payload: vec![byte],
            });
            self.frame_len += 1;
        }
    }

    fn unescape(&mut self, byte: u8) -> Option<u8> {
        if byte == FESC {
            self.escape = true;
            None
        } else {
            let mut b = byte;
            if self.escape {
                if b == TFEND {
                    b = FEND;
                } else if b == TFESC {
                    b = FESC;
                }
                self.escape = false;
            }
            Some(b)
        }
    }
}

fn multi_len(cmd: u8) -> Option<usize> {
    match cmd {
        CMD_FREQUENCY | CMD_BANDWIDTH => Some(4),
        CMD_ST_ALOCK | CMD_LT_ALOCK | CMD_ROM_WRITE => Some(2),
        CMD_FB_WRITE | CMD_DEV_SIG | CMD_FW_HASH | CMD_WIFI_SSID | CMD_WIFI_PSK | CMD_WIFI_IP | CMD_WIFI_NM
        | CMD_DISP_INT | CMD_DISP_ADDR | CMD_DISP_BLNK | CMD_DISP_ROT | CMD_DIS_IA | CMD_DISP_RCND | CMD_NP_INT => {
            Some(64)
        }
        _ => None,
    }
}

pub fn escape_into(out: &mut Vec<u8>, byte: u8) {
    match byte {
        FEND => out.extend_from_slice(&[FESC, TFEND]),
        FESC => out.extend_from_slice(&[FESC, TFESC]),
        _ => out.push(byte),
    }
}

pub fn frame_plain(cmd: u8, payload: &[u8], out: &mut Vec<u8>) {
    out.push(FEND);
    out.push(cmd);
    out.extend_from_slice(payload);
    out.push(FEND);
}

pub fn frame_escaped(cmd: u8, payload: &[u8], out: &mut Vec<u8>) {
    out.push(FEND);
    out.push(cmd);
    for &b in payload {
        escape_into(out, b);
    }
    out.push(FEND);
}

pub fn host_data(payload: &[u8], out: &mut Vec<u8>) {
    frame_escaped(CMD_DATA, payload, out);
}
