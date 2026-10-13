//! `MemoryStorage`, the in-memory store for radio settings saved with `CMD_CONF_SAVE`.

use rnode_core::storage::*;

const SETTINGS: RadioSettings = RadioSettings {
    frequency: 867_200_000,
    bandwidth: 125_000,
    spreading_factor: 8,
    coding_rate: 5,
    tx_power: 14,
};

#[test]
fn memory_storage_round_trip() {
    let mut storage = MemoryStorage::default();
    assert_eq!(storage.load().unwrap(), None);
    storage.save(&SETTINGS).unwrap();
    assert_eq!(storage.load().unwrap(), Some(SETTINGS));
    storage.delete().unwrap();
    assert_eq!(storage.load().unwrap(), None);
    storage.delete().unwrap();
}

#[test]
fn memory_storage_with_settings() {
    let mut storage = MemoryStorage::with_settings(SETTINGS);
    assert_eq!(storage.settings(), Some(SETTINGS));
    assert_eq!(storage.load().unwrap(), Some(SETTINGS));
}
