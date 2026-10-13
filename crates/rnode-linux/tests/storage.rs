//! `TomlStorage`, which keeps the radio settings saved with `CMD_CONF_SAVE`.

mod common;

use std::fs;
use std::path::PathBuf;

use rnode_core::storage::{RadioSettings, Storage};
use rnode_linux::storage::TomlStorage;
use serial_test::serial;

use common::test_dir;

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
