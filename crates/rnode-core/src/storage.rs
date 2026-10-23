//! Persistence of the radio settings saved with `CMD_CONF_SAVE`, and of the EEPROM info block that
//! `rnodeconf --rom` writes (the firmware keeps both in EEPROM).

use core::convert::Infallible;
use core::fmt;

use crate::eeprom::InfoBlock;

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
    /// TX power in dBm as the node reports it to the host, an unsigned byte: the host's request,
    /// clamped and, with an external PA, mapped through its gain table (firmware `lora_txp`); 0xFF
    /// means none was set. The node ignores saved settings with a power from 128 to 254, which
    /// earlier builds wrote for −9…−2 dBm (see [`crate::Node::boot`]).
    pub tx_power: u8,
}

/// What a [`Storage`] holds of the EEPROM's info block (see [`crate::eeprom`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum StoredInfo {
    /// The storage keeps no info block: the node reports its built-in one, which is locked.
    #[default]
    NotKept,
    /// The storage keeps an info block but holds none it can use, so the EEPROM is unprovisioned:
    /// a host can write one with `CMD_ROM_WRITE` (`rnodeconf --rom`).
    Empty,
    /// The info block a host wrote and locked.
    Locked(InfoBlock),
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

    /// The EEPROM info block this storage holds. The default keeps none, so the node reports its
    /// built-in one.
    fn load_info(&mut self) -> Result<StoredInfo, Self::Error> {
        Ok(StoredInfo::NotKept)
    }

    /// Keep `info`, a locked info block the host wrote, so that [`Storage::load_info`] returns it
    /// from now on. The node calls this only after `load_info()` returned [`StoredInfo::Empty`],
    /// so the default, which keeps no info block, does nothing.
    fn save_info(&mut self, info: &InfoBlock) -> Result<(), Self::Error> {
        let _ = info;
        Ok(())
    }
}

/// In-memory [`Storage`], for tests and for running without persistence. It keeps no info block
/// unless made with [`MemoryStorage::keeping_info`] or given one with [`MemoryStorage::set_info`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryStorage {
    settings: Option<RadioSettings>,
    info: StoredInfo,
}

impl MemoryStorage {
    /// Storage holding `settings`, as if they had been saved earlier.
    pub fn with_settings(settings: RadioSettings) -> Self {
        Self {
            settings: Some(settings),
            ..Self::default()
        }
    }

    /// Storage that keeps an EEPROM info block and holds none yet, so the EEPROM is unprovisioned.
    pub fn keeping_info() -> Self {
        Self {
            info: StoredInfo::Empty,
            ..Self::default()
        }
    }

    /// The currently saved settings.
    pub fn settings(&self) -> Option<RadioSettings> {
        self.settings
    }

    /// The info block held now.
    pub fn info(&self) -> &StoredInfo {
        &self.info
    }

    /// Hold `info` from now on, as if it had been stored (or deleted) outside the node.
    pub fn set_info(&mut self, info: StoredInfo) {
        self.info = info;
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

    fn load_info(&mut self) -> Result<StoredInfo, Self::Error> {
        Ok(self.info.clone())
    }

    fn save_info(&mut self, info: &InfoBlock) -> Result<(), Self::Error> {
        if self.info != StoredInfo::NotKept {
            self.info = StoredInfo::Locked(info.clone());
        }
        Ok(())
    }
}
