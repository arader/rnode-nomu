use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use rnode_core::storage::{RadioSettings, Storage};
use serde::{Deserialize, Serialize};

/// TOML-backed radio settings storage. Missing/corrupt files are treated as no saved settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TomlStorage {
    path: Option<PathBuf>,
}

impl TomlStorage {
    pub fn new(path: Option<PathBuf>) -> Self {
        Self { path }
    }

    fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("io error for {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("encode radio settings: {0}")]
    Encode(#[from] toml::ser::Error),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct StoredRadioSettings {
    frequency: u32,
    bandwidth: u32,
    spreading_factor: u8,
    coding_rate: u8,
    tx_power: u8,
}

impl From<RadioSettings> for StoredRadioSettings {
    fn from(value: RadioSettings) -> Self {
        Self {
            frequency: value.frequency,
            bandwidth: value.bandwidth,
            spreading_factor: value.spreading_factor,
            coding_rate: value.coding_rate,
            tx_power: value.tx_power,
        }
    }
}

impl From<StoredRadioSettings> for RadioSettings {
    fn from(value: StoredRadioSettings) -> Self {
        Self {
            frequency: value.frequency,
            bandwidth: value.bandwidth,
            spreading_factor: value.spreading_factor,
            coding_rate: value.coding_rate,
            tx_power: value.tx_power,
        }
    }
}

impl Storage for TomlStorage {
    type Error = StorageError;

    fn load(&mut self) -> Result<Option<RadioSettings>, Self::Error> {
        let Some(path) = self.path() else { return Ok(None) };
        match fs::read_to_string(path) {
            Ok(content) => match toml::from_str::<StoredRadioSettings>(&content) {
                Ok(settings) => Ok(Some(settings.into())),
                Err(e) => {
                    log::warn!("ignoring corrupt radio settings file {}: {e}", path.display());
                    Ok(None)
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(StorageError::Io {
                path: path.into(),
                source,
            }),
        }
    }

    fn save(&mut self, settings: &RadioSettings) -> Result<(), Self::Error> {
        let Some(path) = self.path() else { return Ok(()) };
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent).map_err(|source| StorageError::Io {
                path: parent.into(),
                source,
            })?;
        }
        let temp = path.with_extension(format!("tmp.{}", std::process::id()));
        let content = toml::to_string_pretty(&StoredRadioSettings::from(*settings))?;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&temp)
            .map_err(|source| StorageError::Io {
                path: temp.clone(),
                source,
            })?;
        file.write_all(content.as_bytes()).map_err(|source| StorageError::Io {
            path: temp.clone(),
            source,
        })?;
        file.sync_all().map_err(|source| StorageError::Io {
            path: temp.clone(),
            source,
        })?;
        drop(file);
        fs::rename(&temp, path).map_err(|source| StorageError::Io {
            path: path.into(),
            source,
        })?;
        Ok(())
    }

    fn delete(&mut self) -> Result<(), Self::Error> {
        let Some(path) = self.path() else { return Ok(()) };
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(StorageError::Io {
                path: path.into(),
                source,
            }),
        }
    }
}
