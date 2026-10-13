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
//!     tick(now)                     always
//!     write take_host_output() to the host
//! }
//! shutdown()
//! ```
//!
//! Observable behaviour follows RNode_Firmware v1.86's TNC path: `serial_callback()`,
//! `receive_callback()`, `transmit()`, `startRadio()`/`stopRadio()`, `update_radio_lock()` and
//! the `kiss_indicate_*()` encoders. Deliberate differences: host config commands retune the
//! radio immediately while online, TX/RX stats are incremented, exactly-508-byte TX packets do
//! not send the firmware's redundant header-only third LoRa frame, oversized promiscuous-mode
//! raw frames are rejected/dropped instead of being truncated to one LoRa frame, and host-stream
//! safety replies with arbitrary bytes (`CMD_RANDOM`, `CMD_IMPLICIT`) KISS-escape their payloads,
//! and implicit-header receive is only enabled in promiscuous mode so normal split RNode frames
//! keep explicit LoRa headers.

use crate::RSSI_OFFSET;
use crate::csma::{Access, Csma, StatusUpdate};
use crate::framing::{self, Reassembly, RxReassembler};
use crate::identity::DeviceIdentity;
use crate::kiss;
use crate::phy::{self, Modulation, PhyParams};
use crate::queue::TxQueue;
use crate::radio::{Radio, RxFrame, TxOutcome};
use crate::rng::Rng;
use crate::storage::{RadioSettings, Storage};

/// Firmware `STATUS_INTERVAL_MS`: how often the modem status (DCD, RSSI) is sampled.
pub const STATUS_INTERVAL_MS: u64 = 3;
const OFFLINE_WAKEUP_MS: u64 = 1_000;
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
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            identity: DeviceIdentity::default(),
            max_tx_power: 22,
            queue_max_packets: 200,
            queue_max_bytes: 6144,
        }
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
}

impl<R: Radio, S: Storage> Node<R, S> {
    /// Create a node. `seed` initialises the node's pseudo-random generator (CSMA contention
    /// window, packet sequence numbers); pass real entropy outside of tests.
    pub fn new(radio: R, storage: S, config: NodeConfig, seed: u64) -> Self {
        let queue = TxQueue::new(config.queue_max_packets, config.queue_max_bytes);
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
            csma: Csma::new(),
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
        }
    }

    /// Start-up: load saved radio settings and, if they are complete, bring the radio online.
    pub fn boot(&mut self, now_ms: u64) {
        match self.storage.load() {
            Ok(Some(settings)) => {
                self.apply_settings(settings);
                if !self.radio_locked() {
                    self.start_radio(now_ms);
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
    pub fn service_radio(&mut self, _now_ms: u64) {
        if !self.radio_online {
            return;
        }
        loop {
            match self.radio.poll_rx() {
                Ok(Some(frame)) => self.handle_rx_frame(frame),
                Ok(None) => break,
                Err(e) => {
                    log::warn!("radio RX service failed: {e}");
                    break;
                }
            }
        }
    }

    /// Periodic modem status, CSMA and TX queue work.
    pub fn tick(&mut self, now_ms: u64) {
        if !self.radio_online {
            return;
        }
        if self
            .last_status_ms
            .is_none_or(|last| now_ms.saturating_sub(last) >= STATUS_INTERVAL_MS)
        {
            self.last_status_ms = Some(now_ms);
            if let Some(update) = self.observe_status(now_ms).map(|()| self.csma.status_tick(now_ms)) {
                self.emit_status_update(update);
            }
        }
        self.drain_tx_queue(now_ms);
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

    /// Take the radio offline before the process exits.
    pub fn shutdown(&mut self) {
        if self.radio_online {
            if let Err(e) = self.radio.end() {
                log::warn!("failed to put the radio to sleep: {e}");
            }
            self.radio_online = false;
        }
    }

    /// The node's static configuration.
    pub fn config(&self) -> &NodeConfig {
        &self.config
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
                    self.stop_radio();
                    self.reply_radio_state();
                }
                kiss::RADIO_STATE_ON => {
                    self.start_radio(now_ms);
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
            // Display, Bluetooth, Wi-Fi, ROM and firmware-update commands are irrelevant for a
            // Linux modem process. Firmware either performs board-specific side effects or dumps
            // ROM data; we ignore them harmlessly unless covered above.
            _ => {}
        }
    }

    fn start_radio(&mut self, _now_ms: u64) -> bool {
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
        self.reply_radio_state();
        true
    }

    fn stop_radio(&mut self) {
        if let Err(e) = self.radio.end() {
            log::warn!("radio end failed: {e}");
        }
        self.radio_online = false;
    }

    fn hard_reset(&mut self, now_ms: u64) {
        if self.radio_online {
            self.stop_radio();
        }
        self.queue.clear();
        self.reassembler.reset();
        self.csma = Csma::new();
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

    fn drain_tx_queue(&mut self, now_ms: u64) {
        let mut sent_any = false;
        let mut radio_attempted = false;
        let mut reset_during_batch = false;
        if !self.queue.is_empty() && !self.csma.airtime_locked() && self.observe_status(now_ms).is_some() {
            match self.csma.poll_access(now_ms, &mut self.rng) {
                Access::Wait => {}
                Access::Single => {
                    if let Some(pkt) = self.queue.pop() {
                        let (any_sent, _complete, attempted, reset) = self.transmit_packet(&pkt, now_ms);
                        sent_any |= any_sent;
                        radio_attempted |= attempted;
                        reset_during_batch |= reset;
                    }
                }
                Access::Flush => {
                    while let Some(pkt) = self.queue.pop() {
                        let (any_sent, complete, attempted, reset) = self.transmit_packet(&pkt, now_ms);
                        sent_any |= any_sent;
                        radio_attempted |= attempted;
                        reset_during_batch |= reset;
                        if !complete {
                            break;
                        }
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
            self.reply_error(kiss::ERROR_TXFAILED);
            return (false, false, false, false);
        }
        let frames = if self.promisc {
            if payload.len() > kiss::SINGLE_MTU {
                log::debug!("dropping promiscuous TX frame over the single-frame MTU");
                self.reply_error(kiss::ERROR_TXFAILED);
                return (false, false, false, false);
            }
            vec![payload.to_vec()]
        } else {
            framing::tx_frames(payload, &mut self.rng)
        };
        let mut ok_any = false;
        for frame in frames {
            let frame_len = frame.len();
            match self.radio.transmit(&frame, self.promisc && self.implicit_len != 0) {
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
                    self.reply_error(kiss::ERROR_MODEM_TIMEOUT);
                    self.reply_error(kiss::ERROR_TXFAILED);
                    self.hard_reset(now_ms);
                    return (ok_any, false, false, true);
                }
                Err(e) => {
                    log::warn!("radio transmit failed: {e}");
                    self.reply_error(kiss::ERROR_TXFAILED);
                    return (ok_any, false, true, false);
                }
            }
        }
        self.stat_tx = self.stat_tx.wrapping_add(1);
        (ok_any, true, true, false)
    }

    fn observe_status(&mut self, now_ms: u64) -> Option<()> {
        let carrier = match self.radio.dcd(now_ms) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("radio DCD failed: {e}");
                return None;
            }
        };
        let rssi = match self.radio.current_rssi() {
            Ok(v) => v,
            Err(e) => {
                log::warn!("radio RSSI failed: {e}");
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
                log::debug!("dropping promiscuous RX frame over the single-frame MTU");
                return;
            }
            self.deliver_packet(&data, rssi_dbm, snr_raw);
            return;
        }
        match self.reassembler.process(&data) {
            Reassembly::Complete(packet) => self.deliver_packet(&packet, rssi_dbm, snr_raw),
            Reassembly::Pending | Reassembly::Ignored => {}
        }
    }

    fn deliver_packet(&mut self, packet: &[u8], rssi_dbm: i16, snr_raw: u8) {
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
