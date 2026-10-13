//! Two `rnode-linux` daemon processes on the simulated radio (`radio.driver = "sim"`), sharing one
//! air directory: KISS `CMD_DATA` frames a host writes to one daemon's PTY must reach the host on
//! the other daemon's PTY byte for byte, across the process boundary (issue #31).
//!
//! The payload sizes sit on RNode_Firmware's framing boundaries (`Config.h`, `transmit()`): one
//! LoRa frame carries up to `SINGLE_MTU - HEADER_L` = 254 bytes, 255 bytes is the smallest split
//! packet and 508 (`MTU`) the largest. A 509-byte host frame is dropped silently: firmware
//! `serial_callback()` queues it, `pop_queue()`/`flush_queue()` skip lengths above `MTU`, and no
//! `CMD_ERROR` is sent.
//!
//! The KISS framing here is written out from the firmware's `Framing.h` rather than taken from
//! rnode-core, so the test cannot agree with a broken encoder in the daemon.

use std::collections::VecDeque;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use rnode_linux::config::{Config, RadioDriver};

// Framing.h
const FEND: u8 = 0xC0;
const FESC: u8 = 0xDB;
const TFEND: u8 = 0xDC;
const TFESC: u8 = 0xDD;
const CMD_DATA: u8 = 0x00;
const CMD_FREQUENCY: u8 = 0x01;
const CMD_BANDWIDTH: u8 = 0x02;
const CMD_TXPOWER: u8 = 0x03;
const CMD_SF: u8 = 0x04;
const CMD_CR: u8 = 0x05;
const CMD_RADIO_STATE: u8 = 0x06;
const CMD_DETECT: u8 = 0x08;
const CMD_STAT_RSSI: u8 = 0x23;
const CMD_STAT_SNR: u8 = 0x24;
const CMD_ERROR: u8 = 0x90;
const DETECT_REQ: u8 = 0x73;
const DETECT_RESP: u8 = 0x46;
const RADIO_STATE_ON: u8 = 0x01;

/// The modulation `cargo xtask rns-e2e` uses for its SF7 scenario.
const FREQUENCY_HZ: u32 = 914_875_000;
const BANDWIDTH_HZ: u32 = 125_000;
const TX_POWER_DBM: u8 = 2;
const SPREADING_FACTOR: u8 = 7;
const CODING_RATE: u8 = 5;

/// Generous limits for a slow, shared CI runner. Every wait returns as soon as its condition
/// holds, and the first failed wait ends the test, so a passing run takes about seven seconds,
/// mostly simulated airtime, and a failing one cannot hang.
const START_TIMEOUT: Duration = Duration::from_secs(30);
const REPLY_TIMEOUT: Duration = Duration::from_secs(15);
const PACKET_TIMEOUT: Duration = Duration::from_secs(30);
const EXIT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long to keep listening after the last packet for stray `CMD_DATA` or `CMD_ERROR` frames.
const SETTLE_TIME: Duration = Duration::from_millis(300);

/// The simulated air, relative to the daemons' working directory (the run directory): Unix
/// socket paths are limited to about 104 bytes and `CARGO_TARGET_TMPDIR` can be long.
const AIR_DIR: &str = "air";

struct NodeSpec {
    name: &'static str,
    rssi_dbm: i16,
    snr_db: f32,
    /// `kiss_indicate_stat_rssi()`: `last_rssi + rssi_offset` (157, `Config.h`).
    rssi_byte: u8,
    /// `kiss_indicate_stat_snr()`: the raw packet SNR, in quarter dB.
    snr_byte: u8,
}

const A: usize = 0;
const B: usize = 1;
/// Distinct RSSI/SNR per receiver, so the test also checks whose metadata a host is given.
static NODES: [NodeSpec; 2] = [
    NodeSpec {
        name: "a",
        rssi_dbm: -58,
        snr_db: 9.0,
        rssi_byte: 0x63, // -58 + 157 = 99
        snr_byte: 0x24,  // 9.0 dB * 4 = 36
    },
    NodeSpec {
        name: "b",
        rssi_dbm: -64,
        snr_db: 6.5,
        rssi_byte: 0x5D, // -64 + 157 = 93
        snr_byte: 0x1A,  // 6.5 dB * 4 = 26
    },
];

#[test]
fn two_sim_daemons_carry_kiss_data_between_processes() {
    let run = RunDir::create();
    let mut smoke = Smoke::start(&run.0);
    smoke.bring_radios_online();

    for len in [1, 254, 255, 508] {
        // Both directions, with different bytes; the first byte of each must be KISS-escaped.
        let forward = payload(len, FEND);
        smoke.send_data(A, &forward);
        smoke.expect_packet(B, &forward);
        let back = payload(len, FESC);
        smoke.send_data(B, &back);
        smoke.expect_packet(A, &back);
    }

    // Packets leave a daemon in host order, so the 509-byte frame was never transmitted if the
    // next packet B receives is the one sent after it.
    smoke.send_data(A, &payload(509, FEND));
    let marker = b"sent after the over-MTU frame";
    smoke.send_data(A, marker);
    smoke.expect_packet(B, marker);

    smoke.settle();
    smoke.shut_down();
}

/// `len` bytes counting up from `first` modulo 251, so payloads of 251 bytes or more hold every
/// byte value up to 250, including both bytes KISS escapes (FEND and FESC).
fn payload(len: usize, first: u8) -> Vec<u8> {
    (0..len).map(|i| ((usize::from(first) + i) % 251) as u8).collect()
}

struct Smoke {
    hosts: [Host; 2],
    daemons: Daemons,
}

impl Smoke {
    /// Starts both daemons and opens their PTYs as a host would.
    fn start(run_dir: &Path) -> Self {
        let mut daemons = Daemons(Vec::new());
        for spec in &NODES {
            daemons.0.push(Daemon::spawn(run_dir, spec));
        }
        let deadline = Instant::now() + START_TIMEOUT;
        while let Some(daemon) = daemons.0.iter().find(|daemon| !daemon.pty.exists()) {
            assert!(
                Instant::now() < deadline,
                "daemon {} did not create its PTY {} within {START_TIMEOUT:?}",
                daemon.spec.name,
                daemon.pty.display()
            );
            daemons.assert_running();
            thread::sleep(Duration::from_millis(20));
        }
        let hosts = [Host::open(&daemons.0[A]), Host::open(&daemons.0[B])];
        Self { hosts, daemons }
    }

    /// Configures and starts both radios the way `RNodeInterface` does, and waits for both
    /// daemons to report the radio online (firmware `kiss_indicate_radiostate()`).
    fn bring_radios_online(&mut self) {
        let mut config = Vec::new();
        config.extend(kiss_frame(CMD_DETECT, &[DETECT_REQ]));
        config.extend(kiss_frame(CMD_FREQUENCY, &FREQUENCY_HZ.to_be_bytes()));
        config.extend(kiss_frame(CMD_BANDWIDTH, &BANDWIDTH_HZ.to_be_bytes()));
        config.extend(kiss_frame(CMD_TXPOWER, &[TX_POWER_DBM]));
        config.extend(kiss_frame(CMD_SF, &[SPREADING_FACTOR]));
        config.extend(kiss_frame(CMD_CR, &[CODING_RATE]));
        config.extend(kiss_frame(CMD_RADIO_STATE, &[RADIO_STATE_ON]));
        for host in [A, B] {
            self.write(host, &config);
        }
        for host in [A, B] {
            let replies = self.take_through(host, REPLY_TIMEOUT, "radio-online reply", |frame| {
                frame.cmd == CMD_RADIO_STATE && frame.payload == [RADIO_STATE_ON]
            });
            assert!(
                replies
                    .iter()
                    .any(|frame| frame.cmd == CMD_DETECT && frame.payload == [DETECT_RESP]),
                "daemon {} did not answer CMD_DETECT before going online: {replies:?}",
                NODES[host].name
            );
        }
        self.assert_no_errors();
    }

    fn send_data(&mut self, host: usize, payload: &[u8]) {
        self.write(host, &kiss_frame(CMD_DATA, payload));
    }

    /// Waits for the next `CMD_DATA` frame on `host` and checks that it is `payload`, preceded by
    /// the receiving daemon's packet RSSI and SNR, as firmware `receive_callback()` reports a
    /// packet: `kiss_indicate_stat_rssi()`, `kiss_indicate_stat_snr()`, `kiss_write_packet()`.
    fn expect_packet(&mut self, host: usize, payload: &[u8]) {
        let what = format!("{}-byte packet", payload.len());
        let frames = self.take_through(host, PACKET_TIMEOUT, &what, |frame| frame.cmd == CMD_DATA);
        let (data, before) = frames.split_last().expect("take_through returns the matching frame");
        let name = NODES[host].name;
        if let Some(at) = first_difference(&data.payload, payload) {
            panic!(
                "host {name} expected a {}-byte packet but received {} bytes, first difference at byte {at}: {data:?}",
                payload.len(),
                data.payload.len()
            );
        }
        let spec = &NODES[host];
        let rssi = Frame {
            cmd: CMD_STAT_RSSI,
            payload: vec![spec.rssi_byte],
        };
        let snr = Frame {
            cmd: CMD_STAT_SNR,
            payload: vec![spec.snr_byte],
        };
        assert!(
            before.ends_with(&[rssi, snr]),
            "host {name} expected RSSI {:#04x} and SNR {:#04x} right before the {what}, got {before:?}",
            spec.rssi_byte,
            spec.snr_byte
        );
        self.assert_no_errors();
    }

    /// Keeps reading both PTYs for a moment and checks that nothing else was delivered.
    fn settle(&mut self) {
        let until = Instant::now() + SETTLE_TIME;
        while Instant::now() < until {
            self.pump(until);
        }
        self.assert_no_errors();
        for host in &self.hosts {
            let stray: Vec<&Frame> = host.frames.iter().filter(|frame| frame.cmd == CMD_DATA).collect();
            assert!(
                stray.is_empty(),
                "host {} received unexpected packets: {stray:?}",
                host.name
            );
        }
    }

    /// SIGTERM must stop both daemons cleanly, and each must remove its PTY symlink.
    fn shut_down(mut self) {
        for daemon in &self.daemons.0 {
            let pid = Pid::from_raw(i32::try_from(daemon.child.id()).expect("pid fits in pid_t"));
            kill(pid, Signal::SIGTERM).expect("send SIGTERM to a daemon");
        }
        let deadline = Instant::now() + EXIT_TIMEOUT;
        for daemon in &mut self.daemons.0 {
            let name = daemon.spec.name;
            let status = loop {
                if let Some(status) = daemon.child.try_wait().expect("poll daemon exit") {
                    break status;
                }
                assert!(
                    Instant::now() < deadline,
                    "daemon {name} still running {EXIT_TIMEOUT:?} after SIGTERM"
                );
                thread::sleep(Duration::from_millis(20));
            };
            assert!(status.success(), "daemon {name} exited with {status} after SIGTERM");
            assert!(
                fs::symlink_metadata(&daemon.pty).is_err(),
                "daemon {name} left its PTY symlink {} behind",
                daemon.pty.display()
            );
        }
    }

    /// Removes and returns the frames `host` received up to and including the first one that
    /// matches, reading both PTYs until it arrives.
    fn take_through(
        &mut self,
        host: usize,
        timeout: Duration,
        what: &str,
        matches: impl Fn(&Frame) -> bool,
    ) -> Vec<Frame> {
        let deadline = Instant::now() + timeout;
        loop {
            let frames = &mut self.hosts[host].frames;
            if let Some(at) = frames.iter().position(&matches) {
                return frames.drain(..=at).collect();
            }
            assert!(
                Instant::now() < deadline,
                "host {} received no {what} within {timeout:?}; it received {frames:?}",
                NODES[host].name
            );
            self.pump(deadline);
        }
    }

    /// Waits up to 100 ms (and no later than `until`) for output on either PTY and decodes it.
    fn pump(&mut self, until: Instant) {
        let wait = until
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(100));
        let ready = {
            let mut fds = self
                .hosts
                .each_ref()
                .map(|host| PollFd::new(host.port.as_fd(), PollFlags::POLLIN));
            let millis = u16::try_from(wait.as_millis()).unwrap_or(u16::MAX);
            match poll(&mut fds, PollTimeout::from(millis)) {
                Ok(_) | Err(Errno::EINTR) => {}
                Err(e) => panic!("poll the PTYs: {e}"),
            }
            fds.map(|fd| fd.revents().is_some_and(|events| !events.is_empty()))
        };
        for (host, ready) in self.hosts.iter_mut().zip(ready) {
            if ready {
                host.read_available();
            }
        }
        self.daemons.assert_running();
    }

    /// Writes all of `bytes` to `host`'s PTY, reading output meanwhile so neither side stalls.
    fn write(&mut self, host: usize, mut bytes: &[u8]) {
        let deadline = Instant::now() + REPLY_TIMEOUT;
        while !bytes.is_empty() {
            match self.hosts[host].port.write(bytes) {
                Ok(n) if n > 0 => bytes = &bytes[n..],
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::Interrupted => {}
                Err(e) => panic!("write to host {} PTY: {e}", NODES[host].name),
            }
            if !bytes.is_empty() {
                assert!(
                    Instant::now() < deadline,
                    "host {} PTY accepted no input for {REPLY_TIMEOUT:?}",
                    NODES[host].name
                );
                self.pump(Instant::now() + Duration::from_millis(10));
            }
        }
    }

    fn assert_no_errors(&self) {
        for host in &self.hosts {
            assert!(
                host.errors.is_empty(),
                "daemon {} sent CMD_ERROR: {:?}",
                host.name,
                host.errors
            );
        }
    }
}

/// The host side of one daemon's PTY.
struct Host {
    name: &'static str,
    port: File,
    decoder: KissDecoder,
    /// Frames received and not yet taken by [`Smoke::take_through`].
    frames: VecDeque<Frame>,
    /// Every `CMD_ERROR` frame received.
    errors: Vec<Frame>,
}

impl Host {
    fn open(daemon: &Daemon) -> Self {
        let port = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags((OFlag::O_NOCTTY | OFlag::O_NONBLOCK).bits())
            .open(&daemon.pty)
            .unwrap_or_else(|e| panic!("open PTY {}: {e}", daemon.pty.display()));
        Self {
            name: daemon.spec.name,
            port,
            decoder: KissDecoder::default(),
            frames: VecDeque::new(),
            errors: Vec::new(),
        }
    }

    fn read_available(&mut self) {
        let mut buf = [0_u8; 4096];
        loop {
            match self.port.read(&mut buf) {
                Ok(0) => panic!("host {} PTY reached end of file: its daemon closed it", self.name),
                Ok(n) => {
                    for frame in self.decoder.feed(&buf[..n]) {
                        if frame.cmd == CMD_ERROR {
                            self.errors.push(frame.clone());
                        }
                        self.frames.push_back(frame);
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => panic!("read host {} PTY (has its daemon exited?): {e}", self.name),
            }
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Frame {
    cmd: u8,
    payload: Vec<u8>,
}

impl fmt::Debug for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const SHOWN: usize = 16;
        write!(
            f,
            "{:#04x}:{:02x?}",
            self.cmd,
            &self.payload[..self.payload.len().min(SHOWN)]
        )?;
        if self.payload.len() > SHOWN {
            write!(f, "..({} bytes)", self.payload.len())?;
        }
        Ok(())
    }
}

/// A KISS frame as the firmware's host side writes one: FEND, command, escaped payload, FEND.
fn kiss_frame(cmd: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![FEND, cmd];
    for &byte in payload {
        match byte {
            FEND => out.extend([FESC, TFEND]),
            FESC => out.extend([FESC, TFESC]),
            _ => out.push(byte),
        }
    }
    out.push(FEND);
    out
}

/// KISS decoder: bytes before the first FEND are ignored, and FEND both ends and starts frames.
#[derive(Default)]
struct KissDecoder {
    in_frame: bool,
    escape: bool,
    buf: Vec<u8>,
}

impl KissDecoder {
    fn feed(&mut self, bytes: &[u8]) -> Vec<Frame> {
        let mut frames = Vec::new();
        for &byte in bytes {
            if byte == FEND {
                if self.in_frame && !self.buf.is_empty() {
                    let payload = self.buf.split_off(1);
                    frames.push(Frame {
                        cmd: self.buf[0],
                        payload,
                    });
                }
                self.buf.clear();
                self.in_frame = true;
                self.escape = false;
                continue;
            }
            if !self.in_frame {
                continue;
            }
            if self.escape {
                self.escape = false;
                self.buf.push(match byte {
                    TFEND => FEND,
                    TFESC => FESC,
                    other => other,
                });
            } else if byte == FESC {
                self.escape = true;
            } else {
                self.buf.push(byte);
            }
        }
        frames
    }
}

fn first_difference(got: &[u8], want: &[u8]) -> Option<usize> {
    got.iter()
        .zip(want)
        .position(|(g, w)| g != w)
        .or_else(|| (got.len() != want.len()).then_some(got.len().min(want.len())))
}

/// One daemon process, its PTY symlink and its log.
struct Daemon {
    spec: &'static NodeSpec,
    child: Child,
    pty: PathBuf,
    log: PathBuf,
}

impl Daemon {
    fn spawn(run_dir: &Path, spec: &'static NodeSpec) -> Self {
        let file = |suffix: &str| run_dir.join(format!("{}{suffix}", spec.name));
        let (config_path, pty, log) = (file(".toml"), file(".pty"), file(".log"));
        let config = daemon_config(spec, &pty, &file("-settings.toml"));
        let parsed = Config::from_toml(&config).expect("the smoke-test config must match rnode-linux's schema");
        assert_eq!(parsed.radio.driver, RadioDriver::Sim);
        fs::write(&config_path, config).expect("write daemon config");
        let log_file = File::create(&log).expect("create daemon log");
        let stderr = log_file.try_clone().expect("clone daemon log handle");
        let child = Command::new(env!("CARGO_BIN_EXE_rnode-linux"))
            .arg("--config")
            .arg(&config_path)
            .current_dir(run_dir)
            .stdin(Stdio::null())
            .stdout(log_file)
            .stderr(stderr)
            .spawn()
            .expect("spawn rnode-linux");
        Self { spec, child, pty, log }
    }
}

fn daemon_config(spec: &NodeSpec, pty: &Path, settings: &Path) -> String {
    format!(
        r#"[radio]
driver = "sim"

[radio.sim]
air_dir = "{AIR_DIR}"
node_name = "{name}"
rssi_dbm = {rssi}
snr_db = {snr:?}
loss = 0.0

[pty]
symlink = {pty}

[storage]
radio_settings = {settings}

[logging]
log_level = "debug"
"#,
        name = spec.name,
        rssi = spec.rssi_dbm,
        snr = spec.snr_db,
        pty = toml_string(pty),
        settings = toml_string(settings),
    )
}

fn toml_string(path: &Path) -> String {
    let text = path.to_str().expect("run directory paths are UTF-8");
    toml::Value::String(text.to_owned()).to_string()
}

/// Kills and reaps the daemons when dropped, including while a failed assertion unwinds, and
/// then prints their logs if the test is failing.
struct Daemons(Vec<Daemon>);

impl Daemons {
    fn assert_running(&mut self) {
        for daemon in &mut self.0 {
            if let Some(status) = daemon.child.try_wait().expect("poll daemon") {
                panic!("daemon {} exited early with {status}", daemon.spec.name);
            }
        }
    }
}

impl Drop for Daemons {
    fn drop(&mut self) {
        for daemon in &mut self.0 {
            let _ = daemon.child.kill();
            let _ = daemon.child.wait();
        }
        if thread::panicking() {
            for daemon in &self.0 {
                let log = fs::read_to_string(&daemon.log).unwrap_or_else(|e| format!("<unreadable: {e}>"));
                eprintln!(
                    "----- daemon {} log ({}) -----\n{log}",
                    daemon.spec.name,
                    daemon.log.display()
                );
            }
        }
    }
}

/// A fresh run directory under `CARGO_TARGET_TMPDIR` holding the configs, logs, PTY symlinks
/// and the private simulated air. Removed after a pass, kept for inspection after a failure.
struct RunDir(PathBuf);

impl RunDir {
    fn create() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after 1970")
            .as_nanos();
        let dir =
            Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("sim-daemon-smoke-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).expect("create run directory");
        // rnode-sim only joins an air directory that is private to its owner.
        let air = dir.join(AIR_DIR);
        fs::create_dir(&air).expect("create air directory");
        fs::set_permissions(&air, fs::Permissions::from_mode(0o700)).expect("make air directory private");
        Self(dir)
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        if thread::panicking() {
            eprintln!("kept the run directory {}", self.0.display());
        } else {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
