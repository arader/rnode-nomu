//! The power-down sequence of `[gpio] power_off_on_exit` (#86): with the setting, NRESET and then
//! RXEN go low before the power line, after a stop, an error or an early return, whatever is
//! dropped first; without it, no line changes. Mock lines record what happens to them. Like the
//! daemon's `GpioOutputPin`, clones of a mock line drive the same line, and the line is released
//! when the last clone is dropped (Raspberry Pi kernels keep a released output at its level).

use std::cell::RefCell;
use std::convert::Infallible;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::rc::Rc;
use std::sync::atomic::AtomicBool;

use embedded_hal::delay::DelayNs;
use embedded_hal::digital::{self, ErrorType, InputPin, OutputPin};
use embedded_hal::spi::{self, Operation, SpiDevice};
use rnode_core::{MemoryStorage, Node, NodeConfig, RadioSettings};
use rnode_linux::event_loop::{EventLoopError, IrqSource, NoIrq, run};
use rnode_linux::power::PowerGuard;
use rnode_linux::pty::PtyPair;
use rnode_sx126x::Sx126x;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    High(&'static str),
    Low(&'static str),
    /// The last clone of the line was dropped, so the kernel released it.
    Released(&'static str),
    /// The SX1262 driver sent `SetSleep`.
    Sleep,
}

use Event::{High, Low, Released, Sleep};

type Log = Rc<RefCell<Vec<Event>>>;

/// A mock output line; clones drive the same line.
#[derive(Debug, Clone)]
struct MockLine(Rc<Line>);

#[derive(Debug)]
struct Line {
    name: &'static str,
    log: Log,
    /// Whether driving the line low fails.
    stuck: bool,
}

impl Drop for Line {
    fn drop(&mut self) {
        self.log.borrow_mut().push(Released(self.name));
    }
}

impl MockLine {
    fn new(name: &'static str, log: &Log) -> Self {
        Self(Rc::new(Line {
            name,
            log: Rc::clone(log),
            stuck: false,
        }))
    }

    /// A line that cannot be driven low.
    fn stuck(name: &'static str, log: &Log) -> Self {
        Self(Rc::new(Line {
            name,
            log: Rc::clone(log),
            stuck: true,
        }))
    }
}

#[derive(Debug)]
struct StuckLine;

impl digital::Error for StuckLine {
    fn kind(&self) -> digital::ErrorKind {
        digital::ErrorKind::Other
    }
}

impl ErrorType for MockLine {
    type Error = StuckLine;
}

impl OutputPin for MockLine {
    fn set_low(&mut self) -> Result<(), StuckLine> {
        if self.0.stuck {
            return Err(StuckLine);
        }
        self.0.log.borrow_mut().push(Low(self.0.name));
        Ok(())
    }

    fn set_high(&mut self) -> Result<(), StuckLine> {
        self.0.log.borrow_mut().push(High(self.0.name));
        Ok(())
    }
}

/// The power guard as `RadioGpio::open()` sets it up, and the radio driver's clones of NRESET and
/// RXEN.
fn open_lines(log: &Log, power_off_on_exit: bool) -> (PowerGuard<MockLine>, MockLine, MockLine) {
    let mut guard = PowerGuard::new(MockLine::new("power", log), power_off_on_exit);
    let reset = MockLine::new("reset", log);
    guard.lower_on_power_off("reset", reset.clone());
    let rxen = MockLine::new("rxen", log);
    guard.lower_on_power_off("rxen", rxen.clone());
    (guard, reset, rxen)
}

/// What the SX1262 driver does with the lines in `begin()`: a reset pulse, then RXEN high.
fn begin(reset: &mut MockLine, rxen: &mut MockLine) {
    reset.set_low().unwrap();
    reset.set_high().unwrap();
    rxen.set_high().unwrap();
}

#[test]
fn power_off_on_exit_drives_reset_and_rxen_low_before_the_power_line() {
    let log = Log::default();
    let (guard, mut reset, mut rxen) = open_lines(&log, true);
    begin(&mut reset, &mut rxen);
    // At the end of `run()` the node drops the driver, then `run_sx1262()` drops the guard.
    drop((reset, rxen));
    drop(guard);
    assert_eq!(
        *log.borrow(),
        [
            Low("reset"),
            High("reset"),
            High("rxen"),
            Low("reset"),
            Low("rxen"),
            Low("power"),
            Released("power"),
            Released("reset"),
            Released("rxen"),
        ]
    );
}

/// An early return in `run_sx1262()`, after `RadioGpio::open()`, drops the guard (the first field
/// of `RadioGpio`) before the driver's lines.
#[test]
fn power_off_on_exit_runs_the_same_sequence_when_the_guard_is_dropped_first() {
    let log = Log::default();
    let (guard, reset, rxen) = open_lines(&log, true);
    drop(guard);
    assert_eq!(
        *log.borrow(),
        [Low("reset"), Low("rxen"), Low("power"), Released("power")]
    );
    drop((reset, rxen));
    assert_eq!(log.borrow()[4..], [Released("reset"), Released("rxen")]);
}

#[test]
fn without_power_off_on_exit_no_line_changes() {
    let log = Log::default();
    let (guard, mut reset, mut rxen) = open_lines(&log, false);
    begin(&mut reset, &mut rxen);
    drop((reset, rxen));
    drop(guard);
    // Released at their levels: the rail stays on, NRESET and RXEN stay high.
    assert_eq!(
        *log.borrow(),
        [
            Low("reset"),
            High("reset"),
            High("rxen"),
            Released("power"),
            Released("reset"),
            Released("rxen"),
        ]
    );
}

#[test]
fn power_off_on_exit_still_switches_the_rail_off_when_a_line_cannot_be_driven_low() {
    let log = Log::default();
    let mut guard = PowerGuard::new(MockLine::new("power", &log), true);
    guard.lower_on_power_off("reset", MockLine::stuck("reset", &log));
    guard.lower_on_power_off("rxen", MockLine::new("rxen", &log));
    drop(guard);
    assert_eq!(
        *log.borrow(),
        [
            Low("rxen"),
            Low("power"),
            Released("power"),
            Released("reset"),
            Released("rxen")
        ]
    );
}

/// The settings the node finds saved, so that it starts the radio when `run()` boots it.
const SAVED_SETTINGS: RadioSettings = RadioSettings {
    frequency: 867_200_000,
    bandwidth: 125_000,
    spreading_factor: 7,
    coding_rate: 5,
    tx_power: 2,
};

/// An SX1262 behind a mock SPI bus: it reads back the sync word that `begin()` probes for and 0
/// from every other register, and records `SetSleep`.
struct MockSx1262 {
    log: Log,
}

impl spi::ErrorType for MockSx1262 {
    type Error = Infallible;
}

impl SpiDevice for MockSx1262 {
    fn transaction(&mut self, operations: &mut [Operation<'_, u8>]) -> Result<(), Infallible> {
        let mut written = Vec::new();
        for operation in operations {
            match operation {
                Operation::Write(bytes) => written.extend_from_slice(bytes),
                Operation::Read(out) => {
                    out.fill(0);
                    // ReadRegister of the sync word, 0x0740 and 0x0741: 0x1424 after a reset.
                    match written[..] {
                        [0x1D, 0x07, 0x40, 0x00] => out[0] = 0x14,
                        [0x1D, 0x07, 0x41, 0x00] => out[0] = 0x24,
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        if written == [0x84, 0x00] {
            self.log.borrow_mut().push(Sleep);
        }
        Ok(())
    }
}

struct NotBusy;

impl ErrorType for NotBusy {
    type Error = Infallible;
}

impl InputPin for NotBusy {
    fn is_high(&mut self) -> Result<bool, Infallible> {
        Ok(false)
    }

    fn is_low(&mut self) -> Result<bool, Infallible> {
        Ok(true)
    }
}

struct NoDelay;

impl DelayNs for NoDelay {
    fn delay_ns(&mut self, _: u32) {}
}

/// A DIO1 line that fires at once and then cannot be read, so `run()` fails.
struct FailingIrq {
    read: OwnedFd,
    _write: OwnedFd,
}

impl FailingIrq {
    fn new() -> Self {
        let (read, write) = nix::unistd::pipe().unwrap();
        nix::unistd::write(&write, &[1]).unwrap();
        Self { read, _write: write }
    }
}

impl IrqSource for FailingIrq {
    fn fd(&self) -> Option<BorrowedFd<'_>> {
        Some(self.read.as_fd())
    }

    fn drain(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::other("DIO1 line lost"))
    }
}

/// `run_sx1262()` without the hardware: the real SX1262 driver and event loop, with the mock lines.
fn run_daemon(
    reset: MockLine,
    rxen: Option<MockLine>,
    irq: impl IrqSource,
    shutdown: &AtomicBool,
    log: &Log,
) -> Result<(), EventLoopError> {
    let spi = MockSx1262 { log: Rc::clone(log) };
    let radio = Sx126x::new(spi, NotBusy, reset, rxen, NoDelay, rnode_sx126x::Config::default());
    let node = Node::new(
        radio,
        MemoryStorage::with_settings(SAVED_SETTINGS),
        NodeConfig::default(),
        1,
    );
    run(node, irq, PtyPair::open(None).unwrap(), shutdown)
}

/// A stop: the node puts the modem to sleep, then the guard switches it off.
#[test]
fn stop_puts_the_modem_to_sleep_before_the_guard_switches_it_off() {
    let log = Log::default();
    let (guard, reset, rxen) = open_lines(&log, true);
    // As if SIGTERM arrived as the daemon started: `run()` starts the radio and stops at once.
    let shutdown = AtomicBool::new(true);
    run_daemon(reset, Some(rxen), NoIrq, &shutdown, &log).unwrap();
    drop(guard);
    let log = log.borrow();
    let sleep = log
        .iter()
        .position(|e| *e == Sleep)
        .expect("the node put the modem to sleep");
    assert!(log[..sleep].contains(&High("reset")), "the radio started: {log:?}");
    assert_eq!(
        log[sleep..],
        [
            Sleep,
            Low("reset"),
            Low("rxen"),
            Low("power"),
            Released("power"),
            Released("reset"),
            Released("rxen"),
        ]
    );
}

/// `run()` fails while the radio is on: the modem is not put to sleep, but the guard still
/// switches it off in order. No RXEN line, as on the uConsole AIO V2.
#[test]
fn run_error_still_switches_the_modem_off_in_order() {
    let log = Log::default();
    let mut guard = PowerGuard::new(MockLine::new("power", &log), true);
    let reset = MockLine::new("reset", &log);
    guard.lower_on_power_off("reset", reset.clone());
    let shutdown = AtomicBool::new(false);
    let result = run_daemon(reset, None, FailingIrq::new(), &shutdown, &log);
    assert!(matches!(result, Err(EventLoopError::Irq(_))), "{result:?}");
    let ran = log.borrow().len();
    assert_eq!(
        log.borrow()[..],
        [Low("reset"), High("reset")],
        "the radio started and nothing was released"
    );
    drop(guard);
    assert_eq!(
        log.borrow()[ran..],
        [Low("reset"), Low("power"), Released("power"), Released("reset")]
    );
}
