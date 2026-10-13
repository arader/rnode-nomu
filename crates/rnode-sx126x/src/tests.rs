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

fn dev_with_delay(config: Config) -> (Dev, MockSpi, MockOutput, MockOutput, MockDelay) {
    let spi = MockSpi::default();
    let reset = MockOutput::default();
    let rxen = MockOutput::default();
    let delay = MockDelay::default();
    let radio = Sx126x::new(
        spi.clone(),
        MockInput::low(),
        reset.clone(),
        Some(rxen.clone()),
        delay.clone(),
        config,
    );
    (radio, spi, reset, rxen, delay)
}

fn dev(config: Config) -> (Dev, MockSpi, MockOutput, MockOutput) {
    let (radio, spi, reset, rxen, _) = dev_with_delay(config);
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

fn get_irq_read_count(spi: &MockSpi) -> usize {
    writes(spi).iter().filter(|w| w == &&[OP_GET_IRQ_STATUS, 0x00]).count()
}

fn delay_ms_count(delay: &MockDelay) -> usize {
    delay.delays.borrow().iter().filter(|ns| **ns == 1_000_000).count()
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
fn tx_timeout_fast_configuration_uses_configured_floor() {
    let (mut radio, spi, _, _, delay) = dev_with_delay(Config {
        tx_timeout_ms: 1_200,
        ..Config::default()
    });
    assert_eq!(radio.transmit(&[0xaa], false).unwrap(), TxOutcome::Timeout);
    assert_eq!(delay_ms_count(&delay), 1_200);
    assert_eq!(get_irq_read_count(&spi), 1_201);
}

#[test]
fn tx_timeout_slow_configuration_scales_past_configured_floor() {
    let (mut radio, spi, _, _, delay) = dev_with_delay(Config {
        tx_timeout_ms: 10,
        ..Config::default()
    });
    radio.set_spreading_factor(12).unwrap();
    radio.set_coding_rate(8).unwrap();
    // Independent firmware-estimate arithmetic for SF12/125 kHz/CR8, LDRO on, 18-symbol
    // preamble and a 10-byte payload: ((76 / 40) * 8 + 18 + 0.25 + 8) * 32.768 ms.
    let expected = 3_717;
    assert!(expected > 10);
    assert_eq!(radio.transmit(&[0x55; 10], false).unwrap(), TxOutcome::Timeout);
    assert_eq!(delay_ms_count(&delay), expected as usize);
    assert_eq!(get_irq_read_count(&spi), expected as usize + 1);
}

#[test]
fn tx_timeout_scaled_limit_matches_independent_boundary_value() {
    let (mut radio, _spi, _, _) = dev(Config {
        tx_timeout_ms: 20_000,
        ..Config::default()
    });
    radio.set_spreading_factor(12).unwrap();
    radio.set_coding_rate(8).unwrap();
    // Precomputed from the firmware airtime expression for SF12/125 kHz/CR8, LDRO on,
    // 18-symbol preamble and a 255-byte payload, then applying 2× + 1000 ms.
    let firmware_vector_limit = 29_407;
    assert_eq!(radio.tx_timeout_limit_ms(255), firmware_vector_limit);
}

#[test]
fn tx_timeout_defensive_fallbacks_use_configured_floor() {
    let (mut radio, _spi, _, _) = dev(Config {
        tx_timeout_ms: 1_234,
        ..Config::default()
    });
    radio.bw_hz = 0;
    assert_eq!(radio.tx_timeout_limit_ms(255), 1_234);

    radio.bw_hz = 1;
    radio.sf = u8::MAX;
    assert_eq!(radio.tx_timeout_limit_ms(255), 1_234);
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
