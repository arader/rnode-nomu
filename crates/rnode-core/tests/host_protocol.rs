//! The KISS host protocol: replies to queries and configuration commands, and which host data
//! frames are admitted to the TX queue.

mod common;

use common::{cmd, data, init, node, plain, split_frames};
use rnode_core::kiss::*;
use rnode_core::{DeviceIdentity, MemoryStorage, Node, NodeConfig, Radio, SimMedium};
// The tests below refer to these modules as `crate::phy` and `crate::rng`.
use rnode_core::{phy, rng};

#[test]
fn golden_identity_status_and_plain_escaped_replies() {
    let mut n = node(7);
    let id = DeviceIdentity::default();
    let mut input = Vec::new();
    input.extend(plain(CMD_DETECT, &[DETECT_REQ]));
    input.extend(plain(CMD_FW_VERSION, &[0]));
    input.extend(plain(CMD_PLATFORM, &[0]));
    input.extend(plain(CMD_MCU, &[0]));
    input.extend(plain(CMD_BOARD, &[0]));
    input.extend(cmd(CMD_FREQUENCY, &[0, 0, 0, 0]));
    input.extend(cmd(CMD_BANDWIDTH, &[0, 0, 0, 0]));
    input.extend(plain(CMD_TXPOWER, &[0xFF]));
    input.extend(plain(CMD_SF, &[0xFF]));
    input.extend(plain(CMD_CR, &[0xFF]));
    input.extend(plain(CMD_IMPLICIT, &[7]));
    input.extend(cmd(CMD_ST_ALOCK, &[0xC0, 0xDB]));
    input.extend(cmd(CMD_LT_ALOCK, &[0, 1]));
    input.extend(plain(CMD_PROMISC, &[2]));
    input.extend(plain(CMD_READY, &[0]));
    input.extend(plain(CMD_RADIO_LOCK, &[0]));
    input.extend(plain(CMD_RADIO_STATE, &[0xFF]));
    input.extend(plain(CMD_STAT_RX, &[0]));
    input.extend(plain(CMD_STAT_TX, &[0]));
    input.extend(plain(CMD_STAT_RSSI, &[0]));
    input.extend(plain(CMD_STAT_SNR, &[0]));
    n.handle_host_input(&input, 0);
    let out = n.take_host_output();
    let expected = [
        plain(CMD_DETECT, &[DETECT_RESP]),
        plain(CMD_FW_VERSION, &[id.fw_major, id.fw_minor]),
        plain(CMD_PLATFORM, &[id.platform]),
        plain(CMD_MCU, &[id.mcu]),
        plain(CMD_BOARD, &[id.board]),
        cmd(CMD_FREQUENCY, &[0, 0, 0, 0]),
        cmd(CMD_BANDWIDTH, &[0, 0, 0, 0]),
        plain(CMD_TXPOWER, &[0xFF]),
        plain(CMD_SF, &[0]),
        plain(CMD_CR, &[5]),
        plain(CMD_IMPLICIT, &[7]),
        cmd(CMD_ST_ALOCK, &[0, 0]),
        cmd(CMD_LT_ALOCK, &[0, 1]),
        plain(CMD_PROMISC, &[0]),
        plain(CMD_READY, &[1]),
        plain(CMD_RADIO_LOCK, &[1]),
        plain(CMD_RADIO_STATE, &[0]),
        cmd(CMD_STAT_RX, &[0, 0, 0, 0]),
        cmd(CMD_STAT_TX, &[0, 0, 0, 0]),
        cmd(CMD_STAT_RSSI, &[121]),
        cmd(CMD_STAT_SNR, &[0x80]),
    ]
    .concat();
    assert_eq!(out, expected);
}

#[test]
fn random_reply_escapes_frame_delimiter_bytes() {
    let mut n = node(164);
    n.handle_host_input(&plain(CMD_RANDOM, &[0]), 0);
    assert_eq!(n.take_host_output(), vec![FEND, CMD_RANDOM, FESC, TFEND, FEND]);
}

#[test]
fn scripted_reticulum_init_sequence_validates_state() {
    let mut n = node(1);
    let out = init(&mut n);
    let frames = split_frames(&out);
    assert_eq!(frames[0], (CMD_FREQUENCY, 867_200_000u32.to_be_bytes().to_vec()));
    assert_eq!(frames[1], (CMD_BANDWIDTH, 125_000u32.to_be_bytes().to_vec()));
    assert_eq!(frames[2], (CMD_TXPOWER, vec![14]));
    assert_eq!(frames[3], (CMD_SF, vec![7]));
    assert_eq!(frames[4], (CMD_CR, vec![5]));
    assert!(frames.iter().any(|f| *f == (CMD_RADIO_STATE, vec![1])));
    assert!(frames.iter().any(|(c, _)| *c == CMD_STAT_PHYPRM));
    assert!(n.is_radio_online());
    assert_eq!(n.radio().frequency(), 867_200_000);
    assert_eq!(n.radio().bandwidth(), 125_000);
    assert_eq!(n.radio().tx_power(), 14);
    assert_eq!(n.radio().spreading_factor(), 7);
}

#[test]
fn queue_limits_ignore_empty_oversize_and_full_without_error() {
    let medium = SimMedium::new();
    let mut n = Node::new(
        medium.radio(),
        MemoryStorage::default(),
        NodeConfig {
            queue_max_packets: 2,
            queue_max_bytes: 5,
            ..NodeConfig::default()
        },
        1,
    );
    let mut input = Vec::new();
    input.extend(data(&[]));
    input.extend(data(&[1, 2, 3]));
    input.extend(data(&[4, 5]));
    input.extend(data(&[6]));
    n.handle_host_input(&input, 0);
    assert_eq!(n.queued_packets(), 2);
    assert_eq!(n.queued_bytes(), 5);
    n.handle_host_input(&plain(CMD_READY, &[0]), 0);
    assert_eq!(n.take_host_output(), plain(CMD_READY, &[0]));
}

#[test]
fn oversize_kiss_frame_is_ignored_with_queue_capacity_available() {
    let medium = SimMedium::new();
    let mut n = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 1);
    n.handle_host_input(&data(&vec![7; 509]), 0);
    assert_eq!(n.queued_packets(), 0);
    assert_eq!(n.queued_bytes(), 0);
}

/// Firmware `serial_callback()` reads the `CMD_TXPOWER` byte as unsigned (`int txp = sbyte;`) and
/// clamps it to 22 dBm on an SX1262 without a PA; `kiss_indicate_txpower()` then reports
/// `lora_txp`. These are the replies of an RNode, with the radio off and on (#75).
#[test]
fn txpower_reads_the_byte_unsigned_and_clamps_like_the_firmware() {
    let golden = [(0x00, 0), (0x16, 22), (0x17, 22), (0x80, 22), (0xFE, 22)];
    let mut off = node(1);
    let mut on = node(1);
    init(&mut on);
    for (request, reply) in golden {
        off.handle_host_input(&plain(CMD_TXPOWER, &[request]), 0);
        assert_eq!(off.take_host_output(), plain(CMD_TXPOWER, &[reply]), "{request:#04X}");
        on.handle_host_input(&plain(CMD_TXPOWER, &[request]), 0);
        assert_eq!(on.take_host_output(), plain(CMD_TXPOWER, &[reply]), "{request:#04X}");
        assert_eq!(on.radio().tx_power(), reply as i8, "{request:#04X}");
    }
    // 0xFF asks for the power in use.
    on.handle_host_input(&plain(CMD_TXPOWER, &[0xFF]), 0);
    assert_eq!(on.take_host_output(), plain(CMD_TXPOWER, &[22]));

    // A lower ceiling takes the place of 22.
    let medium = SimMedium::new();
    let config = NodeConfig {
        max_tx_power: 10,
        ..NodeConfig::default()
    };
    let mut capped = Node::new(medium.radio(), MemoryStorage::default(), config, 1);
    init(&mut capped);
    for (request, reply) in [(0x00, 0), (0x0A, 10), (0x0B, 10), (0x16, 10), (0x80, 10), (0xFE, 10)] {
        capped.handle_host_input(&plain(CMD_TXPOWER, &[request]), 0);
        assert_eq!(
            capped.take_host_output(),
            plain(CMD_TXPOWER, &[reply]),
            "{request:#04X}"
        );
        assert_eq!(capped.radio().tx_power(), reply as i8, "{request:#04X}");
    }
}

#[test]
fn leave_resets_packet_rssi_snr_only_on_ff() {
    let medium = SimMedium::new();
    let mut n = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 1);
    init(&mut n);
    n.take_host_output();
    n.radio().inject_rx(vec![0x10, 1, 2, 3], -42, 0x20);
    n.service_radio(0);
    n.take_host_output();

    let mut query = Vec::new();
    query.extend(plain(CMD_LEAVE, &[0x00]));
    query.extend(plain(CMD_STAT_RSSI, &[0]));
    query.extend(plain(CMD_STAT_SNR, &[0]));
    n.handle_host_input(&query, 0);
    assert_eq!(
        split_frames(&n.take_host_output()),
        vec![(CMD_STAT_RSSI, vec![115]), (CMD_STAT_SNR, vec![0x20])]
    );

    let mut leave = Vec::new();
    leave.extend(plain(CMD_LEAVE, &[0xFF]));
    leave.extend(plain(CMD_STAT_RSSI, &[0]));
    leave.extend(plain(CMD_STAT_SNR, &[0]));
    n.handle_host_input(&leave, 0);
    assert_eq!(
        split_frames(&n.take_host_output()),
        vec![(CMD_STAT_RSSI, vec![121]), (CMD_STAT_SNR, vec![0x80])]
    );
}

#[test]
fn online_config_commands_retune_radio_and_reply_with_applied_values() {
    let medium = SimMedium::new();
    let mut n = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 1);
    init(&mut n);
    n.take_host_output();

    let mut input = Vec::new();
    input.extend(cmd(CMD_FREQUENCY, &868_100_000u32.to_be_bytes()));
    input.extend(cmd(CMD_BANDWIDTH, &124_000u32.to_be_bytes())); // SimRadio quantizes up to 125 kHz.
    input.extend(plain(CMD_TXPOWER, &[30])); // clamped by NodeConfig::max_tx_power.
    input.extend(plain(CMD_SF, &[8]));
    input.extend(plain(CMD_CR, &[9]));
    n.handle_host_input(&input, 10);

    let frames = split_frames(&n.take_host_output());
    assert_eq!(frames[0], (CMD_FREQUENCY, 868_100_000u32.to_be_bytes().to_vec()));
    let phy_bw = crate::phy::PhyParams::new(crate::phy::Modulation {
        bandwidth_hz: 125_000,
        spreading_factor: 7,
        coding_rate: 5,
    })
    .stats_payload()
    .to_vec();
    let phy_sf = crate::phy::PhyParams::new(crate::phy::Modulation {
        bandwidth_hz: 125_000,
        spreading_factor: 8,
        coding_rate: 5,
    })
    .stats_payload()
    .to_vec();
    let phy_cr = crate::phy::PhyParams::new(crate::phy::Modulation {
        bandwidth_hz: 125_000,
        spreading_factor: 8,
        coding_rate: 8,
    })
    .stats_payload()
    .to_vec();
    assert_eq!(frames[1], (CMD_STAT_PHYPRM, phy_bw));
    assert_eq!(frames[2], (CMD_BANDWIDTH, 125_000u32.to_be_bytes().to_vec()));
    assert_eq!(frames[3], (CMD_TXPOWER, vec![22]));
    assert_eq!(frames[4], (CMD_STAT_PHYPRM, phy_sf));
    assert_eq!(frames[5], (CMD_SF, vec![8]));
    assert_eq!(frames[6], (CMD_STAT_PHYPRM, phy_cr));
    assert_eq!(frames[7], (CMD_CR, vec![8]));
    assert!(n.is_radio_online());
    assert_eq!(n.radio().frequency(), 868_100_000);
    assert_eq!(n.radio().bandwidth(), 125_000);
    assert_eq!(n.radio().tx_power(), 22);
    assert_eq!(n.radio().spreading_factor(), 8);
    assert_eq!(n.radio().coding_rate(), 8);
}

#[test]
fn radio_lock_rejects_start_until_required_fields_are_set() {
    let medium = SimMedium::new();
    let mut n = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 1);
    let mut input = Vec::new();
    input.extend(cmd(CMD_FREQUENCY, &867_200_000u32.to_be_bytes()));
    input.extend(cmd(CMD_BANDWIDTH, &125_000u32.to_be_bytes()));
    input.extend(plain(CMD_TXPOWER, &[14]));
    input.extend(plain(CMD_RADIO_STATE, &[RADIO_STATE_ON]));
    n.handle_host_input(&input, 0);
    assert!(!n.is_radio_online());
    assert!(
        n.take_host_output()
            .windows(4)
            .any(|w| w == plain(CMD_RADIO_STATE, &[0]))
    );
}

#[test]
fn take_host_output_drains_buffer() {
    let mut n = node(1);
    n.handle_host_input(&plain(CMD_DETECT, &[DETECT_REQ]), 0);
    assert_eq!(n.take_host_output(), plain(CMD_DETECT, &[DETECT_RESP]));
    assert!(n.take_host_output().is_empty());
}

#[test]
fn randomized_host_input_never_panics() {
    let mut n = node(0xFEED);
    let mut rng = crate::rng::Rng::new(99);
    let mut bytes = Vec::new();
    for _ in 0..20_000 {
        bytes.push(rng.byte());
    }
    n.handle_host_input(&bytes, 0);
}
