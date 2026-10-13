use std::fmt;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::time::Instant;

use embedded_hal::spi::{ErrorKind, Operation};

#[derive(Debug, thiserror::Error)]
pub enum LinuxSpiError {
    #[error("spi io: {0}")]
    Io(#[from] io::Error),
    #[error("unsupported non-u8 spi word")]
    UnsupportedWord,
}

impl embedded_hal::spi::Error for LinuxSpiError {
    fn kind(&self) -> ErrorKind {
        ErrorKind::Other
    }
}

/// CSV recorder for hardware SPI debugging.
#[allow(dead_code)]
pub struct SpiRecorder {
    writer: BufWriter<Box<dyn Write + Send>>,
    start: Instant,
    write_failed: bool,
    #[cfg(test)]
    warned_failures: usize,
}

impl fmt::Debug for SpiRecorder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpiRecorder")
            .field("start", &self.start)
            .field("write_failed", &self.write_failed)
            .finish_non_exhaustive()
    }
}

impl SpiRecorder {
    pub fn create(path: impl AsRef<Path>) -> Result<Self, LinuxSpiError> {
        Self::from_writer(File::create(path)?)
    }

    fn from_writer(writer: impl Write + Send + 'static) -> Result<Self, LinuxSpiError> {
        let mut writer = BufWriter::new(Box::new(writer) as Box<dyn Write + Send>);
        writeln!(writer, "timestamp_us,direction,tx,rx")?;
        writer.flush()?;
        Ok(Self {
            writer,
            start: Instant::now(),
            write_failed: false,
            #[cfg(test)]
            warned_failures: 0,
        })
    }

    #[allow(dead_code)]
    fn record(&mut self, direction: &str, tx: &[u8], rx: &[u8]) -> Result<(), LinuxSpiError> {
        let result = writeln!(
            self.writer,
            "{},{},{},{}",
            self.start.elapsed().as_micros(),
            direction,
            hex(tx),
            hex(rx)
        )
        .and_then(|()| self.writer.flush());
        if let Err(e) = result {
            self.check(e);
        }
        Ok(())
    }

    #[allow(dead_code)]
    fn check(&mut self, error: io::Error) {
        if !self.write_failed {
            log::warn!("SPI trace write failed, the trace is incomplete from here on: {error}");
            self.write_failed = true;
            #[cfg(test)]
            {
                self.warned_failures += 1;
            }
        }
    }

    #[cfg(test)]
    fn warned_failures_for_tests(&self) -> usize {
        self.warned_failures
    }
}

#[allow(dead_code)]
fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpiSegmentKind {
    Read,
    Write,
    Transfer,
    InPlace,
    DelayUs(u16),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpiSegmentPlan {
    pub op_index: Option<usize>,
    pub kind: SpiSegmentKind,
    pub tx: Vec<u8>,
    pub rx_len: usize,
}

pub fn build_segment_plan(operations: &[Operation<'_, u8>]) -> Vec<SpiSegmentPlan> {
    let mut segments = Vec::new();
    for (op_index, op) in operations.iter().enumerate() {
        match op {
            Operation::Read(buf) => segments.push(SpiSegmentPlan {
                op_index: Some(op_index),
                kind: SpiSegmentKind::Read,
                tx: vec![0; buf.len()],
                rx_len: buf.len(),
            }),
            Operation::Write(buf) => segments.push(SpiSegmentPlan {
                op_index: Some(op_index),
                kind: SpiSegmentKind::Write,
                tx: buf.to_vec(),
                rx_len: buf.len(),
            }),
            Operation::Transfer(read, write) => {
                let len = read.len().max(write.len());
                let mut tx = write.to_vec();
                tx.resize(len, 0);
                segments.push(SpiSegmentPlan {
                    op_index: Some(op_index),
                    kind: SpiSegmentKind::Transfer,
                    tx,
                    rx_len: len,
                });
            }
            Operation::TransferInPlace(buf) => segments.push(SpiSegmentPlan {
                op_index: Some(op_index),
                kind: SpiSegmentKind::InPlace,
                tx: buf.to_vec(),
                rx_len: buf.len(),
            }),
            Operation::DelayNs(ns) => {
                if *ns > 0 {
                    segments.push(SpiSegmentPlan {
                        op_index: None,
                        kind: SpiSegmentKind::DelayUs(ns.div_ceil(1_000).min(u32::from(u16::MAX)) as u16),
                        tx: Vec::new(),
                        rx_len: 0,
                    });
                }
            }
        }
    }
    segments
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use embedded_hal::spi::ErrorType;
    use spidev::{SpiModeFlags, Spidev, SpidevOptions, SpidevTransfer};

    /// `embedded-hal` SPI device implemented with Linux spidev.
    #[derive(Debug)]
    pub struct LinuxSpiDevice {
        dev: Spidev,
        recorder: Option<SpiRecorder>,
    }

    impl LinuxSpiDevice {
        pub fn open(
            device: impl AsRef<Path>,
            speed_hz: u32,
            recorder: Option<SpiRecorder>,
        ) -> Result<Self, LinuxSpiError> {
            let mut dev = Spidev::open(device)?;
            let options = SpidevOptions::new()
                .bits_per_word(8)
                .max_speed_hz(speed_hz)
                .mode(SpiModeFlags::SPI_MODE_0)
                .build();
            dev.configure(&options)?;
            Ok(Self { dev, recorder })
        }
    }

    impl ErrorType for LinuxSpiDevice {
        type Error = LinuxSpiError;
    }

    impl embedded_hal::spi::SpiDevice<u8> for LinuxSpiDevice {
        fn transaction(&mut self, operations: &mut [Operation<'_, u8>]) -> Result<(), Self::Error> {
            struct Segment {
                plan: SpiSegmentPlan,
                rx: Vec<u8>,
            }

            let mut segments: Vec<Segment> = build_segment_plan(operations)
                .into_iter()
                .map(|plan| {
                    let rx = vec![0; plan.rx_len];
                    Segment { plan, rx }
                })
                .collect();

            if segments.is_empty() {
                return Ok(());
            }

            {
                let mut transfers: Vec<SpidevTransfer<'_, '_>> = segments
                    .iter_mut()
                    .map(|segment| match segment.plan.kind {
                        SpiSegmentKind::DelayUs(microseconds) => SpidevTransfer::delay(microseconds),
                        _ => SpidevTransfer::read_write(&segment.plan.tx, &mut segment.rx),
                    })
                    .collect();
                self.dev.transfer_multiple(&mut transfers)?;
            }

            if let Some(recorder) = &mut self.recorder {
                for segment in &segments {
                    match segment.plan.kind {
                        SpiSegmentKind::DelayUs(_) => {}
                        _ => recorder.record("TXRX", &segment.plan.tx, &segment.rx)?,
                    }
                }
            }

            for segment in segments {
                let Some(op_index) = segment.plan.op_index else {
                    continue;
                };
                match (&mut operations[op_index], segment.plan.kind) {
                    (Operation::Read(buf), SpiSegmentKind::Read) => buf.copy_from_slice(&segment.rx[..buf.len()]),
                    (Operation::Transfer(read, _), SpiSegmentKind::Transfer) => {
                        read.copy_from_slice(&segment.rx[..read.len()]);
                    }
                    (Operation::TransferInPlace(buf), SpiSegmentKind::InPlace) => {
                        buf.copy_from_slice(&segment.rx[..buf.len()]);
                    }
                    _ => {}
                }
            }
            Ok(())
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::LinuxSpiDevice;

#[cfg(test)]
mod tests;
