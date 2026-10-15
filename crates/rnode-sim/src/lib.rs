//! Simulated LoRa radios that share an on-host "air" directory.
//!
//! Each [`SimRadio`] binds one Unix datagram socket named `<node_name>.sock` in the configured
//! [`SimConfig::air_dir`]. A transmission is broadcast to every other `*.sock` in that directory:
//! first a carrier-start record, then (after Semtech LoRa time-on-air has elapsed) the completed
//! frame record. This deliberately models the half-duplex SX1262 path used by RNode_Firmware:
//! receivers must be awake, in receive mode for the whole frame, not transmitting, and configured
//! for the same frequency, bandwidth, spreading factor and low-data-rate optimisation. Low data
//! rate follows `rnode_core::phy::low_data_rate`, which intentionally preserves the firmware's
//! integer boundary behaviour and therefore models the LDRO interop failure from issue #4.
//!
//! The simulation is process-local to one filesystem namespace and requires no networking, so it
//! works in CI containers and on macOS. Keep `air_dir` paths short: Unix `sun_path` is about 104
//! bytes on macOS. Tests use short relative paths under `target/` for that reason.
//!
//! Unlike IRQ-pulse hardware drivers, simulated carrier detect is driven by explicit carrier-start
//! and frame-complete datagrams, so [`Radio::dcd`] ignores its `now_ms` argument and
//! [`Radio::set_dcd_timing`] is intentionally a no-op for this simulator. Stale carrier records are retained briefly
//! after DCD falls so that a delayed frame-complete datagram still delivers the frame.

use rnode_core::phy::{self, Modulation, PhyParams};
use rnode_core::{Radio, RxCrcErrors, RxFrame, RxSignal, TxOutcome};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const MAGIC: &[u8; 4] = b"RNSM";
const VERSION: u8 = 1;
const KIND_CARRIER: u8 = 1;
const KIND_FRAME: u8 = 2;
const MAX_SOCKET_PATH: usize = 103;
const DEFAULT_BANDWIDTH_HZ: u32 = 125_000;
const DEFAULT_SPREADING_FACTOR: u8 = 7;
const DEFAULT_CODING_RATE: u8 = 5;
const DEFAULT_TX_POWER_DBM: i8 = 17;
const NOISE_FLOOR_DBM: i16 = -110;
const STALE_CARRIER_RETENTION: Duration = Duration::from_secs(30);
const AIR_DIR_OWNER_PROBE: &str = ".rnode-sim-owner-check";
const MAX_TRACKED_CARRIERS: usize = 256;
const MAX_AIRTIME_US: u64 = 3_600_000_000;
const MAX_RX_QUEUE: usize = 256;
// u32 frequency + u32 bandwidth + six u8 packet fields + u64 airtime + u16 payload length.
const AIR_HEADER_FIXED_AFTER_SENDER_LEN: usize = 24;
static AIR_DIR_OWNER_PROBE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy)]
struct ToaParams {
    len: usize,
    bandwidth_hz: u32,
    spreading_factor: u8,
    coding_rate: u8,
    preamble_symbols: u16,
    crc: bool,
    implicit: bool,
    low_data_rate: bool,
}

/// Configuration for a simulated LoRa radio.
#[derive(Debug, Clone)]
pub struct SimConfig {
    /// Shared directory that acts as the medium.
    pub air_dir: PathBuf,
    /// Unique radio name; its socket is `<air_dir>/<node_name>.sock`.
    pub node_name: String,
    /// Packet/carrier RSSI reported by this receiver.
    pub rssi_dbm: i16,
    /// Packet SNR reported by this receiver, in dB.
    pub snr_db: f32,
    /// Probability in `0.0..=1.0` that an otherwise valid received frame is dropped.
    pub loss: f32,
    /// Probability in `0.0..=1.0` that an otherwise valid received frame that was not lost fails
    /// its payload CRC: like a modem, the radio raises its receive interrupt, drops the frame in
    /// [`Radio::poll_rx`]'s place and counts it, with its RSSI and SNR, for
    /// [`Radio::take_rx_crc_errors`].
    pub crc_error: f32,
    /// Seed for deterministic receive loss and CRC errors.
    pub seed: u64,
}

impl SimConfig {
    /// Build a config with firmware-test-friendly defaults (`RSSI=-60`, `SNR=8`, no loss, no
    /// CRC errors).
    pub fn new(air_dir: impl Into<PathBuf>, node_name: impl Into<String>) -> Self {
        Self {
            air_dir: air_dir.into(),
            node_name: node_name.into(),
            rssi_dbm: -60,
            snr_db: 8.0,
            loss: 0.0,
            crc_error: 0.0,
            seed: 0,
        }
    }
}

/// Error returned by the simulated radio.
#[derive(Debug)]
pub enum SimError {
    /// Underlying filesystem/socket error.
    Io(io::Error),
    /// Payloads are limited to the LoRa/RNode one-byte length range.
    FrameTooLong(usize),
    /// Malformed simulation packet received from the shared air directory.
    MalformedPacket,
    /// Internal worker state was poisoned.
    Poisoned,
}

impl fmt::Display for SimError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "sim radio I/O error: {e}"),
            Self::FrameTooLong(len) => write!(f, "LoRa frame too long: {len} bytes"),
            Self::MalformedPacket => f.write_str("malformed simulated-air packet"),
            Self::Poisoned => f.write_str("sim radio state lock poisoned"),
        }
    }
}

impl std::error::Error for SimError {}

impl From<io::Error> for SimError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

/// Simulated LoRa modem implementing [`rnode_core::Radio`].
#[derive(Debug)]
pub struct SimRadio {
    inner: Arc<Mutex<Inner>>,
    socket: UnixDatagram,
    irq_rx: UnixDatagram,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    socket_path: PathBuf,
}

#[derive(Debug)]
struct Inner {
    config: SimConfig,
    awake: bool,
    rx_mode: bool,
    implicit_len: Option<u8>,
    transmitting: bool,
    frequency_hz: u32,
    bandwidth_hz: u32,
    spreading_factor: u8,
    coding_rate: u8,
    tx_power_dbm: i8,
    preamble_symbols: u16,
    preamble_overridden: bool,
    crc: bool,
    low_data_rate: bool,
    rx_queue: VecDeque<RxFrame>,
    carriers: HashMap<String, Carrier>,
    rng: u64,
    current_rssi_dbm: i16,
    crc_errors: RxCrcErrors,
}

#[derive(Debug, Clone)]
struct Carrier {
    header: AirHeader,
    end: Instant,
    receivable: bool,
    collided: bool,
}

#[derive(Debug, Clone)]
struct AirHeader {
    kind: u8,
    sender: String,
    frequency_hz: u32,
    bandwidth_hz: u32,
    spreading_factor: u8,
    coding_rate: u8,
    crc: bool,
    implicit: bool,
    implicit_len: u8,
    low_data_rate: bool,
    airtime_us: u64,
}

impl SimRadio {
    /// Bind this radio's socket and start its receive worker.
    pub fn new(config: SimConfig) -> io::Result<Self> {
        validate_node_name(&config.node_name)?;
        ensure_private_air_dir(&config.air_dir)?;
        let socket_path = config.air_dir.join(format!("{}.sock", config.node_name));
        if socket_path.as_os_str().as_bytes().len() > MAX_SOCKET_PATH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Unix socket path too long: {}", socket_path.display()),
            ));
        }
        reject_symlink_socket_placeholder(&socket_path)?;
        match fs::remove_file(&socket_path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let socket = UnixDatagram::bind(&socket_path)?;
        socket.set_read_timeout(Some(Duration::from_millis(10)))?;
        let worker_socket = socket.try_clone()?;
        let tx_socket = socket.try_clone()?;
        let (irq_tx, irq_rx) = UnixDatagram::pair()?;
        irq_rx.set_nonblocking(true)?;
        irq_tx.set_nonblocking(true)?;
        let worker_irq = irq_tx.try_clone()?;
        let inner = Arc::new(Mutex::new(Inner::new(config)));
        let worker_inner = Arc::clone(&inner);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || worker_loop(worker_socket, worker_irq, worker_inner, worker_stop));
        Ok(Self {
            inner,
            socket: tx_socket,
            irq_rx,
            stop,
            worker: Some(worker),
            socket_path,
        })
    }

    /// Readable when a completed receive frame may be ready for [`Radio::poll_rx`].
    pub fn irq_fd(&self) -> BorrowedFd<'_> {
        self.irq_rx.as_fd()
    }

    /// Semtech LoRa time-on-air for the current modulation and packet settings.
    pub fn time_on_air(&self, len: usize, implicit: bool) -> Result<Duration, SimError> {
        let inner = self.inner.lock().map_err(|_| SimError::Poisoned)?;
        Ok(time_on_air(ToaParams {
            len,
            bandwidth_hz: inner.bandwidth_hz,
            spreading_factor: inner.spreading_factor,
            coding_rate: inner.coding_rate,
            preamble_symbols: inner.preamble_symbols,
            crc: inner.crc,
            implicit,
            low_data_rate: inner.low_data_rate,
        }))
    }

    fn set_modulation_defaults(inner: &mut Inner) {
        inner.low_data_rate = phy::low_data_rate(inner.spreading_factor, inner.bandwidth_hz);
        let p = PhyParams::new(Modulation {
            bandwidth_hz: inner.bandwidth_hz,
            spreading_factor: inner.spreading_factor,
            coding_rate: inner.coding_rate,
        });
        if !inner.preamble_overridden {
            inner.preamble_symbols = p.preamble_symbols;
        }
        inner.invalidate_carriers();
    }
}

impl Drop for SimRadio {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let _ = fs::remove_file(&self.socket_path);
    }
}

impl Inner {
    fn new(config: SimConfig) -> Self {
        let bandwidth_hz = DEFAULT_BANDWIDTH_HZ;
        let spreading_factor = DEFAULT_SPREADING_FACTOR;
        let coding_rate = DEFAULT_CODING_RATE;
        let seed = config.seed;
        let p = PhyParams::new(Modulation {
            bandwidth_hz,
            spreading_factor,
            coding_rate,
        });
        Self {
            config,
            awake: false,
            rx_mode: false,
            implicit_len: None,
            transmitting: false,
            frequency_hz: 0,
            bandwidth_hz,
            spreading_factor,
            coding_rate,
            tx_power_dbm: DEFAULT_TX_POWER_DBM,
            preamble_symbols: p.preamble_symbols,
            preamble_overridden: false,
            crc: true,
            low_data_rate: phy::low_data_rate(spreading_factor, bandwidth_hz),
            rx_queue: VecDeque::new(),
            carriers: HashMap::new(),
            rng: seed,
            current_rssi_dbm: NOISE_FLOOR_DBM,
            crc_errors: RxCrcErrors::default(),
        }
    }

    fn invalidate_carriers(&mut self) {
        for carrier in self.carriers.values_mut() {
            carrier.receivable = false;
        }
    }

    fn matches_carrier(&self, h: &AirHeader) -> bool {
        self.awake
            && self.rx_mode
            && !self.transmitting
            && self.frequency_hz == h.frequency_hz
            && self.bandwidth_hz == h.bandwidth_hz
            && self.spreading_factor == h.spreading_factor
            && self.low_data_rate == h.low_data_rate
    }

    fn accepts_frame(&self, h: &AirHeader, payload_len: usize) -> bool {
        if !self.matches_carrier(h) {
            return false;
        }
        match (self.implicit_len, h.implicit) {
            (None, false) => true,
            (Some(len), true) => {
                usize::from(len) == payload_len
                    && h.implicit_len == len
                    && self.coding_rate == h.coding_rate
                    && self.crc == h.crc
            }
            _ => false,
        }
    }

    fn refresh_carrier_sense(&mut self) -> bool {
        let now = Instant::now();
        self.prune_stale_carriers(now);
        let detected = self
            .carriers
            .values()
            .any(|carrier| carrier.receivable && carrier.end > now);
        self.current_rssi_dbm = if detected {
            self.config.rssi_dbm
        } else {
            NOISE_FLOOR_DBM
        };
        detected
    }

    fn prune_stale_carriers(&mut self, now: Instant) {
        self.carriers
            .retain(|_, carrier| carrier.end + STALE_CARRIER_RETENTION > now);
    }

    fn loss_drop(&mut self) -> bool {
        self.chance(self.config.loss)
    }

    fn crc_error(&mut self) -> bool {
        self.chance(self.config.crc_error)
    }

    /// True with probability `p`, drawing from the seeded generator only when `p` is strictly
    /// between 0 and 1, so that a setting of 0 leaves the other draws as they were.
    fn chance(&mut self, p: f32) -> bool {
        if p <= 0.0 {
            return false;
        }
        if p >= 1.0 {
            return true;
        }
        self.next_unit_f32() < p
    }

    fn next_unit_f32(&mut self) -> f32 {
        self.rng = self
            .rng
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let v = (self.rng >> 40) as u32;
        (v as f32) / ((1u32 << 24) as f32)
    }
}

impl Radio for SimRadio {
    type Error = SimError;

    fn begin(&mut self, frequency_hz: u32) -> Result<(), Self::Error> {
        let mut inner = self.inner.lock().map_err(|_| SimError::Poisoned)?;
        inner.frequency_hz = frequency_hz;
        inner.awake = true;
        inner.rx_mode = false;
        inner.implicit_len = None;
        inner.invalidate_carriers();
        Ok(())
    }

    fn end(&mut self) -> Result<(), Self::Error> {
        let mut inner = self.inner.lock().map_err(|_| SimError::Poisoned)?;
        inner.awake = false;
        inner.rx_mode = false;
        inner.implicit_len = None;
        inner.invalidate_carriers();
        Ok(())
    }

    fn set_frequency(&mut self, hz: u32) -> Result<(), Self::Error> {
        let mut inner = self.inner.lock().map_err(|_| SimError::Poisoned)?;
        inner.frequency_hz = hz;
        inner.invalidate_carriers();
        Ok(())
    }

    fn frequency(&self) -> u32 {
        self.inner.lock().map(|inner| inner.frequency_hz).unwrap_or_default()
    }

    fn set_bandwidth(&mut self, hz: u32) -> Result<(), Self::Error> {
        let mut inner = self.inner.lock().map_err(|_| SimError::Poisoned)?;
        inner.bandwidth_hz = quantize_bandwidth(hz);
        Self::set_modulation_defaults(&mut inner);
        Ok(())
    }

    fn bandwidth(&self) -> u32 {
        self.inner
            .lock()
            .map(|inner| inner.bandwidth_hz)
            .unwrap_or(DEFAULT_BANDWIDTH_HZ)
    }

    fn set_spreading_factor(&mut self, sf: u8) -> Result<(), Self::Error> {
        let mut inner = self.inner.lock().map_err(|_| SimError::Poisoned)?;
        inner.spreading_factor = sf.clamp(5, 12);
        Self::set_modulation_defaults(&mut inner);
        Ok(())
    }

    fn spreading_factor(&self) -> u8 {
        self.inner
            .lock()
            .map(|inner| inner.spreading_factor)
            .unwrap_or(DEFAULT_SPREADING_FACTOR)
    }

    fn set_coding_rate(&mut self, denominator: u8) -> Result<(), Self::Error> {
        let mut inner = self.inner.lock().map_err(|_| SimError::Poisoned)?;
        inner.coding_rate = denominator.clamp(5, 8);
        inner.invalidate_carriers();
        Ok(())
    }

    fn coding_rate(&self) -> u8 {
        self.inner
            .lock()
            .map(|inner| inner.coding_rate)
            .unwrap_or(DEFAULT_CODING_RATE)
    }

    fn set_tx_power(&mut self, dbm: i8) -> Result<(), Self::Error> {
        let mut inner = self.inner.lock().map_err(|_| SimError::Poisoned)?;
        inner.tx_power_dbm = dbm.clamp(-9, 22);
        Ok(())
    }

    fn tx_power(&self) -> i8 {
        self.inner
            .lock()
            .map(|inner| inner.tx_power_dbm)
            .unwrap_or(DEFAULT_TX_POWER_DBM)
    }

    fn set_preamble_length(&mut self, symbols: u16) -> Result<(), Self::Error> {
        let mut inner = self.inner.lock().map_err(|_| SimError::Poisoned)?;
        inner.preamble_symbols = symbols;
        inner.preamble_overridden = true;
        inner.invalidate_carriers();
        Ok(())
    }

    fn set_crc(&mut self, enabled: bool) -> Result<(), Self::Error> {
        let mut inner = self.inner.lock().map_err(|_| SimError::Poisoned)?;
        inner.crc = enabled;
        inner.invalidate_carriers();
        Ok(())
    }

    fn low_data_rate(&self) -> bool {
        self.inner.lock().map(|inner| inner.low_data_rate).unwrap_or(false)
    }

    fn receive(&mut self, implicit_len: Option<u8>) -> Result<(), Self::Error> {
        let mut inner = self.inner.lock().map_err(|_| SimError::Poisoned)?;
        inner.rx_mode = true;
        inner.implicit_len = implicit_len;
        Ok(())
    }

    fn transmit(&mut self, frame: &[u8], implicit: bool) -> Result<TxOutcome, Self::Error> {
        if frame.len() > u8::MAX as usize {
            return Err(SimError::FrameTooLong(frame.len()));
        }
        let (air_dir, self_name, header, sleep_for) = {
            let mut inner = self.inner.lock().map_err(|_| SimError::Poisoned)?;
            inner.transmitting = true;
            inner.rx_mode = false;
            inner.implicit_len = None;
            inner.invalidate_carriers();
            let airtime = time_on_air(ToaParams {
                len: frame.len(),
                bandwidth_hz: inner.bandwidth_hz,
                spreading_factor: inner.spreading_factor,
                coding_rate: inner.coding_rate,
                preamble_symbols: inner.preamble_symbols,
                crc: inner.crc,
                implicit,
                low_data_rate: inner.low_data_rate,
            });
            let airtime_us = airtime.as_micros().min(u128::from(u64::MAX)) as u64;
            let h = AirHeader {
                kind: KIND_CARRIER,
                sender: inner.config.node_name.clone(),
                frequency_hz: inner.frequency_hz,
                bandwidth_hz: inner.bandwidth_hz,
                spreading_factor: inner.spreading_factor,
                coding_rate: inner.coding_rate,
                crc: inner.crc,
                implicit,
                implicit_len: if implicit { frame.len() as u8 } else { 0 },
                low_data_rate: inner.low_data_rate,
                airtime_us,
            };
            (inner.config.air_dir.clone(), inner.config.node_name.clone(), h, airtime)
        };
        let tx_result: Result<(), SimError> = (|| {
            broadcast(&self.socket, &air_dir, &self_name, &encode_packet(&header, &[])?)?;
            thread::sleep(sleep_for);
            let mut frame_header = header;
            frame_header.kind = KIND_FRAME;
            broadcast(
                &self.socket,
                &air_dir,
                &self_name,
                &encode_packet(&frame_header, frame)?,
            )?;
            Ok(())
        })();
        let mut inner = self.inner.lock().map_err(|_| SimError::Poisoned)?;
        inner.transmitting = false;
        tx_result?;
        Ok(TxOutcome::Done)
    }

    fn poll_rx(&mut self) -> Result<Option<RxFrame>, Self::Error> {
        let mut inner = self.inner.lock().map_err(|_| SimError::Poisoned)?;
        let mut buf = [0u8; 128];
        let frame = inner.rx_queue.pop_front();
        if frame.is_some() {
            let _ = self.irq_rx.recv(&mut buf);
        } else {
            while self.irq_rx.recv(&mut buf).is_ok() {}
        }
        Ok(frame)
    }

    fn take_rx_crc_errors(&mut self) -> RxCrcErrors {
        self.inner
            .lock()
            .map(|mut inner| std::mem::take(&mut inner.crc_errors))
            .unwrap_or_default()
    }

    fn dcd(&mut self, _now_ms: u64) -> Result<bool, Self::Error> {
        let mut inner = self.inner.lock().map_err(|_| SimError::Poisoned)?;
        Ok(inner.refresh_carrier_sense())
    }

    fn current_rssi(&mut self) -> Result<i16, Self::Error> {
        let mut inner = self.inner.lock().map_err(|_| SimError::Poisoned)?;
        inner.refresh_carrier_sense();
        Ok(inner.current_rssi_dbm)
    }

    fn set_dcd_timing(&mut self, _preamble_ms: u32, _header_ms: u32) {}
}

fn worker_loop(socket: UnixDatagram, irq: UnixDatagram, inner: Arc<Mutex<Inner>>, stop: Arc<AtomicBool>) {
    let mut buf = [0u8; 2048];
    while !stop.load(Ordering::Relaxed) {
        match socket.recv(&mut buf) {
            Ok(len) => match decode_packet(&buf[..len]) {
                Ok((header, payload)) => handle_air_packet(&inner, &irq, header, payload),
                Err(e) => log::debug!("ignoring malformed simulated-air packet: {e}"),
            },
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::TimedOut
                    || e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => {
                log::debug!("sim radio worker recv failed: {e}");
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

fn handle_air_packet(inner: &Arc<Mutex<Inner>>, irq: &UnixDatagram, header: AirHeader, payload: Vec<u8>) {
    let Ok(mut inner) = inner.lock() else {
        return;
    };
    match header.kind {
        KIND_CARRIER => {
            let now = Instant::now();
            inner.prune_stale_carriers(now);
            if !inner.carriers.contains_key(&header.sender) && inner.carriers.len() >= MAX_TRACKED_CARRIERS {
                log::debug!("sim radio carrier table full; ignoring carrier from {}", header.sender);
                return;
            }
            let airtime_us = header.airtime_us.clamp(1, MAX_AIRTIME_US);
            let end = now + Duration::from_micros(airtime_us);
            let receivable = inner.matches_carrier(&header);
            let mut carrier = Carrier {
                header: header.clone(),
                end,
                receivable,
                collided: false,
            };
            if carrier.receivable {
                for other in inner.carriers.values_mut() {
                    if other.receivable && other.end > now {
                        other.collided = true;
                        carrier.collided = true;
                    }
                }
                inner.current_rssi_dbm = inner.config.rssi_dbm;
            }
            inner.carriers.insert(header.sender.clone(), carrier);
        }
        KIND_FRAME => {
            let carrier = inner.carriers.remove(&header.sender);
            let Some(carrier) = carrier else {
                return;
            };
            if !carrier.receivable || carrier.collided || carrier.header.airtime_us != header.airtime_us {
                return;
            }
            if !inner.accepts_frame(&header, payload.len()) || inner.loss_drop() {
                return;
            }
            let snr_raw = (inner.config.snr_db * 4.0).round() as i8 as u8;
            let rssi_dbm = inner.config.rssi_dbm;
            if inner.crc_error() {
                // A modem raises RX done for a frame that fails its CRC too; its driver drops it.
                inner.crc_errors.count = inner.crc_errors.count.saturating_add(1);
                inner.crc_errors.last = Some(RxSignal { rssi_dbm, snr_raw });
                let _ = irq.send(&[1]);
                return;
            }
            if inner.rx_queue.len() >= MAX_RX_QUEUE {
                log::debug!("sim radio rx queue full; dropping frame from {}", header.sender);
                return;
            }
            inner.rx_queue.push_back(RxFrame {
                data: payload,
                rssi_dbm,
                snr_raw,
            });
            let _ = irq.send(&[1]);
        }
        _ => {}
    }
}

fn broadcast(socket: &UnixDatagram, air_dir: &Path, self_name: &str, packet: &[u8]) -> Result<(), SimError> {
    for entry in fs::read_dir(air_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("sock") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if stem == self_name {
            continue;
        }
        if let Err(e) = socket.send_to(packet, &path) {
            log::debug!("failed to send simulated-air packet to {}: {e}", path.display());
        }
    }
    Ok(())
}

fn validate_node_name(node_name: &str) -> io::Result<()> {
    let valid = !node_name.is_empty()
        && node_name != "."
        && node_name != ".."
        && !node_name.as_bytes().contains(&b'/')
        && !node_name.as_bytes().contains(&0);
    if valid {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid simulated radio node name: {node_name:?}"),
        ))
    }
}

fn ensure_private_air_dir(air_dir: &Path) -> io::Result<()> {
    match fs::symlink_metadata(air_dir) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("simulated air_dir is a symlink: {}", air_dir.display()),
            ));
        }
        Ok(metadata) => return validate_existing_air_dir(air_dir, &metadata),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    if let Some(parent) = air_dir.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    match DirBuilder::new().mode(0o700).create(air_dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(air_dir)?;
            validate_existing_air_dir(air_dir, &metadata)
        }
        Err(e) => Err(e),
    }
}

fn validate_existing_air_dir(air_dir: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    if !metadata.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("simulated air_dir is not a directory: {}", air_dir.display()),
        ));
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "simulated air_dir must be private to its owner (mode 0700): {}",
                air_dir.display()
            ),
        ));
    }
    verify_air_dir_owner(air_dir, metadata.uid())?;
    Ok(())
}

fn verify_air_dir_owner(air_dir: &Path, dir_uid: u32) -> io::Result<()> {
    let counter = AIR_DIR_OWNER_PROBE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let probe = air_dir.join(format!("{}.{}.{}", AIR_DIR_OWNER_PROBE, std::process::id(), counter));
    let file = OpenOptions::new().write(true).create_new(true).open(&probe)?;
    let probe_uid = file.metadata()?.uid();
    drop(file);
    let _ = fs::remove_file(&probe);
    if probe_uid == dir_uid {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "simulated air_dir is not owned by the current effective user: {}",
                air_dir.display()
            ),
        ))
    }
}

fn reject_symlink_socket_placeholder(socket_path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(socket_path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("simulated radio socket path is a symlink: {}", socket_path.display()),
        )),
        Ok(metadata) if !metadata.file_type().is_socket() && !metadata.file_type().is_file() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "simulated radio socket path is not a stale socket or file: {}",
                socket_path.display()
            ),
        )),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn encode_packet(header: &AirHeader, payload: &[u8]) -> Result<Vec<u8>, SimError> {
    if header.sender.len() > u8::MAX as usize || payload.len() > u16::MAX as usize {
        return Err(SimError::MalformedPacket);
    }
    let mut out = Vec::with_capacity(32 + header.sender.len() + payload.len());
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.push(header.kind);
    out.push(header.sender.len() as u8);
    out.extend_from_slice(header.sender.as_bytes());
    out.extend_from_slice(&header.frequency_hz.to_be_bytes());
    out.extend_from_slice(&header.bandwidth_hz.to_be_bytes());
    out.push(header.spreading_factor);
    out.push(header.coding_rate);
    out.push(u8::from(header.crc));
    out.push(u8::from(header.implicit));
    out.push(header.implicit_len);
    out.push(u8::from(header.low_data_rate));
    out.extend_from_slice(&header.airtime_us.to_be_bytes());
    out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

fn decode_packet(buf: &[u8]) -> Result<(AirHeader, Vec<u8>), SimError> {
    if buf.len() < 7 || &buf[..4] != MAGIC || buf[4] != VERSION {
        return Err(SimError::MalformedPacket);
    }
    let kind = buf[5];
    let sender_len = usize::from(buf[6]);
    let mut idx = 7;
    if buf.len() < idx + sender_len + AIR_HEADER_FIXED_AFTER_SENDER_LEN {
        return Err(SimError::MalformedPacket);
    }
    let sender = std::str::from_utf8(&buf[idx..idx + sender_len])
        .map_err(|_| SimError::MalformedPacket)?
        .to_owned();
    idx += sender_len;
    let frequency_hz = read_u32(buf, &mut idx)?;
    let bandwidth_hz = read_u32(buf, &mut idx)?;
    let spreading_factor = read_u8(buf, &mut idx)?;
    let coding_rate = read_u8(buf, &mut idx)?;
    let crc = read_u8(buf, &mut idx)? != 0;
    let implicit = read_u8(buf, &mut idx)? != 0;
    let implicit_len = read_u8(buf, &mut idx)?;
    let low_data_rate = read_u8(buf, &mut idx)? != 0;
    let airtime_us = read_u64(buf, &mut idx)?;
    let payload_len = usize::from(read_u16(buf, &mut idx)?);
    if buf.len() != idx + payload_len {
        return Err(SimError::MalformedPacket);
    }
    let payload = buf[idx..].to_vec();
    Ok((
        AirHeader {
            kind,
            sender,
            frequency_hz,
            bandwidth_hz,
            spreading_factor,
            coding_rate,
            crc,
            implicit,
            implicit_len,
            low_data_rate,
            airtime_us,
        },
        payload,
    ))
}

fn read_u8(buf: &[u8], idx: &mut usize) -> Result<u8, SimError> {
    let value = *buf.get(*idx).ok_or(SimError::MalformedPacket)?;
    *idx += 1;
    Ok(value)
}

fn read_u16(buf: &[u8], idx: &mut usize) -> Result<u16, SimError> {
    let bytes = buf.get(*idx..*idx + 2).ok_or(SimError::MalformedPacket)?;
    *idx += 2;
    Ok(u16::from_be_bytes(
        bytes.try_into().map_err(|_| SimError::MalformedPacket)?,
    ))
}

fn read_u32(buf: &[u8], idx: &mut usize) -> Result<u32, SimError> {
    let bytes = buf.get(*idx..*idx + 4).ok_or(SimError::MalformedPacket)?;
    *idx += 4;
    Ok(u32::from_be_bytes(
        bytes.try_into().map_err(|_| SimError::MalformedPacket)?,
    ))
}

fn read_u64(buf: &[u8], idx: &mut usize) -> Result<u64, SimError> {
    let bytes = buf.get(*idx..*idx + 8).ok_or(SimError::MalformedPacket)?;
    *idx += 8;
    Ok(u64::from_be_bytes(
        bytes.try_into().map_err(|_| SimError::MalformedPacket)?,
    ))
}

fn quantize_bandwidth(hz: u32) -> u32 {
    const TABLE: [u32; 10] = [
        7_800, 10_400, 15_600, 20_800, 31_250, 41_700, 62_500, 125_000, 250_000, 500_000,
    ];
    TABLE.into_iter().find(|&bw| hz <= bw).unwrap_or(500_000)
}

fn time_on_air(params: ToaParams) -> Duration {
    let sf = f64::from(params.spreading_factor);
    let tsym = 2f64.powi(i32::from(params.spreading_factor)) / f64::from(params.bandwidth_hz);
    let de = if params.low_data_rate { 1.0 } else { 0.0 };
    let ih = if params.implicit { 1.0 } else { 0.0 };
    let crc_bits = if params.crc { 16.0 } else { 0.0 };
    let low_sf = params.spreading_factor < 7;
    let header_bits = if low_sf { 20.0 } else { 28.0 };
    let numerator = 8.0 * params.len as f64 - 4.0 * sf + header_bits + crc_bits - 20.0 * ih;
    let denominator = if low_sf { 4.0 * sf } else { 4.0 * (sf - 2.0 * de) };
    let payload_symbols = 8.0 + (numerator / denominator).ceil().max(0.0) * f64::from(params.coding_rate);
    let preamble_extra = if low_sf { 6.25 } else { 4.25 };
    let seconds = (f64::from(params.preamble_symbols) + preamble_extra + payload_symbols) * tsym;
    Duration::from_secs_f64(seconds.max(0.001))
}

#[cfg(test)]
mod tests;
