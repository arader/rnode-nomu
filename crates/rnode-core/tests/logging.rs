//! Per-packet logging (#42): debug lines for received, sent and dropped packets, trace hex dumps,
//! and no effect on what the node sends.

mod common;

use log::{Level, LevelFilter};

use common::log_capture::{Captured, capture};
use common::{cmd, data, init, listener, node, plain, split_frames, tick_until};
use rnode_core::kiss::*;
use rnode_core::{MemoryStorage, Node, NodeConfig, Radio, RadioSettings, RxSignal, SimMedium, SimRadio};

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
        messages(&cap.take(), Level::Debug, "TX packet"),
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
        messages(&records, Level::Debug, "TX packet"),
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
        messages(&records, Level::Debug, "TX packet"),
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
    assert_eq!(records.len(), 4, "{records:?}");
    assert_eq!(messages(&records, Level::Debug, "RX packet").len(), 1);
    assert_eq!(messages(&records, Level::Debug, "TX packet").len(), 1);
    assert_eq!(messages(&records, Level::Debug, "TX waiting").len(), 1);
    assert_eq!(messages(&records, Level::Debug, "TX after waiting").len(), 1);
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

/// Issue #51: the packets a stop request left queued are dropped at shutdown, with a debug line
/// each like the other drops, and counted at info.
#[test]
fn packets_queued_at_shutdown_are_dropped_with_their_reason() {
    let (medium, _air, mut n) = online_node();
    n.handle_host_input(&[data(&[1; 300]), data(&[2; 20]), data(&[3; 30])].concat(), 0);
    let cap = capture(LevelFilter::Debug);
    let mut asked = 0;
    while n.queued_packets() == 3 && medium.now_ms() < 5_000 {
        n.tick_with(medium.now_ms(), || {
            asked += 1;
            asked > 1
        });
        medium.advance_ms(1);
    }
    n.shutdown();
    let records = cap.take();
    assert_eq!(messages(&records, Level::Debug, "TX packet").len(), 1, "{records:?}");
    assert_eq!(
        messages(&records, Level::Debug, "dropping"),
        [
            "dropping 20-byte TX packet: shutting down",
            "dropping 30-byte TX packet: shutting down",
        ]
    );
    assert_eq!(
        messages(&records, Level::Info, "shutting down"),
        ["shutting down with 2 TX packets queued; dropping them"]
    );
    assert_eq!(n.queued_packets(), 0);
    assert!(!n.is_radio_online());

    // Packets queued while the radio is off are dropped the same way.
    let mut offline = node(2);
    offline.handle_host_input(&data(&[4; 5]), 0);
    offline.shutdown();
    let records = cap.take();
    assert_eq!(
        messages(&records, Level::Info, "shutting down"),
        ["shutting down with 1 TX packet queued; dropping it"]
    );
    assert_eq!(
        messages(&records, Level::Debug, "dropping"),
        ["dropping 5-byte TX packet: shutting down"]
    );

    // Nothing queued, nothing to report.
    offline.shutdown();
    assert!(messages(&cap.take(), Level::Info, "shutting down").is_empty());
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

/// The level and message of every record in `records`.
fn lines(records: &[Captured]) -> Vec<(Level, &str)> {
    records.iter().map(|r| (r.level, r.message.as_str())).collect()
}

/// Run `n` as the daemon's event loop does while idle: service the radio and tick every
/// `STATUS_INTERVAL_MS` of medium time until `until_ms`, calling `before` ahead of each step.
fn run_idle(
    n: &mut Node<SimRadio, MemoryStorage>,
    medium: &SimMedium,
    until_ms: u64,
    mut before: impl FnMut(&mut Node<SimRadio, MemoryStorage>, u64),
) {
    while medium.now_ms() < until_ms {
        before(n, medium.now_ms());
        n.service_radio(medium.now_ms());
        n.tick(medium.now_ms());
        medium.advance_ms(rnode_core::node::STATUS_INTERVAL_MS);
    }
}

/// Issue #53: a DCD read that fails on every 3 ms status tick for 24 s logs one warning, then a
/// summary per 10 s, and an info line once it has worked for 10 s; not 8000 warnings.
#[test]
fn persistent_status_failures_log_one_warning_then_summaries_and_a_recovery() {
    let (medium, _air, mut n) = online_node();
    n.radio_mut().inject_dcd_errors(8_000);
    let cap = capture(LevelFilter::Info);
    run_idle(&mut n, &medium, 40_000, |_, _| {});
    // Failures at 0, 3, ... 23997 ms; summaries at the first failure 10 s after the last line.
    assert_eq!(
        lines(&cap.take()),
        [
            (Level::Warn, "radio DCD failed: simulated DCD error"),
            (
                Level::Warn,
                "radio DCD failed 3334 more times in the last 10.0 s: simulated DCD error"
            ),
            (
                Level::Warn,
                "radio DCD failed 3334 more times in the last 10.0 s: simulated DCD error"
            ),
            (Level::Info, "radio DCD recovered after 8000 failures in 24.0 s"),
        ]
    );
}

/// Issue #53: a call that fails every other time stays one episode: a warning, a summary per 10 s
/// and one recovery, instead of a warning and a recovery line for each of its 3334 failures.
#[test]
fn intermittent_status_failures_are_summarised_as_one_episode() {
    let (medium, _air, mut n) = online_node();
    let cap = capture(LevelFilter::Info);
    run_idle(&mut n, &medium, 40_000, |n, now| {
        if now < 20_000 && now % 6 == 0 {
            n.radio_mut().inject_dcd_errors(1);
        }
    });
    assert_eq!(
        lines(&cap.take()),
        [
            (Level::Warn, "radio DCD failed: simulated DCD error"),
            (
                Level::Warn,
                "radio DCD failed 1667 more times in the last 10.0 s: simulated DCD error"
            ),
            (Level::Info, "radio DCD recovered after 3334 failures in 20.0 s"),
        ]
    );
}

/// Issue #53: the RSSI read and the receive poll are limited the same way, each on its own; a
/// fault that recurs after a recovery is a new episode with a new first warning.
#[test]
fn rssi_and_rx_failures_are_rate_limited_per_call() {
    let (medium, _air, mut n) = online_node();
    n.radio_mut().inject_rssi_errors(5);
    n.radio_mut().inject_rx_errors(4_000);
    let cap = capture(LevelFilter::Info);
    run_idle(&mut n, &medium, 30_000, |n, now| {
        if now == 25_002 {
            n.radio_mut().inject_rssi_errors(1);
        }
    });
    assert_eq!(
        lines(&cap.take()),
        [
            (Level::Warn, "radio RX service failed: simulated RX error"),
            (Level::Warn, "radio RSSI failed: simulated RSSI error"),
            (
                Level::Warn,
                "radio RX service failed 3334 more times in the last 10.0 s: simulated RX error"
            ),
            (Level::Info, "radio RSSI recovered after 5 failures in 12 ms"),
            (Level::Info, "radio RX service recovered after 4000 failures in 12.0 s"),
            (Level::Warn, "radio RSSI failed: simulated RSSI error"),
        ]
    );
}

/// Issue #54: one info line on each online and offline transition, saying why, with the settings
/// as the radio applied them (here a 30 kHz bandwidth quantised to 31.25 kHz), and nothing when a
/// request changes nothing.
#[test]
fn radio_online_and_offline_transitions_are_logged_at_info_with_the_settings() {
    let medium = SimMedium::new();
    let saved = RadioSettings {
        frequency: 433_775_000,
        bandwidth: 30_000,
        spreading_factor: 10,
        coding_rate: 6,
        tx_power: 0,
    };
    let mut n = Node::new(
        medium.radio(),
        MemoryStorage::with_settings(saved),
        NodeConfig::default(),
        1,
    );
    let cap = capture(LevelFilter::Info);
    n.boot(0);
    n.handle_host_input(&plain(CMD_RADIO_STATE, &[RADIO_STATE_ON]), 0);
    n.handle_host_input(&plain(CMD_RADIO_STATE, &[RADIO_STATE_OFF]), 0);
    n.handle_host_input(&plain(CMD_RADIO_STATE, &[RADIO_STATE_OFF]), 0);
    init(&mut n);
    n.handle_host_input(&plain(CMD_RESET, &[CMD_RESET_BYTE]), 0);
    n.shutdown();
    n.shutdown();
    let records = cap.take();
    assert!(records.iter().all(|r| r.level == Level::Info), "{records:?}");
    let saved_line = "433.775 MHz, 31.25 kHz, SF10, CR 4/6, 0 dBm";
    let host_line = "867.2 MHz, 125 kHz, SF7, CR 4/5, 14 dBm";
    assert_eq!(
        messages(&records, Level::Info, ""),
        [
            format!("radio online (saved settings): {saved_line}"),
            format!("radio offline (host request): {saved_line}"),
            format!("radio online (host request): {host_line}"),
            format!("radio offline (reset): {host_line}"),
            format!("radio online (saved settings): {saved_line}"),
            format!("radio offline (shutdown): {saved_line}"),
        ]
    );

    // A radio that fails to start never comes online.
    let mut radio = medium.radio();
    radio.inject_begin_failures(1);
    let mut failing = Node::new(radio, MemoryStorage::with_settings(saved), NodeConfig::default(), 2);
    failing.boot(0);
    assert_eq!(
        lines(&cap.take()),
        [(Level::Warn, "radio begin failed: simulated begin failure")]
    );
}

/// The frequency and bandwidth formats in the online and offline lines.
#[test]
fn radio_settings_lines_print_frequencies_without_trailing_zeros() {
    let mut n = node(1);
    let cap = capture(LevelFilter::Info);
    for (frequency, bandwidth) in [
        (868_000_000_u32, 7_800_u32),
        (915_000_001, 10_400),
        (2_400_000_000, 62_500),
        (169_400_000, 500_000),
    ] {
        n.handle_host_input(&cmd(CMD_FREQUENCY, &frequency.to_be_bytes()), 0);
        n.handle_host_input(&cmd(CMD_BANDWIDTH, &bandwidth.to_be_bytes()), 0);
        n.handle_host_input(&[plain(CMD_TXPOWER, &[0]), plain(CMD_SF, &[12])].concat(), 0);
        n.handle_host_input(&plain(CMD_RADIO_STATE, &[RADIO_STATE_ON]), 0);
        n.handle_host_input(&plain(CMD_RADIO_STATE, &[RADIO_STATE_OFF]), 0);
    }
    assert_eq!(
        messages(&cap.take(), Level::Info, "radio online"),
        [
            "radio online (host request): 868 MHz, 7.8 kHz, SF12, CR 4/5, 0 dBm",
            "radio online (host request): 915.000001 MHz, 10.4 kHz, SF12, CR 4/5, 0 dBm",
            "radio online (host request): 2400 MHz, 62.5 kHz, SF12, CR 4/5, 0 dBm",
            "radio online (host request): 169.4 MHz, 500 kHz, SF12, CR 4/5, 0 dBm",
        ]
    );
}

/// When the host retunes the online radio, one info line gives the settings the radio now runs
/// with, 100 ms after the first config command of a burst, so the online line is never stale for
/// long. Nothing is logged when the settings end up unchanged, and going offline reports them.
#[test]
fn host_retunes_of_the_online_radio_are_logged_once_per_burst() {
    let (medium, _air, mut n) = online_node();
    let cap = capture(LevelFilter::Info);
    let info = |cap: &common::log_capture::Capture| messages(&cap.take(), Level::Info, "");

    // RNodeInterface's initRadio() as it reconnects with other settings: five config commands and
    // RADIO_STATE on, back to back, here arriving in two reads that split a frame.
    let burst = [
        cmd(CMD_FREQUENCY, &868_100_000u32.to_be_bytes()),
        cmd(CMD_BANDWIDTH, &250_000u32.to_be_bytes()),
        plain(CMD_TXPOWER, &[17]),
        plain(CMD_SF, &[9]),
        plain(CMD_CR, &[6]),
        plain(CMD_RADIO_STATE, &[RADIO_STATE_ON]),
    ]
    .concat();
    medium.set_now_ms(1_000);
    n.handle_host_input(&burst[..10], 1_000);
    n.handle_host_input(&burst[10..], 1_001);
    run_idle(&mut n, &medium, 1_100, |_, _| {});
    assert!(info(&cap).is_empty(), "not before 100 ms");
    run_idle(&mut n, &medium, 2_000, |_, _| {});
    assert_eq!(
        info(&cap),
        ["radio settings changed: 868.1 MHz, 250 kHz, SF9, CR 4/6, 17 dBm"]
    );

    // The same settings again, or a change undone within the burst: nothing changed.
    n.handle_host_input(&burst, 2_000);
    n.handle_host_input(&plain(CMD_SF, &[10]), 2_001);
    n.handle_host_input(&plain(CMD_SF, &[9]), 2_050);
    run_idle(&mut n, &medium, 3_000, |_, _| {});
    assert!(info(&cap).is_empty());

    // Each later burst gets its own line.
    n.handle_host_input(&plain(CMD_SF, &[10]), 3_000);
    run_idle(&mut n, &medium, 4_000, |_, _| {});
    n.handle_host_input(&plain(CMD_CR, &[8]), 4_000);
    run_idle(&mut n, &medium, 5_000, |_, _| {});
    assert_eq!(
        info(&cap),
        [
            "radio settings changed: 868.1 MHz, 250 kHz, SF10, CR 4/6, 17 dBm",
            "radio settings changed: 868.1 MHz, 250 kHz, SF10, CR 4/8, 17 dBm",
        ]
    );

    // Turned off right after a retune: the offline line has the new settings.
    n.handle_host_input(&plain(CMD_TXPOWER, &[10]), 5_000);
    n.handle_host_input(&plain(CMD_RADIO_STATE, &[RADIO_STATE_OFF]), 5_001);
    run_idle(&mut n, &medium, 6_000, |_, _| {});
    assert_eq!(
        info(&cap),
        ["radio offline (host request): 868.1 MHz, 250 kHz, SF10, CR 4/8, 10 dBm"]
    );

    // Settings changed while offline are in the online line, not a change line.
    n.handle_host_input(&plain(CMD_SF, &[8]), 6_000);
    n.handle_host_input(&plain(CMD_RADIO_STATE, &[RADIO_STATE_ON]), 6_001);
    run_idle(&mut n, &medium, 7_000, |_, _| {});
    assert_eq!(
        info(&cap),
        ["radio online (host request): 868.1 MHz, 250 kHz, SF8, CR 4/8, 10 dBm"]
    );
}

/// What a scripted session shows to the outside, step by step: host output of both nodes, the
/// time each transmission completed and the node's next wake-up.
type Observed = (Vec<Vec<u8>>, Vec<u64>);

/// Two nodes exchange packets of every size class, with drops, driver errors (TX, status reads and
/// the receive poll), stray split halves and a reset with packets queued on the way.
fn scripted_session() -> Observed {
    let medium = SimMedium::new();
    let mut a = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 1);
    let mut b = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 2);
    let mut outputs = vec![init(&mut a), init(&mut b)];
    a.radio_mut().inject_dcd_errors(3);
    a.radio_mut().inject_rssi_errors(2);
    b.radio_mut().inject_rx_errors(2);
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
    for failure in ["radio DCD failed", "radio RSSI failed", "radio RX service failed"] {
        assert!(records.iter().any(|r| r.message.starts_with(failure)), "{failure}");
    }
    assert_eq!(traced, quiet);
}

/// Issue #69: frames that fail their payload CRC are dropped by the driver, as the firmware's
/// `handleDio0Rise()` does, and logged at debug: one line per service of the radio that found
/// any, with the count since the last line and the signal of the last one.
#[test]
fn crc_failed_frames_are_logged_at_debug_with_the_count_since_the_last_report() {
    let (_medium, _air, mut n) = online_node();
    let cap = capture(LevelFilter::Debug);
    let signal = |rssi_dbm, snr_raw| Some(RxSignal { rssi_dbm, snr_raw });
    n.radio().inject_rx_crc_error(signal(-97, 0xF3));
    n.service_radio(0);
    // A driver that counted several frames before the node took them, the last without a signal.
    n.radio().inject_rx_crc_error(signal(-101, 0xEC));
    n.radio().inject_rx_crc_error(signal(-90, 0x08));
    n.radio_mut().poll_rx().unwrap();
    n.radio_mut().poll_rx().unwrap();
    n.service_radio(3);
    n.radio().inject_rx_crc_error(None);
    n.radio().inject_rx_crc_error(None);
    n.radio_mut().poll_rx().unwrap();
    n.service_radio(6);
    n.service_radio(6);
    n.radio().inject_rx_crc_error(None);
    n.service_radio(9);
    // A good frame after a bad one: the poll that drops the bad frame ends that service, as on a
    // modem with one receive buffer; the good frame is delivered by the next.
    n.radio().inject_rx_crc_error(signal(-50, 0x10));
    n.radio().inject_rx(vec![0x40, 1], -50, 0x10);
    n.service_radio(12);
    n.service_radio(15);

    assert_eq!(
        lines(&cap.take()),
        [
            (
                Level::Debug,
                "dropping received LoRa frame: CRC error (RSSI -97 dBm, SNR -3.25 dB)"
            ),
            (
                Level::Debug,
                "dropping 2 received LoRa frames: CRC error (last: RSSI -90 dBm, SNR 2.00 dB)"
            ),
            (Level::Debug, "dropping 2 received LoRa frames: CRC error"),
            (Level::Debug, "dropping received LoRa frame: CRC error"),
            (
                Level::Debug,
                "dropping received LoRa frame: CRC error (RSSI -50 dBm, SNR 4.00 dB)"
            ),
            (
                Level::Debug,
                "RX packet: 1 byte, 1 frame, header 0x40 (seq 4), RSSI -50 dBm, SNR 4.00 dB"
            ),
        ]
    );
    // The host hears nothing of the dropped frames.
    let frames = split_frames(&n.take_host_output());
    assert_eq!(frames.iter().filter(|(cmd, _)| *cmd == CMD_DATA).count(), 1);
}

/// The debug lines of `records` about waits for channel access, in order.
fn waits(records: &[Captured]) -> Vec<String> {
    messages(records, Level::Debug, "TX ")
        .into_iter()
        .filter(|line| line.starts_with("TX waiting") || line.starts_with("TX after waiting"))
        .collect()
}

/// Issue #69: every access to the channel waits for DIFS and a random contention window
/// (firmware `tx_queue_handler()`); the wait gets one debug line when it starts and one when the
/// node transmits. At 125 kHz and SF7 a slot is 24 ms and DIFS 48 ms; seed 1 draws a window of
/// 8 slots, so the packet goes out 48 + 8 × 24 = 240 ms after it was queued.
#[test]
fn a_wait_for_the_contention_window_is_logged_when_it_starts_and_ends() {
    let (medium, _air, mut n) = online_node();
    let cap = capture(LevelFilter::Debug);
    n.handle_host_input(&data(&[1, 2, 3]), 0);
    run_idle(&mut n, &medium, 1_000, |_, _| {});
    let records = cap.take();
    assert_eq!(
        messages(&records, Level::Debug, "TX "),
        [
            "TX waiting for channel access: contention window; 1 packet queued".to_owned(),
            "TX after waiting 240 ms for channel access: contention window 240 ms".to_owned(),
            messages(&records, Level::Debug, "TX packet")[0].clone(),
        ]
    );
}

/// A node that queues a packet while another node's frame is on the air waits for the carrier,
/// then for DIFS and its contention window (seed 2 also draws 8 slots: 240 ms).
#[test]
fn a_wait_for_a_carrier_is_logged_with_the_time_for_each_reason() {
    let medium = SimMedium::new();
    let mut a = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 1);
    let mut b = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 2);
    init(&mut a);
    init(&mut b);
    a.handle_host_input(&data(&[1; 10]), 0);
    let cap = capture(LevelFilter::Debug);
    // a transmits at 240 ms, and the simulated frame keeps the channel busy until 340 ms: b,
    // ticking every 3 ms, sees the carrier from 252 to 339 ms and a free channel at 342 ms.
    while medium.now_ms() < 1_000 {
        let now = medium.now_ms();
        if now == 252 {
            b.handle_host_input(&data(&[2; 20]), now);
        }
        for n in [&mut a, &mut b] {
            n.service_radio(now);
            n.tick(now);
        }
        medium.advance_ms(rnode_core::node::STATUS_INTERVAL_MS);
    }
    let b_waits: Vec<String> = waits(&cap.take())
        .into_iter()
        .filter(|line| line.contains("carrier"))
        .collect();
    assert_eq!(
        b_waits,
        [
            "TX waiting for channel access: carrier detected; 1 packet queued",
            "TX after waiting 330 ms for channel access: carrier detected 90 ms, contention window 240 ms",
        ]
    );
}

/// With interference avoidance on, the -292 dBm noise-floor sentinel makes the idle channel look
/// like interference until 128 samples have been taken (381 ms); then DIFS and the contention
/// window follow. A failed DCD read leaves CSMA/CA without an observation: no channel status.
#[test]
fn waits_for_interference_and_without_channel_status_are_logged_with_their_reason() {
    let medium = SimMedium::new();
    let config = NodeConfig {
        avoid_interference: true,
        ..NodeConfig::default()
    };
    let mut n = Node::new(medium.radio(), MemoryStorage::default(), config, 1);
    init(&mut n);
    let cap = capture(LevelFilter::Debug);
    n.handle_host_input(&data(&[1, 2, 3]), 0);
    run_idle(&mut n, &medium, 1_500, |_, _| {});
    assert_eq!(
        waits(&cap.take()),
        [
            "TX waiting for channel access: interference; 1 packet queued",
            "TX after waiting 621 ms for channel access: interference 381 ms, contention window 240 ms",
        ]
    );

    // Each tick reads DCD twice (the status sample and the access check): 20 errors are 30 ms.
    let (medium, _air, mut n) = online_node();
    n.radio_mut().inject_dcd_errors(20);
    let cap = capture(LevelFilter::Debug);
    n.handle_host_input(&data(&[1, 2, 3]), 0);
    run_idle(&mut n, &medium, 1_000, |_, _| {});
    let records = cap.take();
    assert_eq!(
        waits(&records),
        [
            "TX waiting for channel access: no channel status; 1 packet queued",
            "TX after waiting 270 ms for channel access: no channel status 30 ms, contention window 240 ms",
        ]
    );
    assert_eq!(
        messages(&records, Level::Warn, ""),
        ["radio DCD failed: simulated DCD error"]
    );
}

/// The airtime limit blocks TX until the short-term airtime has aged out of its two 7.5 s bins
/// (firmware `AIRTIME_BINLEN_MS`), and only the once-a-second update (every 333 samples) sees
/// that: the first packet's airtime is in bin 0, so the lock set at 1002 ms lasts until the update
/// at 15 996 ms, the first in bin 2. That wait of 15 s still gives one line when it starts and one
/// when it ends. Seed 1's third draw, after a window and a header byte, is 14 slots: 384 ms.
#[test]
fn a_wait_for_the_airtime_limit_logs_two_lines_however_long_it_lasts() {
    let (medium, _air, mut n) = online_node();
    n.handle_host_input(&cmd(CMD_ST_ALOCK, &1u16.to_be_bytes()), 0); // 0.01 %
    n.handle_host_input(&data(&[1, 2, 3]), 0);
    run_idle(&mut n, &medium, 1_000, |_, _| {});
    let cap = capture(LevelFilter::Debug);
    n.handle_host_input(&data(&[4, 5, 6]), medium.now_ms());
    run_idle(&mut n, &medium, 20_000, |_, _| {});
    assert_eq!(n.queued_packets(), 0);
    assert_eq!(
        waits(&cap.take()),
        [
            "TX waiting for channel access: airtime limit; 1 packet queued",
            "TX after waiting 15.4 s for channel access: airtime limit 15.0 s, contention window 384 ms",
        ]
    );
}

/// Going offline ends a wait (#80 review): a reset drops the waiting packet, and the packet queued
/// after it starts a fresh wait that counts only its own 240 ms. The first wait has no channel
/// status until 300 ms (200 DCD errors, two reads per tick), so a carried-over wait would read
/// `TX after waiting 540 ms for channel access: no channel status 300 ms, contention window 240 ms`.
/// A modem TX timeout resets the node too, but it cannot end a wait: the wait ended when access
/// was granted, before the transmission.
#[test]
fn a_reset_ends_a_wait_and_the_next_wait_is_logged_afresh() {
    let (medium, _air, mut n) = online_node();
    n.handle_host_input(&plain(CMD_CONF_SAVE, &[0]), 0); // So the reset brings the radio back.
    n.radio_mut().inject_dcd_errors(200);
    let cap = capture(LevelFilter::Debug);
    n.handle_host_input(&data(&[1, 2, 3]), 0);
    run_idle(&mut n, &medium, 1_000, |n, now| {
        if now == 300 {
            n.handle_host_input(&plain(CMD_RESET, &[CMD_RESET_BYTE]), now);
            n.handle_host_input(&data(&[4, 5, 6]), now);
        }
    });
    let records = cap.take();
    assert_eq!(
        messages(&records, Level::Debug, "dropping"),
        ["dropping 3-byte TX packet: queue cleared by reset"]
    );
    assert_eq!(
        waits(&records),
        [
            "TX waiting for channel access: no channel status; 1 packet queued",
            "TX waiting for channel access: contention window; 1 packet queued",
            "TX after waiting 240 ms for channel access: contention window 240 ms",
        ]
    );
    assert_eq!(n.queued_packets(), 0);
}

/// The same when the host turns the radio off and on: the packet stays queued while the radio is
/// off, and waits afresh once it is back. A carried-over wait would charge the 300 ms offline to
/// the reason seen last: `TX after waiting 840 ms … no channel status 600 ms, …`.
#[test]
fn going_offline_ends_a_wait_and_the_packet_waits_afresh_when_the_radio_is_back() {
    let (medium, _air, mut n) = online_node();
    n.radio_mut().inject_dcd_errors(200);
    let cap = capture(LevelFilter::Debug);
    n.handle_host_input(&data(&[1, 2, 3]), 0);
    run_idle(&mut n, &medium, 1_500, |n, now| match now {
        300 => n.handle_host_input(&plain(CMD_RADIO_STATE, &[RADIO_STATE_OFF]), now),
        600 => n.handle_host_input(&plain(CMD_RADIO_STATE, &[RADIO_STATE_ON]), now),
        _ => {}
    });
    let records = cap.take();
    assert_eq!(
        waits(&records),
        [
            "TX waiting for channel access: no channel status; 1 packet queued",
            "TX waiting for channel access: contention window; 1 packet queued",
            "TX after waiting 240 ms for channel access: contention window 240 ms",
        ]
    );
    assert_eq!(messages(&records, Level::Debug, "TX packet").len(), 1);
    assert_eq!(n.queued_packets(), 0);
}

/// Two nodes exercise every new debug line (#69): CRC errors, and waits for the contention
/// window, a carrier, interference, the airtime limit and a failed DCD read.
fn channel_access_session() -> Observed {
    let medium = SimMedium::new();
    let config = NodeConfig {
        avoid_interference: true,
        ..NodeConfig::default()
    };
    let mut a = Node::new(medium.radio(), MemoryStorage::default(), config, 1);
    let mut b = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 2);
    let mut outputs = vec![init(&mut a), init(&mut b)];
    let mut times = Vec::new();
    a.handle_host_input(&data(&[1; 10]), 0);
    while medium.now_ms() < 25_000 {
        let now = medium.now_ms();
        match now {
            252 => b.handle_host_input(&data(&[2; 20]), now),
            900 => {
                a.radio_mut().inject_dcd_errors(20);
                a.handle_host_input(&data(&[3; 30]), now);
            }
            1_800 => {
                b.radio().inject_rx_crc_error(Some(RxSignal {
                    rssi_dbm: -97,
                    snr_raw: 0xF3,
                }));
                b.radio().inject_rx_crc_error(None);
                b.handle_host_input(&cmd(CMD_ST_ALOCK, &1u16.to_be_bytes()), now);
                b.handle_host_input(&[data(&[4; 40]), data(&[5; 50])].concat(), now);
            }
            _ => {}
        }
        for n in [&mut a, &mut b] {
            n.service_radio(now);
            n.tick(now);
        }
        if now % 300 == 0 {
            times.extend([a.next_wakeup_ms(now), b.next_wakeup_ms(now)]);
            outputs.extend([a.take_host_output(), b.take_host_output()]);
        }
        medium.advance_ms(rnode_core::node::STATUS_INTERVAL_MS);
    }
    outputs.extend([a.take_host_output(), b.take_host_output()]);
    assert_eq!((a.queued_packets(), b.queued_packets()), (0, 0));
    (outputs, times)
}

#[test]
fn channel_access_and_crc_logging_does_not_change_host_output_or_timing() {
    let quiet = {
        let _cap = capture(LevelFilter::Off);
        channel_access_session()
    };
    let cap = capture(LevelFilter::Trace);
    let traced = channel_access_session();
    let records = cap.take();
    for reason in [
        "contention window",
        "carrier detected",
        "interference",
        "airtime limit",
        "no channel status",
    ] {
        assert!(
            waits(&records).iter().any(|line| line.contains(reason)),
            "the session waits for {reason}: {records:?}"
        );
    }
    assert_eq!(
        messages(&records, Level::Debug, "dropping received LoRa frame: CRC error"),
        [
            "dropping received LoRa frame: CRC error (RSSI -97 dBm, SNR -3.25 dB)",
            "dropping received LoRa frame: CRC error",
        ]
    );
    assert_eq!(traced, quiet);
}
