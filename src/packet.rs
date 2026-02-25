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
