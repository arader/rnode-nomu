//! RNode split framing: packets over 254 bytes travel as two LoRa frames sharing one header.

use rnode_core::kiss::*;
// The tests below refer to these modules as `crate::framing` and `crate::rng`.
use rnode_core::{framing, rng};

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
