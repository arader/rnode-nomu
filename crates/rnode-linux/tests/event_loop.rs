//! Event-loop tests: the poll timeout derived from `Node::next_wakeup_ms()`, shutdown (also in the
//! middle of a long TX burst), IRQ draining, a received frame travelling from a simulated radio's
//! IRQ to the host's PTY, and host packets travelling from the PTY to a peer on the simulated air.
//! Also the node settings the config file selects (interference avoidance, the TX power ceiling),
//! as a host sees them through the real `Node`, and a settings file it refuses.

mod common;

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use rnode_core::phy::{Modulation, PhyParams, airtime_ms, low_data_rate};
use rnode_core::{MemoryStorage, Node, NodeConfig, Radio, RadioSettings, RxFrame, Storage, TxOutcome};
use rnode_linux::config::Config;
use rnode_linux::event_loop::{FdIrqSource, NoIrq, POLL_TIMEOUT_CAP_MS, cap_timeout_ms, compute_timeout_ms, run};
use rnode_linux::pty::PtyPair;
use rnode_linux::storage::TomlStorage;
use rnode_sim::{SimConfig, SimRadio};
use serial_test::serial;

use common::test_dir;

/// Firmware `STATUS_INTERVAL_MS` (`Config.h`): an online RNode samples DCD/RSSI this often.
const FIRMWARE_STATUS_INTERVAL_MS: u16 = 3;

const SAVED_SETTINGS: RadioSettings = RadioSettings {
    frequency: 867_200_000,
    bandwidth: 125_000,
    spreading_factor: 7,
    coding_rate: 5,
    tx_power: 14,
};

/// The bytes Reticulum's `RNodeInterface.initRadio()` writes for 867.2 MHz, 125 kHz, 14 dBm,
/// SF7, CR 4/5 without airtime limits: `setFrequency()`, `setBandwidth()`, `setTXPower()`,
/// `setSpreadingFactor()`, `setCodingRate()`, then `setRadioState(RADIO_STATE_ON)`.
const RNODEINTERFACE_INIT_RADIO: &[u8] = &[
    0xC0, 0x01, 0x33, 0xB0, 0x6C, 0x00, 0xC0, // CMD_FREQUENCY 867_200_000
    0xC0, 0x02, 0x00, 0x01, 0xE8, 0x48, 0xC0, // CMD_BANDWIDTH 125_000
    0xC0, 0x03, 0x0E, 0xC0, // CMD_TXPOWER 14
    0xC0, 0x04, 0x07, 0xC0, // CMD_SF 7
    0xC0, 0x05, 0x05, 0xC0, // CMD_CR 5
    0xC0, 0x06, 0x01, 0xC0, // CMD_RADIO_STATE on
];
const RADIO_STATE_OFF: &[u8] = &[0xC0, 0x06, 0x00, 0xC0];
const RADIO_STATE_ON_REPLY: &[u8] = &[0xC0, 0x06, 0x01, 0xC0];

/// A radio that reads back whatever it was configured with and never receives. With `air` set, it
/// also records what it transmits on a simulated clock.
#[derive(Debug, Default)]
struct TestRadio {
    frequency: u32,
    bandwidth: u32,
    spreading_factor: u8,
    coding_rate: u8,
    tx_power: i8,
    air: Option<SimulatedAir>,
}

/// Transmissions that take simulated airtime instead of wall-clock time, so a test of a long TX
/// burst neither waits for it nor depends on the machine's speed. The daemon's shutdown flag is
/// raised as frame `stop_on_frame` (counted from 1) starts, as a SIGTERM arriving while that frame
/// is on the air would.
#[derive(Debug)]
struct SimulatedAir {
    log: Arc<Mutex<AirLog>>,
    shutdown: Arc<AtomicBool>,
    stop_on_frame: usize,
}

/// What a [`SimulatedAir`] radio transmitted.
#[derive(Debug, Default)]
struct AirLog {
    /// The length of each frame, in order.
    frames: Vec<usize>,
    /// Simulated time: the airtime of every frame so far, in ms.
    clock_ms: f32,
    /// The simulated time at which the shutdown flag was raised.
    stopped_at_ms: Option<f32>,
}

/// Airtime in ms of a `len`-byte frame (firmware `add_airtime()`).
fn frame_airtime_ms(len: usize, modulation: Modulation) -> f32 {
    let phy = PhyParams::new(modulation);
    let low_data_rate = low_data_rate(modulation.spreading_factor, modulation.bandwidth_hz);
    airtime_ms(len, modulation, low_data_rate, phy.preamble_symbols, phy.symbol_time_ms)
}

impl Radio for TestRadio {
    type Error = std::convert::Infallible;
    fn begin(&mut self, frequency_hz: u32) -> Result<(), Self::Error> {
        self.frequency = frequency_hz;
        Ok(())
    }
    fn end(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
    fn set_frequency(&mut self, hz: u32) -> Result<(), Self::Error> {
        self.frequency = hz;
        Ok(())
    }
    fn frequency(&self) -> u32 {
        self.frequency
    }
    fn set_bandwidth(&mut self, hz: u32) -> Result<(), Self::Error> {
        self.bandwidth = hz;
        Ok(())
    }
    fn bandwidth(&self) -> u32 {
        self.bandwidth
    }
    fn set_spreading_factor(&mut self, sf: u8) -> Result<(), Self::Error> {
        self.spreading_factor = sf;
        Ok(())
    }
    fn spreading_factor(&self) -> u8 {
        self.spreading_factor
    }
    fn set_coding_rate(&mut self, denominator: u8) -> Result<(), Self::Error> {
        self.coding_rate = denominator;
        Ok(())
    }
    fn coding_rate(&self) -> u8 {
        self.coding_rate
    }
    fn set_tx_power(&mut self, dbm: i8) -> Result<(), Self::Error> {
        self.tx_power = dbm;
        Ok(())
    }
    fn tx_power(&self) -> i8 {
        self.tx_power
    }
    fn set_preamble_length(&mut self, _: u16) -> Result<(), Self::Error> {
        Ok(())
    }
    fn set_crc(&mut self, _: bool) -> Result<(), Self::Error> {
        Ok(())
    }
    fn low_data_rate(&self) -> bool {
        false
    }
    fn receive(&mut self, _: Option<u8>) -> Result<(), Self::Error> {
        Ok(())
    }
    fn transmit(&mut self, frame: &[u8], _: bool) -> Result<TxOutcome, Self::Error> {
        if let Some(air) = &self.air {
            let modulation = Modulation {
                bandwidth_hz: self.bandwidth,
                spreading_factor: self.spreading_factor,
                coding_rate: self.coding_rate,
            };
            let mut log = air.log.lock().unwrap();
            if log.frames.len() + 1 == air.stop_on_frame {
                air.shutdown.store(true, Ordering::Relaxed);
                log.stopped_at_ms = Some(log.clock_ms);
            }
            log.frames.push(frame.len());
            log.clock_ms += frame_airtime_ms(frame.len(), modulation);
        }
        Ok(TxOutcome::Done)
    }
    fn poll_rx(&mut self) -> Result<Option<RxFrame>, Self::Error> {
        Ok(None)
    }
    fn dcd(&mut self, _: u64) -> Result<bool, Self::Error> {
        Ok(false)
    }
    fn current_rssi(&mut self) -> Result<i16, Self::Error> {
        Ok(-120)
    }
    fn set_dcd_timing(&mut self, _: u32, _: u32) {}
}

fn test_node(storage: MemoryStorage) -> Node<TestRadio, MemoryStorage> {
    Node::new(TestRadio::default(), storage, NodeConfig::default(), 1)
}

#[test]
fn event_loop_timeout_uses_node_wakeup_and_cap() {
    let node = test_node(MemoryStorage::default());
    assert!(!node.is_radio_online());
    // An offline node has nothing to do for much longer than the cap, so the loop still wakes at
    // the cap to notice shutdown and run the radio-service safety net.
    assert!(node.next_wakeup_ms(100) > 100 + POLL_TIMEOUT_CAP_MS);
    assert_eq!(compute_timeout_ms(&node, 100), POLL_TIMEOUT_CAP_MS as u16);

    assert_eq!(cap_timeout_ms(7), 7);
    assert_eq!(cap_timeout_ms(POLL_TIMEOUT_CAP_MS + 10), POLL_TIMEOUT_CAP_MS as u16);
    assert_eq!(cap_timeout_ms(u64::MAX), POLL_TIMEOUT_CAP_MS as u16);
}

#[test]
fn event_loop_timeout_follows_status_interval_once_host_starts_radio() {
    let mut node = test_node(MemoryStorage::default());
    node.handle_host_input(RNODEINTERFACE_INIT_RADIO, 100);
    assert!(node.is_radio_online());
    let replies = node.take_host_output();
    assert!(replies.ends_with(RADIO_STATE_ON_REPLY), "{replies:02X?}");

    // The first status sample is due immediately after start-up.
    assert_eq!(compute_timeout_ms(&node, 100), 0);
    node.tick(100);
    assert_eq!(compute_timeout_ms(&node, 100), FIRMWARE_STATUS_INTERVAL_MS);
    assert_eq!(compute_timeout_ms(&node, 101), FIRMWARE_STATUS_INTERVAL_MS - 1);
    assert_eq!(compute_timeout_ms(&node, 103), 0);
    assert_eq!(compute_timeout_ms(&node, 160), 0, "an overdue tick must not wait");
    node.tick(160);
    assert_eq!(compute_timeout_ms(&node, 160), FIRMWARE_STATUS_INTERVAL_MS);

    node.handle_host_input(RADIO_STATE_OFF, 161);
    assert!(!node.is_radio_online());
    assert_eq!(compute_timeout_ms(&node, 161), POLL_TIMEOUT_CAP_MS as u16);
}

#[test]
fn event_loop_timeout_follows_status_interval_after_boot_with_saved_settings() {
    let mut node = test_node(MemoryStorage::with_settings(SAVED_SETTINGS));
    assert_eq!(compute_timeout_ms(&node, 0), POLL_TIMEOUT_CAP_MS as u16);
    // Firmware `validate_status()`: saved settings → `eeprom_conf_load()` and `startRadio()`.
    node.boot(0);
    assert!(node.is_radio_online());
    assert_eq!(compute_timeout_ms(&node, 0), 0);
    node.tick(0);
    assert_eq!(compute_timeout_ms(&node, 0), FIRMWARE_STATUS_INTERVAL_MS);
}

#[test]
fn avoid_interference_starts_from_config_and_host_can_switch_it() {
    let cfg = Config::from_toml("[radio]\ndriver = \"sim\"\navoid_interference = true\n").unwrap();
    let mut node = Node::new(TestRadio::default(), MemoryStorage::default(), cfg.node_config(), 1);
    assert!(node.avoid_interference());
    // What rnodeconf's `set_disable_interference_avoidance()` sends for --ia-disable/--ia-enable.
    node.handle_host_input(&[0xC0, 0x69, 0x01, 0xC0], 0);
    assert!(!node.avoid_interference());
    node.handle_host_input(&[0xC0, 0x69, 0x00, 0xC0], 1);
    assert!(node.avoid_interference());
}

/// `CMD_TXPOWER` with one byte, as `RNodeInterface.setTXPower()` sends it and firmware
/// `kiss_indicate_txpower()` answers; `0xFF` asks for the current power.
fn txpower_frame(dbm: u8) -> [u8; 4] {
    [0xC0, 0x03, dbm, 0xC0]
}

/// The TX power ceiling a config selects, as a host sees it through the real `Node`. The test radio
/// applies any power it is given, so every clamp here is the node's.
#[test]
fn tx_power_ceiling_from_config_clamps_and_echoes_host_requests() {
    let cfg = Config::from_toml("[radio]\ndriver = \"sim\"\nmax_tx_power_dbm = 10\n").unwrap();
    let mut node = Node::new(TestRadio::default(), MemoryStorage::default(), cfg.node_config(), 1);

    // RNodeInterface asks for 14 dBm while the radio is off. Like firmware `kiss_indicate_txpower()`
    // after its clamp, the node answers with the power it will use, so Reticulum reports a TX power
    // mismatch, as with an RNode asked for more than its hardware can do.
    node.handle_host_input(RNODEINTERFACE_INIT_RADIO, 0);
    let replies = node.take_host_output();
    assert!(
        replies.windows(4).any(|frame| frame == txpower_frame(10)),
        "{replies:02X?}"
    );
    assert!(
        !replies.windows(4).any(|frame| frame == txpower_frame(14)),
        "{replies:02X?}"
    );
    assert!(replies.ends_with(RADIO_STATE_ON_REPLY), "{replies:02X?}");
    assert_eq!(node.radio().tx_power(), 10, "the radio started above the ceiling");

    // With the radio on, requests above the ceiling set the ceiling.
    for dbm in [11, 14, 22, 23, 0x7F] {
        node.handle_host_input(&txpower_frame(dbm), 1);
        assert_eq!(node.take_host_output(), txpower_frame(10), "{dbm} dBm");
        assert_eq!(node.radio().tx_power(), 10, "{dbm} dBm");
    }
    // Requests up to the ceiling are applied unchanged, and a query reports the power in use.
    for dbm in [10, 2, 0] {
        node.handle_host_input(&txpower_frame(dbm), 2);
        assert_eq!(node.take_host_output(), txpower_frame(dbm), "{dbm} dBm");
        assert_eq!(node.radio().tx_power(), dbm as i8, "{dbm} dBm");
    }
    node.handle_host_input(&txpower_frame(0xFF), 3);
    assert_eq!(node.take_host_output(), txpower_frame(0));
}

#[test]
fn tx_power_ceiling_from_config_applies_to_saved_settings() {
    let cfg = Config::from_toml("[radio]\ndriver = \"sim\"\nmax_tx_power_dbm = 10\n").unwrap();
    let saved = RadioSettings {
        tx_power: 22,
        ..SAVED_SETTINGS
    };
    let mut node = Node::new(
        TestRadio::default(),
        MemoryStorage::with_settings(saved),
        cfg.node_config(),
        1,
    );
    // Settings saved before the ceiling was lowered bring the radio up at the ceiling.
    node.boot(0);
    assert!(node.is_radio_online());
    assert_eq!(node.radio().tx_power(), 10);
    node.take_host_output();
    node.handle_host_input(&txpower_frame(0xFF), 1);
    assert_eq!(node.take_host_output(), txpower_frame(10));
}

/// #75: a settings file that an earlier build saved for −9 dBm holds `tx_power = 247`. Read as
/// unsigned that would be the most power allowed, so the node ignores the file and waits for a
/// host; the host's next `CMD_CONF_SAVE` replaces it.
#[test]
fn settings_file_with_a_negative_power_from_an_earlier_build_is_ignored() {
    let dir = test_dir("negative-saved-power");
    let path = dir.join("radio_settings.toml");
    // TomlStorage is unchanged: this is the file an earlier build saved after a host set -9 dBm.
    let negative = RadioSettings {
        tx_power: (-9_i8) as u8,
        ..SAVED_SETTINGS
    };
    TomlStorage::new(Some(path.clone())).save(&negative).unwrap();
    let earlier_build =
        "frequency = 867200000\nbandwidth = 125000\nspreading_factor = 7\ncoding_rate = 5\ntx_power = 247\n";
    assert_eq!(std::fs::read_to_string(&path).unwrap(), earlier_build);
    let cfg = Config::from_toml("[radio]\ndriver = \"sim\"\n").unwrap();

    let mut node = Node::new(
        TestRadio::default(),
        TomlStorage::new(Some(path.clone())),
        cfg.node_config(),
        1,
    );
    node.boot(0);
    assert!(!node.is_radio_online());
    assert_eq!((node.radio().frequency(), node.radio().tx_power()), (0, 0));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), earlier_build);

    node.handle_host_input(RNODEINTERFACE_INIT_RADIO, 1);
    assert!(node.is_radio_online());
    node.handle_host_input(&[0xC0, 0x53, 0x00, 0xC0], 2); // CMD_CONF_SAVE
    assert_eq!(
        TomlStorage::new(Some(path.clone())).load().unwrap(),
        Some(SAVED_SETTINGS)
    );
    let mut restarted = Node::new(TestRadio::default(), TomlStorage::new(Some(path)), cfg.node_config(), 2);
    restarted.boot(0);
    assert!(restarted.is_radio_online());
    assert_eq!(restarted.radio().tx_power(), 14);
}

#[test]
#[serial]
fn event_loop_run_exits_when_shutdown_flag_is_set() {
    let dir = test_dir("event-loop");
    let link = dir.join("rnode0");
    let pty = PtyPair::open(Some(&link)).unwrap();
    let node = Node::new(TestRadio::default(), MemoryStorage::default(), NodeConfig::default(), 2);
    let shutdown = Arc::new(AtomicBool::new(false));
    let thread_shutdown = Arc::clone(&shutdown);
    let handle = std::thread::spawn(move || run(node, NoIrq, pty, &thread_shutdown));
    std::thread::sleep(Duration::from_millis(25));
    shutdown.store(true, Ordering::Relaxed);
    handle.join().unwrap().unwrap();
    assert!(!link.exists());
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct PipeIrq {
    read_fd: std::os::fd::OwnedFd,
    drained: Arc<AtomicBool>,
}

#[cfg(target_os = "linux")]
impl rnode_linux::event_loop::IrqSource for PipeIrq {
    fn fd(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        Some(self.read_fd.as_fd())
    }

    fn drain(&mut self) -> std::io::Result<()> {
        let mut byte = [0_u8; 1];
        let _ = nix::unistd::read(&self.read_fd, &mut byte).map_err(std::io::Error::other)?;
        self.drained.store(true, Ordering::Relaxed);
        Ok(())
    }
}

#[cfg(target_os = "linux")]
#[test]
#[serial]
fn event_loop_run_drains_readable_irq() {
    let dir = test_dir("event-loop-irq");
    let pty = PtyPair::open(Some(&dir.join("rnode0"))).unwrap();
    let node = Node::new(TestRadio::default(), MemoryStorage::default(), NodeConfig::default(), 3);
    let (read_fd, write_fd) = nix::unistd::pipe().unwrap();
    nix::unistd::write(&write_fd, &[1]).unwrap();
    drop(write_fd);
    let drained = Arc::new(AtomicBool::new(false));
    let irq = PipeIrq {
        read_fd,
        drained: Arc::clone(&drained),
    };
    let shutdown = Arc::new(AtomicBool::new(false));
    let thread_shutdown = Arc::clone(&shutdown);
    let handle = std::thread::spawn(move || run(node, irq, pty, &thread_shutdown));
    for _ in 0..50 {
        if drained.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    shutdown.store(true, Ordering::Relaxed);
    handle.join().unwrap().unwrap();
    assert!(drained.load(Ordering::Relaxed));
}

/// A private simulated-air directory. The path stays relative (to the crate directory, the
/// working directory of integration tests) because Unix socket paths are limited to ~104 bytes.
struct AirDir {
    path: PathBuf,
}

impl AirDir {
    fn new(name: &str) -> Self {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        Self {
            path: PathBuf::from(format!("target/air-{name}-{}-{nonce}", std::process::id())),
        }
    }
}

impl Drop for AirDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Read from the host end of the PTY until `seen` contains `needle`; false on timeout.
fn read_until(host: &mut File, seen: &mut Vec<u8>, needle: &[u8], timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while !seen.windows(needle.len()).any(|window| window == needle) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        let mut fds = [PollFd::new(host.as_fd(), PollFlags::POLLIN)];
        let wait = PollTimeout::from(u16::try_from(left.as_millis()).unwrap_or(u16::MAX));
        if poll(&mut fds, wait).unwrap() == 0 {
            continue;
        }
        let mut buf = [0_u8; 1024];
        let n = host.read(&mut buf).unwrap();
        seen.extend_from_slice(&buf[..n]);
    }
    true
}

/// Sets the shutdown flag when dropped, so a failing assertion still stops the daemon thread.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Issue #2 acceptance: a frame received by the simulated radio raises its IRQ fd, which the event
/// loop services, and the host reads the KISS frames RNode_Firmware's `receive_callback()` sends.
#[test]
#[serial]
fn event_loop_delivers_simulated_rx_irq_to_host_as_kiss_frames() {
    let air = AirDir::new("rx");
    let mut daemon_radio = SimConfig::new(&air.path, "daemon");
    daemon_radio.rssi_dbm = -70;
    daemon_radio.snr_db = 7.0;
    let radio = SimRadio::new(daemon_radio).unwrap();
    let irq = FdIrqSource::new(radio.irq_fd().try_clone_to_owned().unwrap());
    let node = Node::new(
        radio,
        MemoryStorage::with_settings(SAVED_SETTINGS),
        NodeConfig::default(),
        4,
    );
    let pty = PtyPair::open(None).unwrap();
    let mut host = OpenOptions::new()
        .read(true)
        .write(true)
        .open(pty.slave_path())
        .unwrap();
    let shutdown = Arc::new(AtomicBool::new(false));
    let thread_shutdown = Arc::clone(&shutdown);
    let daemon = std::thread::spawn(move || run(node, irq, pty, &thread_shutdown));
    let stop = StopOnDrop(Arc::clone(&shutdown));

    // Boot starts the radio from the saved settings and reports it once it is receiving.
    let mut seen = Vec::new();
    assert!(
        read_until(&mut host, &mut seen, RADIO_STATE_ON_REPLY, Duration::from_secs(5)),
        "radio never reported online; host read {seen:02X?}"
    );

    let mut peer = SimRadio::new(SimConfig::new(&air.path, "peer")).unwrap();
    peer.begin(SAVED_SETTINGS.frequency).unwrap();
    peer.set_bandwidth(SAVED_SETTINGS.bandwidth).unwrap();
    peer.set_spreading_factor(SAVED_SETTINGS.spreading_factor).unwrap();
    peer.set_coding_rate(SAVED_SETTINGS.coding_rate).unwrap();
    // A single-frame packet as firmware `transmit()` sends it: sequence in the high nibble of the
    // header byte, FLAG_SPLIT clear, then the payload.
    let sent = peer.transmit(&[0x50, b'h', b'e', b'l', b'l', b'o'], false).unwrap();
    assert_eq!(sent, TxOutcome::Done);

    // `kiss_indicate_stat_rssi()` (-70 dBm + RSSI_OFFSET 157 = 0x57), `kiss_indicate_stat_snr()`
    // (7.0 dB × 4 = 0x1C), then `kiss_write_packet()` with the payload.
    let expected = [
        0xC0, 0x23, 0x57, 0xC0, 0xC0, 0x24, 0x1C, 0xC0, 0xC0, 0x00, b'h', b'e', b'l', b'l', b'o', 0xC0,
    ];
    assert!(
        read_until(&mut host, &mut seen, &expected, Duration::from_secs(5)),
        "received frame never reached the host; host read {seen:02X?}"
    );

    drop(stop);
    daemon.join().unwrap().unwrap();
}

/// The bytes Reticulum's `RNodeInterface.process_outgoing()` writes for one packet: the payload
/// passed through `KISS.escape()` (`DB` → `DB DD`, `C0` → `DB DC`) inside `C0 00 … C0`.
fn rnodeinterface_data_frame(payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![0xC0, 0x00];
    for &byte in payload {
        match byte {
            0xDB => frame.extend_from_slice(&[0xDB, 0xDD]),
            0xC0 => frame.extend_from_slice(&[0xDB, 0xDC]),
            _ => frame.push(byte),
        }
    }
    frame.push(0xC0);
    frame
}

/// The frames a simulated radio receives until it has `count` of them or `timeout` expires.
fn receive_frames(radio: &mut SimRadio, count: usize, timeout: Duration) -> Vec<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    let mut frames = Vec::new();
    loop {
        while let Some(frame) = radio.poll_rx().unwrap() {
            frames.push(frame.data);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if frames.len() >= count || left.is_zero() {
            return frames;
        }
        let mut fds = [PollFd::new(radio.irq_fd(), PollFlags::POLLIN)];
        let wait = u16::try_from(left.as_millis()).unwrap_or(u16::MAX).min(100);
        poll(&mut fds, PollTimeout::from(wait)).unwrap();
    }
}

/// The host-to-radio path through the real `run()`: the host starts the radio and sends packets
/// as Reticulum's RNodeInterface does, and a peer on the same simulated air must receive exactly
/// the LoRa frames firmware `transmit()` sends. A packet of up to 254 bytes goes out as one frame:
/// a header byte (`random(256) & 0xF0`, so FLAG_SPLIT clear) and the payload. A longer one goes out
/// as two frames with the same header, FLAG_SPLIT (0x01) set: 255 bytes, then the remainder.
#[test]
#[serial]
fn event_loop_transmits_host_kiss_frames_on_simulated_air() {
    let air = AirDir::new("tx");
    let radio = SimRadio::new(SimConfig::new(&air.path, "daemon")).unwrap();
    let irq = FdIrqSource::new(radio.irq_fd().try_clone_to_owned().unwrap());
    let node = Node::new(radio, MemoryStorage::default(), NodeConfig::default(), 5);
    let pty = PtyPair::open(None).unwrap();
    let mut host = OpenOptions::new()
        .read(true)
        .write(true)
        .open(pty.slave_path())
        .unwrap();
    let shutdown = Arc::new(AtomicBool::new(false));
    let thread_shutdown = Arc::clone(&shutdown);
    let daemon = std::thread::spawn(move || run(node, irq, pty, &thread_shutdown));
    let stop = StopOnDrop(Arc::clone(&shutdown));

    // A peer listening with the settings RNODEINTERFACE_INIT_RADIO selects.
    let mut peer = SimRadio::new(SimConfig::new(&air.path, "peer")).unwrap();
    peer.begin(867_200_000).unwrap();
    peer.set_bandwidth(125_000).unwrap();
    peer.set_spreading_factor(7).unwrap();
    peer.set_coding_rate(5).unwrap();
    peer.receive(None).unwrap();

    host.write_all(RNODEINTERFACE_INIT_RADIO).unwrap();
    let mut seen = Vec::new();
    assert!(
        read_until(&mut host, &mut seen, RADIO_STATE_ON_REPLY, Duration::from_secs(5)),
        "radio never reported online; host read {seen:02X?}"
    );

    // The largest single-frame payload and a split one. Both contain FEND (0xC0) and FESC (0xDB),
    // so the host escapes them and the node must unescape them.
    let single: Vec<u8> = (0..254_u32).map(|i| i as u8).collect();
    let split: Vec<u8> = (0..300_u32).map(|i| (i * 7) as u8).collect();
    host.write_all(&rnodeinterface_data_frame(&single)).unwrap();
    host.write_all(&rnodeinterface_data_frame(&split)).unwrap();

    let frames = receive_frames(&mut peer, 3, Duration::from_secs(10));
    drop(stop);
    daemon.join().unwrap().unwrap();
    let summary: Vec<(usize, u8)> = frames.iter().map(|frame| (frame.len(), frame[0])).collect();
    assert_eq!(frames.len(), 3, "peer received (length, header) {summary:02X?}");

    assert_eq!(frames[0].len(), 255);
    assert_eq!(frames[0][0] & 0x0F, 0x00, "single-frame header {:02X}", frames[0][0]);
    assert_eq!(frames[0][1..], single[..]);

    assert_eq!((frames[1].len(), frames[2].len()), (255, 47));
    assert_eq!(frames[1][0] & 0x0F, 0x01, "split header {:02X}", frames[1][0]);
    assert_eq!(frames[2][0], frames[1][0], "both halves carry the same header");
    assert_eq!(frames[1][1..], split[..254]);
    assert_eq!(frames[2][1..], split[254..]);
}

/// The settings of the issue #51 report: SF12 at 125 kHz, where a split packet takes about 12 s.
const SF12_SETTINGS: RadioSettings = RadioSettings {
    spreading_factor: 12,
    ..SAVED_SETTINGS
};

/// Issue #51: SIGTERM during a long TX burst. The host queues eight split packets at SF12, and the
/// shutdown flag goes up as the first frame of the third packet starts. `run()` must finish that
/// packet, both of its frames, and return without starting the fourth: one packet's airtime after
/// the signal, not the six packets (over a minute) a firmware `flush_queue()` would still send.
/// Airtime is simulated, so the test takes as long as channel access (DIFS and the contention
/// window, under 2 s at SF12), whatever the burst.
#[test]
#[serial]
fn event_loop_ends_a_tx_burst_one_packet_after_shutdown_is_requested() {
    const PACKETS: usize = 8;
    const STOP_DURING_PACKET: usize = 3;
    let shutdown = Arc::new(AtomicBool::new(false));
    let log = Arc::new(Mutex::new(AirLog::default()));
    let radio = TestRadio {
        air: Some(SimulatedAir {
            log: Arc::clone(&log),
            shutdown: Arc::clone(&shutdown),
            stop_on_frame: 2 * STOP_DURING_PACKET - 1,
        }),
        ..TestRadio::default()
    };
    let node = Node::new(
        radio,
        MemoryStorage::with_settings(SF12_SETTINGS),
        NodeConfig::default(),
        6,
    );
    let pty = PtyPair::open(None).unwrap();
    let mut host = OpenOptions::new()
        .read(true)
        .write(true)
        .open(pty.slave_path())
        .unwrap();
    let (result_tx, result_rx) = mpsc::channel();
    let thread_shutdown = Arc::clone(&shutdown);
    let daemon = std::thread::spawn(move || result_tx.send(run(node, NoIrq, pty, &thread_shutdown)).unwrap());
    let stop = StopOnDrop(Arc::clone(&shutdown));

    // All packets reach the queue long before the first transmission: channel access waits at
    // least DIFS (200 ms at SF12) after the first one.
    let packet = [0x55_u8; 300];
    host.write_all(&rnodeinterface_data_frame(&packet).repeat(PACKETS))
        .unwrap();
    let result = result_rx
        .recv_timeout(Duration::from_secs(60))
        .expect("run() did not return after the shutdown flag was raised");
    result.unwrap();
    drop(stop);
    daemon.join().unwrap();

    let log = log.lock().unwrap();
    let frames = [255_usize, 47];
    assert_eq!(
        log.frames,
        frames.repeat(STOP_DURING_PACKET),
        "frame lengths on the air: the packet in flight is finished, the next is not started"
    );
    let modulation = Modulation {
        bandwidth_hz: SF12_SETTINGS.bandwidth,
        spreading_factor: SF12_SETTINGS.spreading_factor,
        coding_rate: SF12_SETTINGS.coding_rate,
    };
    let packet_ms: f32 = frames.iter().map(|&len| frame_airtime_ms(len, modulation)).sum();
    assert!(packet_ms > 10_000.0, "a long packet: {packet_ms} ms");
    let after_stop_ms = log.clock_ms - log.stopped_at_ms.expect("the shutdown flag was raised");
    assert!(
        (after_stop_ms - packet_ms).abs() < 1.0,
        "airtime between the signal and the exit: {after_stop_ms} ms, one packet: {packet_ms} ms"
    );
}
