use rnode_linux::config::{Config, RadioParams, RadioSettings};

const FULL_CONFIG: &str = r#"
[spi]
device = "/dev/spidev0.0"
speed_hz = 8_000_000

[gpio]
chip = "gpiochip1"
reset_line = 17
busy_line = 27
dio0_line = 22
rxen_line = 5

[radio]
preamble_symbols = 20
enable_crc = false
enable_tcxo = true
tcxo_voltage = "1.8V"
radio_settings = "/tmp/test_rnode_settings.toml"

[pty]
symlink = "/tmp/test_rnode"

[csma]
enabled = false
slot_min_ms = 10
slot_max_ms = 50
"#;

#[test]
fn parse_full_config() {
    let config = Config::from_str(FULL_CONFIG).unwrap();
    assert_eq!(config.spi.device, "/dev/spidev0.0");
    assert_eq!(config.spi.speed_hz, 8_000_000);
    assert_eq!(config.gpio.chip, "gpiochip1");
    assert_eq!(config.gpio.reset_line, 17);
    assert_eq!(config.gpio.busy_line, 27);
    assert_eq!(config.gpio.dio0_line, 22);
    assert_eq!(config.gpio.rxen_line, Some(5));
    assert_eq!(config.radio.preamble_symbols, 20);
    assert!(!config.radio.enable_crc);
    assert!(config.radio.enable_tcxo);
    assert_eq!(config.radio.tcxo_voltage.as_deref(), Some("1.8V"));
    assert_eq!(config.radio.radio_settings.as_deref(), Some("/tmp/test_rnode_settings.toml"));
    assert_eq!(config.pty.symlink.as_deref(), Some("/tmp/test_rnode"));
    assert!(!config.csma.enabled);
    assert_eq!(config.csma.slot_min_ms, 10);
    assert_eq!(config.csma.slot_max_ms, 50);
}

#[test]
fn parse_minimal_config() {
    let toml = r#"
[spi]
device = "/dev/spidev0.1"

[gpio]
reset_line = 4
busy_line = 5
dio0_line = 6

[radio]

[pty]
"#;
    let config = Config::from_str(toml).unwrap();
    assert_eq!(config.spi.device, "/dev/spidev0.1");
    assert_eq!(config.spi.speed_hz, 16_000_000); // default
    assert_eq!(config.gpio.chip, "gpiochip0"); // default
    assert!(config.gpio.rxen_line.is_none());
    assert_eq!(config.radio.preamble_symbols, 18); // default
    assert!(config.radio.enable_crc); // default true
    assert!(!config.radio.enable_tcxo); // default false
    assert!(config.radio.radio_settings.is_none());
    assert!(config.pty.symlink.is_none());
    // CSMA defaults
    assert!(config.csma.enabled);
    assert_eq!(config.csma.slot_min_ms, 24);
    assert_eq!(config.csma.slot_max_ms, 100);
}

#[test]
fn parse_missing_spi_device_fails() {
    let toml = r#"
[spi]

[gpio]
reset_line = 4
busy_line = 5
dio0_line = 6

[radio]

[pty]
"#;
    assert!(Config::from_str(toml).is_err());
}

#[test]
fn parse_negative_rxen_line() {
    let toml = r#"
[spi]
device = "/dev/spidev0.0"

[gpio]
reset_line = 4
busy_line = 5
dio0_line = 6
rxen_line = -1

[radio]

[pty]
"#;
    let config = Config::from_str(toml).unwrap();
    assert_eq!(config.gpio.rxen_line, Some(-1));
}

#[test]
fn load_nonexistent_file_returns_error() {
    let result = Config::load("/nonexistent/path/config.toml");
    assert!(result.is_err());
}

// --- RadioSettings tests ---

#[test]
fn radio_settings_save_load_roundtrip() {
    let path = "/tmp/rnode_test_settings_roundtrip.toml";
    let settings = RadioSettings {
        frequency: 868_000_000,
        bandwidth: 250_000,
        spreading_factor: 10,
        coding_rate: 7,
        tx_power: 14,
    };
    settings.save(path).unwrap();
    let loaded = RadioSettings::load(path).unwrap();
    assert_eq!(loaded, settings);
    RadioSettings::delete(path).unwrap();
}

#[test]
fn radio_settings_load_nonexistent_returns_none() {
    assert!(RadioSettings::load("/tmp/rnode_test_nonexistent_settings.toml").is_none());
}

#[test]
fn radio_settings_delete_nonexistent_is_ok() {
    assert!(RadioSettings::delete("/tmp/rnode_test_nonexistent_delete.toml").is_ok());
}

#[test]
fn radio_settings_delete_removes_file() {
    let path = "/tmp/rnode_test_settings_delete.toml";
    let settings = RadioSettings {
        frequency: 915_000_000,
        bandwidth: 125_000,
        spreading_factor: 7,
        coding_rate: 5,
        tx_power: 2,
    };
    settings.save(path).unwrap();
    assert!(RadioSettings::load(path).is_some());
    RadioSettings::delete(path).unwrap();
    assert!(RadioSettings::load(path).is_none());
}

// --- RadioParams tests ---

#[test]
fn radio_params_new_is_locked() {
    let params = RadioParams::new();
    assert!(params.is_locked());
}

#[test]
fn radio_params_fully_configured_is_not_locked() {
    let settings = RadioSettings {
        frequency: 915_000_000,
        bandwidth: 125_000,
        spreading_factor: 7,
        coding_rate: 5,
        tx_power: 2,
    };
    let params = RadioParams::from_settings(&settings);
    assert!(!params.is_locked());
}

#[test]
fn radio_params_missing_freq_is_locked() {
    let settings = RadioSettings {
        frequency: 0,
        bandwidth: 125_000,
        spreading_factor: 7,
        coding_rate: 5,
        tx_power: 2,
    };
    let params = RadioParams::from_settings(&settings);
    assert!(params.is_locked());
}

#[test]
fn radio_params_missing_bw_is_locked() {
    let settings = RadioSettings {
        frequency: 915_000_000,
        bandwidth: 0,
        spreading_factor: 7,
        coding_rate: 5,
        tx_power: 2,
    };
    let params = RadioParams::from_settings(&settings);
    assert!(params.is_locked());
}

#[test]
fn radio_params_to_settings_roundtrip() {
    let settings = RadioSettings {
        frequency: 868_000_000,
        bandwidth: 250_000,
        spreading_factor: 10,
        coding_rate: 7,
        tx_power: 14,
    };
    let params = RadioParams::from_settings(&settings);
    assert_eq!(params.to_settings(), settings);
}
