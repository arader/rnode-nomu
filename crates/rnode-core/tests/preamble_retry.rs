//! A preamble length the modem did not take (#81): the node keeps the PHY parameters of the new
//! modulation and calls `set_preamble_length()` again before each transmission and each time it
//! re-enters receive, until a call succeeds. Failures are logged like the DCD and RSSI reads.
//!
//! `init()` configures 125 kHz, SF7 and CR 4/5, with a 24-symbol preamble. The tests change to
//! SF8, for which the firmware's `updateBitrate()` gives 2.048 ms symbols, an 18-symbol preamble
//! (37 ms) and a 41 ms header.

mod common;

use log::{Level, LevelFilter};

use common::log_capture::capture;
use common::{cmd, data, last_chtm_airtime, lines, online_node, plain, run_until_sent};
use rnode_core::Radio;
use rnode_core::kiss::*;

/// `CMD_STAT_PHYPRM` for 125 kHz and SF8 (firmware `kiss_indicate_phy_stats()`): symbol time
/// 2048 µs, 488 symbols/s, 18 preamble symbols, 37 ms preamble, 24 ms slot, 48 ms DIFS.
const PHYPRM_125K_SF8: [u8; 12] = [0x08, 0x00, 0x01, 0xE8, 0x00, 0x12, 0x00, 0x25, 0x00, 0x18, 0x00, 0x30];

/// `CMD_STAT_PHYPRM` for 125 kHz and SF6: symbol time 512 µs, 1953 symbols/s, 47 preamble symbols
/// (⌈24 ms / 0.512 ms⌉), 25 ms preamble, 24 ms slot, 48 ms DIFS.
const PHYPRM_125K_SF6: [u8; 12] = [0x02, 0x00, 0x07, 0xA1, 0x00, 0x2F, 0x00, 0x19, 0x00, 0x18, 0x00, 0x30];

const FAILED: &str = "radio set_preamble_length failed: simulated set_preamble_length error";
const RETUNED: &str = "radio settings changed: 867.2 MHz, 125 kHz, SF8, CR 4/5, 14 dBm";
const RETUNED_SF6: &str = "radio settings changed: 867.2 MHz, 125 kHz, SF6, CR 4/5, 14 dBm";

#[test]
fn a_failed_preamble_length_is_retried_before_the_next_transmission() {
    let (medium, mut air, mut n) = online_node();
    air.set_spreading_factor(8).unwrap();
    n.radio_mut().inject_preamble_length_errors(1);
    let cap = capture(LevelFilter::Info);
    n.handle_host_input(&plain(CMD_SF, &[8]), 0);

    // The node keeps the parameters of SF8: it reports them, and times DCD and airtime by them.
    assert_eq!(
        n.take_host_output(),
        [cmd(CMD_STAT_PHYPRM, &PHYPRM_125K_SF8), plain(CMD_SF, &[8])].concat()
    );
    assert_eq!(n.radio().dcd_timing(), (37, 41));
    assert_eq!(
        n.radio().preamble_length(),
        24,
        "the modem still has the preamble of SF7"
    );
    assert_eq!(lines(&cap.take()), [(Level::Warn, FAILED)]);

    n.handle_host_input(&data(&[0x5A; 99]), 0);
    run_until_sent(&mut n, &medium);
    assert_eq!(
        n.radio().tx_preamble_lengths(),
        [18],
        "retried before the frame went out"
    );
    assert_eq!(air.queued_rx(), 1);
    // Firmware `add_airtime()`: (8 × 100 + 16 − 4 × 8 + 8 + 20) / (4 × 8) × 5 + 18 + 0.25 + 8 =
    // 153.125 symbols of 2.048 ms, 313 ms of the 15 s window.
    assert_eq!(last_chtm_airtime(&n.take_host_output()), 208);
    assert_eq!(
        lines(&cap.take()),
        [
            (Level::Info, RETUNED),
            (Level::Info, "radio set_preamble_length recovered after 1 failure"),
        ]
    );
}

/// When the retry before the transmission fails too, the frame goes out with the old preamble,
/// and the node calls `set_preamble_length()` again as it re-enters receive after the burst.
#[test]
fn a_preamble_length_still_failing_at_tx_is_retried_when_the_node_reenters_receive() {
    let (medium, _air, mut n) = online_node();
    n.radio_mut().inject_preamble_length_errors(2);
    let cap = capture(LevelFilter::Info);
    n.handle_host_input(&plain(CMD_SF, &[8]), 0);
    n.handle_host_input(&data(&[1]), 0);
    run_until_sent(&mut n, &medium);
    assert_eq!(n.radio().tx_preamble_lengths(), [24]);
    assert_eq!(n.radio().preamble_length(), 18, "taken as the node re-entered receive");

    n.handle_host_input(&data(&[2]), medium.now_ms());
    run_until_sent(&mut n, &medium);
    assert_eq!(n.radio().tx_preamble_lengths(), [24, 18]);
    // The second failure, before the transmission at 240 ms, is counted but not logged on its own.
    assert_eq!(
        lines(&cap.take()),
        [
            (Level::Warn, FAILED),
            (Level::Info, RETUNED),
            (
                Level::Info,
                "radio set_preamble_length recovered after 2 failures in 240 ms"
            ),
        ]
    );
}

/// Retries that keep failing are rate-limited like the DCD and RSSI reads: a warning for the first
/// failure, then one summary per 10 s, and an info line once a call works. The host makes the node
/// re-enter receive every 100 ms here (`CMD_PROMISC` off), and the node retries each time.
#[test]
fn repeated_preamble_length_failures_are_rate_limited() {
    let (_medium, _air, mut n) = online_node();
    n.radio_mut().inject_preamble_length_errors(usize::MAX);
    let cap = capture(LevelFilter::Info);
    n.handle_host_input(&plain(CMD_SF, &[8]), 0);
    for now in (100..25_000).step_by(100) {
        n.handle_host_input(&plain(CMD_PROMISC, &[0]), now);
    }
    assert_eq!(n.radio().preamble_length(), 24);
    n.radio_mut().inject_preamble_length_errors(0);
    n.handle_host_input(&plain(CMD_PROMISC, &[0]), 25_000);
    assert_eq!(n.radio().preamble_length(), 18);

    // Failures at 0, 100, … 24 900 ms; summaries at 10 and 20 s.
    let summary = "radio set_preamble_length failed 100 more times in the last 10.0 s: \
                   simulated set_preamble_length error";
    assert_eq!(
        lines(&cap.take()),
        [
            (Level::Warn, FAILED),
            (Level::Warn, summary),
            (Level::Warn, summary),
            (
                Level::Info,
                "radio set_preamble_length recovered after 250 failures in 24.9 s"
            ),
        ]
    );
}

/// A retune while a retry is pending (#84 review): the retune's own call programs the preamble
/// length of the new modulation, SF6's 47 symbols, and when it works nothing is left to retry.
/// The next frame goes out with 47 symbols, and `set_preamble_length()` is not called again,
/// neither before the frame nor when the node re-enters receive after it.
#[test]
fn a_retune_while_a_retry_is_pending_programs_the_new_length_and_ends_the_retry() {
    let (medium, mut air, mut n) = online_node();
    air.set_spreading_factor(6).unwrap();
    n.radio_mut().inject_preamble_length_errors(1);
    let cap = capture(LevelFilter::Info);
    n.handle_host_input(&plain(CMD_SF, &[8]), 0);
    assert_eq!(n.radio().preamble_length(), 24, "SF8's 18 symbols are pending");
    n.handle_host_input(&plain(CMD_SF, &[6]), 0);
    assert_eq!(
        n.take_host_output(),
        [
            cmd(CMD_STAT_PHYPRM, &PHYPRM_125K_SF8),
            plain(CMD_SF, &[8]),
            cmd(CMD_STAT_PHYPRM, &PHYPRM_125K_SF6),
            plain(CMD_SF, &[6]),
        ]
        .concat()
    );
    assert_eq!(n.radio().preamble_length(), 47);
    assert_eq!(n.radio().dcd_timing(), (25, 11));
    // Radio start, the failure at SF8, and SF6's call.
    assert_eq!(n.radio().preamble_length_calls(), 3);
    assert_eq!(
        lines(&cap.take()),
        [
            (Level::Warn, FAILED),
            (Level::Info, "radio set_preamble_length recovered after 1 failure"),
        ]
    );

    n.handle_host_input(&data(&[1, 2, 3]), 0);
    run_until_sent(&mut n, &medium);
    assert_eq!(n.radio().tx_preamble_lengths(), [47]);
    assert_eq!(n.radio().preamble_length_calls(), 3, "nothing left to retry");
    assert_eq!(air.queued_rx(), 1);
    assert_eq!(lines(&cap.take()), [(Level::Info, RETUNED_SF6)]);
}

/// When the retune's call fails too, the retry before the next frame programs the length of the
/// modulation the modem now has, SF6's 47 symbols, not the 18 of SF8 that failed first.
#[test]
fn a_retry_after_a_failed_retune_programs_the_length_of_the_new_modulation() {
    let (medium, mut air, mut n) = online_node();
    air.set_spreading_factor(6).unwrap();
    n.radio_mut().inject_preamble_length_errors(2);
    let cap = capture(LevelFilter::Info);
    n.handle_host_input(&plain(CMD_SF, &[8]), 0);
    medium.advance_ms(50);
    n.handle_host_input(&plain(CMD_SF, &[6]), medium.now_ms());
    assert_eq!(n.radio().preamble_length(), 24);
    assert_eq!(n.radio().dcd_timing(), (25, 11));

    n.handle_host_input(&data(&[1, 2, 3]), medium.now_ms());
    run_until_sent(&mut n, &medium);
    assert_eq!(n.radio().tx_preamble_lengths(), [47], "SF6's length, not SF8's");
    assert_eq!(n.radio().preamble_length(), 47);
    // Radio start, the failures at SF8 and SF6, and one retry before the frame.
    assert_eq!(n.radio().preamble_length_calls(), 4);
    assert_eq!(air.queued_rx(), 1);
    assert_eq!(
        lines(&cap.take()),
        [
            (Level::Warn, FAILED),
            (Level::Info, RETUNED_SF6),
            (
                Level::Info,
                "radio set_preamble_length recovered after 2 failures in 50 ms"
            ),
        ]
    );
}
