//! Hardware-independent RNode logic.
//!
//! This crate holds everything that decides *what* an RNode does, with no I/O of its own:
//! the KISS host protocol, the TX queue, RNode framing (split packets), the PHY/CSMA logic and
//! the [`Node`] state machine that ties them together. Time is passed in explicitly (`now_ms`,
//! a monotonic millisecond clock) and randomness comes from a seed, so every behaviour can be
//! tested deterministically.
//!
//! The crate talks to the outside world through two traits:
//! - [`Radio`], implemented by modem drivers (e.g. `rnode-sx126x`) and by simulated radios;
//! - [`Storage`], which persists the radio settings saved with `CMD_CONF_SAVE`.
//!
//! The behaviour follows RNode_Firmware (v1.86) wherever it is observable by the host or on the
//! air; deliberate differences are documented where they occur.

pub mod csma;
mod fault_log;
pub mod framing;
pub mod identity;
pub mod kiss;
pub mod node;
mod packet_log;
pub mod phy;
pub mod queue;
pub mod radio;
pub mod rng;
#[cfg(any(test, feature = "sim"))]
pub mod sim;
pub mod storage;

pub use identity::DeviceIdentity;
pub use node::{Node, NodeConfig};
pub use radio::{Radio, RxFrame, TxOutcome};
#[cfg(any(test, feature = "sim"))]
pub use sim::{SimMedium, SimRadio};
pub use storage::{MemoryStorage, RadioSettings, Storage};

/// Offset added to RSSI values in dBm before they are sent to the host as a byte (firmware and
/// Reticulum `RSSI_OFFSET`).
pub const RSSI_OFFSET: i32 = 157;
