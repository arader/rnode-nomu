//! The RNode state machine.
//!
//! [`Node`] is the hardware-independent equivalent of the firmware's main loop: it consumes
//! bytes from the host, drives a [`Radio`], and produces bytes for the host. The platform layer
//! (e.g. the Linux daemon) owns the I/O and calls into the node:
//!
//! ```text
//! boot(now)                         once at start-up
//! loop {
//!     wait for host input, the radio IRQ line, or next_wakeup_ms()
//!     handle_host_input(bytes, now) when the host sent data
//!     service_radio(now)            when the radio IRQ fired (and periodically as a safety net)
//!     tick_with(now, should_stop)   always; should_stop() reports a pending shutdown request
//!     write take_host_output() to the host
//! }
//! shutdown()                        drops the packets still queued and puts the radio to sleep
//! ```
//!
//! Observable behaviour follows RNode_Firmware v1.86's TNC path: `serial_callback()`,
//! `receive_callback()`, `transmit()`, `startRadio()`/`stopRadio()`, `update_radio_lock()` and
//! the `kiss_indicate_*()` encoders. Deliberate differences:
//!
//! - host config commands retune the radio immediately while online;
//! - TX/RX stats are incremented;
//! - exactly-508-byte TX packets do not send the firmware's redundant header-only third LoRa
//!   frame;
//! - oversized promiscuous-mode raw frames are rejected/dropped instead of being truncated to one
//!   LoRa frame;
//! - host-stream safety replies with arbitrary bytes (`CMD_RANDOM`, `CMD_IMPLICIT`) KISS-escape
//!   their payloads;
//! - implicit-header receive is only enabled in promiscuous mode, so normal split RNode frames
//!   keep explicit LoRa headers;
//! - a TX burst can be cut short. The firmware's `flush_queue()` sends every queued packet back
//!   to back and never stops; each [`Radio::transmit`] blocks for the frame's airtime, so a full
//!   queue at SF11/SF12 keeps one tick busy for minutes. [`Node::tick_with`] asks the platform
//!   before each packet whether to stop, so the platform can exit within one packet's airtime of
//!   a shutdown request, and [`Node::shutdown`] drops the packets left in the queue. The check is
//!   made between packets only, never between the two frames of a split packet: a receiver keeps
//!   a lone first half, and `receive_callback()` would join it to the next split frame that
//!   carries the same 4-bit sequence number (1 in 16), corrupting that packet;
//! - `CMD_DIS_IA` changes interference avoidance for the running process only. The firmware's
//!   `dia_conf_save()` writes the setting to EEPROM and reboots; we neither write anything nor
//!   reset. [`NodeConfig::avoid_interference`] (the daemon's config file) is the persistent source
//!   of truth, and the value set by the host survives `CMD_RESET` the way the EEPROM byte
//!   survives the firmware's reboot. Like the firmware, the command sends no reply.
//!
//! Logging (`log` facade): at debug level, one line per packet delivered to the host (after
//! split reassembly), one per host packet sent to the radio (before splitting, with its outcome)
//! and one per dropped packet with the reason; at trace level, a hex dump of each delivered or
//! sent payload. Packet lines come from this module; drops are logged where they are detected (the
//! KISS decoder, the TX queue, the split reassembler and this module). Radio driver errors and
//! modem TX timeouts, which hard-reset the node, are also logged at warn level. At info level, one
//! line reports each time the radio goes online or offline, with why and the settings it runs
//! with; one reports the new settings when the host retunes the online radio, once per burst of
//! config commands; and one counts the packets still queued at shutdown. Errors of the radio calls
//! made every few milliseconds (DCD and RSSI reads, receive polling) are rate-limited: a warning
//! for the first, then at most one summary per 10 s while the call keeps failing, and an info line
//! once it has worked for 10 s. Formatting is lazy, so disabled levels cost nothing, and logging
//! never changes what the node sends to the host or the radio.

use core::fmt;

use crate::RSSI_OFFSET;
use crate::csma::{Access, Csma, StatusUpdate};
use crate::fault_log::FaultLog;
use crate::framing::{self, Reassembly, RxReassembler};
use crate::identity::DeviceIdentity;
use crate::kiss;
use crate::packet_log::{HexDump, PacketSummary, RadioSummary, plural};
use crate::phy::{self, Modulation, PhyParams};
use crate::queue::TxQueue;
use crate::radio::{Radio, RxFrame, TxOutcome};
use crate::rng::Rng;
use crate::storage::{RadioSettings, Storage};

/// Firmware `STATUS_INTERVAL_MS`: how often the modem status (DCD, RSSI) is sampled.
pub const STATUS_INTERVAL_MS: u64 = 3;
const OFFLINE_WAKEUP_MS: u64 = 1_000;
/// How long after the host first changes the settings of the online radio the change is logged.
/// The config commands of one burst (RNodeInterface sends them back to back) give one line, and
/// the last line logged is never stale for longer than this.
const RETUNE_LOG_DELAY_MS: u64 = 100;
const MIN_TX_POWER_DBM: i8 = -9;

/// Static configuration of a [`Node`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeConfig {
    /// What the node reports to the host.
    pub identity: DeviceIdentity,
    /// Highest TX power in dBm the host may request; higher requests are clamped
    /// (firmware: 22 for an SX1262 without external PA).
    pub max_tx_power: u8,
    /// Maximum number of packets in the TX queue (firmware `CONFIG_QUEUE_MAX_LENGTH`).
    pub queue_max_packets: usize,
    /// Maximum number of bytes in the TX queue (firmware `CONFIG_QUEUE_SIZE`).
    pub queue_max_bytes: usize,
    /// Whether interference avoidance is on at start-up (firmware `avoid_interference`). When it
    /// is on, the channel counts as busy while the RSSI is more than
    /// [`crate::csma::CSMA_INFR_THRESHOLD_DB`] above the noise floor without a LoRa carrier
    /// (firmware `medium_free()`). That includes the first
    /// [`crate::csma::NOISE_FLOOR_SAMPLES`] status samples with the radio on (about 384 ms) after
    /// start-up or a reset, because the noise floor starts at a -292 dBm sentinel.
    ///
    /// The default, `false`, is the firmware's effective default: `setup()` turns avoidance on
    /// only when the EEPROM byte `ADDR_CONF_DIA` is `0x00`, so erased EEPROM (`0xFF`) leaves it off
    /// and only `rnodeconf --ia-enable` turns it on. The host can change the running value with
    /// `CMD_DIS_IA` (see [`Node::avoid_interference`]); this field stays the start-up default.
    pub avoid_interference: bool,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            identity: DeviceIdentity::default(),
            max_tx_power: 22,
            queue_max_packets: 200,
            queue_max_bytes: 6144,
            avoid_interference: false,
        }
    }
}

/// A fresh channel-access state with interference avoidance set as given.
fn new_csma(avoid_interference: bool) -> Csma {
    let mut csma = Csma::new();
    csma.set_avoid_interference(avoid_interference);
    csma
}

/// Log a host packet handed to the radio: a debug line with its outcome, then a trace hex dump.
fn log_tx(summary: PacketSummary, payload: &[u8], outcome: fmt::Arguments<'_>) {
    log::debug!("TX packet: {summary}: {outcome}");
    if log::log_enabled!(log::Level::Trace) {
        log::trace!("TX payload, {} bytes:{}", payload.len(), HexDump(payload));
    }
}

fn clamp_tx_power_byte(raw: u8, max_tx_power: u8) -> u8 {
    let max_dbm = (max_tx_power.min(i8::MAX as u8)) as i8;
    (raw as i8).clamp(MIN_TX_POWER_DBM, max_dbm) as u8
}

/// An RNode: host protocol, TX queue, framing and channel access around a [`Radio`].
#[derive(Debug)]
pub struct Node<R: Radio, S: Storage> {
    radio: R,
    storage: S,
    config: NodeConfig,
    seed: u64,
    rng: Rng,
    radio_online: bool,
    host_output: Vec<u8>,
    decoder: kiss::Decoder,
    queue: TxQueue,
    reassembler: RxReassembler,
    csma: Csma,
    avoid_interference: bool,
    freq: u32,
    bw: u32,
    txp: u8,
    sf: u8,
    cr: u8,
    implicit_len: u8,
    promisc: bool,
    last_rssi: i16,
    last_snr_raw: u8,
    stat_rx: u32,
    stat_tx: u32,
    last_status_ms: Option<u64>,
    phy: Option<PhyParams>,
    dcd_faults: FaultLog,
    rssi_faults: FaultLog,
    rx_faults: FaultLog,
    /// The settings in the last online or settings-changed line.
    logged_settings: Option<RadioSummary>,
    /// When the host started retuning the online radio, if that is not logged yet.
    retune_pending_since: Option<u64>,
}

impl<R: Radio, S: Storage> Node<R, S> {
    /// Create a node. `seed` initialises the node's pseudo-random generator (CSMA contention
    /// window, packet sequence numbers); pass real entropy outside of tests.
    pub fn new(radio: R, storage: S, config: NodeConfig, seed: u64) -> Self {
        let queue = TxQueue::new(config.queue_max_packets, config.queue_max_bytes);
        let avoid_interference = config.avoid_interference;
        Self {
            radio,
            storage,
            config,
            seed,
            rng: Rng::new(seed),
            radio_online: false,
            host_output: Vec::new(),
            decoder: kiss::Decoder::new(),
            queue,
            reassembler: RxReassembler::new(),
            csma: new_csma(avoid_interference),
            avoid_interference,
            freq: 0,
            bw: 0,
            txp: 0xFF,
            sf: 0,
            cr: 5,
            implicit_len: 0,
            promisc: false,
            last_rssi: -292,
            last_snr_raw: 0x80,
            stat_rx: 0,
            stat_tx: 0,
            last_status_ms: None,
            phy: None,
            dcd_faults: FaultLog::new("radio DCD"),
            rssi_faults: FaultLog::new("radio RSSI"),
            rx_faults: FaultLog::new("radio RX service"),
            logged_settings: None,
            retune_pending_since: None,
        }
    }

    /// Start-up: load saved radio settings and, if they are complete, bring the radio online.
    pub fn boot(&mut self, now_ms: u64) {
        match self.storage.load() {
            Ok(Some(settings)) => {
                self.apply_settings(settings);
                if !self.radio_locked() {
                    self.start_radio(now_ms, "saved settings");
                }
            }
            Ok(None) => {}
            Err(e) => log::warn!("failed to load saved radio settings: {e}"),
        }
    }

    /// Process bytes received from the host (firmware `serial_callback()` for each byte).
    pub fn handle_host_input(&mut self, bytes: &[u8], now_ms: u64) {
        let mut events = Vec::new();
        for &b in bytes {
            self.decoder.push(b, &mut events);
            for event in events.drain(..) {
                self.handle_event(event, now_ms);
            }
        }
    }

    /// Service received radio frames and deliver complete host packets preceded by RSSI/SNR.
    pub fn service_radio(&mut self, now_ms: u64) {
        if !self.radio_online {
            return;
        }
        loop {
            match self.radio.poll_rx() {
                Ok(frame) => {
                    self.rx_faults.succeeded(now_ms);
                    match frame {
                        Some(frame) => self.handle_rx_frame(frame),
                        None => break,
                    }
                }
                Err(e) => {
                    self.rx_faults.failed(now_ms, &e);
                    break;
                }
            }
        }
    }

    /// Periodic modem status, CSMA and TX queue work: [`Node::tick_with`] that never stops a TX
    /// burst early.
    pub fn tick(&mut self, now_ms: u64) {
        self.tick_with(now_ms, || false);
    }

    /// Periodic modem status, CSMA and TX queue work. `should_stop` is asked before each packet
    /// of a TX burst; once it returns true the burst ends, and the packets not yet sent stay
    /// queued for a later tick or for [`Node::shutdown`] to drop. A packet already on the air,
    /// including the second frame of a split packet, is always finished (see the module docs).
    ///
    /// Pass the platform's shutdown request, so that a burst of long transmissions cannot delay
    /// exit by more than one packet's airtime.
    pub fn tick_with(&mut self, now_ms: u64, mut should_stop: impl FnMut() -> bool) {
        if !self.radio_online {
            return;
        }
        self.log_retune(now_ms);
        if self
            .last_status_ms
            .is_none_or(|last| now_ms.saturating_sub(last) >= STATUS_INTERVAL_MS)
        {
            self.last_status_ms = Some(now_ms);
            if let Some(update) = self.observe_status(now_ms).map(|()| self.csma.status_tick(now_ms)) {
                self.emit_status_update(update);
            }
        }
        self.drain_tx_queue(now_ms, &mut should_stop);
    }

    /// Take the bytes queued for the host (KISS frames); the caller writes them out.
    pub fn take_host_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.host_output)
    }

    /// The monotonic time in ms at which [`Node::tick`] next needs to run.
    pub fn next_wakeup_ms(&self, now_ms: u64) -> u64 {
        if self.radio_online {
            self.last_status_ms.map_or(now_ms, |last| last + STATUS_INTERVAL_MS)
        } else {
            now_ms + OFFLINE_WAKEUP_MS
        }
    }

    /// Whether the radio is online (configured and receiving).
    pub fn is_radio_online(&self) -> bool {
        self.radio_online
    }

    /// Drop the packets still queued for TX (a debug line each) and take the radio offline before
    /// the process exits.
    pub fn shutdown(&mut self) {
        let queued = self.queue.len();
        if queued > 0 {
            log::info!(
                "shutting down with {queued} TX packet{} queued; dropping {}",
                plural(queued),
                if queued == 1 { "it" } else { "them" }
            );
            while let Some(packet) = self.queue.pop() {
                log::debug!("dropping {}-byte TX packet: shutting down", packet.len());
            }
        }
        if self.radio_online {
            self.stop_radio("shutdown");
        }
    }

    /// The node's static configuration.
    pub fn config(&self) -> &NodeConfig {
        &self.config
    }
    /// Whether interference avoidance is on now: [`NodeConfig::avoid_interference`] until the
    /// host changes it with `CMD_DIS_IA`. The host's value lasts until the process exits,
    /// including across `CMD_RESET`.
    pub fn avoid_interference(&self) -> bool {
        self.avoid_interference
    }
    /// The radio driven by this node.
    pub fn radio(&self) -> &R {
        &self.radio
    }
    /// Mutable access to the radio driven by this node.
    pub fn radio_mut(&mut self) -> &mut R {
        &mut self.radio
    }
    /// The settings storage used by this node.
    pub fn storage(&self) -> &S {
        &self.storage
    }
    /// Mutable access to the settings storage used by this node.
    pub fn storage_mut(&mut self) -> &mut S {
        &mut self.storage
    }
    /// The seed the node's pseudo-random generator was created with.
    pub fn seed(&self) -> u64 {
        self.seed
    }
    /// Number of packets waiting for TX.
    pub fn queued_packets(&self) -> usize {
        self.queue.len()
    }
    /// Bytes waiting for TX.
    pub fn queued_bytes(&self) -> usize {
        self.queue.bytes()
    }

    fn handle_event(&mut self, event: kiss::HostEvent, now_ms: u64) {
        match event {
            kiss::HostEvent::Data(data) => {
                let mtu = if self.promisc { kiss::SINGLE_MTU } else { kiss::MTU };
                // Firmware silently drops full/oversize/empty frames at admission; never emit ERROR_QUEUE_FULL.
                let _ = self.queue.push(data, mtu);
            }
            kiss::HostEvent::Command { cmd, payload } => self.handle_command(cmd, &payload, now_ms),
        }
    }

    fn handle_command(&mut self, cmd: u8, payload: &[u8], now_ms: u64) {
        let b0 = payload.first().copied().unwrap_or(0);
        match cmd {
            kiss::CMD_FREQUENCY if payload.len() == 4 => {
                let value = u32::from_be_bytes(payload.try_into().unwrap());
                if value != 0 {
                    self.freq = value;
                    if self.radio_online {
                        let _ = self.apply_radio(|r| r.set_frequency(value));
                    }
                }
                self.reply_u32_escaped(kiss::CMD_FREQUENCY, self.freq);
            }
            kiss::CMD_BANDWIDTH if payload.len() == 4 => {
                let value = u32::from_be_bytes(payload.try_into().unwrap());
                if value != 0 {
                    self.bw = value;
                    if self.radio_online && self.apply_radio(|r| r.set_bandwidth(value)).is_ok() {
                        self.bw = self.radio.bandwidth();
                        self.update_phy();
                    }
                }
                self.reply_u32_escaped(kiss::CMD_BANDWIDTH, self.bw);
            }
            kiss::CMD_TXPOWER => {
                if b0 != 0xFF {
                    self.txp = clamp_tx_power_byte(b0, self.config.max_tx_power);
                    if self.radio_online {
                        let txp = self.txp as i8;
                        if self.apply_radio(|r| r.set_tx_power(txp)).is_ok() {
                            self.txp = self.radio.tx_power() as u8;
                        }
                    }
                }
                kiss::frame_plain(kiss::CMD_TXPOWER, &[self.txp], &mut self.host_output);
            }
            kiss::CMD_SF => {
                if b0 != 0xFF {
                    self.sf = b0.clamp(5, 12);
                    if self.radio_online {
                        let sf = self.sf;
                        if self.apply_radio(|r| r.set_spreading_factor(sf)).is_ok() {
                            self.sf = self.radio.spreading_factor();
                            self.update_phy();
                        }
                    }
                }
                kiss::frame_plain(kiss::CMD_SF, &[self.sf], &mut self.host_output);
            }
            kiss::CMD_CR => {
                if b0 != 0xFF {
                    self.cr = b0.clamp(5, 8);
                    if self.radio_online {
                        let cr = self.cr;
                        if self.apply_radio(|r| r.set_coding_rate(cr)).is_ok() {
                            self.cr = self.radio.coding_rate();
                            self.update_phy();
                        }
                    }
                }
                kiss::frame_plain(kiss::CMD_CR, &[self.cr], &mut self.host_output);
            }
            kiss::CMD_IMPLICIT => {
                self.implicit_len = b0;
                if self.radio_online {
                    self.enter_receive();
                }
                kiss::frame_escaped(kiss::CMD_IMPLICIT, &[self.implicit_len], &mut self.host_output);
            }
            kiss::CMD_LEAVE => {
                if b0 == 0xFF {
                    self.last_rssi = -292;
                    self.last_snr_raw = 0x80;
                }
            }
            kiss::CMD_RADIO_STATE => match b0 {
                0xFF => self.reply_radio_state(),
                kiss::RADIO_STATE_OFF => {
                    self.stop_radio("host request");
                    self.reply_radio_state();
                }
                kiss::RADIO_STATE_ON => {
                    self.start_radio(now_ms, "host request");
                }
                _ => {}
            },
            kiss::CMD_ST_ALOCK if payload.len() == 2 => {
                self.csma
                    .set_st_airtime_limit(u16::from_be_bytes(payload.try_into().unwrap()));
                self.reply_u16_escaped(kiss::CMD_ST_ALOCK, self.csma.st_airtime_limit());
            }
            kiss::CMD_LT_ALOCK if payload.len() == 2 => {
                self.csma
                    .set_lt_airtime_limit(u16::from_be_bytes(payload.try_into().unwrap()));
                self.reply_u16_escaped(kiss::CMD_LT_ALOCK, self.csma.lt_airtime_limit());
            }
            kiss::CMD_STAT_RX => self.reply_u32_escaped(kiss::CMD_STAT_RX, self.stat_rx),
            kiss::CMD_STAT_TX => self.reply_u32_escaped(kiss::CMD_STAT_TX, self.stat_tx),
            kiss::CMD_STAT_RSSI => self.reply_stat_rssi(),
            kiss::CMD_STAT_SNR => self.reply_stat_snr(),
            kiss::CMD_RADIO_LOCK => kiss::frame_plain(
                kiss::CMD_RADIO_LOCK,
                &[u8::from(self.radio_locked())],
                &mut self.host_output,
            ),
            kiss::CMD_RANDOM => kiss::frame_escaped(kiss::CMD_RANDOM, &[self.rng.byte()], &mut self.host_output),
            kiss::CMD_DETECT if b0 == kiss::DETECT_REQ => {
                kiss::frame_plain(kiss::CMD_DETECT, &[kiss::DETECT_RESP], &mut self.host_output)
            }
            kiss::CMD_PROMISC => {
                if b0 == 0x01 {
                    self.promisc = true;
                    if self.radio_online {
                        self.enter_receive();
                    }
                }
                if b0 == 0x00 {
                    self.promisc = false;
                    if self.radio_online {
                        self.enter_receive();
                    }
                }
                kiss::frame_plain(kiss::CMD_PROMISC, &[u8::from(self.promisc)], &mut self.host_output);
            }
            kiss::CMD_READY => kiss::frame_plain(
                kiss::CMD_READY,
                &[u8::from(!self.queue.is_full())],
                &mut self.host_output,
            ),
            kiss::CMD_RESET if b0 == kiss::CMD_RESET_BYTE => self.hard_reset(now_ms),
            kiss::CMD_FW_VERSION => {
                let id = self.config.identity;
                kiss::frame_plain(kiss::CMD_FW_VERSION, &[id.fw_major, id.fw_minor], &mut self.host_output);
            }
            kiss::CMD_PLATFORM => kiss::frame_plain(
                kiss::CMD_PLATFORM,
                &[self.config.identity.platform],
                &mut self.host_output,
            ),
            kiss::CMD_MCU => kiss::frame_plain(kiss::CMD_MCU, &[self.config.identity.mcu], &mut self.host_output),
            kiss::CMD_BOARD => kiss::frame_plain(kiss::CMD_BOARD, &[self.config.identity.board], &mut self.host_output),
            kiss::CMD_CONF_SAVE => self.conf_save(),
            kiss::CMD_CONF_DELETE => {
                if let Err(e) = self.storage.delete() {
                    log::warn!("failed to delete radio settings: {e}");
                }
            }
            // Firmware `dia_conf_save()`: 0x00 enables interference avoidance and any other byte
            // disables it. The firmware stores the byte in EEPROM and reboots without replying;
            // we only change the running node (see the module docs).
            kiss::CMD_DIS_IA if payload.len() == 1 => {
                self.avoid_interference = payload[0] == 0x00;
                self.csma.set_avoid_interference(self.avoid_interference);
                log::info!(
                    "host {} interference avoidance (CMD_DIS_IA) until the process exits",
                    if self.avoid_interference { "enabled" } else { "disabled" }
                );
            }
            // Display, Bluetooth, Wi-Fi, ROM and firmware-update commands are irrelevant for a
            // Linux modem process. Firmware either performs board-specific side effects or dumps
            // ROM data; we ignore them harmlessly unless covered above.
            _ => {}
        }
        let retune = matches!(
            cmd,
            kiss::CMD_FREQUENCY | kiss::CMD_BANDWIDTH | kiss::CMD_TXPOWER | kiss::CMD_SF | kiss::CMD_CR
        );
        if retune && self.radio_online {
            self.retune_pending_since.get_or_insert(now_ms);
        }
    }

    /// Log the settings of the online radio once [`RETUNE_LOG_DELAY_MS`] have passed since the
    /// host started retuning it, if they differ from the last line logged.
    fn log_retune(&mut self, now_ms: u64) {
        let Some(since) = self.retune_pending_since else {
            return;
        };
        if now_ms.saturating_sub(since) < RETUNE_LOG_DELAY_MS {
            return;
        }
        self.retune_pending_since = None;
        let settings = self.radio_summary();
        if self.logged_settings != Some(settings) {
            log::info!("radio settings changed: {settings}");
            self.logged_settings = Some(settings);
        }
    }

    /// Bring the radio online (firmware `startRadio()`); `cause` is for the log.
    fn start_radio(&mut self, _now_ms: u64, cause: &str) -> bool {
        if self.radio_online {
            self.reply_radio_state();
            return true;
        }
        if self.radio_locked() {
            self.radio_online = false;
            self.reply_radio_state();
            return false;
        }
        if let Err(e) = self.radio.begin(self.freq) {
            log::warn!("radio begin failed: {e}");
            self.reply_error(kiss::ERROR_INITRADIO);
            self.radio_online = false;
            self.reply_radio_state();
            return false;
        }
        self.radio_online = true;
        self.csma.reset();
        self.last_status_ms = None;
        let txp = clamp_tx_power_byte(self.txp, self.config.max_tx_power);
        self.txp = txp;
        if self.apply_radio(|r| r.set_tx_power(txp as i8)).is_ok() {
            self.txp = self.radio.tx_power() as u8;
        }
        let bw = self.bw;
        if self.apply_radio(|r| r.set_bandwidth(bw)).is_ok() {
            self.bw = self.radio.bandwidth();
        }
        let sf = self.sf;
        if self.apply_radio(|r| r.set_spreading_factor(sf)).is_ok() {
            self.sf = self.radio.spreading_factor();
        }
        let cr = self.cr;
        if self.apply_radio(|r| r.set_coding_rate(cr)).is_ok() {
            self.cr = self.radio.coding_rate();
        }
        let _ = self.apply_radio(|r| r.set_crc(true));
        self.update_phy();
        self.enter_receive();
        let settings = self.radio_summary();
        log::info!("radio online ({cause}): {settings}");
        self.logged_settings = Some(settings);
        self.retune_pending_since = None;
        self.reply_radio_state();
        true
    }

    /// Put the radio to sleep (firmware `stopRadio()`); `cause` is for the log.
    fn stop_radio(&mut self, cause: &str) {
        if let Err(e) = self.radio.end() {
            log::warn!("failed to put the radio to sleep: {e}");
        }
        if self.radio_online {
            // This line has the current settings, so a pending settings-changed line is moot.
            log::info!("radio offline ({cause}): {}", self.radio_summary());
            self.retune_pending_since = None;
        }
        self.radio_online = false;
    }

    /// The settings the radio runs with, for the online and offline lines.
    fn radio_summary(&self) -> RadioSummary {
        RadioSummary {
            frequency_hz: self.freq,
            bandwidth_hz: self.bw,
            spreading_factor: self.sf,
            coding_rate: self.cr,
            tx_power_dbm: self.txp as i8,
        }
    }

    fn hard_reset(&mut self, now_ms: u64) {
        if self.radio_online {
            self.stop_radio("reset");
        }
        if log::log_enabled!(log::Level::Debug) {
            while let Some(packet) = self.queue.pop() {
                log::debug!("dropping {}-byte TX packet: queue cleared by reset", packet.len());
            }
        }
        self.queue.clear();
        self.reassembler.reset();
        // A fresh CSMA, as after the firmware's reboot, with the avoidance setting the host may
        // have changed: the firmware rereads it from EEPROM at boot.
        self.csma = new_csma(self.avoid_interference);
        self.decoder.reset();
        self.radio_online = false;
        self.freq = 0;
        self.bw = 0;
        self.txp = 0xFF;
        self.sf = 0;
        self.cr = 5;
        self.implicit_len = 0;
        self.promisc = false;
        self.last_rssi = -292;
        self.last_snr_raw = 0x80;
        self.stat_rx = 0;
        self.stat_tx = 0;
        self.last_status_ms = None;
        self.phy = None;
        self.boot(now_ms);
    }

    fn radio_locked(&self) -> bool {
        !(self.freq != 0 && self.bw != 0 && self.txp != 0xFF && self.sf != 0)
    }

    fn apply_settings(&mut self, settings: RadioSettings) {
        self.freq = settings.frequency;
        self.bw = settings.bandwidth;
        self.sf = settings.spreading_factor.clamp(5, 12);
        self.cr = settings.coding_rate.clamp(5, 8);
        self.txp = clamp_tx_power_byte(settings.tx_power, self.config.max_tx_power);
    }

    fn conf_save(&mut self) {
        if self.radio_online {
            let settings = RadioSettings {
                frequency: self.freq,
                bandwidth: self.bw,
                spreading_factor: self.sf,
                coding_rate: self.cr,
                tx_power: self.txp,
            };
            if let Err(e) = self.storage.save(&settings) {
                log::warn!("failed to save radio settings: {e}");
            }
        }
    }

    fn update_phy(&mut self) {
        if !self.radio_online || self.bw == 0 || self.sf == 0 {
            return;
        }
        let phy = PhyParams::new(Modulation {
            bandwidth_hz: self.bw,
            spreading_factor: self.sf,
            coding_rate: self.cr,
        });
        self.phy = Some(phy);
        let _ = self.apply_radio(|r| r.set_preamble_length(phy.preamble_symbols));
        self.radio.set_dcd_timing(phy.preamble_time_ms, phy.header_time_ms);
        self.csma.set_phy(&phy);
        kiss::frame_escaped(kiss::CMD_STAT_PHYPRM, &phy.stats_payload(), &mut self.host_output);
    }

    /// Enter RX. Deliberately unlike firmware `lora_receive()`, implicit headers are used only
    /// in promiscuous/raw mode; normal RNode split framing remains explicit-header LoRa.
    fn enter_receive(&mut self) {
        let implicit = (self.promisc && self.implicit_len != 0).then_some(self.implicit_len);
        let _ = self.apply_radio(|r| r.receive(implicit));
    }

    fn drain_tx_queue(&mut self, now_ms: u64, should_stop: &mut dyn FnMut() -> bool) {
        let mut sent_any = false;
        let mut radio_attempted = false;
        let mut reset_during_batch = false;
        if !self.queue.is_empty() && !self.csma.airtime_locked() && self.observe_status(now_ms).is_some() {
            let access = self.csma.poll_access(now_ms, &mut self.rng);
            if access != Access::Wait {
                // Firmware `pop_queue()` sends one packet and `flush_queue()` all of them; unlike
                // the firmware, either stops before a packet once `should_stop` says so.
                while !self.queue.is_empty() && !should_stop() {
                    let Some(pkt) = self.queue.pop() else { break };
                    let (any_sent, complete, attempted, reset) = self.transmit_packet(&pkt, now_ms);
                    sent_any |= any_sent;
                    radio_attempted |= attempted;
                    reset_during_batch |= reset;
                    if access == Access::Single || !complete {
                        break;
                    }
                }
            }
        }
        if reset_during_batch {
            return;
        }
        if sent_any {
            let update = self.csma.update_airtime(now_ms);
            self.emit_status_update(update);
        }
        if radio_attempted && self.radio_online {
            self.enter_receive();
        }
    }

    fn transmit_packet(&mut self, payload: &[u8], now_ms: u64) -> (bool, bool, bool, bool) {
        if !self.radio_online {
            log::debug!(
                "dropping {}-byte TX packet: radio offline; sent ERROR_TXFAILED",
                payload.len()
            );
            self.reply_error(kiss::ERROR_TXFAILED);
            return (false, false, false, false);
        }
        let frames = if self.promisc {
            if payload.len() > kiss::SINGLE_MTU {
                log::debug!(
                    "dropping {}-byte TX packet: exceeds the {}-byte single-frame MTU in promiscuous mode; \
                     sent ERROR_TXFAILED",
                    payload.len(),
                    kiss::SINGLE_MTU
                );
                self.reply_error(kiss::ERROR_TXFAILED);
                return (false, false, false, false);
            }
            vec![payload.to_vec()]
        } else {
            framing::tx_frames(payload, &mut self.rng)
        };
        let summary = PacketSummary {
            len: payload.len(),
            frames: frames.len(),
            header: if self.promisc { None } else { Some(frames[0][0]) },
        };
        let mut ok_any = false;
        for (index, frame) in frames.iter().enumerate() {
            let frame_len = frame.len();
            match self.radio.transmit(frame, self.promisc && self.implicit_len != 0) {
                Ok(TxOutcome::Done) => {
                    ok_any = true;
                    if let Some(phy) = self.phy {
                        let cost = phy::airtime_ms(
                            frame_len,
                            Modulation {
                                bandwidth_hz: self.bw,
                                spreading_factor: self.sf,
                                coding_rate: self.cr,
                            },
                            self.radio.low_data_rate(),
                            phy.preamble_symbols,
                            phy.symbol_time_ms,
                        );
                        self.csma.add_airtime(now_ms, cost);
                    }
                }
                Ok(TxOutcome::Timeout) => {
                    log::warn!(
                        "modem timed out during TX; hard-resetting the node and dropping {} queued TX packet{}",
                        self.queue.len(),
                        plural(self.queue.len())
                    );
                    log_tx(
                        summary,
                        payload,
                        format_args!(
                            "modem timeout on frame {}/{}; sent ERROR_MODEM_TIMEOUT, ERROR_TXFAILED; hard reset",
                            index + 1,
                            summary.frames
                        ),
                    );
                    self.reply_error(kiss::ERROR_MODEM_TIMEOUT);
                    self.reply_error(kiss::ERROR_TXFAILED);
                    self.hard_reset(now_ms);
                    return (ok_any, false, false, true);
                }
                Err(e) => {
                    log::warn!("radio transmit failed: {e}");
                    log_tx(
                        summary,
                        payload,
                        format_args!(
                            "radio error on frame {}/{}: {e}; sent ERROR_TXFAILED",
                            index + 1,
                            summary.frames
                        ),
                    );
                    self.reply_error(kiss::ERROR_TXFAILED);
                    return (ok_any, false, true, false);
                }
            }
        }
        self.stat_tx = self.stat_tx.wrapping_add(1);
        log_tx(summary, payload, format_args!("ok"));
        (ok_any, true, true, false)
    }

    fn observe_status(&mut self, now_ms: u64) -> Option<()> {
        let carrier = match self.radio.dcd(now_ms) {
            Ok(v) => {
                self.dcd_faults.succeeded(now_ms);
                v
            }
            Err(e) => {
                self.dcd_faults.failed(now_ms, &e);
                return None;
            }
        };
        let rssi = match self.radio.current_rssi() {
            Ok(v) => {
                self.rssi_faults.succeeded(now_ms);
                v
            }
            Err(e) => {
                self.rssi_faults.failed(now_ms, &e);
                return None;
            }
        };
        self.csma.observe(now_ms, carrier, rssi);
        Some(())
    }

    fn handle_rx_frame(&mut self, frame: RxFrame) {
        let RxFrame {
            data,
            rssi_dbm,
            snr_raw,
        } = frame;
        if self.promisc {
            if data.len() > kiss::SINGLE_MTU {
                log::debug!(
                    "dropping {}-byte promiscuous RX frame: exceeds the {}-byte single-frame MTU",
                    data.len(),
                    kiss::SINGLE_MTU
                );
                return;
            }
            self.deliver_packet(&data, None, rssi_dbm, snr_raw);
            return;
        }
        match self.reassembler.process(&data) {
            Reassembly::Complete(packet) => self.deliver_packet(&packet, data.first().copied(), rssi_dbm, snr_raw),
            Reassembly::Pending | Reassembly::Ignored => {}
        }
    }

    /// Send a received packet to the host, preceded by its RSSI and SNR. `header` is the RNode
    /// header byte of the frame that completed the packet; `None` for a promiscuous raw frame.
    fn deliver_packet(&mut self, packet: &[u8], header: Option<u8>, rssi_dbm: i16, snr_raw: u8) {
        let split = header.is_some_and(|h| h & kiss::FLAG_SPLIT != 0);
        let summary = PacketSummary {
            len: packet.len(),
            frames: if split { 2 } else { 1 },
            header,
        };
        // The values sent with CMD_STAT_RSSI / CMD_STAT_SNR below; the SNR byte is in 0.25 dB.
        log::debug!(
            "RX packet: {summary}, RSSI {rssi_dbm} dBm, SNR {:.2} dB",
            f32::from(snr_raw as i8) * 0.25
        );
        if log::log_enabled!(log::Level::Trace) {
            log::trace!("RX payload, {} bytes:{}", packet.len(), HexDump(packet));
        }
        self.last_rssi = rssi_dbm;
        self.last_snr_raw = snr_raw;
        self.reply_stat_rssi();
        self.reply_stat_snr();
        kiss::host_data(packet, &mut self.host_output);
        self.stat_rx = self.stat_rx.wrapping_add(1);
    }

    fn emit_status_update(&mut self, update: StatusUpdate) {
        if update.channel_stats_due {
            kiss::frame_escaped(
                kiss::CMD_STAT_CHTM,
                &self.csma.channel_stats_payload(),
                &mut self.host_output,
            );
        }
        if update.csma_changed {
            kiss::frame_escaped(kiss::CMD_STAT_CSMA, &self.csma.csma_payload(), &mut self.host_output);
        }
    }

    fn apply_radio<T>(&mut self, f: impl FnOnce(&mut R) -> Result<T, R::Error>) -> Result<T, ()> {
        match f(&mut self.radio) {
            Ok(v) => Ok(v),
            Err(e) => {
                log::warn!("radio operation failed: {e}");
                Err(())
            }
        }
    }

    fn reply_radio_state(&mut self) {
        kiss::frame_plain(
            kiss::CMD_RADIO_STATE,
            &[u8::from(self.radio_online)],
            &mut self.host_output,
        );
    }

    fn reply_error(&mut self, error: u8) {
        kiss::frame_plain(kiss::CMD_ERROR, &[error], &mut self.host_output);
    }

    fn reply_stat_rssi(&mut self) {
        let val = (i32::from(self.last_rssi) + RSSI_OFFSET) as u8;
        kiss::frame_escaped(kiss::CMD_STAT_RSSI, &[val], &mut self.host_output);
    }

    fn reply_stat_snr(&mut self) {
        kiss::frame_escaped(kiss::CMD_STAT_SNR, &[self.last_snr_raw], &mut self.host_output);
    }

    fn reply_u16_escaped(&mut self, cmd: u8, value: u16) {
        kiss::frame_escaped(cmd, &value.to_be_bytes(), &mut self.host_output);
    }

    fn reply_u32_escaped(&mut self, cmd: u8, value: u32) {
        kiss::frame_escaped(cmd, &value.to_be_bytes(), &mut self.host_output);
    }
}
