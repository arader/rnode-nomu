//! Firmware-exact SX1262 driver over `embedded-hal` 1.0.
//!
//! The command order, register values and packet handling mirror RNode_Firmware v1.86
//! `sx126x.cpp` so the Linux daemon interoperates with real RNodes on the air.

use core::convert::Infallible;
use core::fmt;
use core::str::FromStr;

use embedded_hal::delay::DelayNs;
use embedded_hal::digital::{
    Error as DigitalError, ErrorKind as DigitalErrorKind, ErrorType as DigitalErrorType, InputPin, OutputPin,
};
use embedded_hal::spi::{Operation, SpiDevice};
use rnode_core::phy;
use rnode_core::radio::{Radio, RxFrame, TxOutcome};

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
    /// Blocking TX timeout, matching firmware `LORA_MODEM_TIMEOUT_MS` by default.
    pub tx_timeout_ms: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            tcxo: None,
            dio2_as_rf_switch: true,
            ocp: 0x28,
            busy_timeout_ms: 100,
            tx_timeout_ms: 20_000,
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

        let mut irq = self.get_irq_status()?;
        let mut waited = 0;
        while (irq[1] & IRQ_TX_DONE_MASK) == 0 && waited < self.config.tx_timeout_ms {
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
    fn poll_rx(&mut self) -> Result<Option<RxFrame>, Self::Error> {
        let irq = self.get_irq_status()?;
        if (irq[1] & IRQ_RX_DONE_MASK) == 0 {
            return Ok(None);
        }
        self.clear_irq_status(irq)?;
        self.preamble_detected_at = None;
        if (irq[1] & IRQ_PAYLOAD_CRC_ERROR_MASK) != 0 {
            return Ok(None);
        }
        let mut rx = [0u8; 2];
        self.execute_opcode_read(OP_RX_BUFFER_STATUS, &mut rx)?;
        let len = rx[0] as usize;
        self.fifo_rx_addr_ptr = rx[1];
        let mut data = vec![0u8; len];
        self.read_buffer(self.fifo_rx_addr_ptr, &mut data)?;
        let mut status = [0u8; 3];
        self.execute_opcode_read(OP_PACKET_STATUS, &mut status)?;
        Ok(Some(RxFrame {
            data,
            rssi_dbm: -(status[0] as i16) / 2,
            snr_raw: status[1],
        }))
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

    /// Firmware `sx126x::currentRssi()`.
    fn current_rssi(&mut self) -> Result<i16, Self::Error> {
        let mut out = [0u8];
        self.execute_opcode_read(OP_CURRENT_RSSI, &mut out)?;
        Ok(-(out[0] as i16) / 2)
    }

    fn set_dcd_timing(&mut self, preamble_ms: u32, header_ms: u32) {
        self.dcd_preamble_ms = preamble_ms;
        self.dcd_header_ms = header_ms;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum OpLog {
        Write(Vec<u8>),
        Read(usize),
    }

    #[derive(Debug, Default)]
    struct MockSpiState {
        log: Vec<Vec<OpLog>>,
        reads: VecDeque<Vec<u8>>,
    }

    #[derive(Debug, Clone, Default)]
    struct MockSpi(Rc<RefCell<MockSpiState>>);

    impl MockSpi {
        fn push(&self, data: &[u8]) {
            self.0.borrow_mut().reads.push_back(data.to_vec());
        }
    }

    impl embedded_hal::spi::ErrorType for MockSpi {
        type Error = Infallible;
    }

    impl SpiDevice for MockSpi {
        fn transaction(&mut self, operations: &mut [Operation<'_, u8>]) -> Result<(), Self::Error> {
            let mut tx = Vec::new();
            for op in operations {
                match op {
                    Operation::Write(bytes) => tx.push(OpLog::Write(bytes.to_vec())),
                    Operation::Read(buf) => {
                        tx.push(OpLog::Read(buf.len()));
                        let data = self
                            .0
                            .borrow_mut()
                            .reads
                            .pop_front()
                            .unwrap_or_else(|| vec![0; buf.len()]);
                        for (dst, src) in buf.iter_mut().zip(data) {
                            *dst = src;
                        }
                    }
                    Operation::Transfer(read, write) => {
                        tx.push(OpLog::Write(write.to_vec()));
                        tx.push(OpLog::Read(read.len()));
                    }
                    Operation::TransferInPlace(bytes) => tx.push(OpLog::Write(bytes.to_vec())),
                    Operation::DelayNs(_) => {}
                }
            }
            self.0.borrow_mut().log.push(tx);
            Ok(())
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct SpiFail;

    impl fmt::Display for SpiFail {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("spi fail")
        }
    }

    impl embedded_hal::spi::Error for SpiFail {
        fn kind(&self) -> embedded_hal::spi::ErrorKind {
            embedded_hal::spi::ErrorKind::Other
        }
    }

    #[derive(Debug, Clone, Default)]
    struct FailingSpi;

    impl embedded_hal::spi::ErrorType for FailingSpi {
        type Error = SpiFail;
    }

    impl SpiDevice for FailingSpi {
        fn transaction(&mut self, _operations: &mut [Operation<'_, u8>]) -> Result<(), Self::Error> {
            Err(SpiFail)
        }
    }

    #[derive(Debug, Clone, Default)]
    struct FailingInput;

    impl DigitalErrorType for FailingInput {
        type Error = embedded_hal::digital::ErrorKind;
    }

    impl InputPin for FailingInput {
        fn is_high(&mut self) -> Result<bool, Self::Error> {
            Err(embedded_hal::digital::ErrorKind::Other)
        }

        fn is_low(&mut self) -> Result<bool, Self::Error> {
            self.is_high().map(|v| !v)
        }
    }

    #[derive(Debug, Clone, Default)]
    struct MockInput {
        values: Rc<RefCell<VecDeque<bool>>>,
    }
    impl MockInput {
        fn low() -> Self {
            Self::default()
        }

        fn busy(values: impl IntoIterator<Item = bool>) -> Self {
            Self {
                values: Rc::new(RefCell::new(values.into_iter().collect())),
            }
        }
    }
    impl DigitalErrorType for MockInput {
        type Error = Infallible;
    }
    impl InputPin for MockInput {
        fn is_high(&mut self) -> Result<bool, Self::Error> {
            Ok(self.values.borrow_mut().pop_front().unwrap_or(false))
        }
        fn is_low(&mut self) -> Result<bool, Self::Error> {
            self.is_high().map(|v| !v)
        }
    }

    #[derive(Debug, Clone, Default)]
    struct MockOutput {
        levels: Rc<RefCell<Vec<bool>>>,
    }
    impl DigitalErrorType for MockOutput {
        type Error = Infallible;
    }
    impl OutputPin for MockOutput {
        fn set_low(&mut self) -> Result<(), Self::Error> {
            self.levels.borrow_mut().push(false);
            Ok(())
        }
        fn set_high(&mut self) -> Result<(), Self::Error> {
            self.levels.borrow_mut().push(true);
            Ok(())
        }
    }

    #[derive(Debug, Clone, Default)]
    struct MockDelay {
        delays: Rc<RefCell<Vec<u32>>>,
    }
    impl DelayNs for MockDelay {
        fn delay_ns(&mut self, ns: u32) {
            self.delays.borrow_mut().push(ns);
        }
    }

    type Dev = Sx126x<MockSpi, MockInput, MockOutput, MockOutput, MockDelay>;

    fn dev(config: Config) -> (Dev, MockSpi, MockOutput, MockOutput) {
        let spi = MockSpi::default();
        let reset = MockOutput::default();
        let rxen = MockOutput::default();
        let radio = Sx126x::new(
            spi.clone(),
            MockInput::low(),
            reset.clone(),
            Some(rxen.clone()),
            MockDelay::default(),
            config,
        );
        (radio, spi, reset, rxen)
    }

    fn writes(spi: &MockSpi) -> Vec<Vec<u8>> {
        spi.0
            .borrow()
            .log
            .iter()
            .flat_map(|tx| tx.iter())
            .filter_map(|op| match op {
                OpLog::Write(w) => Some(w.clone()),
                OpLog::Read(_) => None,
            })
            .collect()
    }

    fn has_seq(spi: &MockSpi, seq: &[u8]) -> bool {
        writes(spi).iter().any(|w| w == seq)
    }

    #[test]
    fn tcxo_from_str_and_default_ocp() {
        let cases = [
            ("1.6", TcxoVoltage::V1_6, 0x00),
            ("1.7V", TcxoVoltage::V1_7, 0x01),
            ("1.8V", TcxoVoltage::V1_8, 0x02),
            ("2.2", TcxoVoltage::V2_2, 0x03),
            ("2.4V", TcxoVoltage::V2_4, 0x04),
            ("2.7", TcxoVoltage::V2_7, 0x05),
            ("3.0V", TcxoVoltage::V3_0, 0x06),
            ("3", TcxoVoltage::V3_0, 0x06),
            ("3.3V", TcxoVoltage::V3_3, 0x07),
        ];
        for (text, voltage, register) in cases {
            assert_eq!(text.parse::<TcxoVoltage>().unwrap(), voltage);
            assert_eq!(voltage.register_value(), register);
        }
        assert!("5.0V".parse::<TcxoVoltage>().is_err());
        assert_eq!(Config::default().ocp, 0x28);
    }

    #[test]
    fn begin_sequence_with_tcxo_dio2_and_rxen() {
        let cfg = Config {
            tcxo: Some(TcxoVoltage::V1_8),
            ..Config::default()
        };
        let (mut radio, spi, reset, rxen) = dev(cfg);
        spi.push(&[0x14]);
        spi.push(&[0x24]);
        spi.push(&[0x00]); // tx clamp
        for _ in 0..3 {
            spi.push(&[0x00]);
        } // crc, final packet params etc iq reads as needed
        radio.begin(868_000_000).unwrap();
        assert_eq!(*reset.levels.borrow(), vec![false, true]);
        assert_eq!(*rxen.levels.borrow(), vec![true]);
        assert!(has_seq(&spi, &[0x80, 0x00]));
        assert!(has_seq(&spi, &[0x89, 0x7f]));
        assert!(has_seq(&spi, &[0x98, 0xd7, 0xdb]));
        assert!(has_seq(&spi, &[0x97, 0x02, 0x00, 0x00, 0xff]));
        assert!(has_seq(&spi, &[0x8a, 0x01]));
        assert!(has_seq(&spi, &[0x0d, 0x07, 0x40, 0x14]));
        assert!(has_seq(&spi, &[0x0d, 0x07, 0x41, 0x24]));
        assert!(has_seq(&spi, &[0x9d, 0x01]));
        assert!(has_seq(&spi, &[0x86, 0x36, 0x40, 0x00, 0x00]));
        assert!(has_seq(&spi, &[0x0d, 0x08, 0xe7, 0x28]));
        assert!(has_seq(&spi, &[0x0d, 0x08, 0xac, 0x96]));
        assert!(has_seq(&spi, &[0x8f, 0x00, 0x00]));
        assert!(has_seq(&spi, &[0x0d, 0x08, 0x89, 0x04]));
        assert!(has_seq(&spi, &[0x08, 0xff, 0xff, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00]));
    }

    #[test]
    fn begin_without_tcxo_dio2_or_rxen() {
        let spi = MockSpi::default();
        let mut radio: Sx126x<_, _, _, NoPin, _> = Sx126x::new(
            spi.clone(),
            MockInput::low(),
            MockOutput::default(),
            None,
            MockDelay::default(),
            Config {
                dio2_as_rf_switch: false,
                ..Config::default()
            },
        );
        spi.push(&[0x44]);
        spi.push(&[0x34]);
        spi.push(&[0x00]);
        for _ in 0..3 {
            spi.push(&[0x00]);
        }
        radio.begin(915_000_000).unwrap();
        assert!(!has_seq(&spi, &[0x9d, 0x01]));
        assert!(!writes(&spi).iter().any(|w| w.first() == Some(&0x97)));
        assert!(has_seq(&spi, &[0x98, 0xe1, 0xe9]));
        assert!(has_seq(&spi, &[0x86, 0x39, 0x30, 0x00, 0x00]));
    }

    #[test]
    fn spreading_factor_clamps_and_writes_modulation_params() {
        let (mut radio, spi, _, _) = dev(Config::default());
        radio.set_spreading_factor(1).unwrap();
        assert_eq!(radio.spreading_factor(), 5);
        assert!(!radio.low_data_rate());
        assert!(has_seq(&spi, &[0x8b, 0x05, 0x04, 0x01, 0x00, 0, 0, 0, 0]));

        radio.set_spreading_factor(99).unwrap();
        assert_eq!(radio.spreading_factor(), 12);
        assert!(radio.low_data_rate());
        assert!(has_seq(&spi, &[0x8b, 0x0c, 0x04, 0x01, 0x01, 0, 0, 0, 0]));
    }

    #[test]
    fn begin_skips_preinit_until_end_marks_reprobe_needed() {
        let (mut radio, spi, _, _) = dev(Config::default());
        spi.push(&[0x14]);
        spi.push(&[0x24]);
        spi.push(&[0x00]);
        for _ in 0..2 {
            spi.push(&[0x00]);
        }
        radio.begin(868_000_000).unwrap();

        spi.push(&[0x00]);
        for _ in 0..2 {
            spi.push(&[0x00]);
        }
        radio.begin(868_000_000).unwrap();

        radio.end().unwrap();
        spi.push(&[0x14]);
        spi.push(&[0x24]);
        spi.push(&[0x00]);
        for _ in 0..2 {
            spi.push(&[0x00]);
        }
        radio.begin(868_000_000).unwrap();

        let sync_msb_reads = writes(&spi)
            .into_iter()
            .filter(|w| w == &[0x1d, 0x07, 0x40, 0x00])
            .count();
        assert_eq!(sync_msb_reads, 2);
    }

    #[test]
    fn bandwidth_quantization_table_matches_firmware() {
        let cases = [
            (7_800, 0x00, 7_800),
            (10_400, 0x08, 10_400),
            (15_600, 0x01, 15_600),
            (20_800, 0x09, 20_800),
            (31_250, 0x02, 31_250),
            (41_700, 0x0a, 41_700),
            (62_500, 0x03, 62_500),
            (125_000, 0x04, 125_000),
            (250_000, 0x05, 250_000),
            (500_000, 0x06, 500_000),
        ];
        for (request, code, quantized) in cases {
            let (mut radio, spi, _, _) = dev(Config::default());
            spi.push(&[0x00]);
            radio.set_bandwidth(request).unwrap();
            assert_eq!(radio.bandwidth(), quantized);
            let ldro = u8::from(phy::low_data_rate(0x07, quantized));
            assert!(has_seq(&spi, &[0x8b, 0x07, code, 0x01, ldro, 0, 0, 0, 0]));
        }
    }

    #[test]
    fn transmit_truncates_to_firmware_max_packet_length() {
        let (mut radio, spi, _, _) = dev(Config {
            tx_timeout_ms: 0,
            ..Config::default()
        });
        for _ in 0..2 {
            spi.push(&[0]);
        }
        spi.push(&[0x00, 0x00]);
        let frame = vec![0xa5; MAX_PKT_LENGTH + 1];
        assert_eq!(radio.transmit(&frame, false).unwrap(), TxOutcome::Timeout);
        let fifo_write = writes(&spi)
            .into_iter()
            .find(|w| w.starts_with(&[OP_FIFO_WRITE, 0x00]))
            .unwrap();
        assert_eq!(fifo_write.len(), MAX_PKT_LENGTH + 2);
        assert!(has_seq(&spi, &[0x8c, 0x00, 0x12, 0x00, 0xff, 0x01, 0, 0, 0, 0]));
    }

    #[test]
    fn dcd_stale_preamble_with_header_does_not_reenter_receive() {
        let (mut radio, spi, _, _) = dev(Config::default());
        radio.set_dcd_timing(10, 5);
        spi.push(&[0x00, IRQ_PREAMBLE_DET_MASK]);
        assert!(radio.dcd(100).unwrap());
        spi.push(&[0x00, IRQ_PREAMBLE_DET_MASK | IRQ_HEADER_DET_MASK]);
        assert!(radio.dcd(116).unwrap());
        assert!(has_seq(&spi, &[0x02, 0x00, IRQ_PREAMBLE_DET_MASK]));
        assert!(!has_seq(&spi, &[0x82, 0xff, 0xff, 0xff]));
    }

    #[test]
    fn set_bandwidth_updates_ldro_and_sensitivity() {
        let (mut radio, spi, _, _) = dev(Config::default());
        radio.set_spreading_factor(12).unwrap();
        spi.push(&[0x00]);
        radio.set_bandwidth(125_000).unwrap();
        assert!(radio.low_data_rate());
        assert!(has_seq(&spi, &[0x8b, 0x0c, 0x04, 0x01, 0x01, 0, 0, 0, 0]));
        assert!(has_seq(&spi, &[0x0d, 0x08, 0x89, 0x04]));
        spi.push(&[0xff]);
        radio.set_bandwidth(500_000).unwrap();
        assert!(!radio.low_data_rate());
        assert!(has_seq(&spi, &[0x8b, 0x0c, 0x06, 0x01, 0x00, 0, 0, 0, 0]));
        assert!(has_seq(&spi, &[0x0d, 0x08, 0x89, 0xfb]));
    }

    #[test]
    fn packet_params_always_apply_iq_errata() {
        let (mut radio, spi, _, _) = dev(Config::default());
        spi.push(&[0x00]);
        radio.set_preamble_length(24).unwrap();
        assert!(has_seq(&spi, &[0x8c, 0x00, 0x18, 0x00, 0xff, 0x01, 0, 0, 0, 0]));
        assert!(has_seq(&spi, &[0x1d, 0x07, 0x36, 0x00]));
        assert!(has_seq(&spi, &[0x0d, 0x07, 0x36, 0x04]));
    }

    #[test]
    fn set_tx_power_clamps_and_uses_configured_ocp() {
        let (mut radio, spi, _, _) = dev(Config {
            ocp: 0x35,
            ..Config::default()
        });
        spi.push(&[0x01]);
        radio.set_tx_power(99).unwrap();
        assert_eq!(radio.tx_power(), 22);
        assert!(has_seq(&spi, &[0x0d, 0x08, 0xd8, 0x1f]));
        assert!(has_seq(&spi, &[0x95, 0x04, 0x07, 0x00, 0x01]));
        assert!(has_seq(&spi, &[0x0d, 0x08, 0xe7, 0x35]));
        assert!(has_seq(&spi, &[0x8e, 22, 0x02]));
        spi.push(&[0x00]);
        radio.set_tx_power(-99).unwrap();
        assert!(has_seq(&spi, &[0x8e, (-9i8) as u8, 0x02]));
    }

    #[test]
    fn transmit_success_and_timeout() {
        let (mut radio, spi, _, _) = dev(Config {
            tx_timeout_ms: 2,
            ..Config::default()
        });
        for _ in 0..2 {
            spi.push(&[0]);
        }
        spi.push(&[0x00, 0x00]);
        spi.push(&[0x00, 0x01]);
        assert_eq!(radio.transmit(&[1, 2, 3], false).unwrap(), TxOutcome::Done);
        assert!(has_seq(&spi, &[0x0e, 0x00, 1, 2, 3]));
        assert!(has_seq(&spi, &[0x83, 0x00, 0x00, 0x00]));
        assert!(has_seq(&spi, &[0x02, 0x00, 0x01]));

        let (mut radio, spi, _, _) = dev(Config {
            tx_timeout_ms: 1,
            ..Config::default()
        });
        for _ in 0..2 {
            spi.push(&[0]);
        }
        spi.push(&[0x00, 0x00]);
        spi.push(&[0x00, 0x00]);
        assert_eq!(radio.transmit(&[9], true).unwrap(), TxOutcome::Timeout);
    }

    #[test]
    fn receive_explicit_and_implicit() {
        let (mut radio, spi, rx_reset, rxen) = dev(Config::default());
        let _ = rx_reset;
        spi.push(&[0]);
        radio.receive(None).unwrap();
        assert!(has_seq(&spi, &[0x8c, 0x00, 0x12, 0x00, 0xff, 0x01, 0, 0, 0, 0]));
        assert!(has_seq(&spi, &[0x82, 0xff, 0xff, 0xff]));
        spi.push(&[0]);
        radio.receive(Some(7)).unwrap();
        assert_eq!(*rxen.levels.borrow(), vec![true, true]);
        assert!(has_seq(&spi, &[0x8c, 0x00, 0x12, 0x01, 0x07, 0x01, 0, 0, 0, 0]));
    }

    #[test]
    fn poll_rx_paths_and_single_fifo_read() {
        let (mut radio, spi, _, _) = dev(Config::default());
        spi.push(&[0x00, 0x00]);
        assert_eq!(radio.poll_rx().unwrap(), None);
        assert!(!has_seq(&spi, &[0x02, 0x00, 0x00]));

        spi.push(&[0x00, IRQ_RX_DONE_MASK | IRQ_PAYLOAD_CRC_ERROR_MASK]);
        assert_eq!(radio.poll_rx().unwrap(), None);
        assert!(has_seq(&spi, &[0x02, 0x00, 0x42]));

        spi.push(&[0x00, IRQ_RX_DONE_MASK]);
        spi.push(&[3, 5]);
        spi.push(&[0xaa, 0xbb, 0xcc]);
        spi.push(&[100, 0xfc, 0]);
        let frame = radio.poll_rx().unwrap().unwrap();
        assert_eq!(frame.data, vec![0xaa, 0xbb, 0xcc]);
        assert_eq!(frame.rssi_dbm, -50);
        assert_eq!(frame.snr_raw, 0xfc);
        let fifo_reads = writes(&spi).into_iter().filter(|w| w == &[0x1e, 5, 0]).count();
        assert_eq!(fifo_reads, 1);
    }

    #[test]
    fn dcd_expires_stale_preamble_and_reenters_receive() {
        let (mut radio, spi, _, _) = dev(Config::default());
        radio.set_dcd_timing(10, 5);
        spi.push(&[0x00, IRQ_PREAMBLE_DET_MASK]);
        assert!(radio.dcd(100).unwrap());
        spi.push(&[0x00, IRQ_PREAMBLE_DET_MASK]);
        spi.push(&[0]);
        assert!(radio.dcd(116).unwrap());
        assert!(has_seq(&spi, &[0x02, 0x00, IRQ_PREAMBLE_DET_MASK]));
        assert!(has_seq(&spi, &[0x82, 0xff, 0xff, 0xff]));
    }

    #[test]
    fn preinit_not_detected_and_end_marks_for_reprobe() {
        let (mut radio, spi, _, _) = dev(Config::default());
        for _ in 0..40 {
            spi.push(&[0]);
        }
        assert_eq!(radio.begin(868_000_000).unwrap_err(), Error::NotDetected);
        let (mut radio, spi, _, _) = dev(Config::default());
        radio.end().unwrap();
        assert!(has_seq(&spi, &[0x84, 0x00]));
        spi.push(&[0x14]);
        spi.push(&[0x24]);
        spi.push(&[0]);
        for _ in 0..3 {
            spi.push(&[0]);
        }
        radio.begin(868_000_000).unwrap();
    }

    #[test]
    fn dcd_no_preamble_poll_clears_cached_detection_time() {
        let (mut radio, spi, _, _) = dev(Config::default());
        radio.set_dcd_timing(10, 5);

        spi.push(&[0x00, IRQ_PREAMBLE_DET_MASK]);
        assert!(radio.dcd(100).unwrap());

        spi.push(&[0x00, 0x00]);
        assert!(!radio.dcd(101).unwrap());

        spi.push(&[0x00, IRQ_PREAMBLE_DET_MASK]);
        assert!(radio.dcd(1_000).unwrap());
        let preamble_clears = writes(&spi)
            .into_iter()
            .filter(|w| w == &[0x02, 0x00, IRQ_PREAMBLE_DET_MASK])
            .count();
        assert_eq!(preamble_clears, 0);
    }

    #[test]
    fn dcd_does_not_reuse_short_packet_preamble_timestamp() {
        let (mut radio, spi, _, _) = dev(Config::default());
        radio.set_dcd_timing(10, 5);

        spi.push(&[0x00, IRQ_PREAMBLE_DET_MASK]);
        assert!(radio.dcd(100).unwrap());

        spi.push(&[0x00, IRQ_RX_DONE_MASK | IRQ_PREAMBLE_DET_MASK]);
        spi.push(&[1, 0]);
        spi.push(&[0xaa]);
        spi.push(&[80, 0, 0]);
        assert_eq!(radio.poll_rx().unwrap().unwrap().data, vec![0xaa]);

        spi.push(&[0x00, IRQ_PREAMBLE_DET_MASK]);
        assert!(radio.dcd(1_000).unwrap());
        let preamble_clears = writes(&spi)
            .into_iter()
            .filter(|w| w == &[0x02, 0x00, IRQ_PREAMBLE_DET_MASK])
            .count();
        assert_eq!(preamble_clears, 0);
    }

    #[test]
    fn coding_rate_clamps_and_writes_modulation_params() {
        let (mut radio, spi, _, _) = dev(Config::default());
        radio.set_coding_rate(7).unwrap();
        assert_eq!(radio.coding_rate(), 7);
        assert!(has_seq(&spi, &[0x8b, 0x07, 0x04, 0x03, 0x00, 0, 0, 0, 0]));

        radio.set_coding_rate(99).unwrap();
        assert_eq!(radio.coding_rate(), 8);
        assert!(has_seq(&spi, &[0x8b, 0x07, 0x04, 0x04, 0x00, 0, 0, 0, 0]));

        radio.set_coding_rate(1).unwrap();
        assert_eq!(radio.coding_rate(), 5);
        assert!(has_seq(&spi, &[0x8b, 0x07, 0x04, 0x01, 0x00, 0, 0, 0, 0]));
    }

    #[test]
    fn spi_and_gpio_errors_are_mapped() {
        let mut spi_radio: Sx126x<_, _, _, NoPin, _> = Sx126x::new(
            FailingSpi,
            MockInput::low(),
            MockOutput::default(),
            None,
            MockDelay::default(),
            Config::default(),
        );
        assert_eq!(spi_radio.current_rssi().unwrap_err(), Error::Spi(SpiFail));

        let mut gpio_radio: Sx126x<_, _, _, NoPin, _> = Sx126x::new(
            MockSpi::default(),
            FailingInput,
            MockOutput::default(),
            None,
            MockDelay::default(),
            Config::default(),
        );
        assert_eq!(
            gpio_radio.current_rssi().unwrap_err(),
            Error::Digital(DigitalErrorKind::Other)
        );
    }

    #[test]
    fn busy_timeout_is_reported_before_transaction() {
        let spi = MockSpi::default();
        let mut radio: Sx126x<_, _, _, NoPin, _> = Sx126x::new(
            spi.clone(),
            MockInput::busy([true, true, true]),
            MockOutput::default(),
            None,
            MockDelay::default(),
            Config {
                busy_timeout_ms: 2,
                ..Config::default()
            },
        );
        assert_eq!(radio.current_rssi().unwrap_err(), Error::BusyTimeout);
        assert!(spi.0.borrow().log.is_empty());
    }

    #[test]
    fn calibrate_image_all_firmware_bands_and_fallback() {
        let cases = [
            (433_000_000, [0x98, 0x6b, 0x6f]),
            (490_000_000, [0x98, 0x75, 0x81]),
            (780_000_000, [0x98, 0xc1, 0xc5]),
            (868_000_000, [0x98, 0xd7, 0xdb]),
            (915_000_000, [0x98, 0xe1, 0xe9]),
            (1_000_000_000, [0x98, 0x00, 0x00]),
        ];
        for (freq, bytes) in cases {
            let (mut radio, spi, _, _) = dev(Config::default());
            radio.calibrate_image(freq).unwrap();
            assert!(has_seq(&spi, &bytes), "freq {freq}");
        }
    }

    #[test]
    fn current_rssi_and_frequency_math() {
        let (mut radio, spi, _, _) = dev(Config::default());
        radio.set_frequency(868_000_000).unwrap();
        radio.set_frequency(915_000_000).unwrap();
        assert!(has_seq(&spi, &[0x86, 0x36, 0x40, 0x00, 0x00]));
        assert!(has_seq(&spi, &[0x86, 0x39, 0x30, 0x00, 0x00]));
        spi.push(&[100]);
        assert_eq!(radio.current_rssi().unwrap(), -50);
    }
}
