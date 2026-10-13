//! Deterministic in-process radio medium for tests.
//!
//! [`SimRadio`] implements [`crate::Radio`] without I/O. Tests advance virtual time on the shared
//! [`SimMedium`]; transmissions are immediately queued at all compatible peer radios and mark the
//! channel busy for a deterministic airtime window, allowing CSMA wait/error paths to be tested.

use crate::radio::{Radio, RxFrame, TxOutcome};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::rc::Rc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimError {
    BeginFailed,
    RadioOffline,
    Injected(&'static str),
}

impl fmt::Display for SimError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BeginFailed => f.write_str("simulated begin failure"),
            Self::RadioOffline => f.write_str("simulated radio is offline"),
            Self::Injected(s) => f.write_str(s),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SimMedium(Rc<RefCell<MediumState>>);

#[derive(Debug, Default)]
struct MediumState {
    now_ms: u64,
    next_id: usize,
    busy_until_ms: u64,
    tx_duration_ms: u64,
    noise_dbm: i16,
    radios: HashMap<usize, Endpoint>,
}

#[derive(Debug)]
struct Endpoint {
    frequency: u32,
    bandwidth: u32,
    sf: u8,
    cr: u8,
    tx_power: i8,
    preamble: u16,
    crc: bool,
    implicit: Option<u8>,
    online: bool,
    rx: VecDeque<RxFrame>,
}

impl Default for Endpoint {
    fn default() -> Self {
        Self {
            frequency: 0,
            bandwidth: 125_000,
            sf: 7,
            cr: 5,
            tx_power: 2,
            preamble: 18,
            crc: false,
            implicit: None,
            online: false,
            rx: VecDeque::new(),
        }
    }
}

impl Default for SimMedium {
    fn default() -> Self {
        Self::new()
    }
}

impl SimMedium {
    pub fn new() -> Self {
        Self(Rc::new(RefCell::new(MediumState {
            tx_duration_ms: 100,
            noise_dbm: -120,
            ..MediumState::default()
        })))
    }

    pub fn now_ms(&self) -> u64 {
        self.0.borrow().now_ms
    }
    pub fn set_now_ms(&self, now_ms: u64) {
        self.0.borrow_mut().now_ms = now_ms;
    }
    pub fn advance_ms(&self, delta: u64) {
        self.0.borrow_mut().now_ms += delta;
    }
    pub fn set_tx_duration_ms(&self, duration_ms: u64) {
        self.0.borrow_mut().tx_duration_ms = duration_ms;
    }
    /// Set the RSSI in dBm that radios read while no transmission is on the air (default -120).
    /// Raising it more than `CSMA_INFR_THRESHOLD_DB` above a node's noise floor simulates
    /// non-LoRa interference: a strong signal without a LoRa carrier.
    pub fn set_noise_dbm(&self, dbm: i16) {
        self.0.borrow_mut().noise_dbm = dbm;
    }

    pub fn radio(&self) -> SimRadio {
        let mut st = self.0.borrow_mut();
        let id = st.next_id;
        st.next_id += 1;
        st.radios.insert(id, Endpoint::default());
        SimRadio {
            id,
            medium: self.clone(),
            begin_failures: 0,
            timeout_next: false,
            fail_next_tx: false,
            fail_tx_call: None,
            timeout_tx_call: None,
            tx_calls: 0,
            receive_calls: 0,
            end_calls: 0,
            dcd_timing: (0, 0),
            dcd_failures: 0,
            rssi_failures: 0,
            rx_failures: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SimRadio {
    id: usize,
    medium: SimMedium,
    begin_failures: usize,
    timeout_next: bool,
    fail_next_tx: bool,
    fail_tx_call: Option<usize>,
    timeout_tx_call: Option<usize>,
    tx_calls: usize,
    receive_calls: usize,
    end_calls: usize,
    dcd_timing: (u32, u32),
    dcd_failures: usize,
    rssi_failures: usize,
    rx_failures: usize,
}

impl SimRadio {
    pub fn inject_begin_failures(&mut self, count: usize) {
        self.begin_failures = count;
    }
    pub fn inject_next_timeout(&mut self) {
        self.timeout_next = true;
    }
    pub fn inject_next_tx_error(&mut self) {
        self.fail_next_tx = true;
    }

    pub fn inject_tx_error_on_call(&mut self, call_index: usize) {
        self.fail_tx_call = Some(call_index);
    }

    pub fn inject_timeout_on_call(&mut self, call_index: usize) {
        self.timeout_tx_call = Some(call_index);
    }

    /// Make the next `count` [`Radio::dcd`] calls fail.
    pub fn inject_dcd_errors(&mut self, count: usize) {
        self.dcd_failures = count;
    }

    /// Make the next `count` [`Radio::current_rssi`] calls fail.
    pub fn inject_rssi_errors(&mut self, count: usize) {
        self.rssi_failures = count;
    }

    /// Make the next `count` [`Radio::poll_rx`] calls fail.
    pub fn inject_rx_errors(&mut self, count: usize) {
        self.rx_failures = count;
    }

    pub fn receive_calls(&self) -> usize {
        self.receive_calls
    }

    pub fn end_calls(&self) -> usize {
        self.end_calls
    }

    pub fn is_online(&self) -> bool {
        self.with_ep(|e| e.online)
    }

    pub fn queued_rx(&self) -> usize {
        self.medium.0.borrow().radios.get(&self.id).map_or(0, |e| e.rx.len())
    }

    pub fn implicit_len(&self) -> Option<u8> {
        self.with_ep(|e| e.implicit)
    }

    pub fn inject_rx(&self, data: Vec<u8>, rssi_dbm: i16, snr_raw: u8) {
        self.with_ep_mut(|e| {
            e.rx.push_back(RxFrame {
                data,
                rssi_dbm,
                snr_raw,
            })
        });
    }

    fn with_ep<T>(&self, f: impl FnOnce(&Endpoint) -> T) -> T {
        let st = self.medium.0.borrow();
        f(st.radios.get(&self.id).expect("sim radio endpoint exists"))
    }

    fn with_ep_mut<T>(&self, f: impl FnOnce(&mut Endpoint) -> T) -> T {
        let mut st = self.medium.0.borrow_mut();
        f(st.radios.get_mut(&self.id).expect("sim radio endpoint exists"))
    }

    /// Whether a transmission is on the air at `now_ms`.
    fn carrier(&self, now_ms: u64) -> bool {
        self.medium.0.borrow().busy_until_ms > now_ms
    }

    /// Count down an injected failure: true if this call fails.
    fn take_failure(remaining: &mut usize) -> bool {
        let fail = *remaining > 0;
        *remaining = remaining.saturating_sub(1);
        fail
    }
}

impl Radio for SimRadio {
    type Error = SimError;

    fn begin(&mut self, frequency_hz: u32) -> Result<(), Self::Error> {
        if self.begin_failures > 0 {
            self.begin_failures -= 1;
            return Err(SimError::BeginFailed);
        }
        self.with_ep_mut(|e| {
            e.frequency = frequency_hz;
            e.online = true;
        });
        Ok(())
    }

    fn end(&mut self) -> Result<(), Self::Error> {
        self.end_calls += 1;
        self.with_ep_mut(|e| e.online = false);
        Ok(())
    }

    fn set_frequency(&mut self, hz: u32) -> Result<(), Self::Error> {
        self.with_ep_mut(|e| e.frequency = hz);
        Ok(())
    }
    fn frequency(&self) -> u32 {
        self.with_ep(|e| e.frequency)
    }

    fn set_bandwidth(&mut self, hz: u32) -> Result<(), Self::Error> {
        let quantized = [
            7_800, 10_400, 15_600, 20_800, 31_250, 41_700, 62_500, 125_000, 250_000, 500_000,
        ]
        .into_iter()
        .find(|&bw| bw >= hz)
        .unwrap_or(500_000);
        self.with_ep_mut(|e| e.bandwidth = quantized);
        Ok(())
    }
    fn bandwidth(&self) -> u32 {
        self.with_ep(|e| e.bandwidth)
    }

    fn set_spreading_factor(&mut self, sf: u8) -> Result<(), Self::Error> {
        self.with_ep_mut(|e| e.sf = sf.clamp(5, 12));
        Ok(())
    }
    fn spreading_factor(&self) -> u8 {
        self.with_ep(|e| e.sf)
    }
    fn set_coding_rate(&mut self, denominator: u8) -> Result<(), Self::Error> {
        self.with_ep_mut(|e| e.cr = denominator.clamp(5, 8));
        Ok(())
    }
    fn coding_rate(&self) -> u8 {
        self.with_ep(|e| e.cr)
    }
    fn set_tx_power(&mut self, dbm: i8) -> Result<(), Self::Error> {
        self.with_ep_mut(|e| e.tx_power = dbm);
        Ok(())
    }
    fn tx_power(&self) -> i8 {
        self.with_ep(|e| e.tx_power)
    }
    fn set_preamble_length(&mut self, symbols: u16) -> Result<(), Self::Error> {
        self.with_ep_mut(|e| e.preamble = symbols);
        Ok(())
    }
    fn set_crc(&mut self, enabled: bool) -> Result<(), Self::Error> {
        self.with_ep_mut(|e| e.crc = enabled);
        Ok(())
    }
    fn low_data_rate(&self) -> bool {
        crate::phy::low_data_rate(self.spreading_factor(), self.bandwidth())
    }
    fn receive(&mut self, implicit_len: Option<u8>) -> Result<(), Self::Error> {
        self.receive_calls += 1;
        self.with_ep_mut(|e| e.implicit = implicit_len);
        Ok(())
    }

    fn transmit(&mut self, frame: &[u8], _implicit: bool) -> Result<TxOutcome, Self::Error> {
        self.tx_calls += 1;
        if self.fail_next_tx || self.fail_tx_call == Some(self.tx_calls) {
            self.fail_next_tx = false;
            if self.fail_tx_call == Some(self.tx_calls) {
                self.fail_tx_call = None;
            }
            return Err(SimError::Injected("simulated TX error"));
        }
        if self.timeout_next || self.timeout_tx_call == Some(self.tx_calls) {
            self.timeout_next = false;
            if self.timeout_tx_call == Some(self.tx_calls) {
                self.timeout_tx_call = None;
            }
            return Ok(TxOutcome::Timeout);
        }
        let mut st = self.medium.0.borrow_mut();
        let Some(src) = st.radios.get(&self.id) else {
            return Err(SimError::RadioOffline);
        };
        if !src.online {
            return Err(SimError::RadioOffline);
        }
        let freq = src.frequency;
        let bw = src.bandwidth;
        let sf = src.sf;
        let now = st.now_ms;
        st.busy_until_ms = st.busy_until_ms.max(now + st.tx_duration_ms);
        for (&id, ep) in st.radios.iter_mut() {
            if id != self.id && ep.online && ep.frequency == freq && ep.bandwidth == bw && ep.sf == sf {
                ep.rx.push_back(RxFrame {
                    data: frame.to_vec(),
                    rssi_dbm: -42,
                    snr_raw: 0x20,
                });
            }
        }
        Ok(TxOutcome::Done)
    }

    fn poll_rx(&mut self) -> Result<Option<RxFrame>, Self::Error> {
        if Self::take_failure(&mut self.rx_failures) {
            return Err(SimError::Injected("simulated RX error"));
        }
        Ok(self.with_ep_mut(|e| e.rx.pop_front()))
    }
    fn dcd(&mut self, now_ms: u64) -> Result<bool, Self::Error> {
        if Self::take_failure(&mut self.dcd_failures) {
            return Err(SimError::Injected("simulated DCD error"));
        }
        Ok(self.carrier(now_ms))
    }
    fn current_rssi(&mut self) -> Result<i16, Self::Error> {
        if Self::take_failure(&mut self.rssi_failures) {
            return Err(SimError::Injected("simulated RSSI error"));
        }
        let carrier = self.carrier(self.medium.now_ms());
        Ok(if carrier { -42 } else { self.medium.0.borrow().noise_dbm })
    }
    fn set_dcd_timing(&mut self, preamble_ms: u32, header_ms: u32) {
        self.dcd_timing = (preamble_ms, header_ms);
    }
}
