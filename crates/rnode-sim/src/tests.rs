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
fn semtech_time_on_air_low_sf_goldens() {
    let cases = [
        (5, 10, 5, false, 12_096),
        (6, 10, 5, false, 21_632),
        (7, 10, 5, false, 41_216),
        (5, 10, 5, true, 10_816),
        (6, 255, 8, false, 363_648),
        (5, 255, 5, false, 137_536),
    ];
    for (spreading_factor, len, coding_rate, implicit, expected_us) in cases {
        let airtime = time_on_air(ToaParams {
            len,
            bandwidth_hz: 125_000,
            spreading_factor,
            coding_rate,
            preamble_symbols: 8,
            crc: true,
            implicit,
            low_data_rate: false,
        });
        assert_eq!(airtime.as_micros(), expected_us, "SF{spreading_factor}, len={len}");
    }
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
fn socket_path_length_accepts_exact_boundary_and_rejects_one_over() {
    let dir = AirDir::new("socket-boundary");
    let base_len = dir.path.as_os_str().as_bytes().len() + 1 + ".sock".len();
    assert!(base_len < MAX_SOCKET_PATH);
    let exact_name = "n".repeat(MAX_SOCKET_PATH - base_len);
    let exact_path = dir.path.join(format!("{exact_name}.sock"));
    assert_eq!(exact_path.as_os_str().as_bytes().len(), MAX_SOCKET_PATH);
    drop(SimRadio::new(SimConfig::new(&dir.path, &exact_name)).expect("exact boundary path"));

    let too_long_name = format!("{exact_name}n");
    let too_long_path = dir.path.join(format!("{too_long_name}.sock"));
    assert_eq!(too_long_path.as_os_str().as_bytes().len(), MAX_SOCKET_PATH + 1);
    let err = SimRadio::new(SimConfig::new(&dir.path, too_long_name)).expect_err("one byte over");
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
    assert!(carrier.end.duration_since(before) <= Duration::from_micros(MAX_AIRTIME_US) + Duration::from_millis(50));
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

#[test]
fn crc_errors_are_counted_with_the_receiver_signal_instead_of_delivered() {
    let dir = AirDir::new("crc");
    let mut a = radio(&dir, "a");
    let mut cfg = SimConfig::new(&dir.path, "b");
    cfg.rssi_dbm = -73;
    cfg.snr_db = 5.25;
    cfg.crc_error = 1.0;
    let mut b = SimRadio::new(cfg).expect("radio");
    b.begin(915_000_000).expect("begin");
    b.set_bandwidth(500_000).expect("bw");
    b.receive(None).expect("rx");
    let irq = UnixDatagram::from(b.irq_fd().try_clone_to_owned().expect("irq fd"));
    irq.set_nonblocking(true).expect("nonblocking");
    assert_eq!(b.take_rx_crc_errors(), RxCrcErrors::default());

    a.transmit(b"bad", false).expect("tx");
    let deadline = Instant::now() + Duration::from_millis(250);
    let mut errors = RxCrcErrors::default();
    while errors.count == 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(2));
        errors = b.take_rx_crc_errors();
    }
    assert_eq!(
        errors,
        RxCrcErrors {
            count: 1,
            last: Some(RxSignal {
                rssi_dbm: -73,
                snr_raw: 21
            })
        }
    );
    // The receive interrupt fired, as a modem's does, but there is no frame to read.
    assert!(irq.recv(&mut [0u8; 8]).is_ok(), "IRQ raised");
    assert_eq!(b.poll_rx().expect("poll"), None);
    assert_eq!(b.take_rx_crc_errors(), RxCrcErrors::default(), "taken");
}

#[test]
fn seeded_crc_errors_are_deterministic_and_leave_loss_draws_alone() {
    let draws = |loss: f32, crc_error: f32| {
        let mut config = SimConfig::new("target/not-used", "crc");
        config.loss = loss;
        config.crc_error = crc_error;
        config.seed = 1;
        let mut inner = Inner::new(config);
        (0..10)
            .map(|_| (inner.loss_drop(), inner.crc_error()))
            .collect::<Vec<_>>()
    };
    let lost = [true, false, false, true, false, false, false, true, false, true];
    // The sequence of seeded_partial_loss_is_deterministic: one generator, drawn only for a
    // probability strictly between 0 and 1.
    assert_eq!(draws(0.5, 0.0), lost.map(|lost| (lost, false)));
    assert_eq!(draws(0.0, 0.5), lost.map(|failed| (false, failed)));
    assert_eq!(draws(0.0, 1.0), [(false, true); 10]);
}
