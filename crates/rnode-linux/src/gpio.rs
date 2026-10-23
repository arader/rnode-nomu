use std::fmt;
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use embedded_hal::digital::{ErrorKind, ErrorType, InputPin, OutputPin};
use gpiocdev::Request;
use gpiocdev::line::{EdgeDetection, Value};

use crate::config::GpioConfig;
use crate::event_loop::IrqSource;
use crate::power::PowerGuard;

#[derive(Debug, thiserror::Error)]
pub enum GpioError {
    #[error("gpio: {0}")]
    Gpio(#[from] gpiocdev::Error),
    #[error("gpio chip {label:?} not found{}", UnreadableList(.unreadable))]
    ChipNotFound {
        label: String,
        /// The chips skipped because their label could not be read.
        unreadable: Vec<UnreadableChip>,
    },
    #[error("io while scanning gpio chips: {0}")]
    Io(#[from] io::Error),
}

/// A `/dev/gpiochip*` whose label could not be read while looking for `[gpio] chip` by label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnreadableChip {
    pub path: PathBuf,
    /// Why: the error opening the chip or reading its info.
    pub error: String,
}

/// The `; could not read the label of ...` part of [`GpioError::ChipNotFound`], if any.
struct UnreadableList<'a>(&'a [UnreadableChip]);

impl fmt::Display for UnreadableList<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some((first, rest)) = self.0.split_first() else {
            return Ok(());
        };
        write!(
            f,
            "; could not read the label of {} ({})",
            first.path.display(),
            first.error
        )?;
        for chip in rest {
            write!(f, ", {} ({})", chip.path.display(), chip.error)?;
        }
        Ok(())
    }
}

impl embedded_hal::digital::Error for GpioError {
    fn kind(&self) -> ErrorKind {
        ErrorKind::Other
    }
}

/// An output line. Clones drive the same line, which the kernel releases when the last clone is
/// dropped: the radio driver drives NRESET and RXEN, and the [`PowerGuard`] keeps clones of them to
/// drive them low before it switches the modem off.
#[derive(Debug, Clone)]
pub struct GpioOutputPin {
    req: Arc<Request>,
    offset: u32,
}

impl GpioOutputPin {
    fn request(chip: &Path, offset: u32, initial: Value, name: &str) -> Result<Self, GpioError> {
        let req = Request::builder()
            .on_chip(chip)
            .with_consumer(name)
            .with_line(offset)
            .as_output(initial)
            .request()?;
        Ok(Self {
            req: Arc::new(req),
            offset,
        })
    }
}

impl ErrorType for GpioOutputPin {
    type Error = GpioError;
}

impl OutputPin for GpioOutputPin {
    fn set_low(&mut self) -> Result<(), Self::Error> {
        self.req.set_value(self.offset, Value::Inactive)?;
        Ok(())
    }

    fn set_high(&mut self) -> Result<(), Self::Error> {
        self.req.set_value(self.offset, Value::Active)?;
        Ok(())
    }
}

#[derive(Debug)]
pub struct GpioInputPin {
    req: Request,
    offset: u32,
}

impl GpioInputPin {
    fn request(chip: &Path, offset: u32, name: &str) -> Result<Self, GpioError> {
        let req = Request::builder()
            .on_chip(chip)
            .with_consumer(name)
            .with_line(offset)
            .as_input()
            .request()?;
        Ok(Self { req, offset })
    }
}

impl ErrorType for GpioInputPin {
    type Error = GpioError;
}

impl InputPin for GpioInputPin {
    fn is_high(&mut self) -> Result<bool, Self::Error> {
        Ok(self.req.value(self.offset)? == Value::Active)
    }

    fn is_low(&mut self) -> Result<bool, Self::Error> {
        Ok(self.req.value(self.offset)? == Value::Inactive)
    }
}

#[derive(Debug)]
pub struct GpioIrqLine {
    req: Request,
}

impl GpioIrqLine {
    fn request(chip: &Path, offset: u32, name: &str) -> Result<Self, GpioError> {
        let req = Request::builder()
            .on_chip(chip)
            .with_consumer(name)
            .with_line(offset)
            .as_input()
            .with_edge_detection(EdgeDetection::RisingEdge)
            .request()?;
        Ok(Self { req })
    }
}

impl IrqSource for GpioIrqLine {
    fn fd(&self) -> Option<BorrowedFd<'_>> {
        Some(self.req.as_fd())
    }

    fn drain(&mut self) -> io::Result<()> {
        loop {
            match self.req.has_edge_event() {
                Ok(true) => {
                    self.req.read_edge_event().map_err(io::Error::other)?;
                }
                Ok(false) => return Ok(()),
                Err(e) => return Err(io::Error::other(e)),
            }
        }
    }
}

/// The modem's lines, as [`RadioGpio::open`] requests them.
#[derive(Debug)]
pub struct RadioGpio {
    /// The power-enable line, if `[gpio] power` is set. It holds clones of `reset` and `rxen`, so
    /// dropping it with `power_off_on_exit` drives them low before the power line, whether or not
    /// the radio driver still holds them.
    pub power: Option<PowerGuard<GpioOutputPin>>,
    pub reset: GpioOutputPin,
    pub busy: GpioInputPin,
    pub dio1: GpioIrqLine,
    pub rxen: Option<GpioOutputPin>,
}

impl RadioGpio {
    /// Request the lines: the power line first, driven high, then, `power_on_delay_ms` later,
    /// NRESET (high), BUSY, DIO1 and RXEN (low). The power guard takes its clone of each output as
    /// soon as it is requested, so if a later line cannot be requested, dropping the guard switches
    /// the modem off in the same order as on exit (with `power_off_on_exit`).
    pub fn open(config: &GpioConfig) -> Result<Self, GpioError> {
        let chip = resolve_chip(&config.chip)?;
        let mut power = config
            .power
            .map(|offset| GpioOutputPin::request(&chip, offset, Value::Active, "rnode-power"))
            .transpose()?
            .map(|pin| PowerGuard::new(pin, config.power_off_on_exit));
        if power.is_some() && config.power_on_delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(config.power_on_delay_ms));
        }
        let reset = GpioOutputPin::request(&chip, config.reset, Value::Active, "rnode-reset")?;
        if let Some(guard) = &mut power {
            guard.lower_on_power_off("reset", reset.clone());
        }
        let busy = GpioInputPin::request(&chip, config.busy, "rnode-busy")?;
        let dio1 = GpioIrqLine::request(&chip, config.dio1, "rnode-dio1")?;
        let rxen = config
            .rxen
            .map(|offset| GpioOutputPin::request(&chip, offset, Value::Inactive, "rnode-rxen"))
            .transpose()?;
        if let (Some(guard), Some(rxen)) = (&mut power, &rxen) {
            guard.lower_on_power_off("rxen", rxen.clone());
        }
        Ok(Self {
            power,
            reset,
            busy,
            dio1,
            rxen,
        })
    }
}

/// The character device for `[gpio] chip`: an absolute path as is, `gpiochipN` under `/dev`, and
/// anything else as the label of a `/dev/gpiochip*` (for example `pinctrl-rp1`).
pub fn resolve_chip(chip: &str) -> Result<PathBuf, GpioError> {
    let as_path = Path::new(chip);
    if as_path.is_absolute() {
        return Ok(as_path.into());
    }
    if chip.starts_with("gpiochip") {
        return Ok(Path::new("/dev").join(chip));
    }
    let mut candidates = Vec::new();
    for entry in std::fs::read_dir("/dev")? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with("gpiochip") {
            candidates.push(entry.path());
        }
    }
    candidates.sort();
    find_chip_by_label(chip, candidates, |path| {
        let info = gpiocdev::chip::Chip::from_path(path)?.info()?;
        Ok::<_, gpiocdev::Error>(info.label)
    })
}

/// The first of `candidates` whose label, as `read_label` gives it, is `label`. A chip whose label
/// cannot be read (no permission, removed during the scan, a failing ioctl) is skipped instead of
/// ending the scan, and listed in [`GpioError::ChipNotFound`] if no chip matches (#55).
pub fn find_chip_by_label<E: fmt::Display>(
    label: &str,
    candidates: impl IntoIterator<Item = PathBuf>,
    mut read_label: impl FnMut(&Path) -> Result<String, E>,
) -> Result<PathBuf, GpioError> {
    let mut unreadable = Vec::new();
    for path in candidates {
        match read_label(&path) {
            Ok(found) if found == label => return Ok(path),
            Ok(_) => {}
            Err(error) => {
                log::debug!("skipping {}: cannot read its label: {error}", path.display());
                unreadable.push(UnreadableChip {
                    path,
                    error: error.to_string(),
                });
            }
        }
    }
    Err(GpioError::ChipNotFound {
        label: label.to_owned(),
        unreadable,
    })
}
