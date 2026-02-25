// Copyright (C) 2024, Mark Qvist
// Ported to Rust for Linux by RNode contributors.
// Licensed under the GNU General Public License v3.0 or later.

use serde::{Deserialize, Serialize};

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
    #[serde(default = "default_preamble")]
    pub preamble_symbols: u16,
    #[serde(default = "default_true")]
    pub enable_crc: bool,
    #[serde(default)]
    pub enable_tcxo: bool,
    pub tcxo_voltage: Option<String>,
    /// Path to the saved radio settings file (simulates EEPROM).
    pub radio_settings: Option<String>,
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

// --- Saved radio settings (simulates EEPROM) ---

/// Radio link parameters persisted to a settings file, analogous to
/// RNode_Firmware's EEPROM-saved configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RadioSettings {
    pub frequency: u32,
    pub bandwidth: u32,
    pub spreading_factor: u8,
    pub coding_rate: u8,
    pub tx_power: i8,
}

impl RadioSettings {
    /// Load saved settings from a TOML file. Returns None if file doesn't exist
    /// or contains invalid data.
    pub fn load(path: &str) -> Option<Self> {
        let content = std::fs::read_to_string(path).ok()?;
        toml::from_str(&content).ok()
    }

    /// Save radio settings to a TOML file.
    pub fn save(&self, path: &str) -> std::io::Result<()> {
        let content = toml::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        std::fs::write(path, content)
    }

    /// Delete the saved radio settings file.
    pub fn delete(path: &str) -> std::io::Result<()> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

// --- In-memory radio parameters with lock logic ---

/// Tracks the current radio link parameters in memory.
/// Mirrors RNode_Firmware's `lora_freq`, `lora_bw`, `lora_sf`, `lora_cr`, `lora_txp` globals
/// and the `update_radio_lock()` function.
#[derive(Debug, Clone)]
pub struct RadioParams {
    pub frequency: u32,
    pub bandwidth: u32,
    pub spreading_factor: u8,
    pub coding_rate: u8,
    pub tx_power: i8,
}

impl RadioParams {
    /// Create params in the "locked" state (all values unset), matching
    /// RNode_Firmware's initial state: freq=0, bw=0, sf=0, cr=0, txp=0xFF.
    pub fn new() -> Self {
        Self {
            frequency: 0,
            bandwidth: 0,
            spreading_factor: 0,
            coding_rate: 0,
            tx_power: 0x7F, // sentinel for "unset" (firmware uses 0xFF in u8)
        }
    }

    /// Load from saved radio settings.
    pub fn from_settings(settings: &RadioSettings) -> Self {
        Self {
            frequency: settings.frequency,
            bandwidth: settings.bandwidth,
            spreading_factor: settings.spreading_factor,
            coding_rate: settings.coding_rate,
            tx_power: settings.tx_power,
        }
    }

    /// Returns true if any required parameter is unset (radio cannot start).
    /// Mirrors `update_radio_lock()` in firmware.
    pub fn is_locked(&self) -> bool {
        self.frequency == 0
            || self.bandwidth == 0
            || self.spreading_factor == 0
            || self.coding_rate == 0
            || self.tx_power == 0x7F
    }

    /// Convert to RadioSettings for persistence.
    pub fn to_settings(&self) -> RadioSettings {
        RadioSettings {
            frequency: self.frequency,
            bandwidth: self.bandwidth,
            spreading_factor: self.spreading_factor,
            coding_rate: self.coding_rate,
            tx_power: self.tx_power,
        }
    }
}

impl Default for RadioParams {
    fn default() -> Self {
        Self::new()
    }
}

