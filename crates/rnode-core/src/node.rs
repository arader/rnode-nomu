//! The RNode state machine.
//!
//! [`Node`] is the hardware-independent equivalent of the firmware's main loop: it consumes
//! bytes from the host, drives a [`Radio`], and produces bytes for the host. The platform layer
//! (e.g. the Linux daemon) owns the I/O and calls into the node:
//!
//! ```text
//! boot(now)                         once at start-up
//! loop {
//!     wait for host input, the radio IRQ line, or next_wakeup_ms()
//!     handle_host_input(bytes, now) when the host sent data
//!     service_radio(now)            when the radio IRQ fired (and periodically as a safety net)
//!     tick(now)                     always
//!     write take_host_output() to the host
//! }
//! shutdown()
//! ```
//!
//! `now` is a monotonic millisecond clock supplied by the caller, which keeps the node
//! deterministic under test.
//!
//! This module currently defines the API contract only; the behaviour (KISS command handling,
//! TX queue, framing, CSMA, statistics) is implemented on top of it (issue #24).

use crate::identity::DeviceIdentity;
use crate::radio::Radio;
use crate::storage::Storage;

/// Firmware `STATUS_INTERVAL_MS`: how often the modem status (DCD, RSSI) is sampled.
pub const STATUS_INTERVAL_MS: u64 = 3;

/// Static configuration of a [`Node`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeConfig {
    /// What the node reports to the host.
    pub identity: DeviceIdentity,
    /// Highest TX power in dBm the host may request; higher requests are clamped
    /// (firmware: 22 for an SX1262 without external PA).
    pub max_tx_power: u8,
    /// Maximum number of packets in the TX queue (firmware `CONFIG_QUEUE_MAX_LENGTH`).
    pub queue_max_packets: usize,
    /// Maximum number of bytes in the TX queue (firmware `CONFIG_QUEUE_SIZE`).
    pub queue_max_bytes: usize,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            identity: DeviceIdentity::default(),
            max_tx_power: 22,
            queue_max_packets: 200,
            queue_max_bytes: 6144,
        }
    }
}

/// An RNode: host protocol, TX queue, framing and channel access around a [`Radio`].
#[derive(Debug)]
pub struct Node<R: Radio, S: Storage> {
    radio: R,
    storage: S,
    config: NodeConfig,
    seed: u64,
    radio_online: bool,
    host_output: Vec<u8>,
}

impl<R: Radio, S: Storage> Node<R, S> {
    /// Create a node. `seed` initialises the node's pseudo-random generator (CSMA contention
    /// window, packet sequence numbers); pass real entropy outside of tests.
    pub fn new(radio: R, storage: S, config: NodeConfig, seed: u64) -> Self {
        Self {
            radio,
            storage,
            config,
            seed,
            radio_online: false,
            host_output: Vec::new(),
        }
    }

    /// Start-up: load saved radio settings and, if they are complete, bring the radio online
    /// (firmware `validate_status()` → `eeprom_conf_load()` → `startRadio()`). Without saved
    /// settings the radio stays offline until the host configures and starts it.
    pub fn boot(&mut self, now_ms: u64) {
        let _ = now_ms;
    }

    /// Process bytes received from the host (firmware `serial_callback()` for each byte).
    pub fn handle_host_input(&mut self, bytes: &[u8], now_ms: u64) {
        let _ = (bytes, now_ms);
    }

    /// Service the radio after its interrupt line fired, or as a periodic safety net: drain
    /// received frames via [`Radio::poll_rx`], reassemble split packets and queue them for the
    /// host preceded by their RSSI and SNR (firmware `receive_callback()`).
    pub fn service_radio(&mut self, now_ms: u64) {
        let _ = now_ms;
    }

    /// Periodic work (firmware `loop()`): modem status sampling every [`STATUS_INTERVAL_MS`],
    /// noise floor and channel utilisation, airtime lock, CSMA and the TX queue, and periodic
    /// statistics for the host.
    pub fn tick(&mut self, now_ms: u64) {
        let _ = now_ms;
    }

    /// Take the bytes queued for the host (KISS frames); the caller writes them out.
    pub fn take_host_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.host_output)
    }

    /// The monotonic time in ms at which [`Node::tick`] next needs to run.
    pub fn next_wakeup_ms(&self, now_ms: u64) -> u64 {
        now_ms + STATUS_INTERVAL_MS
    }

    /// Whether the radio is online (configured and receiving).
    pub fn is_radio_online(&self) -> bool {
        self.radio_online
    }

    /// Take the radio offline before the process exits.
    pub fn shutdown(&mut self) {
        if self.radio_online {
            if let Err(e) = self.radio.end() {
                log::warn!("failed to put the radio to sleep: {e}");
            }
            self.radio_online = false;
        }
    }

    /// The node's static configuration.
    pub fn config(&self) -> &NodeConfig {
        &self.config
    }

    /// The radio driven by this node.
    pub fn radio(&self) -> &R {
        &self.radio
    }

    /// Mutable access to the radio driven by this node.
    pub fn radio_mut(&mut self) -> &mut R {
        &mut self.radio
    }

    /// The settings storage used by this node.
    pub fn storage(&self) -> &S {
        &self.storage
    }

    /// The seed the node's pseudo-random generator was created with.
    pub fn seed(&self) -> u64 {
        self.seed
    }
}
