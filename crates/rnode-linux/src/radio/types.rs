// Copyright (C) 2024, Mark Qvist
// Ported to Rust for Linux by RNode contributors.
// Licensed under the GNU General Public License v3.0 or later.

/// TCXO voltage settings for SX1262.
#[derive(Debug, Clone, Copy)]
pub enum TcxoVoltage {
    V1_6,
    V1_7,
    V1_8,
    V2_2,
    V2_4,
    V2_7,
    V3_0,
    V3_3,
}

impl TcxoVoltage {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "1.6V" => Some(Self::V1_6),
            "1.7V" => Some(Self::V1_7),
            "1.8V" => Some(Self::V1_8),
            "2.2V" => Some(Self::V2_2),
            "2.4V" => Some(Self::V2_4),
            "2.7V" => Some(Self::V2_7),
            "3.0V" => Some(Self::V3_0),
            "3.3V" => Some(Self::V3_3),
            _ => None,
        }
    }

    /// Get the SX1262 DIO3 TCXO voltage register value.
    pub fn register_value(self) -> u8 {
        match self {
            Self::V1_6 => 0x00,
            Self::V1_7 => 0x01,
            Self::V1_8 => 0x02,
            Self::V2_2 => 0x03,
            Self::V2_4 => 0x06,
            Self::V2_7 => 0x06,
            Self::V3_0 => 0x06,
            Self::V3_3 => 0x07,
        }
    }
}

/// Bandwidth code for SX1262 register values.
#[derive(Debug, Clone, Copy)]
pub enum BandwidthCode {
    Bw7_8  = 0x00,
    Bw10_4 = 0x08,
    Bw15_6 = 0x01,
    Bw20_8 = 0x09,
    Bw31_25 = 0x02,
    Bw41_7 = 0x0A,
    Bw62_5 = 0x03,
    Bw125  = 0x04,
    Bw250  = 0x05,
    Bw500  = 0x06,
}

impl BandwidthCode {
    /// Convert a bandwidth in Hz to the SX1262 register code.
    pub fn from_hz(bw: u32) -> Self {
        if bw <= 7_800 {
            Self::Bw7_8
        } else if bw <= 10_400 {
            Self::Bw10_4
        } else if bw <= 15_600 {
            Self::Bw15_6
        } else if bw <= 20_800 {
            Self::Bw20_8
        } else if bw <= 31_250 {
            Self::Bw31_25
        } else if bw <= 41_700 {
            Self::Bw41_7
        } else if bw <= 62_500 {
            Self::Bw62_5
        } else if bw <= 125_000 {
            Self::Bw125
        } else if bw <= 250_000 {
            Self::Bw250
        } else {
            Self::Bw500
        }
    }

    /// Convert register code back to approximate bandwidth in Hz.
    pub fn to_hz(self) -> u32 {
        match self {
            Self::Bw7_8 => 7_800,
            Self::Bw10_4 => 10_400,
            Self::Bw15_6 => 15_600,
            Self::Bw20_8 => 20_800,
            Self::Bw31_25 => 31_250,
            Self::Bw41_7 => 41_700,
            Self::Bw62_5 => 62_500,
            Self::Bw125 => 125_000,
            Self::Bw250 => 250_000,
            Self::Bw500 => 500_000,
        }
    }

    pub fn code(self) -> u8 {
        self as u8
    }
}

