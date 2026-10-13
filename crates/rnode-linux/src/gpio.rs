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
    #[error("gpio chip {0:?} not found")]
    ChipNotFound(String),
    #[error("io while scanning gpio chips: {0}")]
    Io(#[from] io::Error),
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

pub fn resolve_chip(chip: &str) -> Result<PathBuf, GpioError> {
    let as_path = Path::new(chip);
    if as_path.is_absolute() {
        return Ok(as_path.into());
    }
    if chip.starts_with("gpiochip") {
        return Ok(Path::new("/dev").join(chip));
    }
    for entry in std::fs::read_dir("/dev")? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("gpiochip") {
            continue;
        }
        let path = entry.path();
        if let Ok(chip_dev) = gpiocdev::chip::Chip::from_path(&path) {
            if chip_dev.info()?.label == chip {
                return Ok(path);
            }
        }
    }
    Err(GpioError::ChipNotFound(chip.to_owned()))
}
