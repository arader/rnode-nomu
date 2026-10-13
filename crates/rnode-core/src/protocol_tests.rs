use crate::kiss::*;
use crate::{DeviceIdentity, MemoryStorage, Node, NodeConfig, Radio, RadioSettings, SimMedium};

pub(crate) fn cmd(cmd: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![FEND, cmd];
    for &b in payload {
        crate::kiss::escape_into(&mut out, b);
    }
    out.push(FEND);
    out
}

pub(crate) fn plain(cmd: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![FEND, cmd];
    out.extend_from_slice(payload);
    out.push(FEND);
    out
}

pub(crate) fn data(payload: &[u8]) -> Vec<u8> {
    cmd(CMD_DATA, payload)
}

pub(crate) fn node(seed: u64) -> Node<crate::SimRadio, MemoryStorage> {
    let medium = SimMedium::new();
    Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), seed)
}

pub(crate) fn init(n: &mut Node<crate::SimRadio, MemoryStorage>) -> Vec<u8> {
    let mut input = Vec::new();
    input.extend(cmd(CMD_FREQUENCY, &867_200_000u32.to_be_bytes()));
    input.extend(cmd(CMD_BANDWIDTH, &125_000u32.to_be_bytes()));
    input.extend(plain(CMD_TXPOWER, &[14]));
    input.extend(plain(CMD_SF, &[7]));
    input.extend(plain(CMD_CR, &[5]));
    input.extend(plain(CMD_ST_ALOCK, &[0, 0]));
    input.extend(plain(CMD_LT_ALOCK, &[0, 0]));
    input.extend(plain(CMD_RADIO_STATE, &[RADIO_STATE_ON]));
    n.handle_host_input(&input, 0);
    n.take_host_output()
}

pub(crate) fn split_frames(buf: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut frames = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        assert_eq!(buf[i], FEND, "frame must start with FEND at {i}");
        i += 1;
        let cmd = buf[i];
        i += 1;
        let mut p = Vec::new();
        let mut esc = false;
        while i < buf.len() && buf[i] != FEND {
            let mut b = buf[i];
            i += 1;
            if esc {
                if b == TFEND {
                    b = FEND;
                }
                if b == TFESC {
                    b = FESC;
                }
                esc = false;
                p.push(b);
            } else if b == FESC {
                esc = true;
            } else {
                p.push(b);
            }
        }
        assert!(i < buf.len());
        i += 1;
        frames.push((cmd, p));
    }
    frames
}

pub(crate) fn tick_until(
    node: &mut Node<crate::SimRadio, MemoryStorage>,
    medium: &SimMedium,
    max_ms: u64,
    done: impl Fn(&Node<crate::SimRadio, MemoryStorage>) -> bool,
) {
    let start = medium.now_ms();
    while medium.now_ms().saturating_sub(start) <= max_ms {
        node.tick(medium.now_ms());
        if done(node) {
            return;
        }
        medium.advance_ms(25);
    }
}

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

#[test]
fn split_framing_vectors_match_firmware_algorithm_without_508_extra() {
    let mut rng = crate::rng::Rng::new(0x1234);
    for len in [1usize, 254, 255, 400, 508] {
        let payload: Vec<u8> = (0..len).map(|i| i as u8).collect();
        let frames = crate::framing::tx_frames(&payload, &mut rng);
        if len <= 254 {
            assert_eq!(frames.len(), 1);
            assert_eq!(frames[0].len(), len + 1);
            assert_eq!(frames[0][0] & FLAG_SPLIT, 0);
        } else {
            assert_eq!(frames.len(), 2);
            assert_eq!(frames[0].len(), 255);
            assert_eq!(frames[1].len(), len - 254 + 1);
            assert_eq!(frames[0][0], frames[1][0]);
            assert_eq!(frames[0][0] & FLAG_SPLIT, FLAG_SPLIT);
            let mut rx = crate::framing::RxReassembler::new();
            assert!(matches!(rx.process(&frames[0]), crate::framing::Reassembly::Pending));
            assert_eq!(rx.process(&frames[1]), crate::framing::Reassembly::Complete(payload));
        }
    }
}

#[test]
fn split_framing_has_firmware_derived_literal_bytes() {
    let payload: Vec<u8> = (0u8..=254).collect();
    let mut rng = crate::rng::Rng::new(1); // first byte 0x91 after FLAG_SPLIT, matching firmware header = random(256)&0xF0|FLAG_SPLIT.
    let frames = crate::framing::tx_frames(&payload, &mut rng);
    let mut expected_first = Vec::with_capacity(255);
    expected_first.push(0x91);
    expected_first.extend(0u8..=253);
    let expected_second = vec![0x91, 254];
    assert_eq!(frames, vec![expected_first, expected_second]);
}

#[test]
fn split_reassembler_different_sequence_restarts_pending_packet() {
    let mut rx = crate::framing::RxReassembler::new();
    let a_first = vec![0x11, b'a'];
    let b_first = vec![0x21, b'b'];
    let b_second = vec![0x21, b'c'];
    assert!(matches!(rx.process(&a_first), crate::framing::Reassembly::Pending));
    assert!(matches!(rx.process(&b_first), crate::framing::Reassembly::Pending));
    assert_eq!(
        rx.process(&b_second),
        crate::framing::Reassembly::Complete(vec![b'b', b'c'])
    );
}

#[test]
fn receiver_tolerates_real_firmware_header_only_508_tail() {
    let payload = vec![0xAA; 508];
    let header = 0x31;
    let first = [&[header][..], &payload[..254]].concat();
    let second = [&[header][..], &payload[254..]].concat();
    let extra = vec![header];
    let nonsplit = vec![0x40, 1, 2, 3];
    let mut rx = crate::framing::RxReassembler::new();
    assert!(matches!(rx.process(&first), crate::framing::Reassembly::Pending));
    assert_eq!(rx.process(&second), crate::framing::Reassembly::Complete(payload));
    assert!(matches!(rx.process(&extra), crate::framing::Reassembly::Pending));
    assert_eq!(
        rx.process(&nonsplit),
        crate::framing::Reassembly::Complete(vec![1, 2, 3])
    );
}

#[test]
fn two_nodes_exchange_full_host_sessions_with_rssi_snr() {
    let medium = SimMedium::new();
    let mut a = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 1);
    let mut b = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 2);
    init(&mut a);
    a.take_host_output();
    init(&mut b);
    b.take_host_output();
    for len in [1usize, 254, 255, 400, 508] {
        let payload: Vec<u8> = (0..len).map(|i| i as u8).collect();
        a.handle_host_input(&data(&payload), medium.now_ms());
        tick_until(&mut a, &medium, 5_000, |n| n.queued_packets() == 0);
        b.service_radio(medium.now_ms());
        let out = b.take_host_output();
        let frames = split_frames(&out);
        assert_eq!(frames[0], (CMD_STAT_RSSI, vec![115])); // -42 + 157
        assert_eq!(frames[1], (CMD_STAT_SNR, vec![0x20]));
        assert_eq!(frames[2], (CMD_DATA, payload));
        medium.advance_ms(150);
    }
}

#[test]
fn csma_waits_while_medium_busy_then_transmits() {
    let medium = SimMedium::new();
    medium.set_tx_duration_ms(100);
    let mut a = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 1);
    let mut b = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 2);
    init(&mut a);
    a.take_host_output();
    init(&mut b);
    b.take_host_output();
    a.handle_host_input(&data(&[1, 2, 3]), 0);
    tick_until(&mut a, &medium, 5_000, |n| n.queued_packets() == 0);
    b.handle_host_input(&data(&[9]), medium.now_ms());
    b.tick(medium.now_ms());
    assert_eq!(b.queued_packets(), 1, "busy channel should defer b");
    medium.advance_ms(101);
    tick_until(&mut b, &medium, 5_000, |n| n.queued_packets() == 0);
    assert_eq!(b.queued_packets(), 0);
}

#[test]
fn begin_failure_timeout_reset_and_storage_boot_paths() {
    let medium = SimMedium::new();
    let mut radio = medium.radio();
    radio.inject_begin_failures(1);
    let mut n = Node::new(radio, MemoryStorage::default(), NodeConfig::default(), 1);
    let first_out = init(&mut n);
    let first_frames = split_frames(&first_out);
    assert!(first_out.windows(4).any(|w| w == plain(CMD_ERROR, &[ERROR_INITRADIO])));
    assert_eq!(first_frames.last(), Some(&(CMD_RADIO_STATE, vec![0])));
    assert!(!n.is_radio_online());

    init(&mut n);
    n.take_host_output();
    n.handle_host_input(&plain(CMD_CONF_SAVE, &[0]), 0);
    assert!(n.storage().settings().is_some());
    let receive_calls_before_timeout = n.radio().receive_calls();
    n.radio_mut().inject_next_timeout();
    n.handle_host_input(&data(&[1]), 0);
    tick_until(&mut n, &medium, 5_000, |n| n.queued_packets() == 0);
    let out = n.take_host_output();
    assert!(out.windows(4).any(|w| w == plain(CMD_ERROR, &[ERROR_MODEM_TIMEOUT])));
    assert!(out.windows(4).any(|w| w == plain(CMD_ERROR, &[ERROR_TXFAILED])));
    assert!(n.is_radio_online(), "hard reset reloads saved complete settings");
    assert_eq!(n.radio().receive_calls(), receive_calls_before_timeout + 1);

    n.handle_host_input(&plain(CMD_CONF_DELETE, &[0]), 0);
    assert!(n.storage().settings().is_none());
    n.handle_host_input(&plain(CMD_RESET, &[CMD_RESET_BYTE]), 0);
    assert!(!n.is_radio_online());
}

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
fn promisc_oversize_frames_drop_instead_of_truncating_and_implicit_is_raw_only() {
    let medium = SimMedium::new();
    let mut a = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 1);
    let mut b = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 2);
    init(&mut a);
    a.take_host_output();
    init(&mut b);
    b.take_host_output();

    a.handle_host_input(&plain(CMD_IMPLICIT, &[10]), 0);
    assert_eq!(a.radio().implicit_len(), None, "non-promisc RX stays explicit");
    a.handle_host_input(&plain(CMD_PROMISC, &[1]), 0);
    assert_eq!(a.radio().implicit_len(), Some(10));
    a.take_host_output();

    a.handle_host_input(&data(&vec![0x55; 400]), 0);
    assert_eq!(a.queued_packets(), 0, "oversize raw frames are rejected at admission");
    a.tick(0);
    b.service_radio(0);
    assert!(b.take_host_output().is_empty());

    a.radio().inject_rx(vec![0x44; 300], -50, 0x10);
    a.service_radio(0);
    assert!(
        a.take_host_output().is_empty(),
        "oversize raw RX frames are dropped, not truncated"
    );
}

#[test]
fn txpower_clamps_in_signed_dbm_space() {
    let medium = SimMedium::new();
    let mut n = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 1);
    init(&mut n);
    n.take_host_output();
    n.handle_host_input(&plain(CMD_TXPOWER, &[0x80]), 0);
    assert_eq!(n.take_host_output(), plain(CMD_TXPOWER, &[0xF7]));
    assert_eq!(n.radio().tx_power(), -9);
    n.handle_host_input(&plain(CMD_TXPOWER, &[0xF7]), 0);
    assert_eq!(n.take_host_output(), plain(CMD_TXPOWER, &[0xF7]));
    assert_eq!(n.radio().tx_power(), -9);
}

#[test]
fn radio_tx_error_emits_txfailed_without_reset() {
    let medium = SimMedium::new();
    let mut n = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 1);
    init(&mut n);
    n.take_host_output();
    let receive_calls_before = n.radio().receive_calls();
    n.radio_mut().inject_next_tx_error();
    n.handle_host_input(&data(&[1, 2, 3]), 0);
    tick_until(&mut n, &medium, 5_000, |n| n.queued_packets() == 0);
    let out = n.take_host_output();
    assert!(out.windows(4).any(|w| w == plain(CMD_ERROR, &[ERROR_TXFAILED])));
    assert!(!out.windows(4).any(|w| w == plain(CMD_ERROR, &[ERROR_MODEM_TIMEOUT])));
    assert!(
        n.is_radio_online(),
        "driver errors do not perform the timeout hard-reset path"
    );
    assert_eq!(
        n.radio().receive_calls(),
        receive_calls_before + 1,
        "online radio re-enters receive after driver errors"
    );
}

#[test]
fn split_packet_second_frame_tx_error_is_reported() {
    let medium = SimMedium::new();
    let mut n = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 1);
    init(&mut n);
    n.take_host_output();
    n.radio_mut().inject_tx_error_on_call(2);
    n.handle_host_input(&data(&vec![0xAB; 300]), 0);
    tick_until(&mut n, &medium, 5_000, |n| n.queued_packets() == 0);
    let out = n.take_host_output();
    assert!(out.windows(4).any(|w| w == plain(CMD_ERROR, &[ERROR_TXFAILED])));
    assert!(!out.windows(4).any(|w| w == plain(CMD_ERROR, &[ERROR_MODEM_TIMEOUT])));
    assert!(n.is_radio_online());
}

#[test]
fn split_packet_second_frame_timeout_resets_offline_without_receive() {
    let medium = SimMedium::new();
    let mut n = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 1);
    init(&mut n);
    n.take_host_output();
    let receive_calls_before = n.radio().receive_calls();
    n.radio_mut().inject_timeout_on_call(2);
    n.handle_host_input(&data(&vec![0xCD; 300]), 0);
    tick_until(&mut n, &medium, 5_000, |n| n.queued_packets() == 0);
    let out = n.take_host_output();
    assert!(out.windows(4).any(|w| w == plain(CMD_ERROR, &[ERROR_MODEM_TIMEOUT])));
    assert!(out.windows(4).any(|w| w == plain(CMD_ERROR, &[ERROR_TXFAILED])));
    assert!(!n.is_radio_online());
    assert_eq!(
        n.radio().receive_calls(),
        receive_calls_before,
        "offline reset path must not re-enter receive"
    );
}

#[test]
fn timeout_after_prior_success_without_saved_settings_leaves_radio_offline_cleanly() {
    let medium = SimMedium::new();
    let mut n = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 1);
    init(&mut n);
    n.take_host_output();
    let receive_calls_before = n.radio().receive_calls();
    n.radio_mut().inject_timeout_on_call(3);
    let mut input = Vec::new();
    input.extend(data(&[1]));
    input.extend(data(&vec![2; 300]));
    n.handle_host_input(&input, 0);
    tick_until(&mut n, &medium, 5_000, |n| n.queued_packets() == 0);
    let frames = split_frames(&n.take_host_output());
    let first_error = frames
        .iter()
        .position(|f| *f == (CMD_ERROR, vec![ERROR_MODEM_TIMEOUT]))
        .unwrap();
    assert_eq!(frames.get(first_error + 1), Some(&(CMD_ERROR, vec![ERROR_TXFAILED])));
    assert!(!frames[first_error..].iter().any(|(cmd, _)| *cmd == CMD_STAT_CHTM));
    assert!(!n.is_radio_online());
    assert_eq!(n.radio().receive_calls(), receive_calls_before);
}

#[test]
fn timeout_after_prior_success_with_saved_settings_skips_post_reset_batch_epilogue() {
    let medium = SimMedium::new();
    let mut n = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 1);
    init(&mut n);
    n.take_host_output();
    n.handle_host_input(&plain(CMD_CONF_SAVE, &[0]), 0);
    let receive_calls_before = n.radio().receive_calls();
    n.radio_mut().inject_timeout_on_call(3);
    let mut input = Vec::new();
    input.extend(data(&[1]));
    input.extend(data(&vec![2; 300]));
    n.handle_host_input(&input, 0);
    tick_until(&mut n, &medium, 5_000, |n| n.queued_packets() == 0);
    let frames = split_frames(&n.take_host_output());
    assert!(frames.iter().any(|f| *f == (CMD_ERROR, vec![ERROR_MODEM_TIMEOUT])));
    assert!(frames.iter().any(|f| *f == (CMD_ERROR, vec![ERROR_TXFAILED])));
    assert!(!frames.iter().any(|(cmd, _)| *cmd == CMD_STAT_CHTM));
    assert!(n.is_radio_online());
    assert_eq!(n.radio().receive_calls(), receive_calls_before + 1);
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
fn promisc_toggle_after_queue_reports_unsendable_packet() {
    let medium = SimMedium::new();
    let mut n = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 1);
    init(&mut n);
    n.take_host_output();
    n.handle_host_input(&data(&vec![0x99; 300]), 0);
    assert_eq!(n.queued_packets(), 1);
    n.handle_host_input(&plain(CMD_PROMISC, &[1]), 0);
    n.take_host_output();
    tick_until(&mut n, &medium, 5_000, |n| n.queued_packets() == 0);
    let out = n.take_host_output();
    assert!(out.windows(4).any(|w| w == plain(CMD_ERROR, &[ERROR_TXFAILED])));
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
fn stat_counters_count_host_packets_after_real_traffic() {
    let medium = SimMedium::new();
    let mut a = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 1);
    let mut b = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 2);
    init(&mut a);
    a.take_host_output();
    init(&mut b);
    b.take_host_output();

    for payload in [vec![1], vec![2; 300]] {
        a.handle_host_input(&data(&payload), medium.now_ms());
        tick_until(&mut a, &medium, 5_000, |n| n.queued_packets() == 0);
        b.service_radio(medium.now_ms());
        b.take_host_output();
        medium.advance_ms(150);
    }

    a.handle_host_input(&plain(CMD_STAT_TX, &[0]), medium.now_ms());
    b.handle_host_input(&plain(CMD_STAT_RX, &[0]), medium.now_ms());
    let a_frames = split_frames(&a.take_host_output());
    let b_frames = split_frames(&b.take_host_output());
    assert_eq!(
        a_frames.iter().rev().find(|(cmd, _)| *cmd == CMD_STAT_TX),
        Some(&(CMD_STAT_TX, 2u32.to_be_bytes().to_vec()))
    );
    assert_eq!(
        b_frames.iter().rev().find(|(cmd, _)| *cmd == CMD_STAT_RX),
        Some(&(CMD_STAT_RX, 2u32.to_be_bytes().to_vec()))
    );
}

#[test]
fn tx_queue_flushes_multiple_packets_at_normal_rate() {
    let medium = SimMedium::new();
    let mut a = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 1);
    let mut b = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 2);
    init(&mut a);
    a.take_host_output();
    init(&mut b);
    b.take_host_output();
    let mut input = Vec::new();
    input.extend(data(&[1]));
    input.extend(data(&[2]));
    a.handle_host_input(&input, 0);
    tick_until(&mut a, &medium, 5_000, |n| n.queued_packets() == 0);
    assert_eq!(a.queued_packets(), 0);
    b.service_radio(0);
    let frames = split_frames(&b.take_host_output());
    assert_eq!(frames.iter().filter(|(cmd, _)| *cmd == CMD_DATA).count(), 2);
}

#[test]
fn tx_queue_sends_one_packet_per_tick_at_guard_rate() {
    let medium = SimMedium::new();
    let mut n = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 1);
    let mut input = Vec::new();
    input.extend(cmd(CMD_FREQUENCY, &867_200_000u32.to_be_bytes()));
    input.extend(cmd(CMD_BANDWIDTH, &500_000u32.to_be_bytes()));
    input.extend(plain(CMD_TXPOWER, &[14]));
    input.extend(plain(CMD_SF, &[7]));
    input.extend(plain(CMD_CR, &[5]));
    input.extend(plain(CMD_RADIO_STATE, &[RADIO_STATE_ON]));
    n.handle_host_input(&input, 0);
    n.take_host_output();
    let mut queued = Vec::new();
    queued.extend(data(&[1]));
    queued.extend(data(&[2]));
    n.handle_host_input(&queued, 0);
    tick_until(&mut n, &medium, 5_000, |n| n.queued_packets() == 1);
    assert_eq!(n.queued_packets(), 1);
    medium.advance_ms(101);
    tick_until(&mut n, &medium, 5_000, |n| n.queued_packets() == 0);
    assert_eq!(n.queued_packets(), 0);
}

#[test]
fn take_host_output_drains_buffer() {
    let mut n = node(1);
    n.handle_host_input(&plain(CMD_DETECT, &[DETECT_REQ]), 0);
    assert_eq!(n.take_host_output(), plain(CMD_DETECT, &[DETECT_RESP]));
    assert!(n.take_host_output().is_empty());
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

/// Tick `node` every `step_ms` of medium time until `done` holds, for at most `max_ms`; returns
/// the medium time at which `done` first held.
fn tick_every_until(
    node: &mut Node<crate::SimRadio, MemoryStorage>,
    medium: &SimMedium,
    step_ms: u64,
    max_ms: u64,
    done: impl Fn(&Node<crate::SimRadio, MemoryStorage>) -> bool,
) -> Option<u64> {
    let start = medium.now_ms();
    loop {
        node.tick(medium.now_ms());
        if done(node) {
            return Some(medium.now_ms());
        }
        if medium.now_ms() - start >= max_ms {
            return None;
        }
        medium.advance_ms(step_ms);
    }
}

/// The `CMD_STAT_CHTM` payloads in `out`.
fn chtm_payloads(out: &[u8]) -> Vec<Vec<u8>> {
    split_frames(out)
        .into_iter()
        .filter(|(cmd, _)| *cmd == CMD_STAT_CHTM)
        .map(|(_, payload)| payload)
        .collect()
}

/// A radio listening on the channel that `init()` configures.
pub(crate) fn listener(medium: &SimMedium) -> crate::SimRadio {
    let mut radio = medium.radio();
    radio.begin(867_200_000).unwrap();
    radio
}

/// DIFS for the modulation `init()` configures (SF7, 125 kHz, 4/5).
fn init_difs_ms() -> u64 {
    let phy = crate::phy::PhyParams::new(crate::phy::Modulation {
        bandwidth_hz: 125_000,
        spreading_factor: 7,
        coding_rate: 5,
    });
    u64::from(phy.difs_ms)
}

/// Medium time, relative to the radio start, of the status sample that completes the first noise
/// floor (`update_noise_floor()`): until then the floor is the -292 dBm sentinel.
const FLOOR_SAMPLED_AFTER_MS: u64 = (crate::csma::NOISE_FLOOR_SAMPLES as u64 - 1) * crate::node::STATUS_INTERVAL_MS;

#[test]
fn interference_avoidance_is_off_by_default_so_interference_is_reported_but_does_not_block_tx() {
    assert!(!NodeConfig::default().avoid_interference);
    let medium = SimMedium::new();
    let listener = listener(&medium);
    let mut n = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 1);
    assert!(!n.avoid_interference());
    init(&mut n);
    n.take_host_output();

    // Until the first noise floor is sampled, the -292 dBm sentinel makes the idle -120 dBm channel
    // look like interference. Ticking every 25 ms stretches that window to 3.2 s, far beyond DIFS
    // plus the largest contention window, so the packet goes out inside it.
    n.handle_host_input(&data(&[1, 2, 3]), medium.now_ms());
    tick_every_until(&mut n, &medium, 25, 1_000, |n| n.queued_packets() == 0).expect("interference must not defer TX");
    assert_eq!(listener.queued_rx(), 1);
    // kiss_indicate_channel_stats(): byte 10 (ntf) is the current RSSI byte while interference is
    // detected, 0xFF otherwise. -120 + 157 = 37; the sentinel floor -292 + 157 wraps to 121.
    let chtm = chtm_payloads(&n.take_host_output());
    assert_eq!(&chtm.last().expect("CHTM after the TX burst")[8..11], &[37, 121, 37]);

    // Persistent interference once the floor is sampled: -70 dBm is above the -83 dBm
    // recalibration limit, so the floor stays at -120 dBm and interference stays detected.
    tick_every_until(&mut n, &medium, 25, 4_000, |_| false);
    n.take_host_output();
    medium.set_noise_dbm(-70);
    n.handle_host_input(&data(&[4]), medium.now_ms());
    tick_every_until(&mut n, &medium, 25, 1_000, |n| n.queued_packets() == 0).expect("interference must not defer TX");
    assert_eq!(listener.queued_rx(), 2);
    let chtm = chtm_payloads(&n.take_host_output());
    assert_eq!(&chtm.last().expect("CHTM after the TX burst")[8..11], &[87, 37, 87]);
}

#[test]
fn interference_avoidance_from_config_defers_tx_until_floor_is_sampled_and_interference_clears() {
    let medium = SimMedium::new();
    let listener = listener(&medium);
    let config = NodeConfig {
        avoid_interference: true,
        ..NodeConfig::default()
    };
    let mut n = Node::new(medium.radio(), MemoryStorage::default(), config, 1);
    assert!(n.avoid_interference());
    init(&mut n);
    n.take_host_output();

    // Ticking at the firmware's STATUS_INTERVAL_MS, the floor is sampled after 128 samples
    // (381 ms); the medium is free from then on, so TX follows after DIFS and the contention window.
    let start = medium.now_ms();
    n.handle_host_input(&data(&[1, 2, 3]), start);
    let sent_at = tick_every_until(&mut n, &medium, crate::node::STATUS_INTERVAL_MS, 2_000, |n| {
        n.queued_packets() == 0
    })
    .expect("TX once the noise floor is sampled");
    assert!(
        sent_at - start >= FLOOR_SAMPLED_AFTER_MS + init_difs_ms(),
        "sent {} ms after start, inside the boot noise-floor window",
        sent_at - start
    );
    assert_eq!(listener.queued_rx(), 1);

    // Persistent interference defers TX for as long as it lasts, and CHTM reports it.
    medium.advance_ms(200); // Let our own transmission leave the air.
    n.take_host_output();
    medium.set_noise_dbm(-70);
    n.handle_host_input(&data(&[4]), medium.now_ms());
    let deferred = tick_every_until(&mut n, &medium, crate::node::STATUS_INTERVAL_MS, 3_000, |n| {
        n.queued_packets() == 0
    });
    assert_eq!(deferred, None, "TX during interference");
    assert_eq!(listener.queued_rx(), 1);
    let chtm = chtm_payloads(&n.take_host_output());
    assert!(!chtm.is_empty(), "CHTM is reported once per second");
    assert!(chtm.iter().all(|p| p[8..11] == [87, 37, 87]), "{chtm:?}");

    medium.set_noise_dbm(-120);
    let cleared_at = medium.now_ms();
    let sent_at = tick_every_until(&mut n, &medium, crate::node::STATUS_INTERVAL_MS, 1_000, |n| {
        n.queued_packets() == 0
    })
    .expect("TX once the interference clears");
    assert!(sent_at - cleared_at >= init_difs_ms());
    assert_eq!(listener.queued_rx(), 2);
}

#[test]
fn dis_ia_decoder_fires_on_the_first_unescaped_payload_byte_only() {
    let mut decoder = Decoder::new();
    let mut events = Vec::new();
    let input = [
        [FEND, CMD_DIS_IA, FEND].as_slice(),
        &[FEND, CMD_DIS_IA, FESC, TFEND, 0x00, FEND],
        &[FEND, CMD_DIS_IA, 0x00, 0x01, FEND],
    ]
    .concat();
    for b in input {
        decoder.push(b, &mut events);
    }
    assert_eq!(
        events,
        vec![
            HostEvent::Command {
                cmd: CMD_DIS_IA,
                payload: vec![FEND]
            },
            HostEvent::Command {
                cmd: CMD_DIS_IA,
                payload: vec![0x00]
            },
        ]
    );
}

#[test]
fn dis_ia_sets_avoidance_without_reply_following_dia_conf_save() {
    let mut n = node(1);
    // An empty frame never reaches dia_conf_save(): no change, even though 0x00 would enable.
    n.handle_host_input(&plain(CMD_DIS_IA, &[]), 0);
    assert!(!n.avoid_interference());
    for (payload, enabled) in [
        (vec![0x00], true),
        (vec![0x01], false),
        (vec![0x00, 0x01], true), // The firmware reboots after the first byte.
        (vec![0x02], false),
        (vec![0x00], true),
        (vec![FEND], false), // Escaped; any non-zero byte disables.
        (vec![0x00], true),
        (vec![0xFF], false),
    ] {
        n.handle_host_input(&cmd(CMD_DIS_IA, &payload), 0);
        assert_eq!(n.avoid_interference(), enabled, "payload {payload:02X?}");
        assert!(n.take_host_output().is_empty(), "CMD_DIS_IA has no reply");
    }
    n.handle_host_input(&cmd(CMD_DIS_IA, &[0x00]), 0);
    n.handle_host_input(&plain(CMD_DIS_IA, &[]), 0);
    assert!(n.avoid_interference(), "an empty frame does not disable either");
    assert!(!n.config().avoid_interference, "the start-up default is unchanged");
}

#[test]
fn dis_ia_runtime_setting_survives_reset() {
    let medium = SimMedium::new();
    let listener = listener(&medium);
    let mut n = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 1);
    init(&mut n);
    n.handle_host_input(&plain(CMD_CONF_SAVE, &[0]), 0); // So CMD_RESET brings the radio back.
    n.take_host_output();

    // rnodeconf --ia-enable, then CMD_RESET: the reset restarts noise-floor sampling from the
    // sentinel, and the enabled avoidance defers TX for that window.
    n.handle_host_input(&plain(CMD_DIS_IA, &[0x00]), medium.now_ms());
    n.handle_host_input(&plain(CMD_RESET, &[CMD_RESET_BYTE]), medium.now_ms());
    assert!(n.is_radio_online());
    assert!(n.avoid_interference());
    n.take_host_output();
    let start = medium.now_ms();
    n.handle_host_input(&data(&[1]), start);
    let sent_at = tick_every_until(&mut n, &medium, crate::node::STATUS_INTERVAL_MS, 2_000, |n| {
        n.queued_packets() == 0
    })
    .expect("TX once the noise floor is sampled");
    assert!(sent_at - start >= FLOOR_SAMPLED_AFTER_MS + init_difs_ms());
    assert_eq!(listener.queued_rx(), 1);

    // rnodeconf --ia-disable, then CMD_RESET: TX goes out inside the sentinel window again.
    n.take_host_output();
    n.handle_host_input(&plain(CMD_DIS_IA, &[0x01]), medium.now_ms());
    assert!(n.take_host_output().is_empty());
    n.handle_host_input(&plain(CMD_RESET, &[CMD_RESET_BYTE]), medium.now_ms());
    assert!(n.is_radio_online());
    assert!(!n.avoid_interference());
    n.take_host_output();
    n.handle_host_input(&data(&[2]), medium.now_ms());
    tick_every_until(&mut n, &medium, 25, 1_000, |n| n.queued_packets() == 0).expect("interference must not defer TX");
    assert_eq!(listener.queued_rx(), 2);
    let chtm = chtm_payloads(&n.take_host_output());
    assert_eq!(&chtm.last().expect("CHTM after the TX burst")[8..11], &[37, 121, 37]);
}

#[test]
fn dis_ia_changes_channel_access_immediately() {
    let medium = SimMedium::new();
    let listener = listener(&medium);
    let mut n = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 1);
    init(&mut n);
    tick_every_until(&mut n, &medium, 25, 4_000, |_| false); // Sample the -120 dBm noise floor.
    medium.set_noise_dbm(-70);

    n.handle_host_input(&plain(CMD_DIS_IA, &[0x00]), medium.now_ms());
    n.handle_host_input(&data(&[1]), medium.now_ms());
    let deferred = tick_every_until(&mut n, &medium, 25, 2_000, |n| n.queued_packets() == 0);
    assert_eq!(deferred, None, "enabled avoidance defers TX during interference");

    n.handle_host_input(&plain(CMD_DIS_IA, &[0x01]), medium.now_ms());
    tick_every_until(&mut n, &medium, 25, 1_000, |n| n.queued_packets() == 0)
        .expect("disabled avoidance ignores the interference");
    assert_eq!(listener.queued_rx(), 1);
}
