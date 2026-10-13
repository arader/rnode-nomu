//! Firmware-sized transmit queue (`CONFIG_QUEUE_MAX_LENGTH` / `CONFIG_QUEUE_SIZE`).

use std::collections::VecDeque;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxQueue {
    max_packets: usize,
    max_bytes: usize,
    bytes: usize,
    packets: VecDeque<Vec<u8>>,
}

impl TxQueue {
    pub fn new(max_packets: usize, max_bytes: usize) -> Self {
        Self {
            max_packets,
            max_bytes,
            bytes: 0,
            packets: VecDeque::new(),
        }
    }

    pub fn clear(&mut self) {
        self.bytes = 0;
        self.packets.clear();
    }

    pub fn push(&mut self, frame: Vec<u8>, mtu: usize) -> bool {
        if frame.is_empty() || frame.len() > mtu {
            return false;
        }
        if self.packets.len() >= self.max_packets || self.bytes + frame.len() > self.max_bytes {
            log::debug!("dropping TX frame because the firmware-sized queue is full");
            return false;
        }
        self.bytes += frame.len();
        self.packets.push_back(frame);
        true
    }

    pub fn pop(&mut self) -> Option<Vec<u8>> {
        let p = self.packets.pop_front()?;
        self.bytes -= p.len();
        Some(p)
    }

    pub fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }
    pub fn is_full(&self) -> bool {
        self.packets.len() >= self.max_packets || self.bytes >= self.max_bytes
    }
    pub fn len(&self) -> usize {
        self.packets.len()
    }
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}
