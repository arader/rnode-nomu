//! Helpers shared by the integration tests: KISS frame builders, nodes on the simulated medium
//! brought online with Reticulum's init sequence, and the per-thread capture logger.

// Every test crate compiles this whole module but uses only some of it.
#![allow(dead_code)]

pub mod log_capture;

use log::Level;

use rnode_core::kiss::*;
use rnode_core::{MemoryStorage, Node, NodeConfig, Radio, SimMedium, SimRadio};

use log_capture::Captured;

pub(crate) fn cmd(cmd: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![FEND, cmd];
    for &b in payload {
        rnode_core::kiss::escape_into(&mut out, b);
    }
    out.push(FEND);
    out
}

pub(crate) fn plain(cmd: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![FEND, cmd];
    out.extend_from_slice(payload);
    out.push(FEND);
    out
}

pub(crate) fn data(payload: &[u8]) -> Vec<u8> {
    cmd(CMD_DATA, payload)
}

pub(crate) fn node(seed: u64) -> Node<rnode_core::SimRadio, MemoryStorage> {
    let medium = SimMedium::new();
    Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), seed)
}

pub(crate) fn init(n: &mut Node<rnode_core::SimRadio, MemoryStorage>) -> Vec<u8> {
    let mut input = Vec::new();
    input.extend(cmd(CMD_FREQUENCY, &867_200_000u32.to_be_bytes()));
    input.extend(cmd(CMD_BANDWIDTH, &125_000u32.to_be_bytes()));
    input.extend(plain(CMD_TXPOWER, &[14]));
    input.extend(plain(CMD_SF, &[7]));
    input.extend(plain(CMD_CR, &[5]));
    input.extend(plain(CMD_ST_ALOCK, &[0, 0]));
    input.extend(plain(CMD_LT_ALOCK, &[0, 0]));
    input.extend(plain(CMD_RADIO_STATE, &[RADIO_STATE_ON]));
    n.handle_host_input(&input, 0);
    n.take_host_output()
}

pub(crate) fn split_frames(buf: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut frames = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        assert_eq!(buf[i], FEND, "frame must start with FEND at {i}");
        i += 1;
        let cmd = buf[i];
        i += 1;
        let mut p = Vec::new();
        let mut esc = false;
        while i < buf.len() && buf[i] != FEND {
            let mut b = buf[i];
            i += 1;
            if esc {
                if b == TFEND {
                    b = FEND;
                }
                if b == TFESC {
                    b = FESC;
                }
                esc = false;
                p.push(b);
            } else if b == FESC {
                esc = true;
            } else {
                p.push(b);
            }
        }
        assert!(i < buf.len());
        i += 1;
        frames.push((cmd, p));
    }
    frames
}

pub(crate) fn tick_until(
    node: &mut Node<rnode_core::SimRadio, MemoryStorage>,
    medium: &SimMedium,
    max_ms: u64,
    done: impl Fn(&Node<rnode_core::SimRadio, MemoryStorage>) -> bool,
) {
    let start = medium.now_ms();
    while medium.now_ms().saturating_sub(start) <= max_ms {
        node.tick(medium.now_ms());
        if done(node) {
            return;
        }
        medium.advance_ms(25);
    }
}

/// A radio listening on the channel that `init()` configures.
pub(crate) fn listener(medium: &SimMedium) -> rnode_core::SimRadio {
    let mut radio = medium.radio();
    radio.begin(867_200_000).unwrap();
    radio
}

/// A node with seed 1 brought online by `init()`, and a radio listening on its channel.
pub(crate) fn online_node() -> (SimMedium, SimRadio, Node<SimRadio, MemoryStorage>) {
    let medium = SimMedium::new();
    let air = listener(&medium);
    let mut n = Node::new(medium.radio(), MemoryStorage::default(), NodeConfig::default(), 1);
    init(&mut n);
    n.take_host_output();
    (medium, air, n)
}

/// The level and message of every record in `records`.
pub(crate) fn lines(records: &[Captured]) -> Vec<(Level, &str)> {
    records.iter().map(|r| (r.level, r.message.as_str())).collect()
}

/// Tick `n` every millisecond until its TX queue is empty; returns the medium time it emptied.
pub(crate) fn run_until_sent(n: &mut Node<SimRadio, MemoryStorage>, medium: &SimMedium) -> u64 {
    loop {
        n.tick(medium.now_ms());
        if n.queued_packets() == 0 {
            return medium.now_ms();
        }
        assert!(medium.now_ms() < 10_000, "the packet was never sent");
        medium.advance_ms(1);
    }
}

/// The short-term airtime of the last `CMD_STAT_CHTM` in `out`, in 0.01 % units.
pub(crate) fn last_chtm_airtime(out: &[u8]) -> u16 {
    let (_, payload) = split_frames(out)
        .into_iter()
        .rfind(|(cmd, _)| *cmd == CMD_STAT_CHTM)
        .expect("CMD_STAT_CHTM after the TX burst");
    u16::from_be_bytes([payload[0], payload[1]])
}
