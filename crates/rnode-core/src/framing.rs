//! RNode on-air framing: the one-byte sequence/split header and two-frame MTU extension.
//!
//! Firmware references: `transmit()` in `RNode_Firmware.ino` and `receive_callback()` plus
//! `packetSequence()`/`isSplitPacket()` in `Utilities.h`.

use crate::kiss::{FLAG_SPLIT, HEADER_LEN, MTU, SINGLE_MTU};
use crate::rng::Rng;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reassembly {
    Pending,
    Complete(Vec<u8>),
    Ignored,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RxReassembler {
    seq: Option<u8>,
    buf: Vec<u8>,
}

impl Default for RxReassembler {
    fn default() -> Self {
        Self::new()
    }
}

impl RxReassembler {
    pub fn new() -> Self {
        Self {
            seq: None,
            buf: Vec::new(),
        }
    }

    pub fn reset(&mut self) {
        self.seq = None;
        self.buf.clear();
    }

    /// Apply the four firmware split cases. An extra header-only split frame after a 508-byte
    /// packet from real firmware is tolerated: it becomes a new empty pending first half and is
    /// cleared by the next non-split frame without corrupting the already delivered packet.
    pub fn process(&mut self, frame: &[u8]) -> Reassembly {
        if frame.is_empty() {
            return Reassembly::Ignored;
        }
        let header = frame[0];
        let sequence = header >> 4;
        let split = header & FLAG_SPLIT != 0;
        let payload = &frame[1..];
        if split && self.seq.is_none() {
            self.buf.clear();
            self.seq = Some(sequence);
            self.buf.extend_from_slice(payload);
            self.buf.truncate(MTU);
            Reassembly::Pending
        } else if split && self.seq == Some(sequence) {
            let room = MTU.saturating_sub(self.buf.len());
            self.buf.extend_from_slice(&payload[..payload.len().min(room)]);
            self.seq = None;
            Reassembly::Complete(core::mem::take(&mut self.buf))
        } else if split {
            self.buf.clear();
            self.seq = Some(sequence);
            self.buf.extend_from_slice(payload);
            self.buf.truncate(MTU);
            Reassembly::Pending
        } else {
            if self.seq.is_some() {
                self.buf.clear();
                self.seq = None;
            }
            Reassembly::Complete(payload[..payload.len().min(MTU)].to_vec())
        }
    }
}

/// Build normal RNode frames. Packets >254 bytes are split into two LoRa frames; unlike the
/// firmware, a 508-byte packet is not followed by the firmware's accidental extra header-only
/// packet, because it carries no payload and only leaves receivers with stale pending state.
pub fn tx_frames(payload: &[u8], rng: &mut Rng) -> Vec<Vec<u8>> {
    let payload = &payload[..payload.len().min(MTU)];
    let mut header = rng.byte() & 0xF0;
    if payload.len() > SINGLE_MTU - HEADER_LEN {
        header |= FLAG_SPLIT;
        let first_len = SINGLE_MTU - HEADER_LEN;
        vec![
            with_header(header, &payload[..first_len]),
            with_header(header, &payload[first_len..]),
        ]
    } else {
        vec![with_header(header, payload)]
    }
}

fn with_header(header: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 1);
    out.push(header);
    out.extend_from_slice(payload);
    out
}
