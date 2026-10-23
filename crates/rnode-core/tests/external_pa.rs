//! External power amplifiers (firmware `HAS_LORA_PA`): the gain-table mapping of
//! `rnode_core::pa`, and how the node applies it to `CMD_TXPOWER`, at start-up and to saved
//! settings.
//!
//! The golden values come from RNode_Firmware v1.86 (`56775c5`) itself, not from this crate:
//! `map_target_power_to_modem_output()` and `map_modem_output_to_target_power()`
//! (`Utilities.h:1314-1350`) and the Heltec V4 tables `PA_GC1109_VALUES` and `PA_KCT8103L_VALUES`
//! (`Boards.h:439-440`) were copied verbatim into a small C program, with `PA_GAIN_POINTS` 22 and
//! `PA_MAX_OUTPUT` 28, compiled and run for the inputs below. For `CMD_TXPOWER`, the program
//! followed the online path of `serial_callback()` (`RNode_Firmware.ino:857-881`): the clamp to
//! `PA_MAX_OUTPUT`, `setTXPower()`, which stores the mapped-back power in `lora_txp` for
//! `kiss_indicate_txpower()`, and the level `sx126x::setTxPower()` programs after clamping the
//! modem output to −9…22 dBm.

mod common;

use log::{Level, LevelFilter};

use common::log_capture::capture;
use common::{cmd, lines, plain, split_frames};
use rnode_core::kiss::*;
use rnode_core::pa::{ExternalPa, ExternalPaError, GC1109_GAIN_DB, MAX_OUTPUT_DBM};
use rnode_core::sim::Setter;
use rnode_core::{MemoryStorage, Node, NodeConfig, Radio, RadioSettings, SimMedium, SimRadio};

/// `(target dBm, modem output dBm, reported dBm)`: `map_target_power_to_modem_output(target)`,
/// then `map_modem_output_to_target_power()` of that.
type Mapping = (u8, i32, u8);

/// `(CMD_TXPOWER byte, echo, SX1262 level dBm)` with the radio online.
type TxPowerReply = (u8, u8, i8);

/// The mapping through the GC1109 for every target from 0 to 31 dBm.
const GC1109_MAPPING: [Mapping; 32] = [
    (0, -11, 11),
    (1, -10, 11),
    (2, -9, 11),
    (3, -8, 11),
    (4, -7, 11),
    (5, -6, 11),
    (6, -5, 11),
    (7, -4, 11),
    (8, -3, 11),
    (9, -2, 11),
    (10, -1, 11),
    (11, 0, 11),
    (12, 1, 12),
    (13, 2, 13),
    (14, 3, 14),
    (15, 4, 15),
    (16, 5, 16),
    (17, 6, 17),
    (18, 7, 18),
    (19, 8, 19),
    (20, 9, 20),
    (21, 10, 21),
    (22, 11, 22),
    (23, 12, 23),
    (24, 13, 24),
    (25, 14, 25),
    (26, 15, 26),
    (27, 17, 27),
    (28, 19, 28),
    (29, 22, 28),
    (30, 23, 28),
    (31, 24, 28),
];

/// As [`GC1109_MAPPING`], through the KCT8103L.
const KCT8103L_MAPPING: [Mapping; 32] = [
    (0, -13, 13),
    (1, -12, 13),
    (2, -11, 13),
    (3, -10, 13),
    (4, -9, 13),
    (5, -8, 13),
    (6, -7, 13),
    (7, -6, 13),
    (8, -5, 13),
    (9, -4, 13),
    (10, -3, 13),
    (11, -2, 13),
    (12, -1, 13),
    (13, 0, 13),
    (14, 1, 14),
    (15, 2, 15),
    (16, 3, 16),
    (17, 4, 17),
    (18, 5, 18),
    (19, 6, 19),
    (20, 7, 20),
    (21, 8, 21),
    (22, 9, 22),
    (23, 10, 23),
    (24, 11, 24),
    (25, 12, 25),
    (26, 13, 26),
    (27, 15, 27),
    (28, 17, 28),
    (29, 22, 28),
    (30, 23, 28),
    (31, 24, 28),
];

/// The GC1109's replies for the bytes 0 to 28; every byte from 29 to 254 gives what 28 gives.
const GC1109_TXPOWER: [TxPowerReply; 29] = [
    (0, 11, -9),
    (1, 11, -9),
    (2, 11, -9),
    (3, 11, -8),
    (4, 11, -7),
    (5, 11, -6),
    (6, 11, -5),
    (7, 11, -4),
    (8, 11, -3),
    (9, 11, -2),
    (10, 11, -1),
    (11, 11, 0),
    (12, 12, 1),
    (13, 13, 2),
    (14, 14, 3),
    (15, 15, 4),
    (16, 16, 5),
    (17, 17, 6),
    (18, 18, 7),
    (19, 19, 8),
    (20, 20, 9),
    (21, 21, 10),
    (22, 22, 11),
    (23, 23, 12),
    (24, 24, 13),
    (25, 25, 14),
    (26, 26, 15),
    (27, 27, 17),
    (28, 28, 19),
];

/// As [`GC1109_TXPOWER`], through the KCT8103L.
const KCT8103L_TXPOWER: [TxPowerReply; 29] = [
    (0, 13, -9),
    (1, 13, -9),
    (2, 13, -9),
    (3, 13, -9),
    (4, 13, -9),
    (5, 13, -8),
    (6, 13, -7),
    (7, 13, -6),
    (8, 13, -5),
    (9, 13, -4),
    (10, 13, -3),
    (11, 13, -2),
    (12, 13, -1),
    (13, 13, 0),
    (14, 14, 1),
    (15, 15, 2),
    (16, 16, 3),
    (17, 17, 4),
    (18, 18, 5),
    (19, 19, 6),
    (20, 20, 7),
    (21, 21, 8),
    (22, 22, 9),
    (23, 23, 10),
    (24, 24, 11),
    (25, 25, 12),
    (26, 26, 13),
    (27, 27, 15),
    (28, 28, 17),
];

/// A Heltec V4 front end, with its goldens.
struct HeltecV4Pa {
    name: &'static str,
    pa: ExternalPa,
    mapping: &'static [Mapping],
    txpower: &'static [TxPowerReply],
}

fn heltec_v4_pas() -> [HeltecV4Pa; 2] {
    [
        HeltecV4Pa {
            name: "GC1109",
            pa: ExternalPa::gc1109(),
            mapping: &GC1109_MAPPING,
            txpower: &GC1109_TXPOWER,
        },
        HeltecV4Pa {
            name: "KCT8103L",
            pa: ExternalPa::kct8103l(),
            mapping: &KCT8103L_MAPPING,
            txpower: &KCT8103L_TXPOWER,
        },
    ]
}

/// The table of issue #73: target → modem output → reported power.
#[test]
fn issue_73_table() {
    let gc1109: [Mapping; 8] = [
        (0, -11, 11),
        (2, -9, 11),
        (10, -1, 11),
        (13, 2, 13),
        (17, 6, 17),
        (22, 11, 22),
        (27, 17, 27),
        (28, 19, 28),
    ];
    let kct8103l: [Mapping; 8] = [
        (0, -13, 13),
        (2, -11, 13),
        (10, -3, 13),
        (13, 0, 13),
        (17, 4, 17),
        (22, 9, 22),
        (27, 15, 27),
        (28, 17, 28),
    ];
    for (name, pa, rows) in [
        ("GC1109", ExternalPa::gc1109(), gc1109),
        ("KCT8103L", ExternalPa::kct8103l(), kct8103l),
    ] {
        for (target, modem, reported) in rows {
            assert_eq!(pa.modem_output_dbm(target), modem, "{name} {target} dBm");
            assert_eq!(pa.target_power_dbm(modem), reported, "{name} {target} dBm");
        }
    }
}

#[test]
fn every_target_from_0_to_28_dbm_maps_as_in_the_firmware() {
    for HeltecV4Pa { name, pa, mapping, .. } in heltec_v4_pas() {
        for &(target, modem, reported) in mapping.iter().filter(|(target, ..)| *target <= 28) {
            assert_eq!(pa.modem_output_dbm(target), modem, "{name} {target} dBm");
            assert_eq!(pa.target_power_dbm(modem), reported, "{name} {target} dBm");
        }
    }
}

/// Above the table's highest output, 28 dBm, the firmware extrapolates past the last entry with
/// its gain (7 dB), and maps back to that entry. `CMD_TXPOWER` never asks for this, as it clamps
/// to `PA_MAX_OUTPUT` first.
#[test]
fn targets_above_the_table_are_extrapolated_as_in_the_firmware() {
    for HeltecV4Pa { name, pa, mapping, .. } in heltec_v4_pas() {
        for &(target, modem, reported) in mapping.iter().filter(|(target, ..)| *target > 28) {
            assert_eq!(pa.modem_output_dbm(target), modem, "{name} {target} dBm");
            assert_eq!(pa.target_power_dbm(modem), reported, "{name} {target} dBm");
        }
    }
}

/// `map_modem_output_to_target_power()` for modem outputs from −12 to 25 dBm: below 0 and above 21
/// it uses the first and the last entry.
#[test]
fn mapping_back_clamps_the_modem_output_to_the_table() {
    let gc1109 = [
        11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
        26, 27, 27, 28, 28, 28, 28, 28, 28, 28,
    ];
    let kct8103l = [
        13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 26, 27,
        27, 28, 28, 28, 28, 28, 28, 28, 28, 28,
    ];
    for (name, pa, golden) in [
        ("GC1109", ExternalPa::gc1109(), gc1109),
        ("KCT8103L", ExternalPa::kct8103l(), kct8103l),
    ] {
        let reported: Vec<u8> = (-12..=25).map(|modem| pa.target_power_dbm(modem)).collect();
        assert_eq!(reported, golden, "{name}");
    }
}

/// The firmware's mapping on tables it does not ship, run the same way with other
/// `PA_GAIN_POINTS`: a single entry; a gain that grows (outputs 10, 13, 14 dBm), whose gap the
/// search fills with negative modem outputs; and an output that falls (11, 10 dBm), where an
/// earlier output overshoots a target the next one meets.
#[test]
fn other_tables_keep_the_firmware_quirks() {
    // (modem output dBm, reported dBm) for the targets 0 to 20 dBm.
    type Golden = [(i32, u8); 21];
    let cases: [(&[u8], Golden); 3] = [
        (
            &[8],
            [
                (-8, 8),
                (-7, 8),
                (-6, 8),
                (-5, 8),
                (-4, 8),
                (-3, 8),
                (-2, 8),
                (-1, 8),
                (0, 8),
                (1, 8),
                (2, 8),
                (3, 8),
                (4, 8),
                (5, 8),
                (6, 8),
                (7, 8),
                (8, 8),
                (9, 8),
                (10, 8),
                (11, 8),
                (12, 8),
            ],
        ),
        (
            &[10, 12, 12],
            [
                (-10, 10),
                (-9, 10),
                (-8, 10),
                (-7, 10),
                (-6, 10),
                (-5, 10),
                (-4, 10),
                (-3, 10),
                (-2, 10),
                (-1, 10),
                (0, 10),
                (-2, 10),
                (-1, 10),
                (1, 13),
                (2, 14),
                (3, 14),
                (4, 14),
                (5, 14),
                (6, 14),
                (7, 14),
                (8, 14),
            ],
        ),
        (
            &[11, 9],
            [
                (-11, 11),
                (-10, 11),
                (-9, 11),
                (-8, 11),
                (-7, 11),
                (-6, 11),
                (-5, 11),
                (-4, 11),
                (-3, 11),
                (-2, 11),
                (-1, 11),
                (0, 11),
                (3, 10),
                (4, 10),
                (5, 10),
                (6, 10),
                (7, 10),
                (8, 10),
                (9, 10),
                (10, 10),
                (11, 10),
            ],
        ),
    ];
    for (table, golden) in cases {
        let pa = ExternalPa::new(table, 20).unwrap();
        for (target, (modem, reported)) in (0..).zip(golden) {
            assert_eq!(pa.modem_output_dbm(target), modem, "{table:?} {target} dBm");
            assert_eq!(pa.target_power_dbm(modem), reported, "{table:?} {target} dBm");
        }
    }
}

/// A table must have an entry, as the firmware's search returns its −9 dBm start value without
/// one, and nothing it reports may reach 128 dBm, which `Node::boot()` refuses in saved settings.
#[test]
fn new_refuses_tables_whose_power_the_node_could_not_report() {
    assert_eq!(ExternalPa::new([], 20), Err(ExternalPaError::EmptyGainTable));
    assert_eq!(
        ExternalPa::new([11, 127], 28),
        Err(ExternalPaError::OutputTooHigh {
            modem_output_dbm: 1,
            output_dbm: 128
        })
    );
    assert_eq!(
        ExternalPa::new([0; 129], 28),
        Err(ExternalPaError::OutputTooHigh {
            modem_output_dbm: 128,
            output_dbm: 128
        })
    );
    assert_eq!(ExternalPa::new([11], 128), Err(ExternalPaError::MaxOutputTooHigh(128)));
    // At the limit.
    let longest = ExternalPa::new([0; 128], MAX_OUTPUT_DBM).unwrap();
    assert_eq!(longest.target_power_dbm(i32::MAX), MAX_OUTPUT_DBM);
    assert_eq!(longest.modem_output_dbm(MAX_OUTPUT_DBM), 127);
    let steepest = ExternalPa::new([11, 126], MAX_OUTPUT_DBM).unwrap();
    assert_eq!(steepest.target_power_dbm(1), MAX_OUTPUT_DBM);
}

/// The settings `init()` gives, through a PA, on a simulated radio that clamps the TX power to
/// the SX1262's −9…22 dBm, as `sx126x::setTxPower()` does.
fn pa_node(pa: ExternalPa, max_tx_power: u8) -> Node<SimRadio, MemoryStorage> {
    let medium = SimMedium::new();
    let mut radio = medium.radio();
    radio.set_tx_power_range(-9..=22);
    let config = NodeConfig {
        max_tx_power,
        external_pa: Some(pa),
        ..NodeConfig::default()
    };
    Node::new(radio, MemoryStorage::default(), config, 1)
}

/// The commands `init()` sends, with the TX power `txp`.
fn configure(txp: u8) -> Vec<u8> {
    [
        cmd(CMD_FREQUENCY, &867_200_000u32.to_be_bytes()),
        cmd(CMD_BANDWIDTH, &125_000u32.to_be_bytes()),
        plain(CMD_TXPOWER, &[txp]),
        plain(CMD_SF, &[7]),
        plain(CMD_CR, &[5]),
    ]
    .concat()
}

/// Bring `n` online with the TX power `txp`, and return what the node sent the host.
fn bring_online(n: &mut Node<SimRadio, MemoryStorage>, txp: u8) -> Vec<u8> {
    n.handle_host_input(&configure(txp), 0);
    n.handle_host_input(&plain(CMD_RADIO_STATE, &[RADIO_STATE_ON]), 0);
    n.take_host_output()
}

/// The TX power the node reports to a query.
fn queried_tx_power(n: &mut Node<SimRadio, MemoryStorage>) -> Vec<u8> {
    n.handle_host_input(&plain(CMD_TXPOWER, &[0xFF]), 0);
    n.take_host_output()
}

/// Firmware `serial_callback()` with `HAS_LORA_PA`: `CMD_TXPOWER` is clamped to `PA_MAX_OUTPUT`,
/// `setTXPower()` programs the mapped modem output, and the echo is the mapped-back power.
#[test]
fn txpower_through_a_pa_echoes_what_the_firmware_reports() {
    for HeltecV4Pa { name, pa, txpower, .. } in heltec_v4_pas() {
        let mut n = pa_node(pa, 28);
        bring_online(&mut n, 2);
        // Every byte a host can send: like the firmware, the node takes the byte after
        // CMD_TXPOWER as it is, and FEND (0xC0) would end the frame instead.
        let replies = txpower.iter().copied().chain(
            (29..=0xFE)
                .filter(|&byte| byte != FEND)
                .map(|byte| (byte, txpower[28].1, txpower[28].2)),
        );
        for (byte, echo, level) in replies {
            n.handle_host_input(&plain(CMD_TXPOWER, &[byte]), 0);
            assert_eq!(n.take_host_output(), plain(CMD_TXPOWER, &[echo]), "{name} {byte}");
            assert_eq!(n.radio().tx_power(), level, "{name} {byte}");
            assert_eq!(queried_tx_power(&mut n), plain(CMD_TXPOWER, &[echo]), "{name} {byte}");
        }
    }
}

/// The same with a radio that takes every modem output as it is: the node maps back what the
/// radio reads back, the firmware what it asked for, and the two agree.
#[test]
fn txpower_through_a_pa_does_not_depend_on_the_radio_clamping() {
    for HeltecV4Pa {
        name,
        pa,
        mapping,
        txpower,
    } in heltec_v4_pas()
    {
        let medium = SimMedium::new();
        let config = NodeConfig {
            max_tx_power: 28,
            external_pa: Some(pa),
            ..NodeConfig::default()
        };
        let mut n = Node::new(medium.radio(), MemoryStorage::default(), config, 1);
        bring_online(&mut n, 2);
        for (&(byte, echo, _), &(_, modem, _)) in txpower.iter().zip(mapping) {
            n.handle_host_input(&plain(CMD_TXPOWER, &[byte]), 0);
            assert_eq!(n.take_host_output(), plain(CMD_TXPOWER, &[echo]), "{name} {byte}");
            assert_eq!(i32::from(n.radio().tx_power()), modem, "{name} {byte}");
        }
    }
}

/// `setTXPower()` acts only while the radio is online: before, the echo is the clamped request.
/// Turning the radio on maps it, as `startRadio()` calls `setTXPower()`.
#[test]
fn txpower_while_offline_echoes_the_clamped_request_until_the_radio_starts() {
    let mut n = pa_node(ExternalPa::gc1109(), 28);
    for (byte, echo) in [(0, 0), (10, 10), (17, 17), (28, 28), (29, 28), (0xFE, 28)] {
        n.handle_host_input(&plain(CMD_TXPOWER, &[byte]), 0);
        assert_eq!(n.take_host_output(), plain(CMD_TXPOWER, &[echo]), "{byte}");
    }
    assert_eq!(n.radio().tx_power(), 2, "no setter is called while offline");

    n.handle_host_input(&configure(5), 0);
    assert_eq!(
        n.take_host_output(),
        configure(5),
        "offline, each request is echoed as sent"
    );
    n.handle_host_input(&plain(CMD_RADIO_STATE, &[RADIO_STATE_ON]), 0);
    let replies = split_frames(&n.take_host_output());
    assert!(
        replies.contains(&(CMD_RADIO_STATE, vec![RADIO_STATE_ON])),
        "{replies:?}"
    );
    // 5 dBm needs a modem output of −6 dBm, reported as the GC1109's minimum of 11 dBm.
    assert_eq!(n.radio().tx_power(), -6);
    assert_eq!(queried_tx_power(&mut n), plain(CMD_TXPOWER, &[11]));
}

/// The lower of `NodeConfig::max_tx_power` and the PA's maximum output is the ceiling.
#[test]
fn the_lower_of_max_tx_power_and_the_pa_maximum_applies() {
    for (max_tx_power, echo, level) in [(20, 20, 9), (28, 28, 19), (30, 28, 19), (0x7F, 28, 19)] {
        let mut n = pa_node(ExternalPa::gc1109(), max_tx_power);
        bring_online(&mut n, 2);
        n.handle_host_input(&plain(CMD_TXPOWER, &[0xFE]), 0);
        assert_eq!(n.take_host_output(), plain(CMD_TXPOWER, &[echo]), "{max_tx_power}");
        assert_eq!(n.radio().tx_power(), level, "{max_tx_power}");
    }
    // A custom PA with a lower maximum.
    let mut n = pa_node(ExternalPa::new(GC1109_GAIN_DB, 24).unwrap(), 28);
    bring_online(&mut n, 2);
    n.handle_host_input(&plain(CMD_TXPOWER, &[28]), 0);
    assert_eq!(n.take_host_output(), plain(CMD_TXPOWER, &[24]));
    assert_eq!(n.radio().tx_power(), 13);
}

/// Saved settings start the radio through the PA, and `CMD_CONF_SAVE` saves the mapped-back
/// power, as `eeprom_conf_save()` saves `lora_txp` after `setTXPower()` replaced it.
#[test]
fn saved_settings_go_through_the_pa_and_save_the_reported_power() {
    let saved = |tx_power| RadioSettings {
        frequency: 867_200_000,
        bandwidth: 125_000,
        spreading_factor: 7,
        coding_rate: 5,
        tx_power,
    };
    for (tx_power, level, reported) in [(0, -9, 11), (17, 6, 17), (28, 19, 28), (30, 19, 28), (0x7F, 19, 28)] {
        let medium = SimMedium::new();
        let mut radio = medium.radio();
        radio.set_tx_power_range(-9..=22);
        let config = NodeConfig {
            max_tx_power: 28,
            external_pa: Some(ExternalPa::gc1109()),
            ..NodeConfig::default()
        };
        let mut n = Node::new(radio, MemoryStorage::with_settings(saved(tx_power)), config, 1);
        n.boot(0);
        assert!(n.is_radio_online(), "{tx_power}");
        assert_eq!(n.radio().tx_power(), level, "{tx_power}");
        n.take_host_output();
        assert_eq!(queried_tx_power(&mut n), plain(CMD_TXPOWER, &[reported]), "{tx_power}");
        n.handle_host_input(&plain(CMD_CONF_SAVE, &[0]), 0);
        assert_eq!(n.storage().settings(), Some(saved(reported)), "{tx_power}");
    }
}

/// A failed `set_tx_power()` (#74) keeps the modem output the radio last accepted, and the node
/// reports the power that output gives; the log names the modem outputs.
#[test]
fn a_failed_set_tx_power_keeps_the_last_accepted_modem_output() {
    let mut n = pa_node(ExternalPa::gc1109(), 28);
    bring_online(&mut n, 17);
    assert_eq!(n.radio().tx_power(), 6);
    n.radio_mut().inject_setter_errors(Setter::TxPower, 1);
    let cap = capture(LevelFilter::Info);
    n.handle_host_input(&plain(CMD_TXPOWER, &[28]), 0);
    assert_eq!(n.take_host_output(), plain(CMD_TXPOWER, &[17]));
    assert_eq!(n.radio().tx_power(), 6, "restored");
    assert_eq!(
        lines(&cap.take()),
        [(
            Level::Warn,
            "radio set_tx_power(19 dBm) failed: simulated set_tx_power error; keeping 6 dBm"
        )]
    );

    // Below the PA's minimum the modem output the radio accepted is −9 dBm, not the −11 dBm the
    // table asked for: that is what a failure restores.
    n.handle_host_input(&plain(CMD_TXPOWER, &[0]), 0);
    assert_eq!(n.take_host_output(), plain(CMD_TXPOWER, &[11]));
    n.radio_mut().inject_setter_errors(Setter::TxPower, 1);
    n.handle_host_input(&plain(CMD_TXPOWER, &[22]), 0);
    assert_eq!(n.take_host_output(), plain(CMD_TXPOWER, &[11]));
    assert_eq!(n.radio().tx_power(), -9);
    assert_eq!(
        lines(&cap.take()),
        [(
            Level::Warn,
            "radio set_tx_power(11 dBm) failed: simulated set_tx_power error; keeping -9 dBm"
        )]
    );
}

/// When `set_tx_power()` fails as the radio comes online, the modem keeps the 2 dBm `begin()`
/// programmed, which the GC1109 turns into 13 dBm.
#[test]
fn a_set_tx_power_failing_at_start_up_reports_what_begin_programmed_after_the_pa() {
    let mut n = pa_node(ExternalPa::gc1109(), 28);
    n.radio_mut().inject_setter_errors(Setter::TxPower, 1);
    let cap = capture(LevelFilter::Info);
    bring_online(&mut n, 22);
    assert_eq!(n.radio().tx_power(), 2);
    assert_eq!(
        lines(&cap.take()),
        [
            (
                Level::Warn,
                "radio set_tx_power(11 dBm) failed: simulated set_tx_power error; keeping 2 dBm"
            ),
            (
                Level::Info,
                "radio online (host request): 867.2 MHz, 125 kHz, SF7, CR 4/5, 13 dBm"
            ),
        ]
    );
    assert_eq!(queried_tx_power(&mut n), plain(CMD_TXPOWER, &[13]));
}

/// The settings lines show the power after the PA; a debug line shows the modem output.
#[test]
fn the_log_shows_the_power_after_the_pa_and_the_modem_output_at_debug() {
    let mut n = pa_node(ExternalPa::kct8103l(), 28);
    let cap = capture(LevelFilter::Debug);
    bring_online(&mut n, 17);
    n.handle_host_input(&plain(CMD_TXPOWER, &[0]), 0);
    n.tick(200);
    assert_eq!(
        lines(&cap.take()),
        [
            (
                Level::Debug,
                "TX power 17 dBm through the external PA: modem output 4 dBm, reported as 17 dBm"
            ),
            (
                Level::Info,
                "radio online (host request): 867.2 MHz, 125 kHz, SF7, CR 4/5, 17 dBm"
            ),
            (
                Level::Debug,
                "TX power 0 dBm through the external PA: modem output -13 dBm, -9 dBm in use, reported as 13 dBm"
            ),
            (
                Level::Info,
                "radio settings changed: 867.2 MHz, 125 kHz, SF7, CR 4/5, 13 dBm"
            ),
        ]
    );
}
