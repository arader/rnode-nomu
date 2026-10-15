//! Radio setter failures (#46): the node keeps the value the modem last accepted, as the
//! firmware's setters, which read the value in use back, would report it.
//!
//! `init()` configures 867.2 MHz, 125 kHz, 14 dBm, SF7 and CR 4/5. The golden values below are
//! those of the firmware for that modulation: `updateBitrate()` gives a 1.024 ms symbol, a
//! 24-symbol preamble (25 ms), a 21 ms header, a 24 ms CSMA slot and 48 ms DIFS, and
//! `add_airtime()` charges a 100-byte frame (816 / 28 × 5 + 24 + 0.25 + 8) × 1.024 = 182.2 ms.

mod common;

use log::{Level, LevelFilter};

use common::log_capture::capture;
use common::{cmd, data, last_chtm_airtime, lines, online_node, plain, run_until_sent, split_frames};
use rnode_core::kiss::*;
use rnode_core::sim::Setter;
use rnode_core::{MemoryStorage, Node, NodeConfig, Radio, SimMedium, SimRadio};

/// `CMD_STAT_PHYPRM` for 125 kHz, SF7 and CR 4/5 or 4/6 (firmware `kiss_indicate_phy_stats()`):
/// symbol time 1024 µs, 976 symbols/s, 24 preamble symbols, 25 ms preamble, 24 ms slot, 48 ms DIFS.
const PHYPRM_125K_SF7: [u8; 12] = [0x04, 0x00, 0x03, 0xD0, 0x00, 0x18, 0x00, 0x19, 0x00, 0x18, 0x00, 0x30];

/// Assert that `radio` runs the settings and timing `init()` gave it.
fn assert_init_settings(radio: &SimRadio, what: &str) {
    assert_eq!(radio.frequency(), 867_200_000, "{what}");
    assert_eq!(radio.bandwidth(), 125_000, "{what}");
    assert_eq!(radio.tx_power(), 14, "{what}");
    assert_eq!(radio.spreading_factor(), 7, "{what}");
    assert_eq!(radio.coding_rate(), 5, "{what}");
    assert_eq!(radio.preamble_length(), 24, "{what}");
    assert_eq!(radio.dcd_timing(), (25, 21), "{what}");
}

#[test]
fn a_failed_setter_keeps_the_last_accepted_value() {
    // Each request would change what the checks below observe if it took effect: 31.25 kHz and
    // SF10 lengthen the CSMA slot, DIFS, preamble and DCD timing, CR 4/8 the airtime, and
    // 868.1 MHz the channel the listener hears.
    let cases = [
        (
            Setter::Frequency,
            cmd(CMD_FREQUENCY, &868_100_000u32.to_be_bytes()),
            vec![FEND, CMD_FREQUENCY, 0x33, 0xB0, 0x6C, 0x00, FEND], // 867 200 000
            "radio set_frequency(868.1 MHz) failed: simulated set_frequency error; keeping 867.2 MHz",
        ),
        (
            Setter::Bandwidth,
            cmd(CMD_BANDWIDTH, &31_250u32.to_be_bytes()),
            vec![FEND, CMD_BANDWIDTH, 0x00, 0x01, 0xE8, 0x48, FEND], // 125 000
            "radio set_bandwidth(31.25 kHz) failed: simulated set_bandwidth error; keeping 125 kHz",
        ),
        (
            Setter::TxPower,
            plain(CMD_TXPOWER, &[17]),
            vec![FEND, CMD_TXPOWER, 14, FEND],
            "radio set_tx_power(17 dBm) failed: simulated set_tx_power error; keeping 14 dBm",
        ),
        (
            Setter::SpreadingFactor,
            plain(CMD_SF, &[10]),
            vec![FEND, CMD_SF, 7, FEND],
            "radio set_spreading_factor(SF10) failed: simulated set_spreading_factor error; keeping SF7",
        ),
        (
            Setter::CodingRate,
            plain(CMD_CR, &[8]),
            vec![FEND, CMD_CR, 5, FEND],
            "radio set_coding_rate(CR 4/8) failed: simulated set_coding_rate error; keeping CR 4/5",
        ),
    ];
    for (setter, request, echo, warning) in cases {
        let what = format!("{setter:?}");
        let (medium, air, mut n) = online_node();
        let (control_medium, _control_air, mut control) = online_node();
        n.radio_mut().inject_setter_errors(setter, 1);
        let cap = capture(LevelFilter::Info);
        n.handle_host_input(&request, 0);

        // The echo reports the kept value, and no CMD_STAT_PHYPRM precedes it.
        assert_eq!(n.take_host_output(), echo, "{what}");
        assert_eq!(lines(&cap.take()), [(Level::Warn, warning)], "{what}");
        // The simulated setter applies its value before it fails; the node set it back, and
        // changed neither the preamble length nor the DCD timing.
        assert_init_settings(n.radio(), &what);

        // CSMA and airtime are those of the kept settings: the packet goes out when a node that
        // never got the request sends it, costs the same airtime, and is heard on the channel.
        let payload = [0x5A; 99];
        n.handle_host_input(&data(&payload), 0);
        control.handle_host_input(&data(&payload), 0);
        let sent_at = run_until_sent(&mut n, &medium);
        assert_eq!(sent_at, run_until_sent(&mut control, &control_medium), "{what}");
        let out = n.take_host_output();
        assert_eq!(out, control.take_host_output(), "{what}");
        assert_eq!(last_chtm_airtime(&out), 121, "{what}: 182 ms of 15 s");
        assert_eq!(air.queued_rx(), 1, "{what}");
        assert!(
            cap.take().is_empty(),
            "{what}: no settings-changed line, no other warning"
        );
    }
}

/// Without the second call, the SX1262 driver would keep the rejected bandwidth in its cache and
/// program it with the next modulation change, while the node reported the old one.
#[test]
fn the_next_modulation_change_keeps_the_restored_value() {
    let (medium, mut air, mut n) = online_node();
    air.set_spreading_factor(8).unwrap();
    n.radio_mut().inject_setter_errors(Setter::Bandwidth, 1);
    n.handle_host_input(&cmd(CMD_BANDWIDTH, &31_250u32.to_be_bytes()), 0);
    n.handle_host_input(&plain(CMD_SF, &[8]), 0);
    // 125 kHz, SF8: 2048 µs symbols, 488 symbols/s, 18 preamble symbols (37 ms), 24 ms slot.
    let phyprm = vec![0x08, 0x00, 0x01, 0xE8, 0x00, 0x12, 0x00, 0x25, 0x00, 0x18, 0x00, 0x30];
    assert_eq!(
        split_frames(&n.take_host_output()),
        [
            (CMD_BANDWIDTH, 125_000u32.to_be_bytes().to_vec()),
            (CMD_STAT_PHYPRM, phyprm),
            (CMD_SF, vec![8]),
        ]
    );
    assert_eq!(n.radio().bandwidth(), 125_000);
    n.handle_host_input(&data(&[1, 2, 3]), 0);
    run_until_sent(&mut n, &medium);
    assert_eq!(air.queued_rx(), 1, "sent at 125 kHz, SF8");
}

#[test]
fn a_failed_restore_is_logged_as_well() {
    let (_medium, _air, mut n) = online_node();
    n.radio_mut().inject_setter_errors(Setter::Bandwidth, 2);
    let cap = capture(LevelFilter::Info);
    n.handle_host_input(&cmd(CMD_BANDWIDTH, &250_000u32.to_be_bytes()), 0);
    assert_eq!(n.take_host_output(), cmd(CMD_BANDWIDTH, &125_000u32.to_be_bytes()));
    assert_eq!(
        lines(&cap.take()),
        [
            (
                Level::Warn,
                "radio set_bandwidth(250 kHz) failed: simulated set_bandwidth error; keeping 125 kHz"
            ),
            (
                Level::Warn,
                "radio set_bandwidth(125 kHz) failed while restoring it: simulated set_bandwidth error"
            ),
        ]
    );
}

/// While the radio is offline the settings are only stored, as before. When the radio comes
/// online, a setter that fails leaves what `begin()` programmed (the simulated modem keeps its
/// 125 kHz, SF7 defaults); like the firmware's `startRadio()`, the node echoes nothing then, but
/// the online line and later queries report what the radio uses.
#[test]
fn a_setter_failing_at_start_up_keeps_what_begin_programmed() {
    let medium = SimMedium::new();
    let mut n = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 1);
    n.radio_mut().inject_setter_errors(Setter::Bandwidth, 1);
    n.radio_mut().inject_setter_errors(Setter::SpreadingFactor, 1);
    let cap = capture(LevelFilter::Info);
    let config = [
        cmd(CMD_FREQUENCY, &867_200_000u32.to_be_bytes()),
        cmd(CMD_BANDWIDTH, &250_000u32.to_be_bytes()),
        plain(CMD_TXPOWER, &[14]),
        plain(CMD_SF, &[9]),
        plain(CMD_CR, &[6]),
    ]
    .concat();
    n.handle_host_input(&config, 0);
    assert_eq!(n.take_host_output(), config, "offline, each request is echoed as sent");
    assert!(cap.take().is_empty());
    assert_eq!(n.radio().bandwidth(), 125_000, "no setter is called while offline");

    n.handle_host_input(&plain(CMD_RADIO_STATE, &[RADIO_STATE_ON]), 0);
    assert_eq!(
        n.take_host_output(),
        [
            cmd(CMD_STAT_PHYPRM, &PHYPRM_125K_SF7),
            plain(CMD_RADIO_STATE, &[RADIO_STATE_ON]),
        ]
        .concat()
    );
    assert_eq!(
        lines(&cap.take()),
        [
            (
                Level::Warn,
                "radio set_bandwidth(250 kHz) failed: simulated set_bandwidth error; keeping 125 kHz"
            ),
            (
                Level::Warn,
                "radio set_spreading_factor(SF9) failed: simulated set_spreading_factor error; keeping SF7"
            ),
            (
                Level::Info,
                "radio online (host request): 867.2 MHz, 125 kHz, SF7, CR 4/6, 14 dBm"
            ),
        ]
    );
    assert!(n.is_radio_online());
    assert_eq!(n.radio().bandwidth(), 125_000);
    assert_eq!(n.radio().spreading_factor(), 7);
    assert_eq!(n.radio().coding_rate(), 6);
    assert_eq!(n.radio().preamble_length(), 24);
    assert_eq!(n.radio().dcd_timing(), (25, 21));

    n.handle_host_input(&[cmd(CMD_BANDWIDTH, &[0; 4]), plain(CMD_SF, &[0xFF])].concat(), 0);
    assert_eq!(
        n.take_host_output(),
        [cmd(CMD_BANDWIDTH, &125_000u32.to_be_bytes()), plain(CMD_SF, &[7])].concat()
    );
}

/// The same for TX power and coding rate (#74 review): `begin()` leaves the simulated modem at
/// 2 dBm and CR 4/5, and the first packet's airtime is charged at CR 4/5, 121 as in
/// `a_failed_setter_keeps_the_last_accepted_value`; at CR 4/8 it would be 180 (271 ms of 15 s).
#[test]
fn tx_power_and_coding_rate_failing_at_start_up_keep_what_begin_programmed() {
    let medium = SimMedium::new();
    let mut n = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 1);
    n.radio_mut().inject_setter_errors(Setter::TxPower, 1);
    n.radio_mut().inject_setter_errors(Setter::CodingRate, 1);
    let cap = capture(LevelFilter::Info);
    let config = [
        cmd(CMD_FREQUENCY, &867_200_000u32.to_be_bytes()),
        cmd(CMD_BANDWIDTH, &125_000u32.to_be_bytes()),
        plain(CMD_TXPOWER, &[17]),
        plain(CMD_SF, &[7]),
        plain(CMD_CR, &[8]),
    ]
    .concat();
    n.handle_host_input(&config, 0);
    assert_eq!(n.take_host_output(), config, "offline, each request is echoed as sent");
    assert_eq!(
        (n.radio().tx_power(), n.radio().coding_rate()),
        (2, 5),
        "no setter is called"
    );

    n.handle_host_input(&plain(CMD_RADIO_STATE, &[RADIO_STATE_ON]), 0);
    assert_eq!(
        n.take_host_output(),
        [
            cmd(CMD_STAT_PHYPRM, &PHYPRM_125K_SF7),
            plain(CMD_RADIO_STATE, &[RADIO_STATE_ON]),
        ]
        .concat()
    );
    assert_eq!(
        lines(&cap.take()),
        [
            (
                Level::Warn,
                "radio set_tx_power(17 dBm) failed: simulated set_tx_power error; keeping 2 dBm"
            ),
            (
                Level::Warn,
                "radio set_coding_rate(CR 4/8) failed: simulated set_coding_rate error; keeping CR 4/5"
            ),
            (
                Level::Info,
                "radio online (host request): 867.2 MHz, 125 kHz, SF7, CR 4/5, 2 dBm"
            ),
        ]
    );
    assert_eq!((n.radio().tx_power(), n.radio().coding_rate()), (2, 5));
    n.handle_host_input(&[plain(CMD_TXPOWER, &[0xFF]), plain(CMD_CR, &[0xFF])].concat(), 0);
    assert_eq!(
        n.take_host_output(),
        [plain(CMD_TXPOWER, &[2]), plain(CMD_CR, &[5])].concat()
    );

    n.handle_host_input(&data(&[0x5A; 99]), 0);
    run_until_sent(&mut n, &medium);
    assert_eq!(last_chtm_airtime(&n.take_host_output()), 121);
}
