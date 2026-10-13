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
use rnode_core::{Radio, RxFrame, TxOutcome};
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
    /// Seed for deterministic receive loss.
    pub seed: u64,
}

impl SimConfig {
    /// Build a config with firmware-test-friendly defaults (`RSSI=-60`, `SNR=8`, no loss).
    pub fn new(air_dir: impl Into<PathBuf>, node_name: impl Into<String>) -> Self {
        Self {
            air_dir: air_dir.into(),
            node_name: node_name.into(),
            rssi_dbm: -60,
            snr_db: 8.0,
            loss: 0.0,
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
        if self.config.loss <= 0.0 {
            return false;
        }
        if self.config.loss >= 1.0 {
            return true;
        }
        let sample = self.next_unit_f32();
        sample < self.config.loss
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
            if inner.rx_queue.len() >= MAX_RX_QUEUE {
                log::debug!("sim radio rx queue full; dropping frame from {}", header.sender);
                return;
            }
            let snr_raw = (inner.config.snr_db * 4.0).round() as i8 as u8;
            let rssi_dbm = inner.config.rssi_dbm;
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
mod tests {
    use super::*;
    use std::fs;
    use std::os::fd::AsRawFd;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct AirDir {
        path: PathBuf,
    }

    impl AirDir {
        fn new(name: &str) -> Self {
            let nonce = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_nanos();
            let path = PathBuf::from(format!("target/sim-{name}-{}-{nonce}", std::process::id()));
            fs::create_dir_all(&path).expect("create air dir");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("private air dir");
            Self { path }
        }
    }

    impl Drop for AirDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn radio(dir: &AirDir, name: &str) -> SimRadio {
        let mut r = SimRadio::new(SimConfig::new(&dir.path, name)).expect("radio");
        r.begin(915_000_000).expect("begin");
        r.set_bandwidth(500_000).expect("bw");
        r.set_spreading_factor(7).expect("sf");
        r.receive(None).expect("rx");
        r
    }

    fn wait_rx(r: &mut SimRadio) -> Option<RxFrame> {
        let deadline = Instant::now() + Duration::from_millis(250);
        while Instant::now() < deadline {
            if let Some(frame) = r.poll_rx().expect("poll") {
                return Some(frame);
            }
            thread::sleep(Duration::from_millis(2));
        }
        None
    }

    #[test]
    fn delivery_reports_receiver_rssi_and_snr() {
        let dir = AirDir::new("delivery");
        let mut a = radio(&dir, "a");
        let mut cfg = SimConfig::new(&dir.path, "b");
        cfg.rssi_dbm = -73;
        cfg.snr_db = 5.25;
        let mut b = SimRadio::new(cfg).expect("radio");
        b.begin(915_000_000).expect("begin");
        b.set_bandwidth(500_000).expect("bw");
        b.receive(None).expect("rx");
        assert_eq!(a.transmit(b"hello", false).expect("tx"), TxOutcome::Done);
        let frame = wait_rx(&mut b).expect("rx frame");
        assert_eq!(frame.data, b"hello");
        assert_eq!(frame.rssi_dbm, -73);
        assert_eq!(frame.snr_raw, 21);
    }

    #[test]
    fn carrier_detects_during_airtime_and_tx_duration_matches_formula() {
        let dir = AirDir::new("dcd");
        let mut a = radio(&dir, "a");
        let mut b = radio(&dir, "b");
        a.set_preamble_length(64).expect("preamble");
        let expected = a.time_on_air(16, false).expect("toa");
        let start = Instant::now();
        let handle = thread::spawn(move || {
            a.transmit(&[0x55; 16], false).expect("tx");
            start.elapsed()
        });
        let deadline = Instant::now() + Duration::from_millis(80);
        let mut saw_dcd = false;
        while Instant::now() < deadline {
            if b.dcd(0).expect("dcd") {
                saw_dcd = true;
                assert_eq!(b.current_rssi().expect("rssi"), -60);
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert!(saw_dcd);
        let elapsed = handle.join().expect("join");
        assert!(elapsed >= expected);
        assert!(
            elapsed <= expected + Duration::from_millis(30),
            "elapsed={elapsed:?} expected={expected:?}"
        );
        assert_eq!(wait_rx(&mut b).expect("rx").data, vec![0x55; 16]);
        assert_eq!(b.current_rssi().expect("rssi"), NOISE_FLOOR_DBM);
    }

    #[test]
    fn mismatched_modulation_is_not_received() {
        let dir = AirDir::new("mismatch");
        let mut a = radio(&dir, "a");
        let mut b = radio(&dir, "b");
        b.set_frequency(916_000_000).expect("freq");
        a.transmit(b"freq", false).expect("tx");
        assert!(wait_rx(&mut b).is_none());
        b.set_frequency(915_000_000).expect("freq");
        b.set_bandwidth(125_000).expect("bw");
        a.transmit(b"bw", false).expect("tx");
        assert!(wait_rx(&mut b).is_none());
        b.set_bandwidth(500_000).expect("bw");
        b.set_spreading_factor(8).expect("sf");
        a.transmit(b"sf", false).expect("tx");
        assert!(wait_rx(&mut b).is_none());
        b.set_spreading_factor(7).expect("sf");
        a.set_bandwidth(125_000).expect("bw");
        b.set_bandwidth(125_000).expect("bw");
        a.set_spreading_factor(12).expect("sf");
        b.set_spreading_factor(12).expect("sf");
        b.set_bandwidth(250_000).expect("bw");
        a.transmit(b"ldro", false).expect("tx");
        assert!(wait_rx(&mut b).is_none());
    }

    #[test]
    fn ldro_mismatch_alone_is_not_a_matching_carrier() {
        let dir = AirDir::new("ldro-only");
        let mut b = radio(&dir, "b");
        b.set_bandwidth(125_000).expect("bw");
        b.set_spreading_factor(12).expect("sf");
        let inner = b.inner.lock().expect("lock");
        assert!(inner.low_data_rate);
        let header = AirHeader {
            kind: KIND_CARRIER,
            sender: "a".to_owned(),
            frequency_hz: inner.frequency_hz,
            bandwidth_hz: inner.bandwidth_hz,
            spreading_factor: inner.spreading_factor,
            coding_rate: inner.coding_rate,
            crc: inner.crc,
            implicit: false,
            implicit_len: 0,
            low_data_rate: false,
            airtime_us: 1_000,
        };
        assert!(!inner.matches_carrier(&header));
    }

    #[test]
    fn implicit_mode_requires_len_coding_rate_and_crc() {
        let dir = AirDir::new("implicit");
        let mut a = radio(&dir, "a");
        let mut b = radio(&dir, "b");
        b.receive(Some(3)).expect("implicit rx");
        a.transmit(b"abc", true).expect("tx");
        assert_eq!(wait_rx(&mut b).expect("rx").data, b"abc");
        b.receive(Some(4)).expect("implicit rx");
        a.transmit(b"abc", true).expect("tx");
        assert!(wait_rx(&mut b).is_none());
        b.receive(Some(3)).expect("implicit rx");
        b.set_coding_rate(6).expect("cr");
        a.transmit(b"abc", true).expect("tx");
        assert!(wait_rx(&mut b).is_none());
        b.set_coding_rate(5).expect("cr");
        b.set_crc(false).expect("crc");
        a.transmit(b"abc", true).expect("tx");
        assert!(wait_rx(&mut b).is_none());
    }

    #[test]
    fn explicit_and_implicit_header_modes_must_match() {
        let dir = AirDir::new("header-mode");
        let mut b = radio(&dir, "b");
        let mut header = AirHeader {
            kind: KIND_FRAME,
            sender: "a".to_owned(),
            frequency_hz: b.frequency(),
            bandwidth_hz: b.bandwidth(),
            spreading_factor: b.spreading_factor(),
            coding_rate: b.coding_rate(),
            crc: true,
            implicit: true,
            implicit_len: 3,
            low_data_rate: b.low_data_rate(),
            airtime_us: 1_000,
        };
        {
            let inner = b.inner.lock().expect("lock");
            assert!(!inner.accepts_frame(&header, 3));
        }
        b.receive(Some(3)).expect("implicit rx");
        header.implicit = false;
        header.implicit_len = 0;
        let inner = b.inner.lock().expect("lock");
        assert!(!inner.accepts_frame(&header, 3));
    }

    #[test]
    fn half_duplex_sleep_and_collision_drop_frames() {
        let dir = AirDir::new("rules");
        let mut a = radio(&dir, "a");
        let mut b = radio(&dir, "b");
        let mut c = radio(&dir, "c");
        b.end().expect("sleep");
        a.transmit(b"sleep", false).expect("tx");
        assert!(wait_rx(&mut b).is_none());
        b.begin(915_000_000).expect("begin");
        b.set_bandwidth(500_000).expect("bw");
        b.receive(None).expect("rx");
        a.set_preamble_length(64).expect("preamble");
        let handle = thread::spawn(move || a.transmit(b"busy", false).expect("tx"));
        thread::sleep(Duration::from_millis(2));
        b.transmit(b"reply", false).expect("tx");
        handle.join().expect("join");
        assert!(wait_rx(&mut b).is_none());
        b.receive(None).expect("rx");
        c.set_preamble_length(64).expect("preamble");
        let h1 = thread::spawn(move || c.transmit(b"one", false).expect("tx"));
        thread::sleep(Duration::from_millis(1));
        let mut d = radio(&dir, "d");
        d.set_preamble_length(64).expect("preamble");
        let h2 = thread::spawn(move || d.transmit(b"two", false).expect("tx"));
        h1.join().expect("join");
        h2.join().expect("join");
        assert!(wait_rx(&mut b).is_none());
    }

    #[test]
    fn seeded_loss_and_socket_lifecycle() {
        let dir = AirDir::new("loss");
        let mut a = radio(&dir, "a");
        let mut cfg = SimConfig::new(&dir.path, "b");
        cfg.loss = 1.0;
        cfg.seed = 42;
        let sock = dir.path.join("b.sock");
        fs::write(&sock, b"stale").expect("stale");
        let mut b = SimRadio::new(cfg).expect("replace stale");
        assert!(sock.exists());
        b.begin(915_000_000).expect("begin");
        b.set_bandwidth(500_000).expect("bw");
        b.receive(None).expect("rx");
        a.transmit(b"lost", false).expect("tx");
        assert!(wait_rx(&mut b).is_none());
        drop(b);
        assert!(!sock.exists());
    }

    #[test]
    fn semtech_time_on_air_has_golden_value() {
        // Semtech LoRa formula for SF7, 500 kHz, CR 4/5, explicit header, CRC on,
        // 64-symbol preamble, 16-byte payload:
        // T_sym = 2^7/500000 = 0.256 ms; payload symbols = 8 + ceil(144/28)*5 = 38;
        // total symbols = 64 + 4.25 + 38 = 106.25; airtime = 27.2 ms.
        let airtime = time_on_air(ToaParams {
            len: 16,
            bandwidth_hz: 500_000,
            spreading_factor: 7,
            coding_rate: 5,
            preamble_symbols: 64,
            crc: true,
            implicit: false,
            low_data_rate: false,
        });
        assert_eq!(airtime.as_micros(), 27_200);

        // Same formula with implicit header and low-data-rate optimisation:
        // SF12/125 kHz gives T_sym = 32.768 ms and denominator 4*(12-2) = 40.
        // Payload symbols = 8 + ceil((128 - 48 + 28 + 16 - 20)/40)*5 = 23;
        // total symbols = 18 + 4.25 + 23 = 45.25; airtime = 1482.752 ms.
        let implicit_ldr = time_on_air(ToaParams {
            len: 16,
            bandwidth_hz: 125_000,
            spreading_factor: 12,
            coding_rate: 5,
            preamble_symbols: 18,
            crc: true,
            implicit: true,
            low_data_rate: true,
        });
        assert_eq!(implicit_ldr.as_micros(), 1_482_752);

        // SX126x datasheet §6.1.4 low-SF branch: SF5/SF6 use Npreamble + 6.25 and the
        // payload numerator omits the SF7+ extra 8 bits.
        let sf5 = time_on_air(ToaParams {
            len: 16,
            bandwidth_hz: 500_000,
            spreading_factor: 5,
            coding_rate: 5,
            preamble_symbols: 18,
            crc: true,
            implicit: false,
            low_data_rate: false,
        });
        assert_eq!(sf5.as_micros(), 4_624);

        let sf6 = time_on_air(ToaParams {
            len: 16,
            bandwidth_hz: 500_000,
            spreading_factor: 6,
            coding_rate: 5,
            preamble_symbols: 18,
            crc: true,
            implicit: false,
            low_data_rate: false,
        });
        assert_eq!(sf6.as_micros(), 7_968);
    }

    #[test]
    fn seeded_partial_loss_is_deterministic() {
        let mut config = SimConfig::new("target/not-used", "lossy");
        config.loss = 0.5;
        config.seed = 1;
        let mut inner = Inner::new(config);
        let drops: Vec<bool> = (0..10).map(|_| inner.loss_drop()).collect();
        assert_eq!(
            drops,
            vec![true, false, false, true, false, false, false, true, false, true]
        );
    }

    #[test]
    fn decode_packet_rejects_malformed_datagrams() {
        assert!(matches!(decode_packet(b"nope"), Err(SimError::MalformedPacket)));
        let mut packet = encode_packet(
            &AirHeader {
                kind: KIND_FRAME,
                sender: "a".to_owned(),
                frequency_hz: 915_000_000,
                bandwidth_hz: 125_000,
                spreading_factor: 7,
                coding_rate: 5,
                crc: true,
                implicit: false,
                implicit_len: 0,
                low_data_rate: false,
                airtime_us: 1_000,
            },
            b"ok",
        )
        .expect("encode");
        packet[4] = VERSION + 1;
        assert!(matches!(decode_packet(&packet), Err(SimError::MalformedPacket)));
        packet[4] = VERSION;
        packet[7] = 0xff;
        assert!(matches!(decode_packet(&packet), Err(SimError::MalformedPacket)));
        packet[7] = b'a';
        let truncated = &packet[..packet.len() - 1];
        assert!(matches!(decode_packet(truncated), Err(SimError::MalformedPacket)));
        let payload_len_idx = packet.len() - b"ok".len() - 2;
        packet[payload_len_idx] = 0;
        packet[payload_len_idx + 1] = 3;
        assert!(matches!(decode_packet(&packet), Err(SimError::MalformedPacket)));
    }

    #[test]
    fn encode_packet_rejects_oversized_sender() {
        let header = AirHeader {
            kind: KIND_FRAME,
            sender: "n".repeat(256),
            frequency_hz: 915_000_000,
            bandwidth_hz: 125_000,
            spreading_factor: 7,
            coding_rate: 5,
            crc: true,
            implicit: false,
            implicit_len: 0,
            low_data_rate: false,
            airtime_us: 1_000,
        };
        assert!(matches!(encode_packet(&header, b""), Err(SimError::MalformedPacket)));
    }

    #[test]
    fn node_names_cannot_escape_air_dir() {
        let dir = AirDir::new("names");
        for name in ["", ".", "..", "../outside", "nested/name", "nul\0name"] {
            let err = SimRadio::new(SimConfig::new(&dir.path, name)).expect_err("invalid name");
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{name:?}");
        }
    }

    #[test]
    fn long_socket_paths_return_invalid_input() {
        let dir = AirDir::new("long");
        let long_name = "n".repeat(MAX_SOCKET_PATH);
        let err = SimRadio::new(SimConfig::new(&dir.path, long_name)).expect_err("long path");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn stale_socket_cleanup_rejects_symlinks() {
        let dir = AirDir::new("symlink");
        let target = dir.path.join("target");
        fs::write(&target, b"not a socket").expect("target");
        std::os::unix::fs::symlink(&target, dir.path.join("evil.sock")).expect("symlink");
        let err = SimRadio::new(SimConfig::new(&dir.path, "evil")).expect_err("symlink rejected");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(target.exists());
    }

    #[test]
    fn stale_socket_cleanup_rejects_directories() {
        let dir = AirDir::new("placeholder-dir");
        fs::create_dir(dir.path.join("evil.sock")).expect("placeholder dir");
        let err = SimRadio::new(SimConfig::new(&dir.path, "evil")).expect_err("directory rejected");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn missing_air_dir_is_created_private() {
        let dir = AirDir::new("perms");
        let air_dir = dir.path.join("child");
        let _radio = SimRadio::new(SimConfig::new(&air_dir, "a")).expect("radio");
        let mode = fs::metadata(&air_dir).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn existing_permissive_air_dir_is_rejected_without_chmod() {
        let dir = AirDir::new("wide-perms");
        fs::set_permissions(&dir.path, fs::Permissions::from_mode(0o755)).expect("wide perms");
        let err = SimRadio::new(SimConfig::new(&dir.path, "a")).expect_err("wide air_dir");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        let mode = fs::metadata(&dir.path).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn air_dir_symlinks_are_rejected() {
        let dir = AirDir::new("air-symlink");
        let real = dir.path.join("real");
        fs::create_dir(&real).expect("real dir");
        fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).expect("private real dir");
        let link = dir.path.join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let err = SimRadio::new(SimConfig::new(&link, "a")).expect_err("symlink air_dir");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn concurrent_radios_do_not_collide_on_owner_probe() {
        let dir = AirDir::new("owner-probe");
        let mut handles = Vec::new();
        for i in 0..8 {
            let path = dir.path.clone();
            handles.push(thread::spawn(move || {
                let _radio = SimRadio::new(SimConfig::new(path, format!("node-{i}"))).expect("radio");
            }));
        }
        for handle in handles {
            handle.join().expect("thread");
        }
    }

    #[test]
    fn bandwidth_requests_round_up_to_supported_values() {
        let dir = AirDir::new("bw");
        let mut r = radio(&dir, "a");
        r.set_bandwidth(100_000).expect("round up");
        assert_eq!(r.bandwidth(), 125_000);
        r.set_bandwidth(7_799).expect("round up minimum");
        assert_eq!(r.bandwidth(), 7_800);
        r.set_bandwidth(600_000).expect("clamp maximum");
        assert_eq!(r.bandwidth(), 500_000);
    }

    #[test]
    fn explicit_preamble_survives_modulation_changes() {
        let dir = AirDir::new("preamble");
        let mut r = radio(&dir, "a");
        r.set_preamble_length(64).expect("preamble");
        r.set_bandwidth(125_000).expect("bw");
        r.set_spreading_factor(12).expect("sf");
        assert_eq!(r.inner.lock().expect("lock").preamble_symbols, 64);
    }

    #[test]
    fn oversized_frames_are_rejected_before_transmit_state_changes() {
        let dir = AirDir::new("long-frame");
        let mut r = radio(&dir, "a");
        let err = r.transmit(&[0; 256], false).expect_err("too long");
        assert!(matches!(err, SimError::FrameTooLong(256)));
        let inner = r.inner.lock().expect("lock");
        assert!(!inner.transmitting);
        assert!(inner.rx_mode);
    }

    #[test]
    fn failed_transmit_clears_half_duplex_flag() {
        let dir = AirDir::new("txerr");
        let mut a = radio(&dir, "a");
        fs::remove_dir_all(&dir.path).expect("remove air dir");
        assert!(a.transmit(b"fails", false).is_err());
        assert!(!a.inner.lock().expect("lock").transmitting);
    }

    #[test]
    fn dcd_poll_after_airtime_does_not_drop_completion_frame() {
        let dir = AirDir::new("dcd-grace");
        let mut b = radio(&dir, "b");
        let (irq_tx, _irq_rx) = UnixDatagram::pair().expect("irq pair");
        let mut header = AirHeader {
            kind: KIND_CARRIER,
            sender: "a".to_owned(),
            frequency_hz: b.frequency(),
            bandwidth_hz: b.bandwidth(),
            spreading_factor: b.spreading_factor(),
            coding_rate: b.coding_rate(),
            crc: true,
            implicit: false,
            implicit_len: 0,
            low_data_rate: b.low_data_rate(),
            airtime_us: 1_000,
        };
        handle_air_packet(&b.inner, &irq_tx, header.clone(), Vec::new());
        thread::sleep(Duration::from_millis(150));
        assert_eq!(b.current_rssi().expect("rssi"), NOISE_FLOOR_DBM);
        header.kind = KIND_FRAME;
        handle_air_packet(&b.inner, &irq_tx, header, b"late".to_vec());
        assert_eq!(b.poll_rx().expect("poll").expect("frame").data, b"late");
    }

    #[test]
    fn spoofed_carrier_senders_are_bounded() {
        let dir = AirDir::new("carrier-cap");
        let b = radio(&dir, "b");
        let (irq_tx, _irq_rx) = UnixDatagram::pair().expect("irq pair");
        for i in 0..(MAX_TRACKED_CARRIERS + 10) {
            let header = AirHeader {
                kind: KIND_CARRIER,
                sender: format!("sender-{i}"),
                frequency_hz: b.frequency(),
                bandwidth_hz: b.bandwidth(),
                spreading_factor: b.spreading_factor(),
                coding_rate: b.coding_rate(),
                crc: true,
                implicit: false,
                implicit_len: 0,
                low_data_rate: b.low_data_rate(),
                airtime_us: 30_000_000,
            };
            handle_air_packet(&b.inner, &irq_tx, header, Vec::new());
        }
        assert_eq!(b.inner.lock().expect("lock").carriers.len(), MAX_TRACKED_CARRIERS);
    }

    #[test]
    fn huge_airtime_from_wire_is_clamped() {
        let dir = AirDir::new("huge-airtime");
        let b = radio(&dir, "b");
        let (irq_tx, _irq_rx) = UnixDatagram::pair().expect("irq pair");
        let before = Instant::now();
        let header = AirHeader {
            kind: KIND_CARRIER,
            sender: "attacker".to_owned(),
            frequency_hz: b.frequency(),
            bandwidth_hz: b.bandwidth(),
            spreading_factor: b.spreading_factor(),
            coding_rate: b.coding_rate(),
            crc: true,
            implicit: false,
            implicit_len: 0,
            low_data_rate: b.low_data_rate(),
            airtime_us: u64::MAX,
        };
        handle_air_packet(&b.inner, &irq_tx, header, Vec::new());
        let inner = b.inner.lock().expect("lock");
        let carrier = inner.carriers.get("attacker").expect("carrier");
        assert!(
            carrier.end.duration_since(before) <= Duration::from_micros(MAX_AIRTIME_US) + Duration::from_millis(50)
        );
    }

    #[test]
    fn airtime_mismatch_between_carrier_and_frame_is_dropped() {
        let dir = AirDir::new("airtime-mismatch");
        let mut b = radio(&dir, "b");
        let (irq_tx, _irq_rx) = UnixDatagram::pair().expect("irq pair");
        let mut header = AirHeader {
            kind: KIND_CARRIER,
            sender: "a".to_owned(),
            frequency_hz: b.frequency(),
            bandwidth_hz: b.bandwidth(),
            spreading_factor: b.spreading_factor(),
            coding_rate: b.coding_rate(),
            crc: true,
            implicit: false,
            implicit_len: 0,
            low_data_rate: b.low_data_rate(),
            airtime_us: 1_000,
        };
        handle_air_packet(&b.inner, &irq_tx, header.clone(), Vec::new());
        header.kind = KIND_FRAME;
        header.airtime_us = 2_000;
        handle_air_packet(&b.inner, &irq_tx, header, b"bad".to_vec());
        assert!(b.poll_rx().expect("poll").is_none());
    }

    #[test]
    fn accepted_rx_frames_are_bounded() {
        let dir = AirDir::new("rx-cap");
        let b = radio(&dir, "b");
        let (irq_tx, _irq_rx) = UnixDatagram::pair().expect("irq pair");
        for i in 0..(MAX_RX_QUEUE + 10) {
            let mut header = AirHeader {
                kind: KIND_CARRIER,
                sender: format!("sender-{i}"),
                frequency_hz: b.frequency(),
                bandwidth_hz: b.bandwidth(),
                spreading_factor: b.spreading_factor(),
                coding_rate: b.coding_rate(),
                crc: true,
                implicit: false,
                implicit_len: 0,
                low_data_rate: b.low_data_rate(),
                airtime_us: 1_000,
            };
            handle_air_packet(&b.inner, &irq_tx, header.clone(), Vec::new());
            header.kind = KIND_FRAME;
            handle_air_packet(&b.inner, &irq_tx, header, b"x".to_vec());
        }
        assert_eq!(b.inner.lock().expect("lock").rx_queue.len(), MAX_RX_QUEUE);
    }

    #[test]
    fn broadcast_continues_after_one_peer_send_error() {
        let dir = AirDir::new("broadcast-error");
        fs::write(dir.path.join("bad.sock"), b"not a socket").expect("bad peer");
        let good_path = dir.path.join("good.sock");
        let good = UnixDatagram::bind(&good_path).expect("good peer");
        good.set_read_timeout(Some(Duration::from_millis(100)))
            .expect("timeout");
        let sender = UnixDatagram::unbound().expect("sender");
        broadcast(&sender, &dir.path, "sender", b"ping").expect("broadcast");
        let mut buf = [0u8; 8];
        let len = good.recv(&mut buf).expect("receive good peer");
        assert_eq!(&buf[..len], b"ping");
    }

    #[test]
    fn irq_fd_becomes_readable_when_frame_completes() {
        let dir = AirDir::new("irq");
        let mut a = radio(&dir, "a");
        let mut b = radio(&dir, "b");
        let fd = b.irq_fd().as_raw_fd();
        assert!(fd >= 0);
        a.transmit(b"irq", false).expect("tx");
        let deadline = Instant::now() + Duration::from_millis(250);
        let mut ready = false;
        while Instant::now() < deadline {
            if b.poll_rx().expect("poll").is_some() {
                ready = true;
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(ready);
    }
}
