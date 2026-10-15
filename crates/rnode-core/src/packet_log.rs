//! Lazy formatting for the node's log lines: the per-packet debug and trace logs, and the radio
//! settings in the online and offline lines.
//!
//! These types implement [`fmt::Display`] and are passed to the `log` macros as format arguments,
//! so nothing is formatted unless a logger has the record's level enabled.

use core::fmt::{self, Write};

use crate::kiss::FLAG_SPLIT;

/// `"s"` unless `n` is 1.
pub(crate) fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// One packet's shape: `300 bytes, 2 frames, header 0x91 (seq 9, split)`, or
/// `12 bytes, 1 frame, promiscuous` for a raw frame without an RNode header.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PacketSummary {
    /// Payload length in bytes, without the RNode header.
    pub len: usize,
    /// Number of LoRa frames the packet takes on the air.
    pub frames: usize,
    /// The RNode header byte (sequence nibble and split flag); `None` in promiscuous mode.
    pub header: Option<u8>,
}

impl fmt::Display for PacketSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} byte{}, {} frame{}",
            self.len,
            plural(self.len),
            self.frames,
            plural(self.frames)
        )?;
        match self.header {
            Some(header) => {
                write!(f, ", header 0x{header:02X} (seq {}", header >> 4)?;
                if header & FLAG_SPLIT != 0 {
                    f.write_str(", split")?;
                }
                f.write_char(')')
            }
            None => f.write_str(", promiscuous"),
        }
    }
}

/// A multi-line hex dump in the style of `hexdump -C`: one line per 16 bytes with the offset,
/// the bytes in hex and an ASCII gutter. Every line, including the first, starts with a newline,
/// so the dump follows a one-line heading in the same log record.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HexDump<'a>(pub &'a [u8]);

impl fmt::Display for HexDump<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (line, chunk) in self.0.chunks(16).enumerate() {
            write!(f, "\n{:04X} ", line * 16)?;
            for i in 0..16 {
                if i == 8 {
                    f.write_char(' ')?;
                }
                match chunk.get(i) {
                    Some(byte) => write!(f, " {byte:02X}")?,
                    None => f.write_str("   ")?,
                }
            }
            f.write_str("  |")?;
            for &byte in chunk {
                f.write_char(if byte == b' ' || byte.is_ascii_graphic() {
                    char::from(byte)
                } else {
                    '.'
                })?;
            }
            f.write_char('|')?;
        }
        Ok(())
    }
}

/// A frequency or bandwidth in Hz, in the largest unit that keeps it at least 1 and without
/// trailing zeros: `867.2 MHz`, `125 kHz`, `7.8 kHz`, `31.25 kHz`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Hz(pub u32);

impl fmt::Display for Hz {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (scale, unit) = match self.0 {
            1_000_000.. => (1_000_000, "MHz"),
            1_000.. => (1_000, "kHz"),
            _ => (1, "Hz"),
        };
        write!(f, "{}", self.0 / scale)?;
        let mut fraction = self.0 % scale;
        if fraction != 0 {
            let mut digits = scale.ilog10() as usize;
            while fraction % 10 == 0 {
                fraction /= 10;
                digits -= 1;
            }
            write!(f, ".{fraction:0digits$}")?;
        }
        write!(f, " {unit}")
    }
}

/// The radio settings in the online, offline and settings-changed lines:
/// `867.2 MHz, 125 kHz, SF7, CR 4/5, 14 dBm`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RadioSummary {
    pub frequency_hz: u32,
    pub bandwidth_hz: u32,
    pub spreading_factor: u8,
    pub coding_rate: u8,
    pub tx_power_dbm: u8,
}

impl fmt::Display for RadioSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}, {}, SF{}, CR 4/{}, {} dBm",
            Hz(self.frequency_hz),
            Hz(self.bandwidth_hz),
            self.spreading_factor,
            self.coding_rate,
            self.tx_power_dbm
        )
    }
}
