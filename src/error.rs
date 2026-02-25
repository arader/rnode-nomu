// Copyright (C) 2024, Mark Qvist
// Ported to Rust for Linux by RNode contributors.
// Licensed under the GNU General Public License v3.0 or later.

use thiserror::Error;

#[derive(Error, Debug)]
#[allow(dead_code)]
pub enum RNodeError {
    #[error("SPI error: {0}")]
    Spi(#[from] std::io::Error),

    #[error("GPIO error: {0}")]
    Gpio(String),

    #[error("Radio error: {0}")]
    Radio(String),

    #[error("Radio timeout: {0}")]
    RadioTimeout(String),

    #[error("KISS framing error: {0}")]
    Kiss(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("PTY error: {0}")]
    Pty(String),

    #[error("Queue full")]
    QueueFull,
}

pub type Result<T> = std::result::Result<T, RNodeError>;
