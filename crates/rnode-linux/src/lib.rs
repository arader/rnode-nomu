//! Linux platform layer for `rnode_core::Node`.
//!
//! Hardware support is intentionally thin: GPIO, SPI, PTY, persistent settings and the poll-based
//! event loop live here, while KISS/protocol/CSMA state lives in `rnode-core`.
//!
//! # Manual SX1262 smoke test
//!
//! On a Linux host with the radio wired, install the daemon, copy `config.uconsole-aio-v2.toml` to
//! `/etc/rnode-linux.toml`, then run:
//!
//! ```text
//! RUST_LOG=info rnode-linux --config /etc/rnode-linux.toml --spilog spi.csv
//! ```
//!
//! Open the configured PTY symlink from Reticulum as an RNode interface. Confirm that the daemon
//! remains running across host disconnect/reconnect, that DIO1 IRQs increment RX activity, and that
//! removing the antenna or otherwise inducing radio I/O errors logs warnings instead of exiting.

pub mod cli;
pub mod config;
pub mod delay;
pub mod event_loop;
pub mod pty;
pub mod spi;
pub mod storage;

#[cfg(target_os = "linux")]
pub mod gpio;
