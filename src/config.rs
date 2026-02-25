// Copyright (C) 2024, Mark Qvist
// Ported to Rust for Linux by RNode contributors.
// Licensed under the GNU General Public License v3.0 or later.

use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub spi: SpiConfig,
    pub gpio: GpioConfig,
    pub radio: RadioConfig,
    pub pty: PtyConfig,
    #[serde(default)]
    pub csma: CsmaConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SpiConfig {
    pub device: String,
    #[serde(default = "default_spi_speed")]
    pub speed_hz: u32,
}

fn default_spi_speed() -> u32 {
    16_000_000
}

#[derive(Debug, Deserialize, Clone)]
pub struct GpioConfig {
    #[serde(default = "default_gpio_chip")]
    pub chip: String,
    pub reset_line: u32,
    pub busy_line: u32,
    pub dio0_line: u32,
    pub rxen_line: Option<i32>,
}

fn default_gpio_chip() -> String {
    "gpiochip0".to_string()
}

#[derive(Debug, Deserialize, Clone)]
pub struct RadioConfig {
    #[serde(default = "default_frequency")]
    pub frequency: u32,
    #[serde(default = "default_bandwidth")]
    pub bandwidth: u32,
    #[serde(default = "default_sf")]
    pub spreading_factor: u8,
    #[serde(default = "default_cr")]
    pub coding_rate: u8,
    #[serde(default = "default_txp")]
    pub tx_power: i8,
    #[serde(default = "default_preamble")]
    pub preamble_symbols: u16,
    #[serde(default = "default_true")]
    pub enable_crc: bool,
    #[serde(default)]
    pub enable_tcxo: bool,
    pub tcxo_voltage: Option<String>,
}

fn default_frequency() -> u32 {
    915_000_000
}
fn default_bandwidth() -> u32 {
    125_000
}
fn default_sf() -> u8 {
    7
}
fn default_cr() -> u8 {
    5
}
fn default_txp() -> i8 {
    2
}
fn default_preamble() -> u16 {
    18
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, Clone)]
pub struct PtyConfig {
    pub symlink: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct CsmaConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_slot_min")]
    pub slot_min_ms: u64,
    #[serde(default = "default_slot_max")]
    pub slot_max_ms: u64,
}

fn default_slot_min() -> u64 {
    24
}
fn default_slot_max() -> u64 {
    100
}

impl Default for CsmaConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            slot_min_ms: default_slot_min(),
            slot_max_ms: default_slot_max(),
        }
    }
}

impl Config {
    pub fn load(path: &str) -> crate::error::Result<Self> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            crate::error::RNodeError::Config(format!("Failed to read config file '{}': {}", path, e))
        })?;
        let config: Config = toml::from_str(&content).map_err(|e| {
            crate::error::RNodeError::Config(format!("Failed to parse config file '{}': {}", path, e))
        })?;
        Ok(config)
    }

    /// Parse config from a TOML string (for testing).
    pub fn from_str(content: &str) -> std::result::Result<Self, String> {
        toml::from_str(content).map_err(|e| e.to_string())
    }
}

