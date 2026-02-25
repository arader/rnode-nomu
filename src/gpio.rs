// Copyright (C) 2024, Mark Qvist
// Ported to Rust for Linux by RNode contributors.
// Licensed under the GNU General Public License v3.0 or later.

use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use crate::error::{RNodeError, Result};

/// GPIO line direction/mode.
#[derive(Debug, Clone, Copy)]
pub enum Direction {
    Output,
    Input,
}

/// Edge detection mode for input lines.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub enum Edge {
    Rising,
    Falling,
    Both,
}

/// A single GPIO line accessed via Linux sysfs.
///
/// We use sysfs (`/sys/class/gpio`) for maximum compatibility across
/// Linux SBCs without requiring libgpiod libraries at build time.
/// For production use on modern kernels, consider migrating to the
/// character device interface (libgpiod).
#[allow(dead_code)]
pub struct GpioLine {
    line: u32,
    #[allow(dead_code)]
    chip: String,
    exported: bool,
}

impl GpioLine {
    /// Request a GPIO line.
    pub fn new(chip: &str, line: u32, direction: Direction) -> Result<Self> {
        let mut gpio = Self {
            line,
            chip: chip.to_string(),
            exported: false,
        };
        gpio.export()?;
        gpio.set_direction(direction)?;
        Ok(gpio)
    }

    fn export(&mut self) -> Result<()> {
        let export_path = "/sys/class/gpio/export";
        if !Path::new(&self.value_path()).exists() {
            fs::write(export_path, self.line.to_string()).map_err(|e| {
                RNodeError::Gpio(format!("Failed to export GPIO {}: {}", self.line, e))
            })?;
            // Give the kernel a moment to create sysfs entries
            thread::sleep(Duration::from_millis(50));
        }
        self.exported = true;
        Ok(())
    }

    fn base_path(&self) -> String {
        format!("/sys/class/gpio/gpio{}", self.line)
    }

    fn value_path(&self) -> String {
        format!("{}/value", self.base_path())
    }

    fn set_direction(&self, direction: Direction) -> Result<()> {
        let dir_str = match direction {
            Direction::Output => "out",
            Direction::Input => "in",
        };
        let path = format!("{}/direction", self.base_path());
        fs::write(&path, dir_str).map_err(|e| {
            RNodeError::Gpio(format!(
                "Failed to set direction for GPIO {}: {}",
                self.line, e
            ))
        })
    }

    /// Set edge detection mode (for input lines).
    pub fn set_edge(&self, edge: Edge) -> Result<()> {
        let edge_str = match edge {
            Edge::Rising => "rising",
            Edge::Falling => "falling",
            Edge::Both => "both",
        };
        let path = format!("{}/edge", self.base_path());
        fs::write(&path, edge_str).map_err(|e| {
            RNodeError::Gpio(format!(
                "Failed to set edge for GPIO {}: {}",
                self.line, e
            ))
        })
    }

    /// Read the current value of a GPIO line.
    pub fn read(&self) -> Result<bool> {
        let val = fs::read_to_string(&self.value_path()).map_err(|e| {
            RNodeError::Gpio(format!("Failed to read GPIO {}: {}", self.line, e))
        })?;
        Ok(val.trim() == "1")
    }

    /// Set the value of an output GPIO line.
    pub fn write(&self, high: bool) -> Result<()> {
        let val = if high { "1" } else { "0" };
        fs::write(&self.value_path(), val).map_err(|e| {
            RNodeError::Gpio(format!("Failed to write GPIO {}: {}", self.line, e))
        })
    }

    /// Open the value file for poll/epoll-based edge detection.
    /// Returns a file that can be used with nix::poll.
    pub fn open_value_fd(&self) -> Result<fs::File> {
        fs::File::open(&self.value_path()).map_err(|e| {
            RNodeError::Gpio(format!(
                "Failed to open GPIO {} value fd: {}",
                self.line, e
            ))
        })
    }

    /// Get the GPIO line number.
    pub fn line_number(&self) -> u32 {
        self.line
    }
}

impl Drop for GpioLine {
    fn drop(&mut self) {
        if self.exported {
            let _ = fs::write("/sys/class/gpio/unexport", self.line.to_string());
        }
    }
}

/// Collection of GPIO lines needed by a LoRa radio.
#[allow(dead_code)]
pub struct RadioGpio {
    pub reset: GpioLine,
    pub busy: GpioLine,
    pub dio0: GpioLine,
    pub rxen: Option<GpioLine>,
}

impl RadioGpio {
    pub fn new(
        chip: &str,
        reset_line: u32,
        busy_line: u32,
        dio0_line: u32,
        rxen_line: Option<u32>,
    ) -> Result<Self> {
        let reset = GpioLine::new(chip, reset_line, Direction::Output)?;
        let busy = GpioLine::new(chip, busy_line, Direction::Input)?;
        let dio0 = GpioLine::new(chip, dio0_line, Direction::Input)?;
        dio0.set_edge(Edge::Rising)?;

        let rxen = if let Some(line) = rxen_line {
            Some(GpioLine::new(chip, line, Direction::Output)?)
        } else {
            None
        };

        Ok(Self {
            reset,
            busy,
            dio0,
            rxen,
        })
    }

    /// Pulse the reset line low for 10ms then high for 10ms.
    pub fn reset_radio(&self) -> Result<()> {
        self.reset.write(false)?;
        thread::sleep(Duration::from_millis(10));
        self.reset.write(true)?;
        thread::sleep(Duration::from_millis(10));
        Ok(())
    }

    /// Wait for the busy line to go low, with a 100ms timeout.
    pub fn wait_on_busy(&self) -> Result<()> {
        let start = Instant::now();
        while self.busy.read()? {
            if start.elapsed() > Duration::from_millis(100) {
                break;
            }
            thread::sleep(Duration::from_micros(100));
        }
        Ok(())
    }

    /// Enable the RX antenna switch (if present).
    pub fn rx_ant_enable(&self) -> Result<()> {
        if let Some(ref rxen) = self.rxen {
            rxen.write(true)?;
        }
        Ok(())
    }
}
