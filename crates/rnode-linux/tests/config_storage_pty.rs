mod common;

use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use embedded_hal::spi::Operation;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::termios::{ControlFlags, FlushArg, InputFlags, LocalFlags, OutputFlags, tcflush, tcgetattr};
use nix::unistd::{Gid, Group, getegid};
use rnode_core::NodeConfig;
use rnode_core::storage::{RadioSettings, Storage};
use rnode_linux::config::{Config, ConfigError, RadioDriver, Sx1262Config, TCXO_VOLTAGES};
use rnode_linux::pty::{PendingHostOutput, PtyPair, PtyPermissions};
use rnode_linux::spi::{SpiSegmentKind, build_segment_plan};
use rnode_linux::storage::TomlStorage;
use rnode_sx126x::TcxoVoltage;
use serial_test::serial;

use common::test_dir;

fn repo_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(name)
}

#[test]
fn parses_example_configs() {
    let generic = fs::read_to_string(repo_path("config.example.toml")).unwrap();
    let generic_cfg = Config::from_toml(&generic).unwrap();
    assert_eq!(generic_cfg.radio.driver, RadioDriver::Sx1262);
    assert!(!generic_cfg.radio.avoid_interference);
    assert_eq!(generic_cfg.radio.sx1262.tcxo_voltage, None);
    assert_eq!(generic_cfg.radio.sx1262.tx_timeout_ms, None);
    assert_eq!(generic_cfg.warnings(), Vec::<String>::new());

    let uconsole = fs::read_to_string(repo_path("config.uconsole-aio-v2.toml")).unwrap();
    let cfg = Config::from_toml(&uconsole).unwrap();
    assert_eq!(cfg.warnings(), Vec::<String>::new());
    let gpio = cfg.gpio.unwrap();
    assert_eq!(gpio.chip, "gpiochip15");
    assert_eq!(gpio.power, Some(16));
    assert_eq!(gpio.power_on_delay_ms, 100);
    assert!(!gpio.power_off_on_exit);
    assert!(!cfg.radio.avoid_interference);
    // The AIO V2's SX1262 has a 1.8 V TCXO on DIO3.
    let tcxo = cfg.radio.sx1262.tcxo_voltage.as_deref().unwrap();
    assert_eq!(tcxo.parse::<TcxoVoltage>(), Ok(TcxoVoltage::V1_8));
}

#[test]
fn avoid_interference_sets_the_node_config() {
    let omitted = Config::from_toml("[radio]\ndriver = \"sim\"\n").unwrap();
    assert!(!omitted.radio.avoid_interference);
    assert_eq!(omitted.node_config(), NodeConfig::default());
    assert!(!NodeConfig::default().avoid_interference);

    for (value, avoid) in [("true", true), ("false", false)] {
        let cfg = Config::from_toml(&format!("[radio]\ndriver = \"sim\"\navoid_interference = {value}\n")).unwrap();
        assert_eq!(
            cfg.node_config(),
            NodeConfig {
                avoid_interference: avoid,
                ..NodeConfig::default()
            },
            "avoid_interference = {value}"
        );
    }

    for example in ["config.example.toml", "config.uconsole-aio-v2.toml"] {
        let cfg = Config::from_toml(&fs::read_to_string(repo_path(example)).unwrap()).unwrap();
        assert!(!cfg.node_config().avoid_interference, "{example}");
    }
}

#[test]
fn warns_once_when_saved_radio_settings_cannot_persist() {
    let ephemeral = Config::from_toml("[radio]\ndriver = \"sim\"\n").unwrap();
    let warnings = ephemeral.warnings();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("storage.radio_settings"), "{}", warnings[0]);
    assert!(warnings[0].contains("CMD_CONF_SAVE"), "{}", warnings[0]);

    let persistent =
        Config::from_toml("[radio]\ndriver = \"sim\"\n[storage]\nradio_settings = \"settings.toml\"\n").unwrap();
    assert_eq!(persistent.warnings(), Vec::<String>::new());
}

#[test]
fn config_validation_allows_sim_without_hardware_sections() {
    let avoid_cfg = Config::from_toml("[radio]\ndriver = \"sim\"\navoid_interference = true\n").unwrap();
    assert!(avoid_cfg.radio.avoid_interference);

    let gpio_cfg = Config::from_toml("[spi]\ndevice = \"/dev/spidev0.0\"\n[gpio]\nreset = 1\nbusy = 2\ndio1 = 3\npower = 4\npower_on_delay_ms = 250\npower_off_on_exit = true\n[radio]\ndriver = \"sx1262\"\n").unwrap();
    let gpio = gpio_cfg.gpio.unwrap();
    assert_eq!(gpio.power, Some(4));
    assert_eq!(gpio.power_on_delay_ms, 250);
    assert!(gpio.power_off_on_exit);

    let default_power_cfg = Config::from_toml(
        "[spi]\ndevice = \"/dev/spidev0.0\"\n[gpio]\nreset = 1\nbusy = 2\ndio1 = 3\n[radio]\ndriver = \"sx1262\"\n",
    )
    .unwrap();
    let default_gpio = default_power_cfg.gpio.unwrap();
    assert_eq!(default_gpio.power, None);
    assert_eq!(default_gpio.power_on_delay_ms, 100);
    assert!(!default_gpio.power_off_on_exit);

    let cfg = Config::from_toml("[radio]\ndriver = \"sim\"\n[radio.sim]\nloss = 0.25\n").unwrap();
    assert_eq!(cfg.radio.driver, RadioDriver::Sim);
    assert!(cfg.spi.is_none());
    assert!(Config::from_toml("[radio]\ndriver = \"sx1262\"\n").is_err());
    assert!(Config::from_toml("[spi]\ndevice = \"/dev/spidev0.0\"\n[radio]\ndriver = \"sx1262\"\n").is_err());
    assert!(Config::from_toml("[spi]\ndevice = \"/dev/spidev0.0\"\nspeed_hz = 0\n[gpio]\nreset = 1\nbusy = 2\ndio1 = 3\n[radio]\ndriver = \"sx1262\"\n").is_err());
    let tx_timeout_cfg = Config::from_toml("[spi]\ndevice = \"/dev/spidev0.0\"\n[gpio]\nreset = 1\nbusy = 2\ndio1 = 3\n[radio]\ndriver = \"sx1262\"\n[radio.sx1262]\ntx_timeout_ms = 12345\n").unwrap();
    assert_eq!(tx_timeout_cfg.radio.sx1262.tx_timeout_ms, Some(12345));
    assert!(Config::from_toml("[radio]\ndriver = \"sim\"\n[radio.sim]\nloss = 1.5\n").is_err());
    assert!(Config::from_toml("[spi]\ndevice = \"/dev/spidev0.0\"\n[gpio]\nreset = 1\nbusy = 2\ndio1 = 3\n[radio]\ndriver = \"sx1262\"\n[radio.sim]\nloss = 50.0\n").is_ok());
}

#[test]
fn sx1262_driver_config_inherits_driver_timeouts_when_unset() {
    // Firmware `sx126x::waitOnBusy()` gives up after 100 ms; `LORA_MODEM_TIMEOUT_MS` is 20E3.
    let driver_defaults = rnode_sx126x::Config::default();
    assert_eq!(driver_defaults.busy_timeout_ms, 100);
    assert_eq!(driver_defaults.tx_timeout_ms, 20_000);
    assert_eq!(Sx1262Config::default().driver_config().unwrap(), driver_defaults);

    let generic = Config::from_toml(&fs::read_to_string(repo_path("config.example.toml")).unwrap()).unwrap();
    assert_eq!(generic.radio.sx1262.busy_timeout_ms, None);
    assert_eq!(generic.radio.sx1262.driver_config().unwrap(), driver_defaults);

    let uconsole = Config::from_toml(&fs::read_to_string(repo_path("config.uconsole-aio-v2.toml")).unwrap()).unwrap();
    let driver = uconsole.radio.sx1262.driver_config().unwrap();
    assert_eq!(driver.tcxo, Some(TcxoVoltage::V1_8));
    assert_eq!(driver.busy_timeout_ms, 100);
    assert_eq!(driver.tx_timeout_ms, 20_000);

    let tuned = Config::from_toml(
        "[spi]\ndevice = \"/dev/spidev0.0\"\n[gpio]\nreset = 1\nbusy = 2\ndio1 = 3\n[radio]\ndriver = \"sx1262\"\n[radio.sx1262]\ntcxo_voltage = \"3.3V\"\ndio2_as_rf_switch = false\nocp = 0x18\nbusy_timeout_ms = 250\ntx_timeout_ms = 12345\n",
    )
    .unwrap();
    assert_eq!(
        tuned.radio.sx1262.driver_config().unwrap(),
        rnode_sx126x::Config {
            tcxo: Some(TcxoVoltage::V3_3),
            dio2_as_rf_switch: false,
            ocp: 0x18,
            busy_timeout_ms: 250,
            tx_timeout_ms: 12_345,
        }
    );
}

fn sx1262_config_with_tcxo(tcxo: &str) -> Result<Config, ConfigError> {
    Config::from_toml(&format!(
        "[spi]\ndevice = \"/dev/spidev0.0\"\n[gpio]\nreset = 1\nbusy = 2\ndio1 = 3\n[radio]\ndriver = \"sx1262\"\n[radio.sx1262]\ntcxo_voltage = {tcxo:?}\n"
    ))
}

#[test]
fn tcxo_voltage_accepts_what_the_driver_parses() {
    // tcxoVoltage codes of the SX126x `SetDIO3AsTCXOCtrl` command (datasheet table 13-35).
    let cases = [
        ("1.6V", 0x00),
        ("1.7v", 0x01),
        ("1.8V", 0x02),
        ("1.8v", 0x02),
        ("1.8", 0x02),
        (" 1.8V ", 0x02),
        ("2.2V", 0x03),
        ("2.4", 0x04),
        ("2.7V", 0x05),
        ("3.0V", 0x06),
        ("3", 0x06),
        ("3.3V", 0x07),
    ];
    for (text, code) in cases {
        let cfg = sx1262_config_with_tcxo(text).unwrap_or_else(|e| panic!("{text:?} rejected: {e}"));
        // main.rs configures the driver by parsing this same string.
        let tcxo = cfg.radio.sx1262.tcxo_voltage.as_deref().unwrap();
        assert_eq!(tcxo.parse::<TcxoVoltage>().unwrap().register_value(), code, "{text:?}");
    }
}

#[test]
fn tcxo_voltage_rejects_other_values_and_lists_the_choices() {
    for text in ["1.9V", "5.0V", "", "1.8 V", "1,8V", "18", "1.8mV"] {
        assert!(text.parse::<TcxoVoltage>().is_err(), "{text:?}");
        let err = sx1262_config_with_tcxo(text).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(_)), "{text:?}: {err}");
        let message = err.to_string();
        assert!(message.contains(&format!("{text:?}")), "{message}");
        assert!(
            message.contains("1.6V, 1.7V, 1.8V, 2.2V, 2.4V, 2.7V, 3.0V, 3.3V"),
            "{message}"
        );
    }

    // The listed choices name every DIO3 voltage code exactly once, in the driver's spelling.
    let mut codes: Vec<u8> = TCXO_VOLTAGES
        .iter()
        .map(|text| text.parse::<TcxoVoltage>().unwrap().register_value())
        .collect();
    codes.sort_unstable();
    assert_eq!(codes, (0x00..=0x07).collect::<Vec<u8>>());
}

#[test]
fn config_load_reports_real_paths() {
    let dir = test_dir("config-load");
    let missing = dir.join("missing.toml");
    let err = Config::load(&missing).unwrap_err();
    assert!(matches!(err, rnode_linux::config::ConfigError::Read { .. }));

    let invalid = dir.join("invalid.toml");
    fs::write(&invalid, "[radio\n").unwrap();
    let err = Config::load(&invalid).unwrap_err();
    match err {
        rnode_linux::config::ConfigError::Parse { path, .. } => assert_eq!(path, invalid),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn toml_storage_round_trip_atomic_and_corrupt() {
    let dir = test_dir("storage");
    let path = dir.join("settings.toml");
    let mut storage = TomlStorage::new(Some(path.clone()));
    assert_eq!(storage.load().unwrap(), None);
    let settings = RadioSettings {
        frequency: 867_200_000,
        bandwidth: 125_000,
        spreading_factor: 8,
        coding_rate: 5,
        tx_power: 14,
    };
    storage.save(&settings).unwrap();
    assert_eq!(storage.load().unwrap(), Some(settings));
    assert_eq!(
        fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains("tmp"))
            .count(),
        0
    );
    fs::write(&path, "not = [valid").unwrap();
    assert_eq!(storage.load().unwrap(), None);
    storage.delete().unwrap();
    assert!(!path.exists());
}

#[test]
#[serial]
fn toml_storage_saves_bare_filename_in_current_directory() {
    struct CwdGuard(PathBuf);
    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.0);
        }
    }

    let dir = test_dir("storage-bare");
    let old = std::env::current_dir().unwrap();
    std::env::set_current_dir(&dir).unwrap();
    let _guard = CwdGuard(old);

    let mut storage = TomlStorage::new(Some(PathBuf::from("settings.toml")));
    let settings = RadioSettings {
        frequency: 867_200_000,
        bandwidth: 125_000,
        spreading_factor: 8,
        coding_rate: 5,
        tx_power: 14,
    };
    storage.save(&settings).unwrap();
    assert_eq!(storage.load().unwrap(), Some(settings));
}

fn sim_config_with_pty(pty: &str) -> Result<Config, ConfigError> {
    Config::from_toml(&format!(
        "[radio]\ndriver = \"sim\"\n[storage]\nradio_settings = \"settings.toml\"\n[pty]\n{pty}"
    ))
}

#[test]
fn pty_group_resolves_at_load_and_mode_parses_as_octal() {
    // gid 0 exists everywhere ("root" on Linux, "wheel" on macOS).
    let group = Group::from_gid(Gid::from_raw(0)).unwrap().unwrap().name;
    let cfg = sim_config_with_pty(&format!("group = {group:?}\nmode = \"0660\"\n")).unwrap();
    assert_eq!(
        cfg.pty.slave_permissions().unwrap(),
        PtyPermissions {
            gid: Some(0),
            mode: Some(0o660)
        }
    );
    assert_eq!(cfg.warnings(), Vec::<String>::new());

    for (text, mode) in [
        ("0660", 0o660),
        ("660", 0o660),
        ("0600", 0o600),
        ("0640", 0o640),
        ("0666", 0o666),
    ] {
        let cfg = sim_config_with_pty(&format!("mode = {text:?}\n")).unwrap_or_else(|e| panic!("{text:?}: {e}"));
        assert_eq!(cfg.pty.slave_permissions().unwrap().mode, Some(mode), "{text:?}");
    }
    let cfg = sim_config_with_pty("").unwrap();
    assert_eq!(cfg.pty.slave_permissions().unwrap(), PtyPermissions::default());
}

#[test]
fn pty_mode_and_group_validation_errors() {
    let rejected = [
        ("", "octal"),
        ("0x660", "octal"),
        ("0668", "octal"),
        ("010660", "octal"),
        ("rw-rw----", "octal"),
        ("+660", "octal"),
        (" 0660", "octal"),
        ("4660", "setuid"),
        ("06600", "setuid"),
        ("02660", "setuid"),
        ("01660", "setuid"),
        ("0460", "owner read and write"),
        ("0260", "owner read and write"),
        ("0", "owner read and write"),
    ];
    for (text, why) in rejected {
        let err = sim_config_with_pty(&format!("mode = {text:?}\n")).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(_)), "{text:?}: {err}");
        let message = err.to_string();
        assert!(message.contains(&format!("pty.mode {text:?}")), "{message}");
        assert!(message.contains(why), "{text:?}: {message}");
    }

    let err = sim_config_with_pty("group = \"rnode-linux-no-such-group\"\nmode = \"0660\"\n").unwrap_err();
    assert!(matches!(err, ConfigError::Invalid(_)), "{err}");
    assert!(
        err.to_string()
            .contains("pty.group \"rnode-linux-no-such-group\" is not a known group"),
        "{err}"
    );
}

#[test]
fn pty_permissions_that_defeat_their_purpose_warn() {
    let world = sim_config_with_pty("mode = \"0666\"\n").unwrap();
    let warnings = world.warnings();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("pty.mode \"0666\""), "{}", warnings[0]);

    let group = Group::from_gid(Gid::from_raw(0)).unwrap().unwrap().name;
    let group_only = sim_config_with_pty(&format!("group = {group:?}\n")).unwrap();
    let warnings = group_only.warnings();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("pty.group is set without pty.mode"),
        "{}",
        warnings[0]
    );
}

#[test]
#[serial]
fn pty_is_raw_idle_poll_times_out_and_data_round_trips() {
    let dir = test_dir("pty");
    let link = dir.join("rnode0");
    let pty = PtyPair::open(Some(&link)).unwrap();
    assert_eq!(fs::read_link(&link).unwrap(), pty.slave_path());

    // Raw before any host configures it: no echo, line editing, signals or CR/LF translation.
    let termios = tcgetattr(pty.slave_fd_for_tests()).unwrap();
    for flag in [
        LocalFlags::ECHO,
        LocalFlags::ICANON,
        LocalFlags::ISIG,
        LocalFlags::IEXTEN,
    ] {
        assert!(!termios.local_flags.contains(flag), "{flag:?}");
    }
    for flag in [
        InputFlags::ICRNL,
        InputFlags::INLCR,
        InputFlags::IGNCR,
        InputFlags::IXON,
    ] {
        assert!(!termios.input_flags.contains(flag), "{flag:?}");
    }
    assert!(!termios.output_flags.contains(OutputFlags::OPOST));
    assert_eq!(termios.control_flags & ControlFlags::CSIZE, ControlFlags::CS8);

    // No host attached: poll() blocks instead of spinning on POLLHUP.
    let mut fds = [PollFd::new(pty.master_fd(), PollFlags::POLLIN)];
    assert_eq!(poll(&mut fds, PollTimeout::from(20_u16)).unwrap(), 0);

    let mut slave = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(pty.slave_path())
        .unwrap();
    slave.write_all(b"abc").unwrap();
    std::thread::sleep(Duration::from_millis(10));
    let mut buf = [0_u8; 8];
    assert_eq!(pty.read(&mut buf).unwrap(), 3);
    assert_eq!(&buf[..3], b"abc");

    pty.write_nonblocking(b"xyz").unwrap();
    let mut out = [0_u8; 8];
    assert_eq!(slave.read(&mut out).unwrap(), 3);
    assert_eq!(&out[..3], b"xyz");

    // A host that disconnects leaves the master idle, not hung up.
    drop(slave);
    let mut fds = [PollFd::new(pty.master_fd(), PollFlags::POLLIN)];
    assert_eq!(poll(&mut fds, PollTimeout::from(20_u16)).unwrap(), 0);
}

/// Issue #47: the slave gets the configured group and mode before its symlink is published.
#[test]
#[serial]
fn pty_slave_gets_configured_group_and_mode() {
    let slave_stat = |pty: &PtyPair| {
        let fd = pty.slave_fd_for_tests().try_clone_to_owned().unwrap();
        File::from(fd).metadata().unwrap()
    };
    let default_gid = slave_stat(&PtyPair::open(None).unwrap()).gid();

    let dir = test_dir("pty-permissions");
    let link = dir.join("rnode0");
    let egid = getegid().as_raw();
    let permissions = PtyPermissions {
        gid: Some(egid),
        mode: Some(0o660),
    };
    let pty = PtyPair::open_with_permissions(Some(&link), permissions).unwrap();
    let stat = slave_stat(&pty);
    assert_eq!(stat.mode() & 0o7777, 0o660);
    assert_eq!(stat.gid(), egid);
    assert_eq!(fs::read_link(&link).unwrap(), pty.slave_path());

    let mode_only = PtyPair::open_with_permissions(
        None,
        PtyPermissions {
            gid: None,
            mode: Some(0o600),
        },
    )
    .unwrap();
    let stat = slave_stat(&mode_only);
    assert_eq!(stat.mode() & 0o7777, 0o600);
    assert_eq!(stat.gid(), default_gid);
}

#[test]
fn pending_host_output_drops_new_frames_when_writer_stalls() {
    let mut output = PendingHostOutput::default();
    output
        .flush_bytes_with(&[0xC0, 0x01, 0xC0, 0xC0, 0x02, 0xC0], |_| Ok(0))
        .unwrap();
    assert!(output.is_empty());
    assert_eq!(output.dropped_frames_for_tests(), 2);
    assert!(output.has_warned_drop_for_tests());
}

#[test]
fn pending_host_output_finishes_partial_tail_before_next_frame() {
    let mut output = PendingHostOutput::default();
    output
        .flush_bytes_with(&[0xC0, 0x01, 0xC0], |bytes| Ok(bytes.len().min(2)))
        .unwrap();
    assert_eq!(output.buffered_bytes(), vec![0xC0]);

    let mut writes = Vec::new();
    output
        .flush_bytes_with(&[0xC0, 0x02, 0xC0], |bytes| {
            writes.push(bytes.to_vec());
            Ok(bytes.len())
        })
        .unwrap();
    assert_eq!(writes, vec![vec![0xC0], vec![0xC0, 0x02, 0xC0]]);
    assert!(output.is_empty());
    assert_eq!(output.dropped_frames_for_tests(), 0);
}

#[test]
fn pending_host_output_warning_rearms_after_successful_write() {
    let mut output = PendingHostOutput::default();
    output.flush_bytes_with(&[0xC0, 0x01, 0xC0], |_| Ok(0)).unwrap();
    assert!(output.has_warned_drop_for_tests());
    output.flush_bytes_with(&[0xC0, 0x02, 0xC0], |_| Ok(0)).unwrap();
    assert!(output.has_warned_drop_for_tests());
    assert_eq!(output.dropped_frames_for_tests(), 2);

    output
        .flush_bytes_with(&[0xC0, 0x03, 0xC0], |bytes| Ok(bytes.len()))
        .unwrap();
    assert!(!output.has_warned_drop_for_tests());
    output.flush_bytes_with(&[0xC0, 0x04, 0xC0], |_| Ok(0)).unwrap();
    assert!(output.has_warned_drop_for_tests());
    assert_eq!(output.dropped_frames_for_tests(), 3);
}

#[test]
fn pending_host_output_flushes_to_real_pty() {
    let pty = PtyPair::open(None).unwrap();
    let mut slave = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(pty.slave_path())
        .unwrap();
    let mut output = PendingHostOutput::default();
    output.flush_bytes_to(&[0xC0, 0x42, 0xC0], &pty).unwrap();
    let mut buf = [0_u8; 3];
    slave.read_exact(&mut buf).unwrap();
    assert_eq!(buf, [0xC0, 0x42, 0xC0]);
    assert!(output.is_empty());
}

/// A host that stops reading stalls the real PTY, and the frames offered then are dropped. After
/// the host flushes its input (pyserial's `Serial.open()` does `tcflush(TCIFLUSH)`), it reads only
/// the unwritten remainder of the frame that was in flight, then fresh frames: nothing dropped is
/// replayed. The PTY is filled through `PendingHostOutput` up to its first drop, and the test then
/// checks exactly what the host reads. It never assumes the PTY stays full: Linux moves PTY data
/// from the flip buffer into the line discipline asynchronously, so space can free up at any time.
#[test]
#[serial]
fn pending_host_output_real_pty_drops_stale_frames_after_host_flush() {
    let pty = PtyPair::open(None).unwrap();
    let mut host = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(pty.slave_path())
        .unwrap();
    let mut stale = vec![0x55; 256];
    stale[0] = 0xC0;
    stale[255] = 0xC0;
    let fresh = [0xC0, 0x99, 0x01, 0xC0];

    let mut output = PendingHostOutput::default();
    for _ in 0..16_384 {
        if output.dropped_frames_for_tests() > 0 {
            break;
        }
        output.flush_bytes_to(&stale, &pty).unwrap();
    }
    assert_eq!(output.dropped_frames_for_tests(), 1, "the PTY never filled up");
    let in_flight = output.buffered_bytes();
    assert!(
        in_flight.len() < stale.len() && stale.ends_with(&in_flight),
        "{in_flight:02X?}"
    );

    tcflush(host.as_fd(), FlushArg::TCIFLUSH).unwrap();
    output.flush_bytes_to(&fresh, &pty).unwrap();
    assert!(output.is_empty());
    assert_eq!(output.dropped_frames_for_tests(), 1);

    let mut expected = in_flight;
    expected.extend_from_slice(&fresh);
    let mut received = vec![0_u8; expected.len()];
    host.read_exact(&mut received).unwrap();
    assert_eq!(received, expected);
    let mut fds = [PollFd::new(host.as_fd(), PollFlags::POLLIN)];
    assert_eq!(
        poll(&mut fds, PollTimeout::ZERO).unwrap(),
        0,
        "stale output was replayed"
    );
}

#[test]
fn spi_segment_plan_preserves_delay_order_for_linux_transaction() {
    let mut read = [0; 2];
    let ops = [
        Operation::Write(&[0x9f]),
        Operation::DelayNs(1_500),
        Operation::Read(&mut read),
    ];
    let plan = build_segment_plan(&ops);
    assert_eq!(plan.len(), 3);
    assert_eq!(plan[0].kind, SpiSegmentKind::Write);
    assert_eq!(plan[0].op_index, Some(0));
    assert_eq!(plan[0].tx, vec![0x9f]);
    assert_eq!(plan[1].kind, SpiSegmentKind::DelayUs(2));
    assert_eq!(plan[1].op_index, None);
    assert_eq!(plan[2].kind, SpiSegmentKind::Read);
    assert_eq!(plan[2].op_index, Some(2));
    assert_eq!(plan[2].tx, vec![0, 0]);
}

#[test]
fn spi_segment_plan_covers_transfer_variants() {
    let mut transfer_read = [0; 4];
    let mut inplace = [0xAA, 0xBB];
    let ops = [
        Operation::Transfer(&mut transfer_read, &[0x01, 0x02]),
        Operation::TransferInPlace(&mut inplace),
    ];
    let plan = build_segment_plan(&ops);
    assert_eq!(plan[0].kind, SpiSegmentKind::Transfer);
    assert_eq!(plan[0].tx, vec![0x01, 0x02, 0x00, 0x00]);
    assert_eq!(plan[0].rx_len, 4);
    assert_eq!(plan[1].kind, SpiSegmentKind::InPlace);
    assert_eq!(plan[1].tx, vec![0xAA, 0xBB]);
    assert_eq!(plan[1].rx_len, 2);
}

#[cfg(target_os = "linux")]
#[test]
fn gpio_resolve_chip_handles_paths_and_names() {
    assert_eq!(
        rnode_linux::gpio::resolve_chip("/dev/gpiochip15").unwrap(),
        PathBuf::from("/dev/gpiochip15")
    );
    assert_eq!(
        rnode_linux::gpio::resolve_chip("gpiochip15").unwrap(),
        PathBuf::from("/dev/gpiochip15")
    );
    assert!(matches!(
        rnode_linux::gpio::resolve_chip("definitely-not-a-gpio-label"),
        Err(rnode_linux::gpio::GpioError::ChipNotFound(_))
    ));
}
