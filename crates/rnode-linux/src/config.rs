use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::pty::PtyPermissions;

/// Complete daemon configuration.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub spi: Option<SpiConfig>,
    #[serde(default)]
    pub gpio: Option<GpioConfig>,
    #[serde(default)]
    pub radio: RadioConfig,
    #[serde(default)]
    pub pty: PtyConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SpiConfig {
    pub device: PathBuf,
    #[serde(default = "default_spi_speed")]
    pub speed_hz: u32,
}

fn default_spi_speed() -> u32 {
    8_000_000
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GpioConfig {
    #[serde(default = "default_gpio_chip")]
    pub chip: String,
    /// Optional switched radio power rail enable line, driven high before RESET/BUSY/DIO1 are requested.
    #[serde(default)]
    pub power: Option<u32>,
    /// Delay after asserting the optional power rail, in milliseconds.
    #[serde(default = "default_power_on_delay_ms")]
    pub power_on_delay_ms: u64,
    /// Drive the optional power rail low when the daemon exits. Defaults false to avoid power-cycling on restart.
    #[serde(default)]
    pub power_off_on_exit: bool,
    pub reset: u32,
    pub busy: u32,
    pub dio1: u32,
    #[serde(default)]
    pub rxen: Option<u32>,
}

fn default_gpio_chip() -> String {
    "gpiochip0".to_owned()
}

fn default_power_on_delay_ms() -> u64 {
    100
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RadioConfig {
    #[serde(default = "default_radio_driver")]
    pub driver: RadioDriver,
    /// Interference avoidance at startup (`rnode_core::NodeConfig::avoid_interference`): defer TX
    /// while non-LoRa RSSI is more than 11 dB above the sampled noise floor. Defaults off,
    /// matching the firmware's effective behaviour. Enabling it can avoid transmitting over strong
    /// non-LoRa interference, but also defers TX during initial noise-floor sampling (about
    /// 384 ms after radio start) and whenever the threshold is exceeded. A host can switch it
    /// with `CMD_DIS_IA` while the daemon runs (the change survives `CMD_RESET`); this setting
    /// applies again after a restart.
    #[serde(default)]
    pub avoid_interference: bool,
    #[serde(default)]
    pub sx1262: Sx1262Config,
    #[serde(default)]
    pub sim: SimConfig,
}

impl Default for RadioConfig {
    fn default() -> Self {
        Self {
            driver: RadioDriver::Sx1262,
            avoid_interference: false,
            sx1262: Sx1262Config::default(),
            sim: SimConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RadioDriver {
    Sx1262,
    Sim,
}

impl<'de> Deserialize<'de> for RadioDriver {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "sx1262" => Ok(Self::Sx1262),
            "sim" => Ok(Self::Sim),
            _ => Err(serde::de::Error::custom("radio.driver must be \"sx1262\" or \"sim\"")),
        }
    }
}

fn default_radio_driver() -> RadioDriver {
    RadioDriver::Sx1262
}

/// Canonical spellings of the DIO3 TCXO voltages, one per SX126x `SetDIO3AsTCXOCtrl` choice.
///
/// Only used to word errors: whether a value is accepted is decided by
/// `rnode_sx126x::TcxoVoltage::from_str`, the parser the daemon later configures the driver with.
pub const TCXO_VOLTAGES: &[&str] = &["1.6V", "1.7V", "1.8V", "2.2V", "2.4V", "2.7V", "3.0V", "3.3V"];

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Sx1262Config {
    /// DIO3 TCXO voltage string (for example `"1.8V"`), matching RNode_Firmware's
    /// SX126x TCXO setup in `sx126x::preInit()` / `setTCXO()`. Accepted spellings are exactly
    /// those of `rnode_sx126x::TcxoVoltage::from_str` (e.g. `"1.8V"`, `"1.8v"` or `"1.8"`); the
    /// choices are listed in [`TCXO_VOLTAGES`]. Leave unset for boards with a plain crystal.
    #[serde(default)]
    pub tcxo_voltage: Option<String>,
    /// Route DIO2 to the RF switch, matching firmware boards that call `setDio2AsRfSwitchCtrl(true)`.
    #[serde(default = "default_true")]
    pub dio2_as_rf_switch: bool,
    /// SX126x over-current protection register value; default `0x28` mirrors the firmware
    /// `radio_set_ocp()`/SX1262 module default used for RNode-class boards.
    #[serde(default = "default_ocp")]
    pub ocp: u8,
    /// Optional BUSY wait timeout in milliseconds, after which startup/command I/O fails clearly
    /// instead of hanging. When unset, the daemon uses `rnode_sx126x::Config::default()
    /// .busy_timeout_ms`, matching the 100 ms of firmware `sx126x::waitOnBusy()`.
    #[serde(default)]
    pub busy_timeout_ms: Option<u32>,
    /// Optional TX-done wait timeout floor in milliseconds. When unset, the daemon uses
    /// `rnode_sx126x::Config::default().tx_timeout_ms`, matching firmware `LORA_MODEM_TIMEOUT_MS`.
    #[serde(default)]
    pub tx_timeout_ms: Option<u32>,
}

impl Default for Sx1262Config {
    fn default() -> Self {
        Self {
            tcxo_voltage: None,
            dio2_as_rf_switch: true,
            ocp: default_ocp(),
            busy_timeout_ms: None,
            tx_timeout_ms: None,
        }
    }
}

impl Sx1262Config {
    /// The `rnode-sx126x` driver configuration. Unset timeouts inherit the driver's defaults.
    pub fn driver_config(&self) -> Result<rnode_sx126x::Config, ConfigError> {
        let defaults = rnode_sx126x::Config::default();
        Ok(rnode_sx126x::Config {
            tcxo: self.tcxo_voltage.as_deref().map(parse_tcxo_voltage).transpose()?,
            dio2_as_rf_switch: self.dio2_as_rf_switch,
            ocp: self.ocp,
            busy_timeout_ms: self.busy_timeout_ms.unwrap_or(defaults.busy_timeout_ms),
            tx_timeout_ms: self.tx_timeout_ms.unwrap_or(defaults.tx_timeout_ms),
        })
    }
}

fn parse_tcxo_voltage(tcxo: &str) -> Result<rnode_sx126x::TcxoVoltage, ConfigError> {
    tcxo.parse().map_err(|_| {
        ConfigError::Invalid(format!(
            "unsupported radio.sx1262.tcxo_voltage {tcxo:?}; expected one of {}",
            TCXO_VOLTAGES.join(", ")
        ))
    })
}

fn default_ocp() -> u8 {
    0x28
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SimConfig {
    #[serde(default = "default_air_dir")]
    pub air_dir: PathBuf,
    #[serde(default = "default_node_name")]
    pub node_name: String,
    #[serde(default = "default_rssi_dbm")]
    pub rssi_dbm: i16,
    #[serde(default = "default_snr_db")]
    pub snr_db: f32,
    #[serde(default)]
    pub loss: f32,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            air_dir: default_air_dir(),
            node_name: default_node_name(),
            rssi_dbm: default_rssi_dbm(),
            snr_db: default_snr_db(),
            loss: 0.0,
        }
    }
}

fn default_air_dir() -> PathBuf {
    PathBuf::from("target/rnode-air")
}
fn default_node_name() -> String {
    "rnode-linux".to_owned()
}
fn default_rssi_dbm() -> i16 {
    -70
}
fn default_snr_db() -> f32 {
    7.0
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PtyConfig {
    /// Path at which to publish a symlink to the PTY slave, for a stable device name. A symlink
    /// already there (for example from an earlier run) is replaced; anything else makes startup
    /// fail and is left untouched. On exit the symlink is removed if it still points to this
    /// daemon's PTY.
    #[serde(default)]
    pub symlink: Option<PathBuf>,
    /// Group to hand the PTY slave to, so a host running as another user (e.g. `rnsd`) can open
    /// it. The daemon must be a member (systemd `SupplementaryGroups=`). Set `mode` too: the
    /// slave's default mode (usually `0620`) gives the group no read access.
    #[serde(default)]
    pub group: Option<String>,
    /// Permission bits for the PTY slave as an octal string, e.g. `"0660"`. The owner must keep
    /// read and write access; setuid, setgid and sticky bits are rejected.
    #[serde(default)]
    pub mode: Option<String>,
}

impl PtyConfig {
    /// The ownership and permissions to apply to the PTY slave, with `group` resolved to its id.
    pub fn slave_permissions(&self) -> Result<PtyPermissions, ConfigError> {
        Ok(PtyPermissions {
            gid: self.group.as_deref().map(resolve_group).transpose()?,
            mode: self.mode.as_deref().map(parse_pty_mode).transpose()?,
        })
    }
}

fn resolve_group(name: &str) -> Result<u32, ConfigError> {
    match nix::unistd::Group::from_name(name) {
        Ok(Some(group)) => Ok(group.gid.as_raw()),
        Ok(None) => Err(ConfigError::Invalid(format!("pty.group {name:?} is not a known group"))),
        Err(e) => Err(ConfigError::Invalid(format!(
            "pty.group {name:?} could not be looked up: {e}"
        ))),
    }
}

fn parse_pty_mode(text: &str) -> Result<u32, ConfigError> {
    let invalid = |why: &str| ConfigError::Invalid(format!("pty.mode {text:?} {why}"));
    let mode = Some(text)
        .filter(|t| (1..=6).contains(&t.len()) && t.bytes().all(|b| matches!(b, b'0'..=b'7')))
        .and_then(|t| u32::from_str_radix(t, 8).ok())
        .filter(|mode| *mode <= 0o7777)
        .ok_or_else(|| invalid("is not an octal permission string such as \"0660\""))?;
    if mode & 0o7000 != 0 {
        return Err(invalid("must not set the setuid, setgid or sticky bit"));
    }
    if mode & 0o600 != 0o600 {
        return Err(invalid("must give the owner read and write access (0600)"));
    }
    Ok(mode)
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    /// File for the radio settings the host saves with `CMD_CONF_SAVE` (the firmware's EEPROM).
    /// They are loaded at startup and after a host's `CMD_RESET`, and bring the radio online
    /// without a host. When unset, nothing is saved: the radio comes up offline after a restart or
    /// a `CMD_RESET` until a host configures it, and the daemon warns about this at startup.
    #[serde(default)]
    pub radio_settings: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    #[serde(default = "default_log_level")]
    pub log_level: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            log_level: default_log_level(),
        }
    }
}

fn default_log_level() -> String {
    "info".to_owned()
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("failed to parse {path}: {source}")]
    Parse { path: PathBuf, source: toml::de::Error },
    #[error("invalid config: {0}")]
    Invalid(String),
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let content = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.into(),
            source,
        })?;
        Self::from_toml(&content).map_err(|error| match error {
            ConfigError::Parse { source, .. } => ConfigError::Parse {
                path: path.into(),
                source,
            },
            other => other,
        })
    }

    pub fn from_toml(content: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(content).map_err(|source| ConfigError::Parse {
            path: PathBuf::from("<inline>"),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    /// The `rnode_core::Node` configuration this file selects.
    pub fn node_config(&self) -> rnode_core::NodeConfig {
        rnode_core::NodeConfig {
            avoid_interference: self.radio.avoid_interference,
            ..rnode_core::NodeConfig::default()
        }
    }

    /// Non-fatal configuration problems, logged once at startup.
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if self.storage.radio_settings.is_none() {
            warnings.push(
                "storage.radio_settings is not set: radio settings the host saves with CMD_CONF_SAVE \
                 are not kept, so they are lost on restart and on CMD_RESET"
                    .to_owned(),
            );
        }
        if let Some(text) = self.pty.mode.as_deref() {
            if parse_pty_mode(text).is_ok_and(|mode| mode & 0o006 != 0) {
                warnings.push(format!(
                    "pty.mode {text:?} lets every local user open the RNode PTY; prefer pty.group with \"0660\""
                ));
            }
        } else if self.pty.group.is_some() {
            warnings.push(
                "pty.group is set without pty.mode: the slave keeps its default mode (usually 0620), \
                 which gives the group no read access"
                    .to_owned(),
            );
        }
        warnings
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        match self.radio.driver {
            RadioDriver::Sx1262 => {
                if self.spi.is_none() {
                    return Err(ConfigError::Invalid(
                        "[spi] is required when radio.driver = \"sx1262\"".into(),
                    ));
                }
                if self.gpio.is_none() {
                    return Err(ConfigError::Invalid(
                        "[gpio] is required when radio.driver = \"sx1262\"".into(),
                    ));
                }
                if let Some(tcxo) = self.radio.sx1262.tcxo_voltage.as_deref() {
                    parse_tcxo_voltage(tcxo)?;
                }
            }
            RadioDriver::Sim => {
                if !(0.0..=1.0).contains(&self.radio.sim.loss) {
                    return Err(ConfigError::Invalid(
                        "radio.sim.loss must be between 0.0 and 1.0".into(),
                    ));
                }
            }
        }
        self.pty.slave_permissions()?;
        if let Some(spi) = &self.spi {
            if spi.speed_hz == 0 {
                return Err(ConfigError::Invalid("spi.speed_hz must be greater than zero".into()));
            }
        }
        Ok(())
    }
}

impl fmt::Display for RadioDriver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Sx1262 => "sx1262",
            Self::Sim => "sim",
        })
    }
}
