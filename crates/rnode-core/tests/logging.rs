//! Per-packet logging (#42): debug lines for received, sent and dropped packets, trace hex dumps,
//! and no effect on what the node sends.

mod common;

use log::{Level, LevelFilter};

use common::log_capture::{Captured, capture};
use common::{data, init, listener, node, plain, split_frames, tick_until};
use rnode_core::kiss::*;
use rnode_core::{MemoryStorage, Node, NodeConfig, Radio, SimMedium, SimRadio};

/// The messages of `records` at `level` that start with `prefix`.
fn messages(records: &[Captured], level: Level, prefix: &str) -> Vec<String> {
    records
        .iter()
        .filter(|r| r.level == level && r.message.starts_with(prefix))
        .map(|r| r.message.clone())
        .collect()
}

/// An online node with seed 1, and a radio that hears what it sends.
fn online_node() -> (SimMedium, SimRadio, Node<SimRadio, MemoryStorage>) {
    let medium = SimMedium::new();
    let air = listener(&medium);
    let mut n = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 1);
    init(&mut n);
    n.take_host_output();
    (medium, air, n)
}

/// The header bytes of the frames `air` has received.
fn headers(air: &mut SimRadio) -> Vec<u8> {
    std::iter::from_fn(|| air.poll_rx().unwrap())
        .map(|frame| frame.data[0])
        .collect()
}

#[test]
fn capture_is_per_thread() {
    let cap = capture(LevelFilter::Trace);
    std::thread::spawn(|| {
        assert!(!log::log_enabled!(Level::Error));
        log::error!("another thread");
    })
    .join()
    .unwrap();
    log::trace!("this thread");
    let records = cap.take();
    assert_eq!(messages(&records, Level::Trace, ""), ["this thread"]);
    assert_eq!(records.len(), 1);
}

#[test]
fn rx_packets_are_logged_at_debug_with_frames_header_rssi_and_snr() {
    let (_medium, _air, mut n) = online_node();
    let cap = capture(LevelFilter::Debug);
    let payload: Vec<u8> = (0..300u16).map(|i| i as u8).collect();
    n.radio().inject_rx([&[0x31][..], &payload[..254]].concat(), -42, 0x20);
    n.radio().inject_rx([&[0x31][..], &payload[254..]].concat(), -42, 0x20);
    n.radio().inject_rx(vec![0x40, 1, 2, 3], -97, 0xF3);
    n.service_radio(0);
    n.handle_host_input(&plain(CMD_PROMISC, &[1]), 0);
    n.radio().inject_rx(vec![0xAA, 0xBB], -50, 0x10);
    n.service_radio(0);

    let records = cap.take();
    assert_eq!(
        messages(&records, Level::Debug, "RX "),
        [
            "RX packet: 300 bytes, 2 frames, header 0x31 (seq 3, split), RSSI -42 dBm, SNR 8.00 dB",
            "RX packet: 3 bytes, 1 frame, header 0x40 (seq 4), RSSI -97 dBm, SNR -3.25 dB",
            "RX packet: 2 bytes, 1 frame, promiscuous, RSSI -50 dBm, SNR 4.00 dB",
        ]
    );
    assert!(records.iter().all(|r| r.target == "rnode_core::node"));
    // The logged values are the ones sent to the host ahead of each packet: RSSI + 157 and the
    // raw SNR byte in 0.25 dB steps.
    let stats: Vec<_> = split_frames(&n.take_host_output())
        .into_iter()
        .filter(|(cmd, _)| [CMD_STAT_RSSI, CMD_STAT_SNR].contains(cmd))
        .collect();
    assert_eq!(
        stats,
        [
            (CMD_STAT_RSSI, vec![115]),
            (CMD_STAT_SNR, vec![0x20]),
            (CMD_STAT_RSSI, vec![60]),
            (CMD_STAT_SNR, vec![0xF3]),
            (CMD_STAT_RSSI, vec![107]),
            (CMD_STAT_SNR, vec![0x10]),
        ]
    );
}

#[test]
fn tx_packets_are_logged_at_debug_with_frames_and_header() {
    let (medium, mut air, mut n) = online_node();
    let cap = capture(LevelFilter::Debug);
    n.handle_host_input(&data(&[0x55; 300]), medium.now_ms());
    tick_until(&mut n, &medium, 5_000, |n| n.queued_packets() == 0);
    n.handle_host_input(&data(&[1, 2, 3]), medium.now_ms());
    tick_until(&mut n, &medium, 5_000, |n| n.queued_packets() == 0);
    n.handle_host_input(&plain(CMD_PROMISC, &[1]), medium.now_ms());
    n.handle_host_input(&data(&[9, 8, 7, 6]), medium.now_ms());
    tick_until(&mut n, &medium, 5_000, |n| n.queued_packets() == 0);

    let on_air = headers(&mut air);
    assert_eq!(on_air.len(), 4);
    let (split, single) = (on_air[0], on_air[2]);
    assert_eq!(on_air[1], split);
    assert_eq!(on_air[3], 9, "the promiscuous frame is sent raw");
    assert_eq!(
        messages(&cap.take(), Level::Debug, "TX "),
        [
            format!(
                "TX packet: 300 bytes, 2 frames, header 0x{split:02X} (seq {}, split): ok",
                split >> 4
            ),
            format!(
                "TX packet: 3 bytes, 1 frame, header 0x{single:02X} (seq {}): ok",
                single >> 4
            ),
            "TX packet: 4 bytes, 1 frame, promiscuous: ok".to_owned(),
        ]
    );
}

#[test]
fn tx_failures_are_logged_with_their_outcome() {
    let (medium, mut air, mut n) = online_node();
    let cap = capture(LevelFilter::Debug);
    n.radio_mut().inject_tx_error_on_call(2);
    n.handle_host_input(&data(&[0x66; 300]), medium.now_ms());
    tick_until(&mut n, &medium, 5_000, |n| n.queued_packets() == 0);
    let header = headers(&mut air)[0];
    let records = cap.take();
    assert_eq!(
        messages(&records, Level::Debug, "TX "),
        [format!(
            "TX packet: 300 bytes, 2 frames, header 0x{header:02X} (seq {}, split): radio error on frame 2/2: \
             simulated TX error; sent ERROR_TXFAILED",
            header >> 4
        )]
    );
    assert_eq!(
        messages(&records, Level::Warn, ""),
        ["radio transmit failed: simulated TX error"]
    );

    let (medium, mut air, mut n) = online_node();
    let cap = capture(LevelFilter::Debug);
    n.radio_mut().inject_timeout_on_call(2);
    n.handle_host_input(&[data(&[0x77; 300]), data(&[0x78])].concat(), medium.now_ms());
    tick_until(&mut n, &medium, 5_000, |n| n.queued_packets() == 0);
    let header = headers(&mut air)[0];
    let records = cap.take();
    assert_eq!(
        messages(&records, Level::Warn, ""),
        ["modem timed out during TX; hard-resetting the node and dropping 1 queued TX packet"]
    );
    assert_eq!(
        messages(&records, Level::Debug, "TX "),
        [format!(
            "TX packet: 300 bytes, 2 frames, header 0x{header:02X} (seq {}, split): modem timeout on frame 2/2; \
             sent ERROR_MODEM_TIMEOUT, ERROR_TXFAILED; hard reset",
            header >> 4
        )]
    );
    assert_eq!(
        messages(&records, Level::Debug, "dropping"),
        ["dropping 1-byte TX packet: queue cleared by reset"]
    );
}

#[test]
fn payload_hex_dumps_are_logged_only_at_trace() {
    let (medium, _air, mut n) = online_node();
    let rx = [&[0x40][..], b"Hello, RNode!", &[0, 1, 2, 3, 4, 5, 6]].concat();
    let tx: Vec<u8> = (0..17).collect();
    let exchange = |n: &mut Node<SimRadio, MemoryStorage>| {
        n.radio().inject_rx(rx.clone(), -42, 0x20);
        n.service_radio(medium.now_ms());
        n.handle_host_input(&data(&tx), medium.now_ms());
        tick_until(n, &medium, 5_000, |n| n.queued_packets() == 0);
        medium.advance_ms(150);
    };

    let cap = capture(LevelFilter::Info);
    exchange(&mut n);
    assert!(cap.take().is_empty(), "nothing per packet at info");

    cap.set_level(LevelFilter::Debug);
    exchange(&mut n);
    let records = cap.take();
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(messages(&records, Level::Debug, "RX packet").len(), 1);
    assert_eq!(messages(&records, Level::Debug, "TX packet").len(), 1);
    assert!(records.iter().all(|r| !r.message.contains('\n')), "no dump below trace");

    cap.set_level(LevelFilter::Trace);
    exchange(&mut n);
    assert_eq!(
        messages(&cap.take(), Level::Trace, ""),
        [
            [
                "RX payload, 20 bytes:",
                "0000  48 65 6C 6C 6F 2C 20 52  4E 6F 64 65 21 00 01 02  |Hello, RNode!...|",
                "0010  03 04 05 06                                       |....|",
            ]
            .join("\n"),
            [
                "TX payload, 17 bytes:",
                "0000  00 01 02 03 04 05 06 07  08 09 0A 0B 0C 0D 0E 0F  |................|",
                "0010  10                                                |.|",
            ]
            .join("\n"),
        ]
    );
}

#[test]
fn full_size_payload_dump_has_one_short_line_per_16_bytes() {
    let (medium, _air, mut n) = online_node();
    let cap = capture(LevelFilter::Trace);
    n.handle_host_input(&data(&[0xC0; MTU]), medium.now_ms());
    tick_until(&mut n, &medium, 5_000, |n| n.queued_packets() == 0);
    let dumps = messages(&cap.take(), Level::Trace, "TX payload");
    assert_eq!(dumps.len(), 1);
    let lines: Vec<&str> = dumps[0].lines().collect();
    assert_eq!(lines.len(), 1 + 32);
    assert_eq!(lines[0], "TX payload, 508 bytes:");
    for (i, line) in lines[1..].iter().enumerate() {
        assert!(line.starts_with(&format!("{:04X}  C0 C0", i * 16)), "{line}");
        assert!(line.len() <= 74, "{line}");
    }
    assert_eq!(
        lines[32],
        "01F0  C0 C0 C0 C0 C0 C0 C0 C0  C0 C0 C0 C0              |............|"
    );
}

#[test]
fn host_data_drops_are_logged_with_their_reason() {
    let medium = SimMedium::new();
    let config = NodeConfig {
        queue_max_packets: 1,
        ..NodeConfig::default()
    };
    let mut n = Node::new(medium.radio(), MemoryStorage::default(), config, 1);
    let cap = capture(LevelFilter::Debug);
    let input = [
        data(&[FEND; MTU + 1]), // 1018 escaped bytes on the wire, 509 after unescaping.
        data(&[]),
        data(&[1]),
        data(&[2, 3]),
        plain(CMD_PROMISC, &[1]),
        data(&[4; SINGLE_MTU + 1]),
    ]
    .concat();
    n.handle_host_input(&input, 0);
    assert_eq!(n.queued_packets(), 1);
    assert_eq!(
        messages(&cap.take(), Level::Debug, "dropping"),
        [
            "dropping 509-byte host data frame: exceeds the 508-byte MTU",
            "dropping empty host data frame",
            "dropping 2-byte host data frame: TX queue full (1/1 packets, 1/6144 bytes)",
            "dropping 256-byte host data frame: exceeds the 255-byte MTU",
        ]
    );
}

#[test]
fn tx_drops_are_logged_with_their_reason() {
    let (medium, _air, mut n) = online_node();
    let cap = capture(LevelFilter::Debug);
    // Queued as a normal packet, then unsendable once promiscuous mode limits it to one frame.
    n.handle_host_input(&data(&[0x33; 300]), medium.now_ms());
    n.handle_host_input(&plain(CMD_PROMISC, &[1]), medium.now_ms());
    tick_until(&mut n, &medium, 5_000, |n| n.queued_packets() == 0);
    // Packets still queued when the node resets.
    n.handle_host_input(&plain(CMD_RADIO_STATE, &[RADIO_STATE_OFF]), medium.now_ms());
    n.handle_host_input(&[data(&[1]), data(&[2, 3])].concat(), medium.now_ms());
    n.handle_host_input(&plain(CMD_RESET, &[CMD_RESET_BYTE]), medium.now_ms());
    assert_eq!(n.queued_packets(), 0);
    assert_eq!(
        messages(&cap.take(), Level::Debug, "dropping"),
        [
            "dropping 300-byte TX packet: exceeds the 255-byte single-frame MTU in promiscuous mode; \
             sent ERROR_TXFAILED",
            "dropping 1-byte TX packet: queue cleared by reset",
            "dropping 2-byte TX packet: queue cleared by reset",
        ]
    );
}

#[test]
fn rx_drops_are_logged_with_their_reason() {
    let mut n = node(1);
    init(&mut n);
    let cap = capture(LevelFilter::Debug);
    for frame in [
        vec![0x11, b'a'],       // First half, sequence 1...
        vec![0x21, b'b'],       // ...replaced by a first half with sequence 2...
        vec![0x21, b'c'],       // ...which completes.
        vec![0x31, b'x', b'y'], // A stray half...
        vec![0x40, b'z'],       // ...dropped by an unsplit packet.
        vec![],                 // No header at all.
        vec![0x51, b'p'],       // Pending when the node resets.
    ] {
        n.radio().inject_rx(frame, -42, 0x20);
    }
    n.service_radio(0);
    n.handle_host_input(&plain(CMD_RESET, &[CMD_RESET_BYTE]), 0);
    init(&mut n);
    n.handle_host_input(&plain(CMD_PROMISC, &[1]), 0);
    n.radio().inject_rx(vec![0x77; SINGLE_MTU + 1], -42, 0x20);
    n.service_radio(0);

    let records = cap.take();
    assert_eq!(
        messages(&records, Level::Debug, "dropping"),
        [
            "dropping 1-byte incomplete split packet (seq 1): next split frame has seq 2",
            "dropping 2-byte incomplete split packet (seq 3): next frame is not split",
            "dropping empty LoRa frame: no RNode header",
            "dropping 1-byte incomplete split packet (seq 5): reset",
            "dropping 256-byte promiscuous RX frame: exceeds the 255-byte single-frame MTU",
        ]
    );
    assert_eq!(
        messages(&records, Level::Debug, "RX "),
        [
            "RX packet: 2 bytes, 2 frames, header 0x21 (seq 2, split), RSSI -42 dBm, SNR 8.00 dB",
            "RX packet: 1 byte, 1 frame, header 0x40 (seq 4), RSSI -42 dBm, SNR 8.00 dB",
        ]
    );
}

/// What a scripted session shows to the outside, step by step: host output of both nodes, the
/// time each transmission completed and the node's next wake-up.
type Observed = (Vec<Vec<u8>>, Vec<u64>);

/// Two nodes exchange packets of every size class, with drops, a driver error, stray split halves
/// and a reset with packets queued on the way.
fn scripted_session() -> Observed {
    let medium = SimMedium::new();
    let mut a = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 1);
    let mut b = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 2);
    let mut outputs = vec![init(&mut a), init(&mut b)];
    let mut times = Vec::new();
    for (i, len) in [1usize, 254, 255, 508, 300].into_iter().enumerate() {
        let payload: Vec<u8> = (0..len).map(|j| (i + j) as u8).collect();
        let input = [data(&[0; MTU + 1]), data(&[]), data(&payload)].concat();
        if len == 300 {
            // Calls 1-6 sent the earlier packets; fail the second frame of this one.
            a.radio_mut().inject_tx_error_on_call(8);
        }
        a.handle_host_input(&input, medium.now_ms());
        tick_until(&mut a, &medium, 5_000, |n| n.queued_packets() == 0);
        times.extend([medium.now_ms(), a.next_wakeup_ms(medium.now_ms())]);
        // A stray split half, dropped by the unsplit packet after it.
        b.radio().inject_rx(vec![0x91, 0xEE], -101, 0xF6);
        b.radio().inject_rx(vec![0x40, 0xDD], -101, 0xF6);
        b.service_radio(medium.now_ms());
        outputs.extend([a.take_host_output(), b.take_host_output()]);
        medium.advance_ms(150);
    }
    // Packets queued while the radio is off are discarded by the reset, logged or not.
    a.handle_host_input(&plain(CMD_RADIO_STATE, &[RADIO_STATE_OFF]), medium.now_ms());
    a.handle_host_input(&[data(&[1]), data(&[2, 3])].concat(), medium.now_ms());
    a.handle_host_input(&plain(CMD_RESET, &[CMD_RESET_BYTE]), medium.now_ms());
    outputs.push(init(&mut a));
    tick_until(&mut a, &medium, 1_000, |_| false);
    b.service_radio(medium.now_ms());
    outputs.extend([a.take_host_output(), b.take_host_output()]);
    (outputs, times)
}

#[test]
fn logging_at_trace_does_not_change_host_output_or_timing() {
    let quiet = {
        let _cap = capture(LevelFilter::Off);
        scripted_session()
    };
    let cap = capture(LevelFilter::Trace);
    let traced = scripted_session();
    let records = cap.take();
    for prefix in ["RX packet", "TX packet", "dropping", "RX payload", "TX payload"] {
        assert!(
            records.iter().any(|r| r.message.starts_with(prefix)),
            "the session logs {prefix:?}"
        );
    }
    assert!(records.iter().any(|r| r.message.contains("radio error on frame 2/2")));
    assert!(records.iter().any(|r| r.message.ends_with("queue cleared by reset")));
    assert_eq!(traced, quiet);
}
