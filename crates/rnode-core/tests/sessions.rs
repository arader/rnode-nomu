//! Host sessions between two nodes on the simulated medium.

mod common;

use common::{data, init, plain, split_frames, tick_until};
use rnode_core::kiss::*;
use rnode_core::{MemoryStorage, Node, NodeConfig, SimMedium};

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
