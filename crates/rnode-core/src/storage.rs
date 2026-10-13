//! Persistence of the radio settings saved with `CMD_CONF_SAVE` (the firmware keeps them in
//! EEPROM).

use core::convert::Infallible;
use core::fmt;

/// Radio link parameters persisted by `CMD_CONF_SAVE` and loaded at boot (firmware
/// `eeprom_conf_save()` / `eeprom_conf_load()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RadioSettings {
    /// Carrier frequency in Hz.
    pub frequency: u32,
    /// Bandwidth in Hz.
    pub bandwidth: u32,
    /// Spreading factor (5–12).
    pub spreading_factor: u8,
    /// Coding rate denominator (5–8).
    pub coding_rate: u8,
    /// TX power in dBm as sent by the host.
    pub tx_power: u8,
}

/// Where saved radio settings live.
pub trait Storage {
    /// Error type of the backing store.
    type Error: fmt::Debug + fmt::Display;

    /// The saved settings, or `None` if nothing has been saved.
    fn load(&mut self) -> Result<Option<RadioSettings>, Self::Error>;

    /// Persist `settings`, replacing any previously saved ones.
    fn save(&mut self, settings: &RadioSettings) -> Result<(), Self::Error>;

    /// Forget the saved settings (firmware `eeprom_conf_delete()`). Deleting when nothing is
    /// saved is not an error.
    fn delete(&mut self) -> Result<(), Self::Error>;
}

/// In-memory [`Storage`], for tests and for running without persistence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryStorage {
    settings: Option<RadioSettings>,
}

impl MemoryStorage {
    /// Storage holding `settings`, as if they had been saved earlier.
    pub fn with_settings(settings: RadioSettings) -> Self {
        Self {
            settings: Some(settings),
        }
    }

    /// The currently saved settings.
    pub fn settings(&self) -> Option<RadioSettings> {
        self.settings
    }
}

impl Storage for MemoryStorage {
    type Error = Infallible;

    fn load(&mut self) -> Result<Option<RadioSettings>, Self::Error> {
        Ok(self.settings)
    }

    fn save(&mut self, settings: &RadioSettings) -> Result<(), Self::Error> {
        self.settings = Some(*settings);
        Ok(())
    }

    fn delete(&mut self) -> Result<(), Self::Error> {
        self.settings = None;
        Ok(())
    }
}
