//! Behaviour of the CSMA API that every implementation must keep (firmware v1.86):
//! the `CMD_STAT_CHTM` / `CMD_STAT_CSMA` wire payloads and "never transmit on a busy medium".

use rnode_core::csma::{Access, Csma};
use rnode_core::phy::{Modulation, PhyParams};
use rnode_core::rng::Rng;

#[test]
fn channel_stats_payload_initial_state_matches_firmware_encoding() {
    let mut csma = Csma::new();
    csma.observe(0, false, -100);
    let payload = csma.channel_stats_payload();
    // Airtime and utilisation fields are all zero before anything was sent or heard.
    assert_eq!(&payload[0..8], &[0; 8]);
    // current_rssi + 157 = 57; the firmware's initial noise floor of -292 + 157 wraps to 121
    // through the uint8_t cast; 0xFF means "no interference".
    assert_eq!(payload[8], 57);
    assert_eq!(payload[9], 121);
    assert_eq!(payload[10], 0xFF);
}

#[test]
fn csma_payload_reports_band_and_window() {
    assert_eq!(Csma::new().csma_payload(), [1, 0, 15]);
}

#[test]
fn busy_medium_never_grants_access() {
    let mut csma = Csma::new();
    csma.set_phy(&PhyParams::new(Modulation {
        bandwidth_hz: 125_000,
        spreading_factor: 7,
        coding_rate: 5,
    }));
    let mut rng = Rng::new(1);
    for now in (0..2_000).step_by(3) {
        csma.observe(now, true, -60);
        assert_eq!(csma.poll_access(now, &mut rng), Access::Wait, "at {now} ms");
    }
}
