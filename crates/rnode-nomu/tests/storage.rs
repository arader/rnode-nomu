//! `TomlStorage`, which keeps the radio settings saved with `CMD_CONF_SAVE`, and the EEPROM info
//! block that `rnodeconf --rom` writes.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use rnode_core::eeprom::{DeviceInfo, INFO_LEN, InfoBlock};
use rnode_core::storage::{RadioSettings, Storage, StoredInfo};
use rnode_nomu::storage::TomlStorage;
use serial_test::serial;

use common::test_dir;

/// The names of the files in `dir`.
fn file_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A locked info block, as `rnodeconf --rom` writes one, with a signature of `0x5A` bytes.
fn signed_block() -> InfoBlock {
    let mut bytes = *InfoBlock::unsigned(DeviceInfo::homebrew(7)).as_bytes();
    bytes[0x1B..0x9B].fill(0x5A);
    InfoBlock::from_bytes(&bytes).unwrap()
}

/// `[storage] eeprom`: no file means an unprovisioned EEPROM; the info block is written like the
/// settings, to a temporary file renamed into place, with mode 0600, in a directory created if
/// missing, and it is the only thing in the file.
#[test]
fn the_eeprom_file_keeps_the_locked_info_block() {
    assert_eq!(TomlStorage::new(None).load_info().unwrap(), StoredInfo::NotKept);
    let dir = test_dir("storage-eeprom");
    let path = dir.join("state").join("eeprom.bin");
    let mut storage = TomlStorage::new(Some(dir.join("settings.toml"))).with_eeprom(Some(path.clone()));
    assert_eq!(storage.load_info().unwrap(), StoredInfo::Empty);

    let block = signed_block();
    storage.save_info(&block).unwrap();
    assert_eq!(fs::read(&path).unwrap(), block.as_bytes());
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(file_names(&dir.join("state")), ["eeprom.bin"]);
    assert_eq!(storage.load_info().unwrap(), StoredInfo::Locked(block.clone()));
    assert_eq!(
        storage.load().unwrap(),
        None,
        "the radio settings are a file of their own"
    );

    // A new daemon finds it; deleting the file makes the EEPROM unprovisioned again.
    let mut restarted = TomlStorage::new(None).with_eeprom(Some(path.clone()));
    assert_eq!(restarted.load_info().unwrap(), StoredInfo::Locked(block));
    fs::remove_file(&path).unwrap();
    assert_eq!(restarted.load_info().unwrap(), StoredInfo::Empty);
}

/// A file that is not a locked info block rnode-nomu takes, whatever is wrong with it, leaves
/// the EEPROM unprovisioned, and stays as it is until a host locks a new info block.
#[test]
fn an_eeprom_file_that_is_no_valid_info_block_reads_as_unprovisioned() {
    let dir = test_dir("storage-eeprom-corrupt");
    let path = dir.join("eeprom.bin");
    let good = *signed_block().as_bytes();
    let changed = |at: usize, byte: u8| {
        let mut bytes = good.to_vec();
        bytes[at] = byte;
        bytes
    };
    for contents in [
        Vec::new(),
        good[..INFO_LEN - 1].to_vec(),
        [&good[..], &[0x00]].concat(),
        vec![0x73; 10 * 1024],
        vec![0xFF; INFO_LEN],
        changed(0x9B, 0x00),
        changed(0x0B, good[0x0B] ^ 0x01),
        changed(0x00, 0x03),
    ] {
        fs::write(&path, &contents).unwrap();
        let mut storage = TomlStorage::new(None).with_eeprom(Some(path.clone()));
        assert_eq!(storage.load_info().unwrap(), StoredInfo::Empty, "{contents:02x?}");
        assert_eq!(fs::read(&path).unwrap(), contents);
    }

    // A directory where the file should be is an error the node reports, not a crash.
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    let mut storage = TomlStorage::new(None).with_eeprom(Some(path.clone()));
    let error = storage.load_info().unwrap_err().to_string();
    assert!(
        error.starts_with(&format!("io error for {}: ", path.display())),
        "{error}"
    );
}

#[test]
fn toml_storage_round_trip_atomic_and_corrupt() {
    let dir = test_dir("storage");
    let path = dir.join("settings.toml");
    let mut storage = TomlStorage::new(Some(path.clone()));
    assert_eq!(storage.load().unwrap(), None);
    let settings = RadioSettings {
        frequency: 867_200_000,
        bandwidth: 125_000,
        spreading_factor: 8,
        coding_rate: 5,
        tx_power: 14,
    };
    storage.save(&settings).unwrap();
    assert_eq!(storage.load().unwrap(), Some(settings));
    assert_eq!(
        fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains("tmp"))
            .count(),
        0
    );
    fs::write(&path, "not = [valid").unwrap();
    assert_eq!(storage.load().unwrap(), None);
    storage.delete().unwrap();
    assert!(!path.exists());
}

#[test]
#[serial]
fn toml_storage_saves_bare_filename_in_current_directory() {
    struct CwdGuard(PathBuf);
    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.0);
        }
    }

    let dir = test_dir("storage-bare");
    let old = std::env::current_dir().unwrap();
    std::env::set_current_dir(&dir).unwrap();
    let _guard = CwdGuard(old);

    let mut storage = TomlStorage::new(Some(PathBuf::from("settings.toml")));
    let settings = RadioSettings {
        frequency: 867_200_000,
        bandwidth: 125_000,
        spreading_factor: 8,
        coding_rate: 5,
        tx_power: 14,
    };
    storage.save(&settings).unwrap();
    assert_eq!(storage.load().unwrap(), Some(settings));
}
