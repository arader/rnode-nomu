use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use rnode_core::eeprom::{INFO_LEN, InfoBlock};
use rnode_core::storage::{RadioSettings, Storage, StoredInfo};
use serde::{Deserialize, Serialize};

/// File-backed storage: the radio settings in a TOML file, and optionally the EEPROM info block
/// that `rnodeconf --rom` writes in a file of its own. Missing or corrupt files are treated as
/// nothing saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TomlStorage {
    path: Option<PathBuf>,
    eeprom: Option<PathBuf>,
}

impl TomlStorage {
    /// Storage that keeps the radio settings in the TOML file at `path`, and no EEPROM info block.
    pub fn new(path: Option<PathBuf>) -> Self {
        Self { path, eeprom: None }
    }

    /// Keep the EEPROM info block in the file at `eeprom` too (`[storage] eeprom`): its
    /// [`INFO_LEN`] bytes, as the host wrote them. Without a file the EEPROM is unprovisioned; a
    /// file that is not a valid, locked info block is ignored with a warning, which leaves the
    /// EEPROM unprovisioned as well.
    pub fn with_eeprom(mut self, eeprom: Option<PathBuf>) -> Self {
        self.eeprom = eeprom;
        self
    }

    fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

/// Write `content` to `path` as a new file with mode 0600: into a temporary file next to it,
/// flushed to disk, then renamed into place, so that a crash leaves either the old file or the new
/// one. Missing parent directories are created.
fn write_atomically(path: &Path, content: &[u8]) -> Result<(), StorageError> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(|source| StorageError::Io {
            path: parent.into(),
            source,
        })?;
    }
    let temp = path.with_extension(format!("tmp.{}", std::process::id()));
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
    file.write_all(content).map_err(|source| StorageError::Io {
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
    })
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
        let content = toml::to_string_pretty(&StoredRadioSettings::from(*settings))?;
        write_atomically(path, content.as_bytes())
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

    fn load_info(&mut self) -> Result<StoredInfo, Self::Error> {
        let Some(path) = self.eeprom.as_deref() else {
            return Ok(StoredInfo::NotKept);
        };
        let io_error = |source| StorageError::Io {
            path: path.into(),
            source,
        };
        let file = match File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(StoredInfo::Empty),
            Err(source) => return Err(io_error(source)),
        };
        // One byte more than an info block, so that a longer file is noticed without reading it all.
        let mut bytes = Vec::with_capacity(INFO_LEN + 1);
        file.take(INFO_LEN as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        let invalid = if bytes.len() > INFO_LEN {
            format!("it is longer than {INFO_LEN} bytes")
        } else {
            match InfoBlock::from_bytes(&bytes) {
                Ok(block) => return Ok(StoredInfo::Locked(block)),
                Err(e) => e.to_string(),
            }
        };
        log::warn!(
            "ignoring EEPROM file {}: {invalid}; the EEPROM reads as unprovisioned",
            path.display()
        );
        Ok(StoredInfo::Empty)
    }

    fn save_info(&mut self, info: &InfoBlock) -> Result<(), Self::Error> {
        let Some(path) = self.eeprom.as_deref() else {
            return Ok(());
        };
        write_atomically(path, info.as_bytes())
    }
}
