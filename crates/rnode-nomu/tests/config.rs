//! The TOML configuration: the shipped examples, validation and warnings, and the node, SX1262
//! driver and PTY settings it produces.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use nix::unistd::{Gid, Group};
use rnode_core::NodeConfig;
use rnode_core::pa::ExternalPa;
use rnode_nomu::config::{Config, ConfigError, PaModel, RadioDriver, Sx1262Config, TCXO_VOLTAGES};
use rnode_nomu::pty::PtyPermissions;
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

/// HackerGadgets' setup guide gives the AIO V1 and V2 the same SX1262 wiring: SPI1 CE0, RESET 25,
/// BUSY 24, DIO1 26, the RF switch on DIO2 and a TCXO on DIO3. Only the V2 switches the radio's
/// power, on GPIO 16, so the V1 file is the V2 file without that rail. It names the GPIO chip by
/// label, as a V1 may sit on a CM4 or a CM5.
#[test]
fn aio_v1_config_is_the_v2_config_without_the_power_rail() {
    let load = |name: &str| Config::from_toml(&fs::read_to_string(repo_path(name)).unwrap()).unwrap();
    let v1 = load("config.uconsole-aio-v1.toml");
    let v2 = load("config.uconsole-aio-v2.toml");
    assert_eq!(v1.warnings(), Vec::<String>::new());
    let v1_gpio = v1.gpio.as_ref().unwrap();
    assert_eq!(v1_gpio.chip, "pinctrl-rp1");
    assert_eq!(v1_gpio.power, None);

    let mut expected = v2.clone();
    let gpio = expected.gpio.as_mut().unwrap();
    gpio.chip = v1_gpio.chip.clone();
    gpio.power = None;
    // The rail's delay and exit settings do nothing without a rail.
    gpio.power_on_delay_ms = v1_gpio.power_on_delay_ms;
    gpio.power_off_on_exit = v1_gpio.power_off_on_exit;
    assert_eq!(v1, expected);
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

    for example in [
        "config.example.toml",
        "config.uconsole-aio-v1.toml",
        "config.uconsole-aio-v2.toml",
    ] {
        let cfg = Config::from_toml(&fs::read_to_string(repo_path(example)).unwrap()).unwrap();
        assert!(!cfg.node_config().avoid_interference, "{example}");
    }
}

fn config_with_max_tx_power(driver: RadioDriver, dbm: &str) -> Result<Config, ConfigError> {
    let hardware = match driver {
        RadioDriver::Sx1262 => "[spi]\ndevice = \"/dev/spidev0.0\"\n[gpio]\nreset = 1\nbusy = 2\ndio1 = 3\n",
        RadioDriver::Sim => "",
    };
    Config::from_toml(&format!(
        "{hardware}[radio]\ndriver = \"{driver}\"\nmax_tx_power_dbm = {dbm}\n"
    ))
}

#[test]
fn max_tx_power_dbm_sets_the_node_config_ceiling() {
    // Firmware `sx126x::setTxPower()` and the CMD_TXPOWER handler stop an SX1262 without an
    // external PA at 22 dBm; the core's default ceiling is that limit, and both drivers share it.
    assert_eq!(NodeConfig::default().max_tx_power, 22);
    assert_eq!(RadioDriver::Sx1262.max_tx_power_dbm(), 22);
    assert_eq!(RadioDriver::Sim.max_tx_power_dbm(), 22);

    // Unset keeps the behaviour from before the setting existed.
    let omitted = Config::from_toml("[radio]\ndriver = \"sim\"\n").unwrap();
    assert_eq!(omitted.radio.max_tx_power_dbm, None);
    assert_eq!(omitted.node_config(), NodeConfig::default());
    for example in [
        "config.example.toml",
        "config.uconsole-aio-v1.toml",
        "config.uconsole-aio-v2.toml",
    ] {
        let cfg = Config::from_toml(&fs::read_to_string(repo_path(example)).unwrap()).unwrap();
        assert_eq!(cfg.radio.max_tx_power_dbm, None, "{example}");
        assert_eq!(cfg.node_config().max_tx_power, 22, "{example}");
    }

    for driver in [RadioDriver::Sx1262, RadioDriver::Sim] {
        for dbm in [0_u8, 2, 10, 21, 22] {
            let cfg = config_with_max_tx_power(driver, &dbm.to_string())
                .unwrap_or_else(|e| panic!("{driver} with {dbm} dBm rejected: {e}"));
            assert_eq!(cfg.radio.max_tx_power_dbm, Some(i64::from(dbm)));
            assert_eq!(
                cfg.node_config(),
                NodeConfig {
                    max_tx_power: dbm,
                    ..NodeConfig::default()
                },
                "{driver} with {dbm} dBm"
            );
        }
    }
    // Combines with the other node setting; TOML's hex integers work as everywhere in the file.
    let both =
        Config::from_toml("[radio]\ndriver = \"sim\"\navoid_interference = true\nmax_tx_power_dbm = 0x0A\n").unwrap();
    assert_eq!(
        both.node_config(),
        NodeConfig {
            avoid_interference: true,
            max_tx_power: 10,
            ..NodeConfig::default()
        }
    );
}

#[test]
fn max_tx_power_dbm_outside_the_driver_range_is_rejected() {
    for driver in [RadioDriver::Sx1262, RadioDriver::Sim] {
        for dbm in [-1, -9, 23, 28, 37, 127, 255, 1000, i64::MIN, i64::MAX] {
            let err = config_with_max_tx_power(driver, &dbm.to_string()).unwrap_err();
            assert!(matches!(err, ConfigError::Invalid(_)), "{driver} with {dbm}: {err}");
            assert_eq!(
                err.to_string(),
                format!(
                    "invalid config: radio.max_tx_power_dbm = {dbm} is out of range; the {driver} driver allows 0 to 22 dBm"
                )
            );
        }
        // Not an integer: TOML's type error, before validation.
        for text in ["10.5", "\"10\"", "true"] {
            let err = config_with_max_tx_power(driver, text).unwrap_err();
            assert!(matches!(err, ConfigError::Parse { .. }), "{driver} with {text}: {err}");
            assert!(err.to_string().contains("max_tx_power_dbm"), "{err}");
        }
    }
}

#[test]
fn node_config_keeps_an_unvalidated_ceiling_within_the_driver_range() {
    // `Config`'s fields are public, so a caller can bypass `validate()`; the node still never gets a
    // ceiling outside what the driver can produce.
    let mut cfg = Config::from_toml("[radio]\ndriver = \"sim\"\n").unwrap();
    for (dbm, ceiling) in [(-5, 0), (i64::MIN, 0), (23, 22), (i64::MAX, 22), (7, 7)] {
        cfg.radio.max_tx_power_dbm = Some(dbm);
        assert_eq!(cfg.node_config().max_tx_power, ceiling, "{dbm}");
    }
}

#[test]
fn warns_once_when_saved_radio_settings_cannot_persist() {
    let ephemeral = Config::from_toml("[radio]\ndriver = \"sim\"\n").unwrap();
    // Without a file nothing is saved: the radio comes up offline after a restart, and after a
    // host's CMD_RESET too, as `Node::boot()` then finds no settings.
    let expected = "storage.radio_settings is not set: radio settings the host saves with CMD_CONF_SAVE \
                    are not kept, so they are lost on restart and on CMD_RESET";
    assert_eq!(ephemeral.warnings(), [expected]);

    let persistent =
        Config::from_toml("[radio]\ndriver = \"sim\"\n[storage]\nradio_settings = \"settings.toml\"\n").unwrap();
    assert_eq!(persistent.warnings(), Vec::<String>::new());
}

#[test]
fn config_validation_allows_sim_without_hardware_sections() {
    let avoid_cfg = Config::from_toml("[radio]\ndriver = \"sim\"\navoid_interference = true\n").unwrap();
    assert!(avoid_cfg.radio.avoid_interference);

    let gpio_cfg = Config::from_toml("[spi]\ndevice = \"/dev/spidev0.0\"\n[gpio]\nreset = 1\nbusy = 2\ndio1 = 3\npower = 4\npower_on_delay_ms = 250\npower_off_on_exit = true\nrxen = 5\ntxen = 6\n[radio]\ndriver = \"sx1262\"\n").unwrap();
    let gpio = gpio_cfg.gpio.unwrap();
    assert_eq!((gpio.rxen, gpio.txen), (Some(5), Some(6)));
    assert_eq!(gpio.power, Some(4));
    assert_eq!(gpio.power_on_delay_ms, 250);
    assert!(gpio.power_off_on_exit);

    let default_power_cfg = Config::from_toml(
        "[spi]\ndevice = \"/dev/spidev0.0\"\n[gpio]\nreset = 1\nbusy = 2\ndio1 = 3\n[radio]\ndriver = \"sx1262\"\n",
    )
    .unwrap();
    let default_gpio = default_power_cfg.gpio.unwrap();
    assert_eq!((default_gpio.rxen, default_gpio.txen), (None, None));
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
fn sim_crc_error_defaults_to_none_and_is_a_probability() {
    let sim = |table: &str| Config::from_toml(&format!("[radio]\ndriver = \"sim\"\n[radio.sim]\n{table}\n"));
    assert_eq!(sim("").unwrap().radio.sim.crc_error, 0.0);
    assert_eq!(sim("crc_error = 0.25").unwrap().radio.sim.crc_error, 0.25);
    assert_eq!(sim("crc_error = 1.0").unwrap().radio.sim.crc_error, 1.0);
    for value in ["-0.1", "1.5", "nan"] {
        let err = sim(&format!("crc_error = {value}")).unwrap_err().to_string();
        assert!(
            err.contains("radio.sim.crc_error must be between 0.0 and 1.0"),
            "{value}: {err}"
        );
    }
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
            lna_gain_db: 0,
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
    assert!(matches!(err, rnode_nomu::config::ConfigError::Read { .. }));

    let invalid = dir.join("invalid.toml");
    fs::write(&invalid, "[radio\n").unwrap();
    let err = Config::load(&invalid).unwrap_err();
    match err {
        rnode_nomu::config::ConfigError::Parse { path, .. } => assert_eq!(path, invalid),
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

    let err = sim_config_with_pty("group = \"rnode-nomu-no-such-group\"\nmode = \"0660\"\n").unwrap_err();
    assert!(matches!(err, ConfigError::Invalid(_)), "{err}");
    assert!(
        err.to_string()
            .contains("pty.group \"rnode-nomu-no-such-group\" is not a known group"),
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

/// A config with the `sx1262` driver, or the `sim` one, and `radio` appended: `[radio]` keys, then
/// tables such as `[radio.pa]`.
fn config_with_radio(driver: RadioDriver, radio: &str) -> Result<Config, ConfigError> {
    let hardware = match driver {
        RadioDriver::Sx1262 => "[spi]\ndevice = \"/dev/spidev0.0\"\n[gpio]\nreset = 1\nbusy = 2\ndio1 = 3\n",
        RadioDriver::Sim => "",
    };
    Config::from_toml(&format!("{hardware}[radio]\ndriver = \"{driver}\"\n{radio}"))
}

/// The firmware's two Heltec V4 front ends: their tables (`Boards.h` `PA_GC1109_VALUES`,
/// `PA_KCT8103L_VALUES`), `PA_MAX_OUTPUT` of 28 dBm, and the LNA gains `LORA_LNA_GAIN` (17 dB) and
/// `LORA_LNA_KCT8103L_GAIN` (21 dB).
#[test]
fn radio_pa_models_take_the_firmware_tables_and_gains() {
    for (model, name, pa, lna_gain_db) in [
        (PaModel::Gc1109, "gc1109", ExternalPa::gc1109(), 17),
        (PaModel::Kct8103l, "kct8103l", ExternalPa::kct8103l(), 21),
    ] {
        let cfg = config_with_radio(RadioDriver::Sx1262, &format!("[radio.pa]\nmodel = \"{name}\"\n")).unwrap();
        let pa_config = cfg.radio.pa.as_ref().unwrap();
        assert_eq!(pa_config.model(), model);
        assert_eq!(pa_config.max_output_dbm(), 28, "{name}");
        assert_eq!(pa_config.lna_gain_db(), lna_gain_db, "{name}");
        assert_eq!(cfg.radio.tx_power_limit_dbm(), 28, "{name}");
        assert_eq!(
            cfg.node_config(),
            NodeConfig {
                max_tx_power: 28,
                external_pa: Some(pa.clone()),
                external_lna: true,
                ..NodeConfig::default()
            },
            "{name}"
        );
        assert_eq!(
            cfg.radio.sx1262_driver_config().unwrap(),
            rnode_sx126x::Config {
                lna_gain_db,
                ..rnode_sx126x::Config::default()
            },
            "{name}"
        );

        // A lower maximum output, a ceiling below it, and no LNA.
        let cfg = config_with_radio(
            RadioDriver::Sx1262,
            &format!("max_tx_power_dbm = 20\n[radio.pa]\nmodel = \"{name}\"\nmax_output_dbm = 25\nlna_gain_db = 0\n"),
        )
        .unwrap();
        assert_eq!(
            cfg.node_config(),
            NodeConfig {
                max_tx_power: 20,
                external_pa: Some(ExternalPa::new(pa.gain_db(), 25).unwrap()),
                ..NodeConfig::default()
            },
            "{name}"
        );
        assert_eq!(cfg.radio.sx1262_driver_config().unwrap().lna_gain_db, 0, "{name}");
    }
    // Without [radio.pa], neither changes.
    let cfg = config_with_radio(RadioDriver::Sx1262, "").unwrap();
    assert_eq!(cfg.node_config(), NodeConfig::default());
    assert_eq!(
        cfg.radio.sx1262_driver_config().unwrap(),
        rnode_sx126x::Config::default()
    );
}

#[test]
fn radio_pa_custom_takes_its_table_and_maximum() {
    let cfg = config_with_radio(
        RadioDriver::Sx1262,
        "[radio.pa]\nmodel = \"custom\"\ngain_db = [12, 12, 12, 11, 11, 10]\nmax_output_dbm = 15\n",
    )
    .unwrap();
    let pa = ExternalPa::new([12, 12, 12, 11, 11, 10], 15).unwrap();
    assert_eq!(cfg.radio.pa.as_ref().unwrap().model(), PaModel::Custom);
    assert_eq!(
        cfg.node_config(),
        NodeConfig {
            max_tx_power: 15,
            external_pa: Some(pa.clone()),
            ..NodeConfig::default()
        },
        "no LNA by default"
    );
    assert_eq!(cfg.radio.sx1262_driver_config().unwrap().lna_gain_db, 0);

    // The longest table, at the highest gains, reaching its highest output; an LNA.
    let cfg = config_with_radio(
        RadioDriver::Sx1262,
        &format!(
            "[radio.pa]\nmodel = \"custom\"\ngain_db = {:?}\nmax_output_dbm = 40\nlna_gain_db = 30\n",
            [
                40, 39, 38, 37, 36, 35, 34, 33, 32, 31, 30, 29, 28, 27, 26, 25, 24, 23, 22, 21, 20, 19, 18
            ]
        ),
    )
    .unwrap();
    let pa_config = cfg.radio.pa.as_ref().unwrap();
    assert_eq!(pa_config.external_pa().gain_db().len(), 23);
    assert_eq!(pa_config.max_output_dbm(), 40);
    assert!(cfg.node_config().external_lna);
    assert_eq!(cfg.radio.sx1262_driver_config().unwrap().lna_gain_db, 30);
}

/// With the `sim` driver the node maps TX power through the PA as with the SX1262, but the LNA
/// does not apply: the simulated radio reports `rssi_dbm` as it is configured.
#[test]
fn radio_pa_with_the_sim_driver_maps_tx_power_without_the_lna() {
    let cfg = config_with_radio(RadioDriver::Sim, "[radio.pa]\nmodel = \"gc1109\"\n").unwrap();
    assert_eq!(
        cfg.node_config(),
        NodeConfig {
            max_tx_power: 28,
            external_pa: Some(ExternalPa::gc1109()),
            external_lna: false,
            ..NodeConfig::default()
        }
    );
}

#[test]
fn radio_pa_rejects_what_it_cannot_map() {
    let rejected = [
        (
            "model = \"gc1110\"",
            "radio.pa.model must be \"gc1109\", \"kct8103l\" or \"custom\"",
        ),
        ("max_output_dbm = 20", "missing field `model`"),
        (
            "model = \"gc1109\"\nmax_ouput_dbm = 20",
            "unknown field `max_ouput_dbm`",
        ),
        (
            "model = \"gc1109\"\ngain_db = [11]",
            "radio.pa.gain_db is only for model = \"custom\"; model = \"gc1109\" uses the firmware's gain table",
        ),
        (
            "model = \"custom\"\nmax_output_dbm = 20",
            "radio.pa.gain_db is required for model = \"custom\"",
        ),
        (
            "model = \"custom\"\ngain_db = [11]",
            "radio.pa.max_output_dbm is required for model = \"custom\"",
        ),
        (
            "model = \"custom\"\ngain_db = []\nmax_output_dbm = 0",
            "radio.pa.gain_db must have 1 to 23 entries, the gain in dB at modem outputs from 0 dBm up; it has 0",
        ),
        (
            &format!("model = \"custom\"\ngain_db = {:?}\nmax_output_dbm = 20", [10; 24]),
            "radio.pa.gain_db must have 1 to 23 entries, the gain in dB at modem outputs from 0 dBm up; it has 24",
        ),
        (
            "model = \"custom\"\ngain_db = [11, -1]\nmax_output_dbm = 11",
            "radio.pa.gain_db[1] = -1 is out of range: 0 to 40 dB",
        ),
        (
            "model = \"custom\"\ngain_db = [41]\nmax_output_dbm = 20",
            "radio.pa.gain_db[0] = 41 is out of range: 0 to 40 dB",
        ),
        (
            "model = \"custom\"\ngain_db = [11, 12]\nmax_output_dbm = 13",
            "radio.pa.gain_db[1] = 12 after 11: each gain must equal the one before or be 1 dB lower",
        ),
        (
            "model = \"custom\"\ngain_db = [11, 11, 9]\nmax_output_dbm = 11",
            "radio.pa.gain_db[2] = 9 after 11: each gain must equal the one before or be 1 dB lower",
        ),
        (
            "model = \"gc1109\"\nmax_output_dbm = 29",
            "radio.pa.max_output_dbm = 29 is out of range: 0 to 28 dBm, the highest output of the gc1109 gain table",
        ),
        (
            "model = \"kct8103l\"\nmax_output_dbm = -1",
            "radio.pa.max_output_dbm = -1 is out of range: 0 to 28 dBm, the highest output of the kct8103l gain table",
        ),
        (
            "model = \"custom\"\ngain_db = [12, 12, 11]\nmax_output_dbm = 14",
            "radio.pa.max_output_dbm = 14 is out of range: 0 to 13 dBm, the highest output of the custom gain table",
        ),
        (
            "model = \"gc1109\"\nlna_gain_db = 31",
            "radio.pa.lna_gain_db = 31 is out of range: 0 to 30 dB",
        ),
        (
            "model = \"gc1109\"\nlna_gain_db = -17",
            "radio.pa.lna_gain_db = -17 is out of range: 0 to 30 dB",
        ),
    ];
    for driver in [RadioDriver::Sx1262, RadioDriver::Sim] {
        for (table, message) in &rejected {
            let err = config_with_radio(driver, &format!("[radio.pa]\n{table}\n")).unwrap_err();
            assert!(matches!(err, ConfigError::Parse { .. }), "{driver} {table:?}: {err}");
            assert!(err.to_string().contains(message), "{driver} {table:?}: {err}");
        }
    }
}

/// Firmware `serial_callback()` clamps `CMD_TXPOWER` to `PA_MAX_OUTPUT` with `HAS_LORA_PA`: with
/// `[radio.pa]`, its `max_output_dbm` is the default and the upper bound of `max_tx_power_dbm`.
#[test]
fn max_tx_power_dbm_with_a_pa_goes_up_to_its_maximum_output() {
    for driver in [RadioDriver::Sx1262, RadioDriver::Sim] {
        for (pa, limit) in [
            ("model = \"gc1109\"", 28),
            ("model = \"gc1109\"\nmax_output_dbm = 24", 24),
            ("model = \"custom\"\ngain_db = [11, 11, 11]\nmax_output_dbm = 13", 13),
        ] {
            let cfg = config_with_radio(driver, &format!("[radio.pa]\n{pa}\n")).unwrap();
            assert_eq!(cfg.node_config().max_tx_power, limit, "{driver} {pa:?}");
            for dbm in [0, 10, limit] {
                let cfg = config_with_radio(driver, &format!("max_tx_power_dbm = {dbm}\n[radio.pa]\n{pa}\n")).unwrap();
                assert_eq!(cfg.node_config().max_tx_power, dbm, "{driver} {pa:?}");
            }
            let pa_model = cfg.radio.pa.as_ref().unwrap().model();
            for dbm in [i64::from(limit) + 1, -1, 128] {
                let err =
                    config_with_radio(driver, &format!("max_tx_power_dbm = {dbm}\n[radio.pa]\n{pa}\n")).unwrap_err();
                assert!(matches!(err, ConfigError::Invalid(_)), "{driver} {pa:?} {dbm}: {err}");
                assert_eq!(
                    err.to_string(),
                    format!(
                        "invalid config: radio.max_tx_power_dbm = {dbm} is out of range; with the {pa_model} PA of \
                         [radio.pa] it allows 0 to {limit} dBm, its max_output_dbm"
                    )
                );
            }
        }
    }
    // An unvalidated ceiling stays within the PA's maximum too.
    let mut cfg = config_with_radio(
        RadioDriver::Sim,
        "[radio.pa]\nmodel = \"custom\"\ngain_db = [11, 11]\nmax_output_dbm = 12\n",
    )
    .unwrap();
    cfg.radio.max_tx_power_dbm = Some(30);
    assert_eq!(cfg.node_config().max_tx_power, 12);
    assert_eq!(
        cfg.node_config().external_pa,
        Some(ExternalPa::new([11, 11], 12).unwrap())
    );
}
