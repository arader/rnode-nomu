//! The RX- and TX-enable lines around the SX1262's commands: TXEN goes high before a
//! transmission and low before receive and after `SetSleep` (firmware `beginPacket()`,
//! `receive()` and `sleep_now()` with the KCT8103L's CTX), RXEN goes high on receive (firmware
//! `rxAntEnable()`), and with both lines the driver never has both high. One log records the
//! opcode of each SPI transaction and each level written to a line, in order.

use std::cell::{Cell, RefCell};
use std::convert::Infallible;
use std::fmt;
use std::rc::Rc;

use embedded_hal::delay::DelayNs;
use embedded_hal::digital::{ErrorKind, ErrorType, InputPin, OutputPin};
use embedded_hal::spi::{self, Operation, SpiDevice};
use rnode_core::radio::{Radio, TxOutcome};
use rnode_sx126x::{Config, Error, NoPin, Sx126x};

// SX1262 opcodes (datasheet; firmware `sx126x.h`).
const SET_STANDBY: u8 = 0x80;
const SET_SLEEP: u8 = 0x84;
const SET_TX: u8 = 0x83;
const SET_RX: u8 = 0x82;
const SET_PACKET_PARAMS: u8 = 0x8C;
const SET_DIO2_RF_SWITCH: u8 = 0x9D;
const SET_RF_FREQUENCY: u8 = 0x86;
const WRITE_REGISTER: u8 = 0x0D;
const READ_REGISTER: u8 = 0x1D;
const WRITE_BUFFER: u8 = 0x0E;
const GET_IRQ_STATUS: u8 = 0x12;
const CLEAR_IRQ_STATUS: u8 = 0x02;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    /// An SPI transaction, by its opcode.
    Spi(u8),
    High(&'static str),
    Low(&'static str),
}

use Event::{High, Low, Spi};

type Log = Rc<RefCell<Vec<Event>>>;

/// An SX1262 on a mock bus: it answers the sync-word probe of `begin()`, reports TX done at once,
/// reads 0 otherwise, and fails every transaction once `failing` is set.
#[derive(Debug)]
struct Bus {
    log: Log,
    failing: Rc<Cell<bool>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BusError;

impl fmt::Display for BusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("bus error")
    }
}

impl spi::Error for BusError {
    fn kind(&self) -> spi::ErrorKind {
        spi::ErrorKind::Other
    }
}

impl spi::ErrorType for Bus {
    type Error = BusError;
}

impl SpiDevice for Bus {
    fn transaction(&mut self, operations: &mut [Operation<'_, u8>]) -> Result<(), BusError> {
        if self.failing.get() {
            return Err(BusError);
        }
        let mut written = Vec::new();
        for operation in operations {
            match operation {
                Operation::Write(bytes) => written.extend_from_slice(bytes),
                Operation::Read(out) => {
                    out.fill(0);
                    match written[..] {
                        [READ_REGISTER, 0x07, 0x40, 0x00] => out[0] = 0x14,
                        [READ_REGISTER, 0x07, 0x41, 0x00] => out[0] = 0x24,
                        [GET_IRQ_STATUS, 0x00] => out[1] = 0x01, // TxDone
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        self.log.borrow_mut().push(Spi(written[0]));
        Ok(())
    }
}

/// An output line that logs the levels written to it.
#[derive(Debug)]
struct Line {
    name: &'static str,
    log: Log,
}

impl ErrorType for Line {
    type Error = Infallible;
}

impl OutputPin for Line {
    fn set_low(&mut self) -> Result<(), Infallible> {
        self.log.borrow_mut().push(Low(self.name));
        Ok(())
    }

    fn set_high(&mut self) -> Result<(), Infallible> {
        self.log.borrow_mut().push(High(self.name));
        Ok(())
    }
}

/// A line that cannot be driven low once `stuck` is set, as a GPIO line the kernel took back.
#[derive(Debug)]
struct StuckLine {
    line: Line,
    stuck: Rc<Cell<bool>>,
}

impl ErrorType for StuckLine {
    type Error = ErrorKind;
}

impl OutputPin for StuckLine {
    fn set_low(&mut self) -> Result<(), ErrorKind> {
        if self.stuck.get() {
            return Err(ErrorKind::Other);
        }
        self.line.set_low().map_err(|never| match never {})
    }

    fn set_high(&mut self) -> Result<(), ErrorKind> {
        self.line.set_high().map_err(|never| match never {})
    }
}

#[derive(Debug)]
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

#[derive(Debug)]
struct NoDelay;

impl DelayNs for NoDelay {
    fn delay_ns(&mut self, _: u32) {}
}

/// A driver on the mock bus, its log, and the switch that makes the bus fail.
struct Setup<RXEN, TXEN> {
    radio: Sx126x<Bus, NotBusy, Line, RXEN, TXEN, NoDelay>,
    log: Log,
    failing: Rc<Cell<bool>>,
}

impl<RXEN: OutputPin, TXEN: OutputPin> Setup<RXEN, TXEN> {
    /// The events since the last call, emptying the log.
    fn take(&self) -> Vec<Event> {
        std::mem::take(&mut *self.log.borrow_mut())
    }
}

/// A driver with the RX- and TX-enable lines that `rxen` and `txen` make on the shared log.
fn setup<RXEN: OutputPin, TXEN: OutputPin>(
    rxen: impl FnOnce(&Log) -> Option<RXEN>,
    txen: impl FnOnce(&Log) -> Option<TXEN>,
) -> Setup<RXEN, TXEN> {
    let log = Log::default();
    let failing = Rc::new(Cell::new(false));
    let bus = Bus {
        log: Rc::clone(&log),
        failing: Rc::clone(&failing),
    };
    let reset = Line {
        name: "reset",
        log: Rc::clone(&log),
    };
    let radio = Sx126x::new(bus, NotBusy, reset, rxen(&log), txen(&log), NoDelay, Config::default());
    Setup { radio, log, failing }
}

fn rxen(log: &Log) -> Option<Line> {
    Some(Line {
        name: "rxen",
        log: Rc::clone(log),
    })
}

fn txen(log: &Log) -> Option<Line> {
    Some(Line {
        name: "txen",
        log: Rc::clone(log),
    })
}

fn none(_: &Log) -> Option<NoPin> {
    None
}

/// `transmit()` after the lines are switched: SetStandby, SetPacketParams with its IQ register
/// fix (a register read and a write), WriteBuffer, SetPacketParams again, SetTx, then one
/// GetIrqStatus that reports TX done, and ClearIrqStatus.
const TRANSMIT: [Event; 11] = [
    Spi(SET_STANDBY),
    Spi(SET_PACKET_PARAMS),
    Spi(READ_REGISTER),
    Spi(WRITE_REGISTER),
    Spi(WRITE_BUFFER),
    Spi(SET_PACKET_PARAMS),
    Spi(READ_REGISTER),
    Spi(WRITE_REGISTER),
    Spi(SET_TX),
    Spi(GET_IRQ_STATUS),
    Spi(CLEAR_IRQ_STATUS),
];

/// The SetPacketParams of `receive()`, with its IQ register fix, before SetRx.
const RECEIVE_PACKET_PARAMS: [Event; 3] = [Spi(SET_PACKET_PARAMS), Spi(READ_REGISTER), Spi(WRITE_REGISTER)];

/// Only the line events of `events`.
fn lines(events: &[Event]) -> Vec<Event> {
    events.iter().copied().filter(|e| !matches!(e, Spi(_))).collect()
}

#[test]
fn transmit_switches_rxen_off_then_txen_on_before_any_command() {
    let mut s = setup(rxen, txen);
    assert_eq!(s.radio.transmit(&[1, 2, 3], false).unwrap(), TxOutcome::Done);
    assert_eq!(s.take(), [[Low("rxen"), High("txen")].as_slice(), &TRANSMIT].concat());
}

#[test]
fn receive_switches_txen_off_first_and_rxen_on_just_before_set_rx() {
    let mut s = setup(rxen, txen);
    s.radio.receive(None).unwrap();
    assert_eq!(
        s.take(),
        [
            [Low("txen")].as_slice(),
            &RECEIVE_PACKET_PARAMS,
            &[High("rxen"), Spi(SET_RX)]
        ]
        .concat()
    );
}

/// The frames of a burst go out with TXEN high throughout, as in the firmware, whose
/// `beginPacket()` raises CTX for each frame and whose `receive()` alone lowers it.
#[test]
fn a_burst_keeps_the_switch_on_transmit_until_receive() {
    let mut s = setup(rxen, txen);
    s.radio.transmit(&[1], false).unwrap();
    s.radio.transmit(&[2], false).unwrap();
    assert_eq!(lines(&s.take()), [Low("rxen"), High("txen"), Low("rxen"), High("txen")]);
    s.radio.receive(None).unwrap();
    assert_eq!(lines(&s.take()), [Low("txen"), High("rxen")]);
}

/// `begin()` switches to receive where the firmware raises RXEN, after the RF switch setup
/// (SetDIO2AsRfSwitchCtrl) and before SetRfFrequency: TXEN low first.
#[test]
fn begin_switches_txen_off_before_rxen_on() {
    let mut s = setup(rxen, txen);
    s.radio.begin(868_000_000).unwrap();
    let events = s.take();
    let dio2 = events
        .iter()
        .position(|e| *e == Spi(SET_DIO2_RF_SWITCH))
        .expect("SetDIO2AsRfSwitchCtrl");
    assert_eq!(
        events[dio2..dio2 + 4],
        [
            Spi(SET_DIO2_RF_SWITCH),
            Low("txen"),
            High("rxen"),
            Spi(SET_RF_FREQUENCY)
        ]
    );
    assert_eq!(lines(&events), [Low("reset"), High("reset"), Low("txen"), High("rxen")]);
}

/// `end()` puts the modem to sleep, then drives TXEN low, as firmware `sleep_now()` drives the
/// PA's lines low after `stopRadio()`. It does so even when SetSleep fails, and returns that error.
#[test]
fn end_drives_txen_low_after_set_sleep_even_when_it_fails() {
    let mut s = setup(rxen, txen);
    s.radio.transmit(&[1], false).unwrap();
    s.take();
    s.radio.end().unwrap();
    assert_eq!(s.take(), [Spi(SET_SLEEP), Low("txen")]);

    s.radio.transmit(&[1], false).unwrap();
    s.take();
    s.failing.set(true);
    assert!(matches!(s.radio.end(), Err(Error::Spi(_))));
    assert_eq!(s.take(), [Low("txen")]);
}

/// When neither SetSleep nor driving TXEN low works, `end()` keeps both errors in
/// `Error::SleepAndTxen`, whose message puts TXEN first: it leaves the PA switched to transmit.
/// TXEN failing alone is the plain GPIO error it was.
#[test]
fn end_reports_both_errors_when_set_sleep_and_txen_both_fail() {
    let stuck = Rc::new(Cell::new(false));
    let stuck_switch = Rc::clone(&stuck);
    let mut s = setup(rxen, move |log| {
        Some(StuckLine {
            line: txen(log).unwrap(),
            stuck: stuck_switch,
        })
    });
    s.radio.transmit(&[1], false).unwrap();
    s.take();
    stuck.set(true);

    assert_eq!(s.radio.end(), Err(Error::Digital(ErrorKind::Other)), "TXEN alone");
    assert_eq!(s.take(), [Spi(SET_SLEEP)]);

    s.failing.set(true);
    let both = s.radio.end().unwrap_err();
    assert_eq!(
        both,
        Error::SleepAndTxen {
            sleep: Box::new(Error::Spi(BusError)),
            txen: ErrorKind::Other,
        }
    );
    assert_eq!(
        both.to_string(),
        "TXEN could not be driven low (GPIO error: Other), so the PA may still be switched to transmit; \
         SetSleep failed too: SPI error: bus error"
    );
    assert_eq!(s.take(), [], "neither reached the modem nor the line");
}

/// Over a whole session, replaying the levels written: RXEN and TXEN, both low at the start as
/// the daemon requests them, are never high together.
#[test]
fn rxen_and_txen_are_never_high_together() {
    let mut s = setup(rxen, txen);
    s.radio.begin(868_000_000).unwrap();
    s.radio.receive(None).unwrap();
    for frame in [1, 2, 3] {
        s.radio.transmit(&[frame], false).unwrap();
    }
    s.radio.receive(Some(7)).unwrap();
    s.radio.transmit(&[4], true).unwrap();
    s.radio.end().unwrap();
    s.radio.begin(868_000_000).unwrap();
    s.radio.receive(None).unwrap();

    let (mut rx, mut tx) = (false, false);
    let switched = lines(&s.take());
    for (i, event) in switched.iter().enumerate() {
        match *event {
            High("rxen") => rx = true,
            Low("rxen") => rx = false,
            High("txen") => tx = true,
            Low("txen") => tx = false,
            _ => {}
        }
        assert!(!(rx && tx), "both high after {:?}", &switched[..=i]);
    }
    assert!(rx && !tx, "the session ends receiving: {switched:?}");
}

/// A board with TXEN but no RXEN: TXEN alone, around the same commands.
#[test]
fn txen_without_rxen() {
    let mut s = setup(none, txen);
    s.radio.transmit(&[1], false).unwrap();
    assert_eq!(s.take(), [[High("txen")].as_slice(), &TRANSMIT].concat());
    s.radio.receive(None).unwrap();
    assert_eq!(
        s.take(),
        [[Low("txen")].as_slice(), &RECEIVE_PACKET_PARAMS, &[Spi(SET_RX)]].concat()
    );
    s.radio.end().unwrap();
    assert_eq!(s.take(), [Spi(SET_SLEEP), Low("txen")]);
}

/// Without TXEN, RXEN is driven as in the firmware: high on receive, and never low, around a
/// transmission or at sleep either.
#[test]
fn without_txen_rxen_is_driven_as_in_the_firmware() {
    let mut s = setup(rxen, none);
    s.radio.transmit(&[1], false).unwrap();
    assert_eq!(s.take(), TRANSMIT);
    s.radio.receive(None).unwrap();
    assert_eq!(
        s.take(),
        [RECEIVE_PACKET_PARAMS.as_slice(), &[High("rxen"), Spi(SET_RX)]].concat()
    );
    s.radio.end().unwrap();
    assert_eq!(s.take(), [Spi(SET_SLEEP)]);
}
