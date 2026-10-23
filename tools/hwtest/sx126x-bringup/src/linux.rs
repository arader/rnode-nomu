use std::fmt;
use std::io;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use embedded_hal::delay::DelayNs;
use embedded_hal::digital::{ErrorKind as PinErrorKind, ErrorType as PinErrorType, InputPin, OutputPin};
use embedded_hal::spi::{ErrorKind as SpiErrorKind, ErrorType as SpiErrorType, Operation, SpiDevice};
use gpiocdev::line::{Value, Values};
use rnode_core::phy::{self, Modulation};
use rnode_core::radio::{Radio, TxOutcome};
use rnode_sx126x::{Config, NoPin, Sx126x, TcxoVoltage};
use spidev::{SpiModeFlags, Spidev, SpidevOptions, SpidevTransfer};

const DEFAULT_SPI: &str = "/dev/spidev1.0";
const DEFAULT_CHIP: &str = "/dev/gpiochip15";
const DEFAULT_RESET: u32 = 25;
const DEFAULT_BUSY: u32 = 24;
const DEFAULT_DIO1: u32 = 26;
const DEFAULT_FREQ_HZ: u32 = 914_875_000;
const DEFAULT_BW_HZ: u32 = 125_000;
const DEFAULT_SF: u8 = 7;
const DEFAULT_CR: u8 = 5;
const DEFAULT_POWER_DBM: i8 = 2;
const DEFAULT_SPI_HZ: u32 = 16_000_000;

#[derive(Debug, Clone)]
struct Args {
    mode: Mode,
    spi: String,
    gpiochip: String,
    reset: u32,
    busy: u32,
    dio1: u32,
    freq_hz: u32,
    bw_hz: u32,
    sf: u8,
    cr: u8,
    power_dbm: i8,
    count: u32,
    interval_ms: u64,
    start_delay_ms: u64,
    payload_len: usize,
    implicit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Bringup,
    Tx,
    RxIdle,
}

pub fn run() -> Result<()> {
    let args = Args::parse()?;
    eprintln!("sx126x-bringup args: {args:?}");
    validate_regulatory(&args)?;

    let mut radio = open_radio(&args)?;
    let dio1 = GpioInput::open(&args.gpiochip, args.dio1).context("open DIO1 input")?;
    configure_radio(&mut radio, &args)?;

    match args.mode {
        Mode::Bringup => run_bringup(radio, dio1, &args),
        Mode::Tx => run_tx(radio, &args),
        Mode::RxIdle => run_rx_idle(radio, dio1, &args),
    }
}

impl Default for Args {
    fn default() -> Self {
        Self {
            mode: Mode::Bringup,
            spi: DEFAULT_SPI.into(),
            gpiochip: DEFAULT_CHIP.into(),
            reset: DEFAULT_RESET,
            busy: DEFAULT_BUSY,
            dio1: DEFAULT_DIO1,
            freq_hz: DEFAULT_FREQ_HZ,
            bw_hz: DEFAULT_BW_HZ,
            sf: DEFAULT_SF,
            cr: DEFAULT_CR,
            power_dbm: DEFAULT_POWER_DBM,
            count: 3,
            interval_ms: 1500,
            start_delay_ms: 0,
            payload_len: 50,
            implicit: false,
        }
    }
}

impl Args {
    fn parse() -> Result<Self> {
        let mut args = Self::default();
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                "--mode" => {
                    args.mode = match next(&mut it, "--mode")?.as_str() {
                        "bringup" => Mode::Bringup,
                        "tx" => Mode::Tx,
                        "rx-idle" => Mode::RxIdle,
                        other => bail!("unknown --mode {other:?}"),
                    };
                }
                "--spi" => args.spi = next(&mut it, "--spi")?,
                "--gpiochip" => args.gpiochip = next(&mut it, "--gpiochip")?,
                "--reset" => args.reset = parse_next(&mut it, "--reset")?,
                "--busy" => args.busy = parse_next(&mut it, "--busy")?,
                "--dio1" => args.dio1 = parse_next(&mut it, "--dio1")?,
                "--freq-hz" => args.freq_hz = parse_next(&mut it, "--freq-hz")?,
                "--bw-hz" => args.bw_hz = parse_next(&mut it, "--bw-hz")?,
                "--sf" => args.sf = parse_next(&mut it, "--sf")?,
                "--cr" => args.cr = parse_next(&mut it, "--cr")?,
                "--power-dbm" => args.power_dbm = parse_next(&mut it, "--power-dbm")?,
                "--count" => args.count = parse_next(&mut it, "--count")?,
                "--interval-ms" => args.interval_ms = parse_next(&mut it, "--interval-ms")?,
                "--start-delay-ms" => args.start_delay_ms = parse_next(&mut it, "--start-delay-ms")?,
                "--payload-len" => args.payload_len = parse_next(&mut it, "--payload-len")?,
                "--implicit" => args.implicit = true,
                other => bail!("unknown argument {other:?}; use --help"),
            }
        }
        Ok(args)
    }
}

fn print_help() {
    println!(
        "Usage: sx126x-bringup [--mode bringup|tx|rx-idle] [options]\n\
         Defaults target the uConsole AIO V2: /dev/spidev1.0, gpiochip15 reset=25 busy=24 dio1=26.\n\
         Options: --freq-hz N --bw-hz N --sf N --cr N --power-dbm N --count N \\\n         --payload-len N --interval-ms N --start-delay-ms N --implicit"
    );
}

fn next(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
    it.next().with_context(|| format!("missing value for {flag}"))
}

fn parse_next<T>(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: fmt::Display,
{
    next(it, flag)?
        .parse::<T>()
        .map_err(|err| anyhow::anyhow!("invalid value for {flag}: {err}"))
}

fn validate_regulatory(args: &Args) -> Result<()> {
    if !(902_000_000..=928_000_000).contains(&args.freq_hz) {
        bail!(
            "refusing to transmit or configure outside US 902-928 MHz ISM band: {} Hz",
            args.freq_hz
        );
    }
    if args.power_dbm > 10 {
        bail!("refusing power above 10 dBm; requested {} dBm", args.power_dbm);
    }
    Ok(())
}

type RadioDev = Sx126x<LinuxSpiDevice, GpioInput, GpioOutput, NoPin, NoPin, StdDelay>;

fn open_radio(args: &Args) -> Result<RadioDev> {
    let spi = LinuxSpiDevice::open(&args.spi, DEFAULT_SPI_HZ).context("open SPI")?;
    let busy = GpioInput::open(&args.gpiochip, args.busy).context("open BUSY input")?;
    let reset = GpioOutput::open(&args.gpiochip, args.reset, true).context("open RESET output")?;
    let config = Config {
        tcxo: Some(TcxoVoltage::V1_8),
        dio2_as_rf_switch: true,
        ocp: 0x28,
        busy_timeout_ms: 250,
        tx_timeout_ms: 30_000,
        lna_gain_db: 0, // The AIO V2 has no LNA in front of the SX1262.
    };
    Ok(Sx126x::new(spi, busy, reset, None, None, StdDelay, config))
}

fn configure_radio(radio: &mut RadioDev, args: &Args) -> Result<()> {
    radio.begin(args.freq_hz).context("begin SX126x")?;
    radio.set_bandwidth(args.bw_hz).context("set bandwidth")?;
    radio.set_spreading_factor(args.sf).context("set spreading factor")?;
    radio.set_coding_rate(args.cr).context("set coding rate")?;
    radio.set_tx_power(args.power_dbm).context("set TX power")?;
    let phy = phy_params(radio);
    radio
        .set_preamble_length(phy.preamble_symbols)
        .context("set preamble")?;
    radio.set_crc(true).context("enable CRC")?;
    radio.set_dcd_timing(phy.preamble_time_ms, phy.header_time_ms);
    println!(
        "CONFIG freq_hz={} bw_hz={} sf={} cr=4/{} power_dbm={} preamble_symbols={} ldro={} symbol_time_ms={:.3} bitrate_bps={}",
        radio.frequency(),
        radio.bandwidth(),
        radio.spreading_factor(),
        radio.coding_rate(),
        radio.tx_power(),
        phy.preamble_symbols,
        radio.low_data_rate(),
        phy.symbol_time_ms,
        phy.bitrate_bps
    );
    Ok(())
}

fn phy_params(radio: &RadioDev) -> phy::PhyParams {
    phy::PhyParams::new(Modulation {
        bandwidth_hz: radio.bandwidth(),
        spreading_factor: radio.spreading_factor(),
        coding_rate: radio.coding_rate(),
    })
}

fn run_bringup(mut radio: RadioDev, dio1: GpioInput, _args: &Args) -> Result<()> {
    let start = Instant::now();
    let phy = phy_params(&radio);
    println!("BEGIN ok");
    radio.receive(None).context("enter receive for idle RSSI/DCD")?;
    sleep(Duration::from_millis(200));
    let mut rssi = Vec::new();
    for idx in 0..10 {
        let val = radio.current_rssi().context("current RSSI")?;
        let dcd = radio.dcd(start.elapsed().as_millis() as u64).context("DCD")?;
        println!("RSSI sample={} current_rssi_dbm={} dcd={}", idx + 1, val, dcd);
        rssi.push(val);
        sleep(Duration::from_millis(100));
    }
    let avg = rssi.iter().map(|v| i32::from(*v)).sum::<i32>() as f32 / rssi.len() as f32;
    println!("RSSI avg_dbm={avg:.1}");

    for len in [1_usize, 50, 255] {
        transmit_one(&mut radio, len, false)?;
        sleep(Duration::from_millis(500));
    }

    radio.set_spreading_factor(12).context("set SF12")?;
    let phy12 = phy_params(&radio);
    radio
        .set_preamble_length(phy12.preamble_symbols)
        .context("set SF12 preamble")?;
    println!(
        "CASE sf12_ldro bw_hz={} sf={} ldro={} preamble_symbols={}",
        radio.bandwidth(),
        radio.spreading_factor(),
        radio.low_data_rate(),
        phy12.preamble_symbols
    );
    transmit_one(&mut radio, 50, false)?;

    radio.set_bandwidth(31_250).context("set 31.25 kHz")?;
    radio.set_spreading_factor(9).context("set SF9")?;
    let phy9 = phy_params(&radio);
    radio
        .set_preamble_length(phy9.preamble_symbols)
        .context("set SF9 preamble")?;
    println!(
        "CASE sf9_bw31250 bw_hz={} sf={} ldro={} preamble_symbols={}",
        radio.bandwidth(),
        radio.spreading_factor(),
        radio.low_data_rate(),
        phy9.preamble_symbols
    );
    transmit_one(&mut radio, 50, false)?;

    radio.set_bandwidth(DEFAULT_BW_HZ).context("restore bandwidth")?;
    radio.set_spreading_factor(DEFAULT_SF).context("restore SF7")?;
    radio
        .set_preamble_length(phy.preamble_symbols)
        .context("restore preamble")?;
    radio.receive(None).context("enter receive")?;
    sleep(Duration::from_millis(100));
    let rx = radio.poll_rx().context("poll RX idle")?;
    println!(
        "RX_IDLE poll_rx_none={} dio1_high={}",
        rx.is_none(),
        dio1.is_high_level()?
    );
    radio.end().context("end radio")?;
    Ok(())
}

fn run_tx(mut radio: RadioDev, args: &Args) -> Result<()> {
    if args.payload_len > 255 {
        bail!("SX126x raw transmit payload cannot exceed 255 bytes in this tool")
    }
    if args.start_delay_ms > 0 {
        sleep(Duration::from_millis(args.start_delay_ms));
    }
    for idx in 0..args.count {
        println!("TX_INDEX {}", idx + 1);
        transmit_one(&mut radio, args.payload_len, args.implicit)?;
        if idx + 1 != args.count {
            sleep(Duration::from_millis(args.interval_ms));
        }
    }
    radio.end().context("end radio")?;
    Ok(())
}

fn run_rx_idle(mut radio: RadioDev, dio1: GpioInput, args: &Args) -> Result<()> {
    radio.receive(None).context("enter receive")?;
    let start = Instant::now();
    for idx in 0..args.count {
        let rx = radio.poll_rx().context("poll RX")?;
        let dcd = radio.dcd(start.elapsed().as_millis() as u64).context("DCD")?;
        println!(
            "RX_IDLE sample={} poll_rx_none={} dcd={} dio1_high={}",
            idx + 1,
            rx.is_none(),
            dcd,
            dio1.is_high_level()?
        );
        sleep(Duration::from_millis(args.interval_ms));
    }
    radio.end().context("end radio")?;
    Ok(())
}

fn transmit_one(radio: &mut RadioDev, len: usize, implicit: bool) -> Result<()> {
    let payload: Vec<u8> = (0..len)
        .map(|i| (i as u8).wrapping_mul(37).wrapping_add(0x42))
        .collect();
    let phy = phy_params(radio);
    let expected_ms = semtech_time_on_air_ms(ToaParams {
        payload_len: len,
        sf: radio.spreading_factor(),
        bw_hz: radio.bandwidth(),
        cr_denominator: radio.coding_rate(),
        ldro: radio.low_data_rate(),
        preamble_symbols: phy.preamble_symbols,
        implicit_header: implicit,
        crc: true,
    });
    let before = Instant::now();
    let outcome = radio.transmit(&payload, implicit).context("transmit")?;
    let elapsed_ms = before.elapsed().as_secs_f64() * 1000.0;
    println!(
        "TX len={} outcome={:?} elapsed_ms={:.3} semtech_toa_ms={:.3} delta_ms={:.3} sf={} bw_hz={} cr=4/{} ldro={} implicit={}",
        len,
        outcome,
        elapsed_ms,
        expected_ms,
        elapsed_ms - expected_ms,
        radio.spreading_factor(),
        radio.bandwidth(),
        radio.coding_rate(),
        radio.low_data_rate(),
        implicit
    );
    if outcome != TxOutcome::Done {
        bail!("TX did not complete: {outcome:?}");
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct ToaParams {
    payload_len: usize,
    sf: u8,
    bw_hz: u32,
    cr_denominator: u8,
    ldro: bool,
    preamble_symbols: u16,
    implicit_header: bool,
    crc: bool,
}

/// Time on air in ms by the SX126x formula, as Semtech's LoRaMac-node computes it
/// (`RadioGetLoRaTimeOnAirNumerator()` in `src/radio/sx126x/radio.c`). Below SF7 a frame has 2 more
/// symbols of overhead and no 8 extra bits in its block count, and LDRO does not apply.
fn semtech_time_on_air_ms(params: ToaParams) -> f64 {
    let sf = f64::from(params.sf);
    let t_sym_ms = (2_f64.powf(sf) / f64::from(params.bw_hz)) * 1000.0;
    let de = if params.ldro { 1.0 } else { 0.0 };
    let ih = if params.implicit_header { 1.0 } else { 0.0 };
    let crc = if params.crc { 1.0 } else { 0.0 };
    let cr = f64::from(params.cr_denominator.saturating_sub(4));
    let bits = 8.0 * params.payload_len as f64 - 4.0 * sf + 16.0 * crc - 20.0 * ih;
    let (numerator, denominator, overhead) = if params.sf < 7 {
        (bits + 20.0, 4.0 * sf, 6.25)
    } else {
        (bits + 28.0, 4.0 * (sf - 2.0 * de), 4.25)
    };
    let payload_symbols = 8.0 + (numerator / denominator).ceil().max(0.0) * (cr + 4.0);
    (f64::from(params.preamble_symbols) + overhead + payload_symbols) * t_sym_ms
}

#[derive(Debug)]
struct StdDelay;

impl DelayNs for StdDelay {
    fn delay_ns(&mut self, ns: u32) {
        sleep(Duration::from_nanos(u64::from(ns)));
    }
}

#[derive(Debug)]
struct LinuxSpiDevice {
    dev: Spidev,
}

impl LinuxSpiDevice {
    fn open(path: &str, speed_hz: u32) -> io::Result<Self> {
        let mut dev = Spidev::open(path)?;
        let options = SpidevOptions::new()
            .bits_per_word(8)
            .max_speed_hz(speed_hz)
            .mode(SpiModeFlags::SPI_MODE_0)
            .build();
        dev.configure(&options)?;
        Ok(Self { dev })
    }
}

#[derive(Debug)]
struct SpiIoError(io::Error);

impl fmt::Display for SpiIoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for SpiIoError {}

impl embedded_hal::spi::Error for SpiIoError {
    fn kind(&self) -> SpiErrorKind {
        SpiErrorKind::Other
    }
}

impl SpiErrorType for LinuxSpiDevice {
    type Error = SpiIoError;
}

impl SpiDevice for LinuxSpiDevice {
    fn transaction(&mut self, operations: &mut [Operation<'_, u8>]) -> std::result::Result<(), Self::Error> {
        let mut transfers = Vec::with_capacity(operations.len());
        for op in operations {
            match op {
                Operation::Read(buf) => transfers.push(SpidevTransfer::read(buf)),
                Operation::Write(buf) => transfers.push(SpidevTransfer::write(buf)),
                Operation::Transfer(read, write) => transfers.push(SpidevTransfer::read_write(write, read)),
                Operation::TransferInPlace(buf) => transfers.push(SpidevTransfer::read_write_in_place(buf)),
                Operation::DelayNs(ns) => {
                    if !transfers.is_empty() {
                        self.dev.transfer_multiple(&mut transfers).map_err(SpiIoError)?;
                        transfers.clear();
                    }
                    sleep(Duration::from_nanos(u64::from(*ns)));
                }
            }
        }
        if !transfers.is_empty() {
            self.dev.transfer_multiple(&mut transfers).map_err(SpiIoError)?;
        }
        Ok(())
    }
}

#[derive(Debug)]
struct GpioInput {
    line: u32,
    req: gpiocdev::Request,
}

impl GpioInput {
    fn open(chip: &str, line: u32) -> Result<Self> {
        let req = gpiocdev::Request::builder()
            .on_chip(chip)
            .with_line(line)
            .as_input()
            .with_consumer("rnode-hwtest")
            .request()?;
        Ok(Self { line, req })
    }

    fn is_high_level(&self) -> Result<bool> {
        Ok(self.req.value(self.line)? == Value::Active)
    }
}

#[derive(Debug)]
struct GpioOutput {
    line: u32,
    req: gpiocdev::Request,
}

impl GpioOutput {
    fn open(chip: &str, line: u32, high: bool) -> Result<Self> {
        let req = gpiocdev::Request::builder()
            .on_chip(chip)
            .with_line(line)
            .as_output(if high { Value::Active } else { Value::Inactive })
            .with_consumer("rnode-hwtest")
            .request()?;
        Ok(Self { line, req })
    }

    fn set_level(&self, high: bool) -> Result<()> {
        let mut values = Values::default();
        values.set(self.line, if high { Value::Active } else { Value::Inactive });
        self.req.set_values(&values)?;
        Ok(())
    }
}

#[derive(Debug)]
struct PinIoError;

impl embedded_hal::digital::Error for PinIoError {
    fn kind(&self) -> PinErrorKind {
        PinErrorKind::Other
    }
}

impl PinErrorType for GpioInput {
    type Error = PinIoError;
}

impl InputPin for GpioInput {
    fn is_high(&mut self) -> std::result::Result<bool, Self::Error> {
        self.is_high_level().map_err(|_err| PinIoError)
    }

    fn is_low(&mut self) -> std::result::Result<bool, Self::Error> {
        self.is_high().map(|high| !high)
    }
}

impl PinErrorType for GpioOutput {
    type Error = PinIoError;
}

impl OutputPin for GpioOutput {
    fn set_low(&mut self) -> std::result::Result<(), Self::Error> {
        self.set_level(false).map_err(|_err| PinIoError)
    }

    fn set_high(&mut self) -> std::result::Result<(), Self::Error> {
        self.set_level(true).map_err(|_err| PinIoError)
    }
}

#[cfg(test)]
mod tests;
