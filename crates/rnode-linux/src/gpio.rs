use std::fmt;
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

use embedded_hal::digital::{ErrorKind, ErrorType, InputPin, OutputPin};
use gpiocdev::Request;
use gpiocdev::line::{EdgeDetection, Value};

use crate::config::GpioConfig;
use crate::event_loop::IrqSource;

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

#[derive(Debug)]
pub struct GpioOutputPin {
    req: Request,
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
        Ok(Self { req, offset })
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

#[derive(Debug)]
pub struct GpioPowerGuard {
    pin: GpioOutputPin,
    off_on_drop: bool,
}

impl Drop for GpioPowerGuard {
    fn drop(&mut self) {
        if self.off_on_drop {
            let _ = self.pin.set_low();
        }
    }
}

#[derive(Debug)]
pub struct RadioGpio {
    pub power: Option<GpioPowerGuard>,
    pub reset: GpioOutputPin,
    pub busy: GpioInputPin,
    pub dio1: GpioIrqLine,
    pub rxen: Option<GpioOutputPin>,
}

impl RadioGpio {
    pub fn open(config: &GpioConfig) -> Result<Self, GpioError> {
        let chip = resolve_chip(&config.chip)?;
        let power = config
            .power
            .map(|offset| GpioOutputPin::request(&chip, offset, Value::Active, "rnode-power"))
            .transpose()?
            .map(|pin| GpioPowerGuard {
                pin,
                off_on_drop: config.power_off_on_exit,
            });
        if power.is_some() && config.power_on_delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(config.power_on_delay_ms));
        }
        Ok(Self {
            power,
            reset: GpioOutputPin::request(&chip, config.reset, Value::Active, "rnode-reset")?,
            busy: GpioInputPin::request(&chip, config.busy, "rnode-busy")?,
            dio1: GpioIrqLine::request(&chip, config.dio1, "rnode-dio1")?,
            rxen: config
                .rxen
                .map(|offset| GpioOutputPin::request(&chip, offset, Value::Inactive, "rnode-rxen"))
                .transpose()?,
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
