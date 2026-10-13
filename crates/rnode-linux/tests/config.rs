//! The TOML configuration: the shipped examples, validation and warnings, and the node, SX1262
//! driver and PTY settings it produces.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use nix::unistd::{Gid, Group};
use rnode_core::NodeConfig;
use rnode_linux::config::{Config, ConfigError, RadioDriver, Sx1262Config, TCXO_VOLTAGES};
use rnode_linux::pty::PtyPermissions;
use rnode_sx126x::TcxoVoltage;

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
