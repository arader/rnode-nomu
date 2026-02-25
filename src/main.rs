// Copyright (C) 2024, Mark Qvist
// Ported to Rust for Linux by RNode contributors.
// Licensed under the GNU General Public License v3.0 or later.

//! RNode Linux LoRa Daemon
//!
//! A userspace daemon that exposes a PTY serial device for KISS protocol
//! communication and drives an SX1262 LoRa radio via Linux spidev + GPIO.

use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::Parser;
use log::{debug, error, info, warn};

use rnode_linux::config::Config;
use rnode_linux::csma::{self, CsmaState};
use rnode_linux::error;
use rnode_linux::gpio::RadioGpio;
use rnode_linux::kiss::{self, KissCommand, KissParser};
use rnode_linux::packet::{self, PacketQueue, RxReassembler};
use rnode_linux::pty::PtyPair;
use rnode_linux::radio::sx126x::Sx126x;
use rnode_linux::radio::types::TcxoVoltage;
use rnode_linux::radio::Radio;
use rnode_linux::spi::SpiDevice;

#[derive(Parser)]
#[command(name = "rnode-linux", about = "RNode Linux LoRa daemon")]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(short, long, default_value = "config.toml")]
    config: String,
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();

    if let Err(e) = run(&cli.config) {
        error!("Fatal: {}", e);
        std::process::exit(1);
    }
}

fn run(config_path: &str) -> error::Result<()> {
    let config = Config::load(config_path)?;
    info!("Loaded config from {}", config_path);

    // --- Signal handling ---
    setup_signal_handlers();

    // --- Initialize hardware ---
    let spi = SpiDevice::open(&config.spi.device, config.spi.speed_hz)?;
    info!("Opened SPI device: {}", config.spi.device);

    let rxen_line = config
        .gpio
        .rxen_line
        .and_then(|l| if l >= 0 { Some(l as u32) } else { None });

    let gpio = RadioGpio::new(
        &config.gpio.chip,
        config.gpio.reset_line,
        config.gpio.busy_line,
        config.gpio.dio0_line,
        rxen_line,
    )?;
    info!("GPIO initialized");

    let tcxo_voltage = config
        .radio
        .tcxo_voltage
        .as_deref()
        .and_then(TcxoVoltage::from_str)
        .unwrap_or(TcxoVoltage::V3_3);

    let mut radio = Sx126x::new(spi, gpio, config.radio.enable_tcxo, tcxo_voltage, true);

    radio.begin(config.radio.frequency)?;
    radio.set_bandwidth(config.radio.bandwidth)?;
    radio.set_spreading_factor(config.radio.spreading_factor)?;
    radio.set_coding_rate(config.radio.coding_rate)?;
    radio.set_tx_power(config.radio.tx_power)?;
    radio.set_preamble_length(config.radio.preamble_symbols)?;
    if config.radio.enable_crc {
        radio.enable_crc()?;
    }
    info!(
        "Radio initialized: freq={} Hz, bw={} Hz, sf={}, cr={}, txp={} dBm",
        config.radio.frequency,
        config.radio.bandwidth,
        config.radio.spreading_factor,
        config.radio.coding_rate,
        config.radio.tx_power
    );

    radio.receive()?;
    info!("Radio in receive mode");

    // --- Create PTY ---
    let pty = PtyPair::open(config.pty.symlink.as_deref())?;
    info!("PTY ready: {}", pty.slave_path);

    // --- State ---
    let mut kiss_parser = KissParser::new();
    let mut tx_queue = PacketQueue::new();
    let mut rx_reassembler = RxReassembler::new();
    let mut csma = CsmaState::new(
        config.csma.enabled,
        config.csma.slot_min_ms,
        config.csma.slot_max_ms,
    );
    let mut radio_online = true;
    let mut promisc = false;
    let mut stat_rx: u32 = 0;
    let mut stat_tx: u32 = 0;

    // --- Event loop ---
    let mut read_buf = [0u8; 1024];
    let poll_timeout_ms = 5; // 5ms poll interval for responsive DCD/CSMA

    info!("Entering main event loop");

    while GLOBAL_RUNNING.load(Ordering::Relaxed) {
        // Poll PTY master for incoming data
        let mut fds = [libc::pollfd {
            fd: pty.master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        unsafe { libc::poll(fds.as_mut_ptr(), 1, poll_timeout_ms) };

        // --- Read from PTY (host → radio) ---
        if fds[0].revents & libc::POLLIN != 0 {
                match pty.read(&mut read_buf) {
                    Ok(0) => {}
                    Ok(n) => {
                        for &byte in &read_buf[..n] {
                            if let Some(cmd) = kiss_parser.feed(byte) {
                                handle_command(
                                    cmd,
                                    &mut radio,
                                    &pty,
                                    &mut tx_queue,
                                    &mut csma,
                                    &mut radio_online,
                                    &mut promisc,
                                    stat_rx,
                                    stat_tx,
                                )?;
                            }
                        }
                    }
                    Err(e) => {
                        debug!("PTY read error: {}", e);
                    }
                }
            }
        if fds[0].revents & libc::POLLHUP != 0 {
            // Host closed the PTY — keep running and wait for reconnect
            debug!("PTY HUP — host disconnected");
        }

        // --- Check for RX interrupt (DIO0 GPIO) ---
        if radio_online {
            // Poll GPIO for DIO0 rising edge
            if let Ok(true) = check_dio0(&radio) {
                match radio.handle_dio0_rise() {
                    Ok(Some(pkt_len)) => {
                        let mut frame = vec![0u8; pkt_len];
                        if let Ok(n) = radio.read_packet(&mut frame) {
                            frame.truncate(n);
                            stat_rx += 1;

                            let payload = if promisc {
                                Some(rx_reassembler.process_frame_promisc(&frame))
                            } else {
                                rx_reassembler.process_frame(&frame)
                            };

                            if let Some(data) = payload {
                                let kiss_frame = kiss::indicate_data(&data);
                                let _ = pty.write_all(&kiss_frame);
                            }
                        }
                        // Re-enter receive mode
                        let _ = radio.receive();
                    }
                    Ok(None) => {
                        // CRC error, discard
                    }
                    Err(e) => {
                        warn!("RX error: {}", e);
                        let _ = radio.receive();
                    }
                }
            }
        }

        // --- CSMA/DCD update ---
        if radio_online {
            csma.update_dcd(&mut radio);
        }

        // --- TX from queue ---
        if radio_online && !tx_queue.is_empty() && csma.can_transmit() {
            if let Some(data) = tx_queue.pop() {
                let seq = tx_queue.next_seq();
                let frames = packet::prepare_tx_frames(&data, seq);
                let mut tx_ok = true;

                for frame in &frames {
                    let implicit = false;
                    radio.begin_packet(implicit)?;
                    radio.write_packet(frame)?;
                    if !radio.end_packet()? {
                        let _ = pty.write_all(&kiss::indicate_error(kiss::ERROR_TXFAILED));
                        tx_ok = false;
                        break;
                    }

                    // Track airtime
                    let airtime_ms = csma::calculate_airtime_ms(
                        frame.len(),
                        radio.get_spreading_factor(),
                        radio.get_signal_bandwidth(),
                        radio.get_coding_rate() - 4,
                        config.radio.preamble_symbols,
                        implicit,
                        config.radio.enable_crc,
                        false, // low_data_rate checked internally
                    );
                    csma.add_airtime_ms(airtime_ms as u64);
                }

                if tx_ok {
                    stat_tx += 1;
                }

                // Return to receive mode after TX
                radio.receive()?;
            }
        }
    }

    info!("Shutting down");
    let _ = radio.end();
    Ok(())
}

/// Dispatch a parsed KISS command.
fn handle_command(
    cmd: KissCommand,
    radio: &mut Sx126x,
    pty: &PtyPair,
    tx_queue: &mut PacketQueue,
    csma: &mut CsmaState,
    radio_online: &mut bool,
    promisc: &mut bool,
    stat_rx: u32,
    stat_tx: u32,
) -> error::Result<()> {
    match cmd {
        KissCommand::Data(data) => {
            if data.len() > packet::MTU {
                warn!("Packet too large: {} > {}", data.len(), packet::MTU);
                return Ok(());
            }
            if !tx_queue.push(&data) {
                let _ = pty.write_all(&kiss::indicate_error(kiss::ERROR_QUEUE_FULL));
            }
        }
        KissCommand::SetFrequency(freq) => {
            if freq == 0 {
                let _ = pty.write_all(&kiss::indicate_frequency(radio.get_frequency()));
            } else {
                radio.set_frequency(freq)?;
                let _ = pty.write_all(&kiss::indicate_frequency(freq));
            }
        }
        KissCommand::SetBandwidth(bw) => {
            if bw == 0 {
                let _ = pty.write_all(&kiss::indicate_bandwidth(radio.get_signal_bandwidth()));
            } else {
                radio.set_bandwidth(bw)?;
                let _ = pty.write_all(&kiss::indicate_bandwidth(bw));
            }
        }
        KissCommand::SetTxPower(txp) => {
            if txp == 0xFF {
                let _ = pty.write_all(&kiss::indicate_txpower(radio.get_tx_power() as u8));
            } else {
                let clamped = txp.min(22);
                radio.set_tx_power(clamped as i8)?;
                let _ = pty.write_all(&kiss::indicate_txpower(clamped));
            }
        }
        KissCommand::SetSf(sf) => {
            if sf == 0xFF {
                let _ = pty.write_all(&kiss::indicate_spreadingfactor(
                    radio.get_spreading_factor(),
                ));
            } else {
                let sf = sf.clamp(5, 12);
                radio.set_spreading_factor(sf)?;
                let _ = pty.write_all(&kiss::indicate_spreadingfactor(sf));
            }
        }
        KissCommand::SetCr(cr) => {
            if cr == 0xFF {
                let _ = pty.write_all(&kiss::indicate_codingrate(radio.get_coding_rate()));
            } else {
                let cr = cr.clamp(5, 8);
                radio.set_coding_rate(cr)?;
                let _ = pty.write_all(&kiss::indicate_codingrate(cr));
            }
        }
        KissCommand::RadioState(state) => {
            if state == 0xFF {
                let s = if *radio_online {
                    kiss::RADIO_STATE_ON
                } else {
                    kiss::RADIO_STATE_OFF
                };
                let _ = pty.write_all(&kiss::indicate_radiostate(s));
            } else if state == 0x00 {
                radio.standby()?;
                *radio_online = false;
                let _ = pty.write_all(&kiss::indicate_radiostate(kiss::RADIO_STATE_OFF));
            } else if state == 0x01 {
                radio.receive()?;
                *radio_online = true;
                let _ = pty.write_all(&kiss::indicate_radiostate(kiss::RADIO_STATE_ON));
            }
        }
        KissCommand::RadioLock => {
            // Always unlocked on Linux
            let _ = pty.write_all(&kiss::indicate_radio_lock(false));
        }
        KissCommand::Detect => {
            let _ = pty.write_all(&kiss::indicate_detect());
        }
        KissCommand::SetImplicit(len) => {
            if len > 0 {
                radio.set_implicit_header_mode(len)?;
            } else {
                radio.set_explicit_header_mode()?;
            }
            let _ = pty.write_all(&kiss::indicate_implicit_length(len));
        }
        KissCommand::Leave => {
            debug!("Host disconnected (CMD_LEAVE)");
        }
        KissCommand::StAirlock(at) => {
            if at == 0 {
                csma.set_st_airtime_limit(0.0);
            } else {
                let limit = at as f32 / (100.0 * 100.0);
                csma.set_st_airtime_limit(limit);
            }
            let _ = pty.write_all(&kiss::indicate_st_alock(csma.st_airtime_limit_raw()));
        }
        KissCommand::LtAirlock(at) => {
            if at == 0 {
                csma.set_lt_airtime_limit(0.0);
            } else {
                let limit = at as f32 / (100.0 * 100.0);
                csma.set_lt_airtime_limit(limit);
            }
            let _ = pty.write_all(&kiss::indicate_lt_alock(csma.lt_airtime_limit_raw()));
        }
        KissCommand::Promisc(val) => {
            *promisc = val == 0x01;
            let _ = pty.write_all(&kiss::indicate_promisc(*promisc));
        }
        KissCommand::Ready => {
            if tx_queue.is_full() {
                let _ = pty.write_all(&kiss::indicate_not_ready());
            } else {
                let _ = pty.write_all(&kiss::indicate_ready());
            }
        }
        KissCommand::StatRx => {
            let _ = pty.write_all(&kiss::indicate_stat_rx(stat_rx));
        }
        KissCommand::StatTx => {
            let _ = pty.write_all(&kiss::indicate_stat_tx(stat_tx));
        }
        KissCommand::StatRssi => {
            let rssi = radio.current_rssi_raw().unwrap_or(0);
            let _ = pty.write_all(&kiss::indicate_stat_rssi(rssi));
        }
        KissCommand::StatSnr => {
            let snr = radio.packet_snr_raw().unwrap_or(0x80);
            let _ = pty.write_all(&kiss::indicate_stat_snr(snr));
        }
        KissCommand::Blink(_) => {
            // No LED on Linux, ignore
        }
        KissCommand::Random => {
            let val = radio.random().unwrap_or(0);
            let _ = pty.write_all(&kiss::indicate_random(val));
        }
        KissCommand::FwVersion => {
            let _ = pty.write_all(&kiss::indicate_version());
        }
        KissCommand::Platform => {
            let _ = pty.write_all(&kiss::indicate_platform());
        }
        KissCommand::Board => {
            let _ = pty.write_all(&kiss::indicate_board());
        }
        KissCommand::Reset => {
            info!("Reset requested — reinitializing radio");
            let freq = radio.get_frequency();
            let _ = radio.end();
            radio.begin(freq)?;
            radio.receive()?;
            *radio_online = true;
        }
    }
    Ok(())
}

/// Check if DIO0 GPIO is high (indicating an RX done interrupt).
/// In a production implementation, this would poll the GPIO fd via epoll.
/// For now, we poll the IRQ status register directly from the radio.
fn check_dio0(_radio: &Sx126x) -> error::Result<bool> {
    // Placeholder: the main loop should use GPIO edge detection.
    // For initial testing, always return false (RX handled via polling).
    Ok(false)
}

static GLOBAL_RUNNING: AtomicBool = AtomicBool::new(true);

extern "C" fn handle_signal(_sig: libc::c_int) {
    GLOBAL_RUNNING.store(false, Ordering::SeqCst);
}

fn setup_signal_handlers() {
    unsafe {
        libc::signal(libc::SIGINT, handle_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, handle_signal as *const () as libc::sighandler_t);
    }
}
