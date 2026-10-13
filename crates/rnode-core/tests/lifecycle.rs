//! The node lifecycle as the daemon drives it: boot from saved settings, wake-up scheduling and
//! shutdown.

mod common;

use common::init;
use rnode_core::{MemoryStorage, Node, NodeConfig, Radio, RadioSettings, SimMedium};
// The tests below refer to this module as `crate::node`.
use rnode_core::node;

#[test]
fn boot_from_memory_storage_starts_radio() {
    let medium = SimMedium::new();
    let settings = RadioSettings {
        frequency: 867_200_000,
        bandwidth: 125_000,
        spreading_factor: 7,
        coding_rate: 5,
        tx_power: 10,
    };
    let mut n = Node::new(
        medium.radio(),
        MemoryStorage::with_settings(settings),
        NodeConfig::default(),
        3,
    );
    n.boot(0);
    assert!(n.is_radio_online());
    assert_eq!(n.radio().frequency(), settings.frequency);
}

#[test]
fn next_wakeup_tracks_offline_start_and_last_status_tick() {
    let medium = SimMedium::new();
    let mut n = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 1);
    assert_eq!(n.next_wakeup_ms(100), 1_100);
    init(&mut n);
    n.take_host_output();
    assert_eq!(n.next_wakeup_ms(200), 200);
    n.tick(200);
    assert_eq!(n.next_wakeup_ms(999), 200 + crate::node::STATUS_INTERVAL_MS);
}

#[test]
fn shutdown_ends_online_radio_loaded_from_storage_and_ignores_offline_radio() {
    let medium = SimMedium::new();
    let settings = RadioSettings {
        frequency: 867_200_000,
        bandwidth: 125_000,
        spreading_factor: 7,
        coding_rate: 5,
        tx_power: 10,
    };
    let mut online = Node::new(
        medium.radio(),
        MemoryStorage::with_settings(settings),
        NodeConfig::default(),
        1,
    );
    online.boot(0);
    assert!(online.is_radio_online());
    assert!(online.radio().is_online());
    online.shutdown();
    assert!(!online.is_radio_online());
    assert!(!online.radio().is_online());
    assert_eq!(online.radio().end_calls(), 1);

    let mut offline = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 2);
    offline.shutdown();
    assert_eq!(offline.radio().end_calls(), 0);
}
