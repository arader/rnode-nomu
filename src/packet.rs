// Copyright (C) 2024, Mark Qvist
// Ported to Rust for Linux by RNode contributors.
// Licensed under the GNU General Public License v3.0 or later.

//! Packet queue with splitting (>255 byte) and RX reassembly.

#![allow(dead_code)]

use crate::kiss::{FLAG_SPLIT, NIBBLE_FLAGS, NIBBLE_SEQ, SEQ_UNSET};

/// Single LoRa frame MTU.
pub const SINGLE_MTU: usize = 255;
/// Maximum packet size (two split frames).
pub const MTU: usize = 508;
/// Header length prepended to each LoRa frame.
pub const HEADER_L: usize = 1;

/// Default queue capacity in bytes.
const DEFAULT_QUEUE_SIZE: usize = 6144;
/// Maximum number of packets in the queue.
const DEFAULT_QUEUE_MAX_LENGTH: usize = 16;

/// A circular packet queue for outgoing data.
pub struct PacketQueue {
    buffer: Vec<u8>,
    /// Start positions of queued packets.
    starts: Vec<usize>,
    /// Lengths of queued packets.
    lengths: Vec<usize>,
    /// Current write cursor.
    cursor: usize,
    /// Number of bytes currently used.
    used: usize,
    /// Maximum byte capacity.
    capacity: usize,
    /// Maximum number of packets.
    max_packets: usize,
    /// Current sequence number for TX headers.
    seq: u8,
}

impl PacketQueue {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_QUEUE_SIZE, DEFAULT_QUEUE_MAX_LENGTH)
    }

    pub fn with_capacity(capacity: usize, max_packets: usize) -> Self {
        Self {
            buffer: vec![0u8; capacity],
            starts: Vec::with_capacity(max_packets),
            lengths: Vec::with_capacity(max_packets),
            cursor: 0,
            used: 0,
            capacity,
            max_packets,
            seq: 0,
        }
    }

    /// Enqueue a packet. Returns false if queue is full.
    pub fn push(&mut self, data: &[u8]) -> bool {
        if self.starts.len() >= self.max_packets || self.used + data.len() > self.capacity {
            return false;
        }
        let start = self.cursor;
        for &b in data {
            self.buffer[self.cursor] = b;
            self.cursor = (self.cursor + 1) % self.capacity;
        }
        self.starts.push(start);
        self.lengths.push(data.len());
        self.used += data.len();
        true
    }

    /// Pop the next packet from the queue.
    pub fn pop(&mut self) -> Option<Vec<u8>> {
        if self.starts.is_empty() {
            return None;
        }
        let start = self.starts.remove(0);
        let len = self.lengths.remove(0);
        let mut data = Vec::with_capacity(len);
        for i in 0..len {
            data.push(self.buffer[(start + i) % self.capacity]);
        }
        self.used -= len;
        Some(data)
    }

    pub fn is_empty(&self) -> bool {
        self.starts.is_empty()
    }

    pub fn is_full(&self) -> bool {
        self.starts.len() >= self.max_packets || self.used >= self.capacity
    }

    pub fn height(&self) -> usize {
        self.starts.len()
    }

    pub fn queued_bytes(&self) -> usize {
        self.used
    }

    /// Get the next TX sequence number and advance it.
    pub fn next_seq(&mut self) -> u8 {
        let s = self.seq;
        self.seq = self.seq.wrapping_add(1) & 0x0F;
        s
    }
}

/// Prepare packet(s) for radio transmission, handling split logic.
///
/// Returns a vec of LoRa frames, each ≤ SINGLE_MTU bytes (including the 1-byte header).
/// For packets ≤ 254 bytes of payload, returns one frame.
/// For packets 255..=507 bytes, returns two frames with FLAG_SPLIT set.
pub fn prepare_tx_frames(data: &[u8], seq: u8) -> Vec<Vec<u8>> {
    let payload_len = data.len();

    if payload_len <= SINGLE_MTU - HEADER_L {
        // Single frame: header (seq << 4) + payload
        let header = (seq << 4) & NIBBLE_SEQ;
        let mut frame = Vec::with_capacity(HEADER_L + payload_len);
        frame.push(header);
        frame.extend_from_slice(data);
        vec![frame]
    } else {
        // Split into two frames
        let first_payload = SINGLE_MTU - HEADER_L; // 254 bytes
        let second_payload = payload_len - first_payload;
        let header1 = ((seq << 4) & NIBBLE_SEQ) | FLAG_SPLIT;

        let mut frame1 = Vec::with_capacity(SINGLE_MTU);
        frame1.push(header1);
        frame1.extend_from_slice(&data[..first_payload]);

        let header2 = (seq << 4) & NIBBLE_SEQ;
        let mut frame2 = Vec::with_capacity(HEADER_L + second_payload);
        frame2.push(header2);
        frame2.extend_from_slice(&data[first_payload..]);

        vec![frame1, frame2]
    }
}

/// State machine for reassembling split RX packets.
pub struct RxReassembler {
    pending_seq: u8,
    pending_data: Vec<u8>,
    has_pending: bool,
}

impl RxReassembler {
    pub fn new() -> Self {
        Self {
            pending_seq: SEQ_UNSET,
            pending_data: Vec::new(),
            has_pending: false,
        }
    }

    /// Process a received LoRa frame (including the 1-byte header).
    /// Returns `Some(complete_payload)` when a full packet is ready.
    pub fn process_frame(&mut self, frame: &[u8]) -> Option<Vec<u8>> {
        if frame.is_empty() {
            return None;
        }

        let header = frame[0];
        let seq = (header & NIBBLE_SEQ) >> 4;
        let flags = header & NIBBLE_FLAGS;
        let payload = &frame[HEADER_L..];

        if (flags & FLAG_SPLIT) != 0 {
            // First part of a split packet
            self.pending_seq = seq;
            self.pending_data = payload.to_vec();
            self.has_pending = true;
            None
        } else if self.has_pending && seq == self.pending_seq {
            // Second part of a split packet
            self.has_pending = false;
            let mut complete = std::mem::take(&mut self.pending_data);
            complete.extend_from_slice(payload);
            self.pending_seq = SEQ_UNSET;
            Some(complete)
        } else {
            // Non-split single packet
            self.has_pending = false;
            self.pending_seq = SEQ_UNSET;
            self.pending_data.clear();
            Some(payload.to_vec())
        }
    }

    /// Process a frame in promiscuous mode (no reassembly, raw output).
    pub fn process_frame_promisc(&self, frame: &[u8]) -> Vec<u8> {
        frame.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- PacketQueue tests ---

    #[test]
    fn queue_push_pop_basic() {
        let mut q = PacketQueue::new();
        assert!(q.is_empty());
        assert!(q.push(&[1, 2, 3]));
        assert!(!q.is_empty());
        assert_eq!(q.height(), 1);
        assert_eq!(q.queued_bytes(), 3);

        let pkt = q.pop().unwrap();
        assert_eq!(pkt, vec![1, 2, 3]);
        assert!(q.is_empty());
    }

    #[test]
    fn queue_fifo_ordering() {
        let mut q = PacketQueue::new();
        q.push(&[0xAA]);
        q.push(&[0xBB]);
        q.push(&[0xCC]);
        assert_eq!(q.pop().unwrap(), vec![0xAA]);
        assert_eq!(q.pop().unwrap(), vec![0xBB]);
        assert_eq!(q.pop().unwrap(), vec![0xCC]);
        assert!(q.pop().is_none());
    }

    #[test]
    fn queue_pop_empty_returns_none() {
        let mut q = PacketQueue::new();
        assert!(q.pop().is_none());
    }

    #[test]
    fn queue_respects_max_packets() {
        let mut q = PacketQueue::with_capacity(1024, 3);
        assert!(q.push(&[1]));
        assert!(q.push(&[2]));
        assert!(q.push(&[3]));
        assert!(!q.push(&[4])); // Should fail — max 3 packets
        assert!(q.is_full());
    }

    #[test]
    fn queue_respects_capacity() {
        let mut q = PacketQueue::with_capacity(10, 100);
        assert!(q.push(&[0u8; 5]));
        assert!(q.push(&[0u8; 5]));
        assert!(!q.push(&[0u8; 1])); // Should fail — 10 bytes full
    }

    #[test]
    fn queue_reuse_after_pop() {
        let mut q = PacketQueue::with_capacity(10, 2);
        q.push(&[0u8; 5]);
        q.push(&[0u8; 5]);
        assert!(q.is_full());
        q.pop();
        assert!(!q.is_full());
        assert!(q.push(&[0u8; 5])); // Should succeed now
    }

    #[test]
    fn queue_seq_wraps_at_16() {
        let mut q = PacketQueue::new();
        for i in 0..16 {
            assert_eq!(q.next_seq(), i);
        }
        // Should wrap back to 0
        assert_eq!(q.next_seq(), 0);
        assert_eq!(q.next_seq(), 1);
    }

    // --- prepare_tx_frames tests ---

    #[test]
    fn single_frame_small_packet() {
        let data = vec![0xAA; 10];
        let frames = prepare_tx_frames(&data, 5);
        assert_eq!(frames.len(), 1);
        let frame = &frames[0];
        // Header: seq=5 << 4 = 0x50, no split flag
        assert_eq!(frame[0], 0x50);
        assert_eq!(&frame[1..], &data[..]);
    }

    #[test]
    fn single_frame_max_size() {
        // 254 bytes payload = exactly fits in one frame (254 + 1 header = 255)
        let data = vec![0xBB; 254];
        let frames = prepare_tx_frames(&data, 0);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].len(), 255);
        assert_eq!(frames[0][0] & NIBBLE_FLAGS, 0); // No split flag
    }

    #[test]
    fn split_frame_255_bytes() {
        // 255 bytes payload requires split (255 > 254)
        let data = vec![0xCC; 255];
        let frames = prepare_tx_frames(&data, 3);
        assert_eq!(frames.len(), 2);

        // First frame: split flag set, seq=3
        assert_eq!(frames[0][0] & NIBBLE_SEQ, 3 << 4);
        assert_eq!(frames[0][0] & FLAG_SPLIT, FLAG_SPLIT);
        assert_eq!(frames[0].len(), 255); // 1 header + 254 payload

        // Second frame: same seq, no split flag
        assert_eq!(frames[1][0] & NIBBLE_SEQ, 3 << 4);
        assert_eq!(frames[1][0] & FLAG_SPLIT, 0);
        assert_eq!(frames[1].len(), 2); // 1 header + 1 remaining byte
    }

    #[test]
    fn split_frame_max_mtu() {
        // 507 bytes = maximum split payload (254 + 253)
        let data: Vec<u8> = (0..507).map(|i| (i & 0xFF) as u8).collect();
        let frames = prepare_tx_frames(&data, 7);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].len(), 255);
        assert_eq!(frames[1].len(), 254); // 1 header + 253

        // Verify payload integrity
        assert_eq!(&frames[0][1..], &data[..254]);
        assert_eq!(&frames[1][1..], &data[254..]);
    }

    #[test]
    fn split_frame_seq_in_both_halves() {
        let data = vec![0; 300];
        let frames = prepare_tx_frames(&data, 0x0A);
        let seq1 = (frames[0][0] & NIBBLE_SEQ) >> 4;
        let seq2 = (frames[1][0] & NIBBLE_SEQ) >> 4;
        assert_eq!(seq1, 0x0A);
        assert_eq!(seq2, 0x0A);
    }

    // --- RxReassembler tests ---

    #[test]
    fn reassemble_single_frame() {
        let mut r = RxReassembler::new();
        let header = 0x50; // seq=5, no flags
        let frame = [header, 0xAA, 0xBB, 0xCC];
        let result = r.process_frame(&frame);
        assert_eq!(result, Some(vec![0xAA, 0xBB, 0xCC]));
    }

    #[test]
    fn reassemble_split_frames() {
        let mut r = RxReassembler::new();
        let seq = 3u8;

        // First half: split flag set
        let header1 = (seq << 4) | FLAG_SPLIT;
        let frame1 = [header1, 0x01, 0x02, 0x03];
        assert!(r.process_frame(&frame1).is_none());

        // Second half: same seq, no split flag
        let header2 = seq << 4;
        let frame2 = [header2, 0x04, 0x05];
        let result = r.process_frame(&frame2);
        assert_eq!(result, Some(vec![0x01, 0x02, 0x03, 0x04, 0x05]));
    }

    #[test]
    fn reassemble_mismatched_seq_discards_pending() {
        let mut r = RxReassembler::new();

        // First half with seq=1
        let frame1 = [(1 << 4) | FLAG_SPLIT, 0xAA];
        assert!(r.process_frame(&frame1).is_none());

        // Second frame with different seq=2 (not split)
        let frame2 = [2 << 4, 0xBB];
        let result = r.process_frame(&frame2);
        // Should output as a new single packet, not reassemble
        assert_eq!(result, Some(vec![0xBB]));
    }

    #[test]
    fn reassemble_empty_frame_returns_none() {
        let mut r = RxReassembler::new();
        assert!(r.process_frame(&[]).is_none());
    }

    #[test]
    fn promisc_mode_returns_raw() {
        let r = RxReassembler::new();
        let frame = [0x31, 0xAA, 0xBB]; // header + payload
        assert_eq!(r.process_frame_promisc(&frame), frame.to_vec());
    }

    // --- Round-trip: prepare_tx_frames → RxReassembler ---

    #[test]
    fn roundtrip_single_packet() {
        let data = vec![0x42; 100];
        let frames = prepare_tx_frames(&data, 5);
        let mut r = RxReassembler::new();
        let result = r.process_frame(&frames[0]);
        assert_eq!(result, Some(data));
    }

    #[test]
    fn roundtrip_split_packet() {
        let data: Vec<u8> = (0..400).map(|i| (i & 0xFF) as u8).collect();
        let frames = prepare_tx_frames(&data, 9);
        assert_eq!(frames.len(), 2);

        let mut r = RxReassembler::new();
        assert!(r.process_frame(&frames[0]).is_none());
        let result = r.process_frame(&frames[1]);
        assert_eq!(result, Some(data));
    }
}
