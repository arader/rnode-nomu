//! Firmware-exact SX1262 driver over `embedded-hal` 1.0.
//!
//! The command order, register values and packet handling mirror RNode_Firmware v1.86
//! `sx126x.cpp` so the Linux daemon interoperates with real RNodes on the air.
//!
//! One deliberate improvement over firmware is TX-done timeout handling: firmware uses the fixed
//! `LORA_MODEM_TIMEOUT_MS = 20E3` from `sx126x.h`, which can abort valid maximum-size frames at
//! SF12 and narrow bandwidths. This driver treats [`Config::tx_timeout_ms`] as a floor and scales
//! the effective wait with the current packet airtime. The wait loop counts 1 ms delay iterations,
//! so real wall time is somewhat longer than the nominal limit.
//!
//! On a board with an LNA in front of the modem, [`Config::lna_gain_db`] is subtracted from every
//! RSSI the driver reads, as firmware `currentRssi()` and `packetRssi()` subtract `lora_lna_gain`
//! with `HAS_LORA_LNA`. That includes the packet RSSI of received frames, where the firmware's
//! ESP32 builds, the Heltec V4 with its LNA among them, call `packetRssi(uint8_t)`, which leaves
//! the gain in. This driver deliberately compensates both, so that the packet RSSI the host gets
//! and the channel RSSI and noise floor are measured at the same point, the antenna.

use core::convert::Infallible;
use core::fmt;
use core::str::FromStr;

use embedded_hal::delay::DelayNs;
use embedded_hal::digital::{
    Error as DigitalError, ErrorKind as DigitalErrorKind, ErrorType as DigitalErrorType, InputPin, OutputPin,
};
use embedded_hal::spi::{Operation, SpiDevice};
use rnode_core::phy;
use rnode_core::radio::{Radio, RxCrcErrors, RxFrame, RxSignal, TxOutcome};

const OP_RF_FREQ: u8 = 0x86;
const OP_SLEEP: u8 = 0x84;
const OP_STANDBY: u8 = 0x80;
const OP_TX: u8 = 0x83;
const OP_RX: u8 = 0x82;
const OP_PA_CONFIG: u8 = 0x95;
const OP_SET_IRQ_FLAGS: u8 = 0x08;
const OP_CLEAR_IRQ_STATUS: u8 = 0x02;
const OP_GET_IRQ_STATUS: u8 = 0x12;
const OP_RX_BUFFER_STATUS: u8 = 0x13;
const OP_PACKET_STATUS: u8 = 0x14;
const OP_CURRENT_RSSI: u8 = 0x15;
const OP_MODULATION_PARAMS: u8 = 0x8B;
const OP_PACKET_PARAMS: u8 = 0x8C;
const OP_TX_PARAMS: u8 = 0x8E;
const OP_PACKET_TYPE: u8 = 0x8A;
const OP_BUFFER_BASE_ADDR: u8 = 0x8F;
const OP_READ_REGISTER: u8 = 0x1D;
const OP_WRITE_REGISTER: u8 = 0x0D;
const OP_DIO3_TCXO_CTRL: u8 = 0x97;
const OP_DIO2_RF_CTRL: u8 = 0x9D;
const OP_CALIBRATE: u8 = 0x89;
const OP_CALIBRATE_IMAGE: u8 = 0x98;
const OP_FIFO_WRITE: u8 = 0x0E;
const OP_FIFO_READ: u8 = 0x1E;

const IRQ_TX_DONE_MASK: u8 = 0x01;
const IRQ_RX_DONE_MASK: u8 = 0x02;
const IRQ_PREAMBLE_DET_MASK: u8 = 0x04;
const IRQ_HEADER_DET_MASK: u8 = 0x10;
const IRQ_PAYLOAD_CRC_ERROR_MASK: u8 = 0x40;

const REG_OCP: u16 = 0x08E7;
const REG_LNA: u16 = 0x08AC;
const REG_SYNC_WORD_MSB: u16 = 0x0740;
const REG_SYNC_WORD_LSB: u16 = 0x0741;
const REG_TX_CLAMP_CONFIG: u16 = 0x08D8;
const REG_IQ_POLARITY: u16 = 0x0736;
const REG_SENSITIVITY: u16 = 0x0889;

const MASK_CALIBRATE_ALL: u8 = 0x7f;
const MODE_LONG_RANGE_MODE: u8 = 0x01;
const MODE_STDBY_RC: u8 = 0x00;
const MODE_STDBY_XOSC: u8 = 0x01;
const MODE_IMPLICIT_HEADER: u8 = 0x01;
const MODE_EXPLICIT_HEADER: u8 = 0x00;
const MAX_PKT_LENGTH: usize = 255;
const DEFAULT_PREAMBLE_LENGTH: u16 = 18;
const DEFAULT_PAYLOAD_LENGTH: u8 = 255;
const DEFAULT_BW_CODE: u8 = 0x04;
const DEFAULT_BW_HZ: u32 = 125_000;

/// Board-specific SX1262 configuration used by firmware `begin()` helpers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// No TCXO, or the DIO3 voltage used by firmware `enableTCXO()`.
    pub tcxo: Option<TcxoVoltage>,
    /// Whether firmware `begin()` should execute `OP_DIO2_RF_CTRL` with `0x01`.
    pub dio2_as_rf_switch: bool,
    /// OCP register value written by firmware `setTxPower()` (`OCP_TUNED`).
    pub ocp: u8,
    /// Maximum number of one-millisecond polls in firmware `waitOnBusy()`.
    pub busy_timeout_ms: u32,
    /// Floor for the blocking TX-done timeout, in milliseconds.
    ///
    /// Firmware uses fixed `LORA_MODEM_TIMEOUT_MS = 20E3` (`sx126x.h:17`), which can abort and
    /// hard-reset on valid SF12 ≤ 62.5 kHz max-size frames. This driver deliberately improves on
    /// that by waiting for `max(tx_timeout_ms, 2 × airtime + 1000)`; the loop counts 1 ms delay
    /// iterations, so wall-clock time is somewhat longer than the nominal value.
    pub tx_timeout_ms: u32,
    /// Gain in dB of an LNA between the antenna and the modem, subtracted from every RSSI the
    /// driver reads: [`Radio::current_rssi`], the packet RSSI of [`Radio::poll_rx`] and that of
    /// frames dropped for a CRC error (firmware `lora_lna_gain`: `LORA_LNA_GAIN`, 17 dB, or
    /// `LORA_LNA_KCT8103L_GAIN`, 21 dB, on the Heltec V4). 0 for boards without one.
    pub lna_gain_db: u8,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            tcxo: None,
            dio2_as_rf_switch: true,
            ocp: 0x28,
            busy_timeout_ms: 100,
            tx_timeout_ms: 20_000,
            lna_gain_db: 0,
        }
    }
}

/// TCXO voltage choices for `OP_DIO3_TCXO_CTRL`.
///
/// RNode_Firmware's board constants collapse 2.4 V and 2.7 V onto the 3.0 V register value,
/// but the firmware never selects those voltages. This driver deliberately uses the SX1262
/// datasheet encoding for all exposed choices so a Linux configuration cannot over-volt a TCXO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcxoVoltage {
    V1_6,
    V1_7,
    V1_8,
    V2_2,
    V2_4,
    V2_7,
    V3_0,
    V3_3,
}

impl TcxoVoltage {
    /// Value written as byte 0 by `enableTCXO()` / `OP_DIO3_TCXO_CTRL`.
    pub const fn register_value(self) -> u8 {
        match self {
            Self::V1_6 => 0x00,
            Self::V1_7 => 0x01,
            Self::V1_8 => 0x02,
            Self::V2_2 => 0x03,
            Self::V2_4 => 0x04,
            Self::V2_7 => 0x05,
            Self::V3_0 => 0x06,
            Self::V3_3 => 0x07,
        }
    }
}

impl FromStr for TcxoVoltage {
    type Err = ParseTcxoVoltageError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim().trim_end_matches(['v', 'V']);
        match s {
            "1.6" => Ok(Self::V1_6),
            "1.7" => Ok(Self::V1_7),
            "1.8" => Ok(Self::V1_8),
            "2.2" => Ok(Self::V2_2),
            "2.4" => Ok(Self::V2_4),
            "2.7" => Ok(Self::V2_7),
            "3.0" | "3" => Ok(Self::V3_0),
            "3.3" => Ok(Self::V3_3),
            _ => Err(ParseTcxoVoltageError),
        }
    }
}

/// Error returned when parsing a TCXO voltage string fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseTcxoVoltageError;

impl fmt::Display for ParseTcxoVoltageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected an SX126x TCXO voltage such as 1.8V")
    }
}

impl std::error::Error for ParseTcxoVoltageError {}

/// An output pin that ignores all writes, for boards without an RXEN line.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoPin;

impl DigitalErrorType for NoPin {
    type Error = Infallible;
}

impl OutputPin for NoPin {
    fn set_low(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
    fn set_high(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// SX1262 driver errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error<Spi> {
    Spi(Spi),
    Digital(DigitalErrorKind),
    BusyTimeout,
    NotDetected,
}

impl<Spi: fmt::Debug + fmt::Display> fmt::Display for Error<Spi> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spi(err) => write!(f, "SPI error: {err}"),
            Self::Digital(kind) => write!(f, "GPIO error: {kind:?}"),
            Self::BusyTimeout => f.write_str("SX126x BUSY timeout"),
            Self::NotDetected => f.write_str("SX126x not detected"),
        }
    }
}

impl<Spi: fmt::Debug + fmt::Display> std::error::Error for Error<Spi> {}

/// Portable SX1262 driver that mirrors RNode_Firmware v1.86 `sx126x.cpp`.
#[derive(Debug)]
pub struct Sx126x<SPI, BUSY, RESET, RXEN, DELAY> {
    spi: SPI,
    busy: BUSY,
    reset: RESET,
    rxen: Option<RXEN>,
    delay: DELAY,
    config: Config,
    frequency: u32,
    tx_power: i8,
    sf: u8,
    bw_code: u8,
    bw_hz: u32,
    cr: u8,
    ldro: bool,
    preamble_length: u16,
    implicit_header_mode: u8,
    payload_length: u8,
    crc_mode: u8,
    fifo_tx_addr_ptr: u8,
    fifo_rx_addr_ptr: u8,
    preinit_done: bool,
    preamble_detected_at: Option<u64>,
    dcd_preamble_ms: u32,
    dcd_header_ms: u32,
    current_rx_implicit_len: Option<u8>,
    crc_errors: RxCrcErrors,
}

impl<SPI, BUSY, RESET, RXEN, DELAY> Sx126x<SPI, BUSY, RESET, RXEN, DELAY>
where
    SPI: SpiDevice,
    BUSY: InputPin,
    RESET: OutputPin,
    RXEN: OutputPin,
    DELAY: DelayNs,
{
    pub fn new(spi: SPI, busy: BUSY, reset: RESET, rxen: Option<RXEN>, delay: DELAY, config: Config) -> Self {
        Self {
            spi,
            busy,
            reset,
            rxen,
            delay,
            config,
            frequency: 0,
            tx_power: 0,
            sf: 0x07,
            bw_code: DEFAULT_BW_CODE,
            bw_hz: DEFAULT_BW_HZ,
            cr: 0x01,
            ldro: phy::low_data_rate(0x07, DEFAULT_BW_HZ),
            preamble_length: DEFAULT_PREAMBLE_LENGTH,
            implicit_header_mode: MODE_EXPLICIT_HEADER,
            payload_length: DEFAULT_PAYLOAD_LENGTH,
            crc_mode: 1,
            fifo_tx_addr_ptr: 0,
            fifo_rx_addr_ptr: 0,
            preinit_done: false,
            preamble_detected_at: None,
            dcd_preamble_ms: 0,
            dcd_header_ms: 0,
            current_rx_implicit_len: None,
            crc_errors: RxCrcErrors::default(),
        }
    }

    fn map_pin_error<E: DigitalError>(err: E) -> Error<SPI::Error> {
        Error::Digital(err.kind())
    }

    /// Firmware `sx126x::waitOnBusy()`, except this driver reports `BusyTimeout` instead of
    /// proceeding after the configured timeout so callers can handle a stuck BUSY line explicitly.
    fn wait_on_busy(&mut self) -> Result<(), Error<SPI::Error>> {
        for _ in 0..self.config.busy_timeout_ms {
            if !self.busy.is_high().map_err(Self::map_pin_error)? {
                return Ok(());
            }
            self.delay.delay_ms(1);
        }
        if self.busy.is_high().map_err(Self::map_pin_error)? {
            Err(Error::BusyTimeout)
        } else {
            Ok(())
        }
    }

    fn transaction(&mut self, operations: &mut [Operation<'_, u8>]) -> Result<(), Error<SPI::Error>> {
        self.wait_on_busy()?;
        self.spi.transaction(operations).map_err(Error::Spi)
    }

    fn execute_opcode(&mut self, opcode: u8, params: &[u8]) -> Result<(), Error<SPI::Error>> {
        assert!(
            params.len() < 10,
            "SX126x opcode parameter buffer is limited to 9 bytes"
        );
        let mut bytes = [0u8; 10];
        bytes[0] = opcode;
        bytes[1..=params.len()].copy_from_slice(params);
        self.transaction(&mut [Operation::Write(&bytes[..params.len() + 1])])
    }

    fn execute_opcode_read(&mut self, opcode: u8, out: &mut [u8]) -> Result<(), Error<SPI::Error>> {
        let prefix = [opcode, 0x00];
        self.transaction(&mut [Operation::Write(&prefix), Operation::Read(out)])
    }

    fn read_register(&mut self, address: u16) -> Result<u8, Error<SPI::Error>> {
        let prefix = [OP_READ_REGISTER, (address >> 8) as u8, address as u8, 0x00];
        let mut out = [0u8];
        self.transaction(&mut [Operation::Write(&prefix), Operation::Read(&mut out)])?;
        Ok(out[0])
    }

    fn write_register(&mut self, address: u16, value: u8) -> Result<(), Error<SPI::Error>> {
        self.transaction(&mut [Operation::Write(&[
            OP_WRITE_REGISTER,
            (address >> 8) as u8,
            address as u8,
            value,
        ])])
    }

    fn read_buffer(&mut self, offset: u8, out: &mut [u8]) -> Result<(), Error<SPI::Error>> {
        let prefix = [OP_FIFO_READ, offset, 0x00];
        self.transaction(&mut [Operation::Write(&prefix), Operation::Read(out)])
    }

    fn write_buffer(&mut self, offset: u8, data: &[u8]) -> Result<(), Error<SPI::Error>> {
        let mut bytes = [0u8; MAX_PKT_LENGTH + 2];
        bytes[0] = OP_FIFO_WRITE;
        bytes[1] = offset;
        bytes[2..data.len() + 2].copy_from_slice(data);
        self.transaction(&mut [Operation::Write(&bytes[..data.len() + 2])])
    }

    fn reset_pulse(&mut self) -> Result<(), Error<SPI::Error>> {
        self.reset.set_low().map_err(Self::map_pin_error)?;
        self.delay.delay_ms(10);
        self.reset.set_high().map_err(Self::map_pin_error)?;
        self.delay.delay_ms(10);
        Ok(())
    }

    /// Firmware `sx126x::preInit()`: probe sync-word registers for up to two seconds.
    fn pre_init(&mut self) -> Result<(), Error<SPI::Error>> {
        for _ in 0..20 {
            let msb = self.read_register(REG_SYNC_WORD_MSB)?;
            let lsb = self.read_register(REG_SYNC_WORD_LSB)?;
            let sync = u16::from(msb) << 8 | u16::from(lsb);
            if sync == 0x1424 || sync == 0x4434 {
                self.preinit_done = true;
                return Ok(());
            }
            self.delay.delay_ms(100);
        }
        Err(Error::NotDetected)
    }

    /// Firmware `sx126x::calibrate()`.
    fn calibrate(&mut self) -> Result<(), Error<SPI::Error>> {
        self.execute_opcode(OP_STANDBY, &[MODE_STDBY_RC])?;
        self.execute_opcode(OP_CALIBRATE, &[MASK_CALIBRATE_ALL])?;
        self.delay.delay_ms(5);
        self.wait_on_busy()
    }

    /// Firmware `sx126x::calibrate_image()`.
    fn calibrate_image(&mut self, frequency: u32) -> Result<(), Error<SPI::Error>> {
        let image = match frequency {
            430_000_000..=440_000_000 => [0x6b, 0x6f],
            470_000_000..=510_000_000 => [0x75, 0x81],
            779_000_000..=787_000_000 => [0xc1, 0xc5],
            863_000_000..=870_000_000 => [0xd7, 0xdb],
            902_000_000..=928_000_000 => [0xe1, 0xe9],
            _ => [0x00, 0x00],
        };
        self.execute_opcode(OP_CALIBRATE_IMAGE, &image)?;
        self.wait_on_busy()
    }

    fn standby(&mut self) -> Result<(), Error<SPI::Error>> {
        self.execute_opcode(OP_STANDBY, &[MODE_STDBY_XOSC])
    }

    /// Firmware `sx126x::setPacketParams()`, including SX1262 errata 15.4 register 0x0736.
    fn set_packet_params(&mut self) -> Result<(), Error<SPI::Error>> {
        let params = [
            (self.preamble_length >> 8) as u8,
            self.preamble_length as u8,
            self.implicit_header_mode,
            self.payload_length,
            self.crc_mode,
            0x00,
            0x00,
            0x00,
            0x00,
        ];
        self.execute_opcode(OP_PACKET_PARAMS, &params)?;
        let iq = self.read_register(REG_IQ_POLARITY)?;
        self.write_register(REG_IQ_POLARITY, iq | 0x04)
    }

    /// Firmware `sx126x::setModulationParams()`.
    fn set_modulation_params(&mut self) -> Result<(), Error<SPI::Error>> {
        self.execute_opcode(
            OP_MODULATION_PARAMS,
            &[self.sf, self.bw_code, self.cr, u8::from(self.ldro), 0, 0, 0, 0],
        )
    }

    fn handle_low_data_rate(&mut self) {
        self.ldro = phy::low_data_rate(self.sf, self.bw_hz);
    }

    fn tx_timeout_limit_ms(&self, len: usize) -> u32 {
        if self.bw_hz == 0 {
            return self.config.tx_timeout_ms;
        }
        let modulation = phy::Modulation {
            bandwidth_hz: self.bw_hz,
            spreading_factor: self.sf,
            coding_rate: self.cr + 4,
        };
        let symbol_time_ms = phy::PhyParams::new(modulation).symbol_time_ms;
        let airtime_ms = phy::airtime_ms(len, modulation, self.ldro, self.preamble_length, symbol_time_ms);
        if !airtime_ms.is_finite() || airtime_ms < 0.0 {
            return self.config.tx_timeout_ms;
        }
        let scaled = f64::from(airtime_ms) * 2.0 + 1000.0;
        let scaled = if scaled >= f64::from(u32::MAX) {
            u32::MAX
        } else {
            scaled.ceil() as u32
        };
        self.config.tx_timeout_ms.max(scaled)
    }

    /// Firmware `sx126x::optimizeModemSensitivity()`, implementing SX1262 errata 15.1.
    fn optimize_modem_sensitivity(&mut self) -> Result<(), Error<SPI::Error>> {
        let reg = self.read_register(REG_SENSITIVITY)?;
        let value = if self.bw_hz == 500_000 { reg & !0x04 } else { reg | 0x04 };
        self.write_register(REG_SENSITIVITY, value)
    }

    fn set_header_mode(&mut self, implicit_len: Option<u8>) -> Result<(), Error<SPI::Error>> {
        match implicit_len {
            Some(len) => {
                self.implicit_header_mode = MODE_IMPLICIT_HEADER;
                self.payload_length = len;
            }
            None => self.implicit_header_mode = MODE_EXPLICIT_HEADER,
        }
        self.set_packet_params()
    }

    fn rx_ant_enable(&mut self) -> Result<(), Error<SPI::Error>> {
        if let Some(rxen) = &mut self.rxen {
            rxen.set_high().map_err(Self::map_pin_error)?;
        }
        Ok(())
    }

    fn clear_irq_status(&mut self, flags: [u8; 2]) -> Result<(), Error<SPI::Error>> {
        self.execute_opcode(OP_CLEAR_IRQ_STATUS, &flags)
    }

    fn get_irq_status(&mut self) -> Result<[u8; 2], Error<SPI::Error>> {
        let mut out = [0u8; 2];
        self.execute_opcode_read(OP_GET_IRQ_STATUS, &mut out)?;
        Ok(out)
    }

    /// Firmware `sx126x::packetRssi()` and `packetSnrRaw()`, from one `GetPacketStatus` read:
    /// RssiPkt / 2, less [`Config::lna_gain_db`] (see the crate docs).
    fn packet_signal(&mut self) -> Result<RxSignal, Error<SPI::Error>> {
        let mut status = [0u8; 3];
        self.execute_opcode_read(OP_PACKET_STATUS, &mut status)?;
        Ok(RxSignal {
            rssi_dbm: -(status[0] as i16) / 2 - i16::from(self.config.lna_gain_db),
            snr_raw: status[1],
        })
    }
}

impl<SPI, BUSY, RESET, RXEN, DELAY> Radio for Sx126x<SPI, BUSY, RESET, RXEN, DELAY>
where
    SPI: SpiDevice,
    SPI::Error: fmt::Debug + fmt::Display,
    BUSY: InputPin,
    RESET: OutputPin,
    RXEN: OutputPin,
    DELAY: DelayNs,
{
    type Error = Error<SPI::Error>;

    /// Firmware `sx126x::begin()`.
    fn begin(&mut self, frequency_hz: u32) -> Result<(), Self::Error> {
        self.reset_pulse()?;
        if !self.preinit_done {
            self.pre_init()?;
        }
        self.calibrate()?;
        self.calibrate_image(frequency_hz)?;
        if let Some(voltage) = self.config.tcxo {
            self.execute_opcode(OP_DIO3_TCXO_CTRL, &[voltage.register_value(), 0x00, 0x00, 0xff])?;
        }
        self.execute_opcode(OP_PACKET_TYPE, &[MODE_LONG_RANGE_MODE])?;
        self.standby()?;
        self.write_register(REG_SYNC_WORD_MSB, 0x14)?;
        self.write_register(REG_SYNC_WORD_LSB, 0x24)?;
        if self.config.dio2_as_rf_switch {
            self.execute_opcode(OP_DIO2_RF_CTRL, &[0x01])?;
        }
        self.rx_ant_enable()?;
        self.set_frequency(frequency_hz)?;
        self.set_tx_power(2)?;
        self.set_crc(true)?;
        self.write_register(REG_LNA, 0x96)?;
        self.execute_opcode(OP_BUFFER_BASE_ADDR, &[0x00, 0x00])?;
        self.set_modulation_params()?;
        self.optimize_modem_sensitivity()?;
        self.set_packet_params()?;
        self.execute_opcode(
            OP_SET_IRQ_FLAGS,
            &[0xff, 0xff, 0x00, IRQ_RX_DONE_MASK, 0x00, 0x00, 0x00, 0x00],
        )
    }

    /// Firmware `sx126x::end()`.
    fn end(&mut self) -> Result<(), Self::Error> {
        self.execute_opcode(OP_SLEEP, &[0x00])?;
        self.preinit_done = false;
        Ok(())
    }

    /// Firmware `sx126x::setFrequency()`.
    fn set_frequency(&mut self, hz: u32) -> Result<(), Self::Error> {
        self.frequency = hz;
        let frf = (hz as f64 / (32_000_000.0f64 / 33_554_432.0f64)) as u32;
        self.execute_opcode(OP_RF_FREQ, &frf.to_be_bytes())
    }

    fn frequency(&self) -> u32 {
        self.frequency
    }

    /// Firmware `sx126x::setSignalBandwidth()`.
    fn set_bandwidth(&mut self, hz: u32) -> Result<(), Self::Error> {
        let (code, quantized) = if hz <= 7_800 {
            (0x00, 7_800)
        } else if hz <= 10_400 {
            (0x08, 10_400)
        } else if hz <= 15_600 {
            (0x01, 15_600)
        } else if hz <= 20_800 {
            (0x09, 20_800)
        } else if hz <= 31_250 {
            (0x02, 31_250)
        } else if hz <= 41_700 {
            (0x0a, 41_700)
        } else if hz <= 62_500 {
            (0x03, 62_500)
        } else if hz <= 125_000 {
            (0x04, 125_000)
        } else if hz <= 250_000 {
            (0x05, 250_000)
        } else {
            (0x06, 500_000)
        };
        self.bw_code = code;
        self.bw_hz = quantized;
        self.handle_low_data_rate();
        self.set_modulation_params()?;
        self.optimize_modem_sensitivity()
    }

    fn bandwidth(&self) -> u32 {
        self.bw_hz
    }

    /// Firmware `sx126x::setSpreadingFactor()`.
    fn set_spreading_factor(&mut self, sf: u8) -> Result<(), Self::Error> {
        self.sf = sf.clamp(5, 12);
        self.handle_low_data_rate();
        self.set_modulation_params()
    }

    fn spreading_factor(&self) -> u8 {
        self.sf
    }

    /// Firmware `sx126x::setCodingRate4()`.
    fn set_coding_rate(&mut self, denominator: u8) -> Result<(), Self::Error> {
        self.cr = denominator.clamp(5, 8) - 4;
        self.set_modulation_params()
    }

    fn coding_rate(&self) -> u8 {
        self.cr + 4
    }

    /// Firmware `sx126x::setTxPower()`.
    fn set_tx_power(&mut self, dbm: i8) -> Result<(), Self::Error> {
        let clamp = self.read_register(REG_TX_CLAMP_CONFIG)?;
        self.write_register(REG_TX_CLAMP_CONFIG, clamp | 0x1e)?;
        self.execute_opcode(OP_PA_CONFIG, &[0x04, 0x07, 0x00, 0x01])?;
        let level = dbm.clamp(-9, 22);
        self.write_register(REG_OCP, self.config.ocp)?;
        self.execute_opcode(OP_TX_PARAMS, &[level as u8, 0x02])?;
        self.tx_power = level;
        Ok(())
    }

    fn tx_power(&self) -> i8 {
        self.tx_power
    }

    /// Firmware `sx126x::setPreambleLength()`.
    fn set_preamble_length(&mut self, symbols: u16) -> Result<(), Self::Error> {
        self.preamble_length = symbols;
        self.set_packet_params()
    }

    /// Firmware `sx126x::enableCrc()` / `disableCrc()`.
    fn set_crc(&mut self, enabled: bool) -> Result<(), Self::Error> {
        self.crc_mode = u8::from(enabled);
        self.set_packet_params()
    }

    fn low_data_rate(&self) -> bool {
        self.ldro
    }

    /// Firmware `sx126x::receive()`.
    fn receive(&mut self, implicit_len: Option<u8>) -> Result<(), Self::Error> {
        self.current_rx_implicit_len = implicit_len;
        self.set_header_mode(implicit_len)?;
        self.rx_ant_enable()?;
        self.execute_opcode(OP_RX, &[0xff, 0xff, 0xff])
    }

    /// Firmware `beginPacket()` / `write()` / `endPacket()`.
    fn transmit(&mut self, frame: &[u8], implicit: bool) -> Result<TxOutcome, Self::Error> {
        self.standby()?;
        self.implicit_header_mode = if implicit {
            MODE_IMPLICIT_HEADER
        } else {
            MODE_EXPLICIT_HEADER
        };
        self.payload_length = 0;
        self.fifo_tx_addr_ptr = 0;
        self.set_packet_params()?;
        let len = frame.len().min(MAX_PKT_LENGTH);
        self.write_buffer(self.fifo_tx_addr_ptr, &frame[..len])?;
        self.fifo_tx_addr_ptr = self.fifo_tx_addr_ptr.wrapping_add(len as u8);
        self.payload_length = len as u8;
        self.set_packet_params()?;
        self.execute_opcode(OP_TX, &[0x00, 0x00, 0x00])?;

        // RNode_Firmware's airtime estimate omits the final ceil and uses +0.25 rather than
        // Semtech's +4.25 symbols; doubling it and adding one second keeps a conservative margin.
        let timeout_ms = self.tx_timeout_limit_ms(len);
        let mut irq = self.get_irq_status()?;
        let mut waited = 0;
        while (irq[1] & IRQ_TX_DONE_MASK) == 0 && waited < timeout_ms {
            self.delay.delay_ms(1);
            waited += 1;
            irq = self.get_irq_status()?;
        }
        self.clear_irq_status([0x00, IRQ_TX_DONE_MASK])?;
        if (irq[1] & IRQ_TX_DONE_MASK) != 0 {
            Ok(TxOutcome::Done)
        } else {
            Ok(TxOutcome::Timeout)
        }
    }

    /// Firmware `sx126x::handleDio0Rise()` plus one FIFO read and packet status capture.
    ///
    /// A frame that failed its payload CRC is dropped, as the firmware does, and counted for
    /// [`Radio::take_rx_crc_errors`]. Unlike the firmware, which reads nothing more for such a
    /// frame, the driver then reads its packet status (`GetPacketStatus`) for the log. A good
    /// frame's transactions are the same either way.
    fn poll_rx(&mut self) -> Result<Option<RxFrame>, Self::Error> {
        let irq = self.get_irq_status()?;
        if (irq[1] & IRQ_RX_DONE_MASK) == 0 {
            return Ok(None);
        }
        self.clear_irq_status(irq)?;
        self.preamble_detected_at = None;
        if (irq[1] & IRQ_PAYLOAD_CRC_ERROR_MASK) != 0 {
            self.crc_errors.count = self.crc_errors.count.saturating_add(1);
            match self.packet_signal() {
                Ok(signal) => self.crc_errors.last = Some(signal),
                Err(e) => {
                    // Unknown, rather than the signal of an earlier frame.
                    self.crc_errors.last = None;
                    return Err(e);
                }
            }
            return Ok(None);
        }
        let mut rx = [0u8; 2];
        self.execute_opcode_read(OP_RX_BUFFER_STATUS, &mut rx)?;
        let len = rx[0] as usize;
        self.fifo_rx_addr_ptr = rx[1];
        let mut data = vec![0u8; len];
        self.read_buffer(self.fifo_rx_addr_ptr, &mut data)?;
        let RxSignal { rssi_dbm, snr_raw } = self.packet_signal()?;
        Ok(Some(RxFrame {
            data,
            rssi_dbm,
            snr_raw,
        }))
    }

    fn take_rx_crc_errors(&mut self) -> RxCrcErrors {
        core::mem::take(&mut self.crc_errors)
    }

    /// Firmware `sx126x::dcd()`, including stale-preamble expiry from `set_dcd_timing()`.
    /// Unlike the firmware's global timestamp, this clears the cached preamble time when the
    /// preamble IRQ is no longer present so a completed short packet cannot stale-expire the next
    /// packet's legitimate preamble. This is local carrier-detect behaviour, not an on-air change.
    fn dcd(&mut self, now_ms: u64) -> Result<bool, Self::Error> {
        let irq = self.get_irq_status()?;
        let header_detected = (irq[1] & IRQ_HEADER_DET_MASK) != 0;
        let mut carrier_detected = header_detected;
        let mut false_preamble = false;
        if (irq[1] & IRQ_PREAMBLE_DET_MASK) != 0 {
            carrier_detected = true;
            let detected_at = *self.preamble_detected_at.get_or_insert(now_ms);
            if now_ms.saturating_sub(detected_at) > u64::from(self.dcd_preamble_ms + self.dcd_header_ms) {
                self.preamble_detected_at = None;
                if !header_detected {
                    false_preamble = true;
                }
                self.clear_irq_status([0x00, IRQ_PREAMBLE_DET_MASK])?;
            }
        } else {
            self.preamble_detected_at = None;
        }
        if false_preamble {
            self.receive(self.current_rx_implicit_len)?;
        }
        Ok(carrier_detected)
    }

    /// Firmware `sx126x::currentRssi()`: RssiInst / 2, less [`Config::lna_gain_db`].
    fn current_rssi(&mut self) -> Result<i16, Self::Error> {
        let mut out = [0u8];
        self.execute_opcode_read(OP_CURRENT_RSSI, &mut out)?;
        Ok(-(out[0] as i16) / 2 - i16::from(self.config.lna_gain_db))
    }

    fn set_dcd_timing(&mut self, preamble_ms: u32, header_ms: u32) {
        self.dcd_preamble_ms = preamble_ms;
        self.dcd_header_ms = header_ms;
    }
}

#[cfg(test)]
mod tests;
