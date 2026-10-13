//! Channel access through the node: CSMA, the TX queue's send rate and interference avoidance
//! (`CMD_DIS_IA`).

mod common;

use common::{cmd, data, init, listener, node, plain, split_frames, tick_until};
use rnode_core::kiss::*;
use rnode_core::{MemoryStorage, Node, NodeConfig, SimMedium};
// The code below refers to these as `crate::csma`, `crate::node`, `crate::phy` and `crate::SimRadio`.
use rnode_core::{SimRadio, csma, node, phy};

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
