use rnode_linux::config::Config;

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
frequency = 868_000_000
bandwidth = 250_000
spreading_factor = 10
coding_rate = 7
tx_power = 14
preamble_symbols = 20
enable_crc = false
enable_tcxo = true
tcxo_voltage = "1.8V"

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
    assert_eq!(config.radio.frequency, 868_000_000);
    assert_eq!(config.radio.bandwidth, 250_000);
    assert_eq!(config.radio.spreading_factor, 10);
    assert_eq!(config.radio.coding_rate, 7);
    assert_eq!(config.radio.tx_power, 14);
    assert_eq!(config.radio.preamble_symbols, 20);
    assert!(!config.radio.enable_crc);
    assert!(config.radio.enable_tcxo);
    assert_eq!(config.radio.tcxo_voltage.as_deref(), Some("1.8V"));
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
    assert_eq!(config.radio.frequency, 915_000_000); // default
    assert_eq!(config.radio.bandwidth, 125_000); // default
    assert_eq!(config.radio.spreading_factor, 7); // default
    assert_eq!(config.radio.coding_rate, 5); // default
    assert_eq!(config.radio.tx_power, 2); // default
    assert_eq!(config.radio.preamble_symbols, 18); // default
    assert!(config.radio.enable_crc); // default true
    assert!(!config.radio.enable_tcxo); // default false
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
