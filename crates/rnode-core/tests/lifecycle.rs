//! The node lifecycle as the daemon drives it: boot from saved settings (including settings it
//! refuses), wake-up scheduling and shutdown.

mod common;

use log::{Level, LevelFilter};

use common::log_capture::capture;
use common::{cmd, init, plain};
use rnode_core::kiss::*;
use rnode_core::{MemoryStorage, Node, NodeConfig, Radio, RadioSettings, SimMedium, SimRadio};
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

const SAVED: RadioSettings = RadioSettings {
    frequency: 867_200_000,
    bandwidth: 125_000,
    spreading_factor: 7,
    coding_rate: 5,
    tx_power: 10,
};

/// The node's replies to queries for every setting `CMD_CONF_SAVE` keeps.
fn settings_replies(n: &mut Node<SimRadio, MemoryStorage>) -> Vec<u8> {
    let mut query = Vec::new();
    query.extend(cmd(CMD_FREQUENCY, &[0, 0, 0, 0]));
    query.extend(cmd(CMD_BANDWIDTH, &[0, 0, 0, 0]));
    query.extend(plain(CMD_TXPOWER, &[0xFF]));
    query.extend(plain(CMD_SF, &[0xFF]));
    query.extend(plain(CMD_CR, &[0xFF]));
    n.handle_host_input(&query, 0);
    n.take_host_output()
}

/// #75: earlier builds read `CMD_TXPOWER` as signed and saved −9…−2 dBm as 247…254. Read as
/// unsigned, such a byte would start the radio at 22 dBm, so the node ignores the saved settings,
/// warns on every load, and waits for a host as if nothing were saved.
#[test]
fn boot_ignores_saved_settings_with_a_tx_power_from_128_to_254() {
    let mut blank = Node::new(
        SimMedium::new().radio(),
        MemoryStorage::default(),
        NodeConfig::default(),
        1,
    );
    blank.boot(0);
    let unset = settings_replies(&mut blank);

    for tx_power in [0xF7, 0xFE, 0x80] {
        let saved = RadioSettings { tx_power, ..SAVED };
        let medium = SimMedium::new();
        let mut n = Node::new(
            medium.radio(),
            MemoryStorage::with_settings(saved),
            NodeConfig::default(),
            1,
        );
        let cap = capture(LevelFilter::Trace);
        n.boot(0);
        let warning = format!(
            "ignoring saved radio settings: TX power {tx_power} is out of range (earlier builds saved -9 to -2 \
             dBm as 247 to 254); the radio stays off until a host configures it"
        );
        let records = cap.take();
        assert_eq!(
            records
                .iter()
                .map(|r| (r.level, r.message.as_str()))
                .collect::<Vec<_>>(),
            [(Level::Warn, warning.as_str())]
        );
        assert!(!n.is_radio_online(), "{tx_power}");
        assert!(!n.radio().is_online(), "{tx_power}");
        assert!(n.take_host_output().is_empty());
        // Nothing was applied, and the file is left for the host to replace.
        assert_eq!(settings_replies(&mut n), unset, "{tx_power}");
        assert_eq!(n.storage().settings(), Some(saved));

        // A reset loads the settings again, with the same result.
        n.handle_host_input(&plain(CMD_RESET, &[CMD_RESET_BYTE]), 0);
        let warnings: Vec<_> = cap.take().into_iter().filter(|r| r.level <= Level::Warn).collect();
        assert_eq!(
            warnings.iter().map(|r| r.message.as_str()).collect::<Vec<_>>(),
            [warning.as_str()]
        );
        assert!(!n.is_radio_online());

        // A host configures the radio as usual, and what it saves comes up next time.
        init(&mut n);
        assert!(n.is_radio_online());
        n.handle_host_input(&plain(CMD_CONF_SAVE, &[0]), 0);
        assert_eq!(n.storage().settings().map(|s| s.tx_power), Some(14));
        n.handle_host_input(&plain(CMD_RESET, &[CMD_RESET_BYTE]), 0);
        assert!(n.is_radio_online());
        assert_eq!(n.radio().tx_power(), 14);
        assert!(cap.take().iter().all(|r| r.level > Level::Warn));
    }
}

/// Saved powers outside 128…254: up to 127 dBm they are clamped to the ceiling, as a host's request
/// would be; 0xFF means no power was saved and keeps the radio locked, as in firmware
/// `update_radio_lock()` after `eeprom_conf_load()`.
#[test]
fn boot_clamps_saved_powers_up_to_127_and_leaves_0xff_unset() {
    for (tx_power, applied) in [(0, 0), (22, 22), (23, 22), (0x7F, 22)] {
        let saved = RadioSettings { tx_power, ..SAVED };
        let mut n = Node::new(
            SimMedium::new().radio(),
            MemoryStorage::with_settings(saved),
            NodeConfig::default(),
            1,
        );
        n.boot(0);
        assert!(n.is_radio_online(), "{tx_power}");
        assert_eq!(n.radio().tx_power(), applied, "{tx_power}");
    }

    let saved = RadioSettings {
        tx_power: 0xFF,
        ..SAVED
    };
    let mut n = Node::new(
        SimMedium::new().radio(),
        MemoryStorage::with_settings(saved),
        NodeConfig::default(),
        1,
    );
    let cap = capture(LevelFilter::Warn);
    n.boot(0);
    assert!(cap.take().is_empty());
    assert!(!n.is_radio_online());
    assert!(!n.radio().is_online());
    // The rest of the settings are loaded; the host only has to set the power.
    n.handle_host_input(&cmd(CMD_FREQUENCY, &[0, 0, 0, 0]), 0);
    assert_eq!(n.take_host_output(), cmd(CMD_FREQUENCY, &SAVED.frequency.to_be_bytes()));
    n.handle_host_input(
        &[plain(CMD_TXPOWER, &[2]), plain(CMD_RADIO_STATE, &[RADIO_STATE_ON])].concat(),
        0,
    );
    assert!(n.is_radio_online());
    assert_eq!(n.radio().tx_power(), 2);
}
