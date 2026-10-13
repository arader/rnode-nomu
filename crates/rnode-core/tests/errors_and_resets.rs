//! Errors reported to the host (`CMD_ERROR`), and the resets that follow radio failures.

mod common;

use common::{data, init, plain, split_frames, tick_until};
use rnode_core::kiss::*;
use rnode_core::{MemoryStorage, Node, NodeConfig, SimMedium};

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
