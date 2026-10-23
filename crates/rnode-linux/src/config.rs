use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use rnode_core::pa::ExternalPa;
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
    /// Switch the optional power rail off when the daemon exits: drive `reset`, then `rxen`, then
    /// `txen`, then `power` low, so that no line drives the unpowered modem
    /// ([`crate::power::PowerGuard`]).
    /// Defaults false to avoid power-cycling on restart: the rail and the lines then stay as they
    /// are.
    #[serde(default)]
    pub power_off_on_exit: bool,
    pub reset: u32,
    pub busy: u32,
    pub dio1: u32,
    #[serde(default)]
    pub rxen: Option<u32>,
    /// Optional TX-enable line of a PA or RF switch, such as the TXEN of an Ebyte E22-900M30S or
    /// the KCT8103L's CTX: requested low, driven high before each transmission and low before
    /// receive and when the modem sleeps (`rnode_sx126x` crate docs).
    #[serde(default)]
    pub txen: Option<u32>,
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
    /// Highest TX power in dBm the host may set (`rnode_core::NodeConfig::max_tx_power`). Higher
    /// `CMD_TXPOWER` requests and saved settings are clamped to it, and the node reports the
    /// clamped value back, as firmware `kiss_indicate_txpower()` reports the power it applied.
    /// Unset, it is [`RadioConfig::tx_power_limit_dbm`]: the driver's maximum, or with
    /// `[radio.pa]` the PA's maximum output.
    ///
    /// [`Config::validate`] accepts 0 up to that limit:
    /// - Not below 0: the `CMD_TXPOWER` byte is unsigned dBm, as in firmware `serial_callback()`,
    ///   and Reticulum's `RNodeInterface` rejects a `txpower` below 0, so no host asks for less;
    ///   `NodeConfig::max_tx_power` is a `u8` too.
    /// - Not above the limit: the modem, or the PA, cannot go higher, and while the radio is off
    ///   the node echoes a request clamped only to the ceiling. A host asking for, say, 25 dBm
    ///   would be told 25 dBm and transmit at 22, instead of getting the mismatch that the
    ///   default ceiling gives it.
    #[serde(default)]
    pub max_tx_power_dbm: Option<i64>,
    #[serde(default)]
    pub sx1262: Sx1262Config,
    #[serde(default)]
    pub sim: SimConfig,
    /// `[radio.pa]`: the power amplifier between the modem and the antenna, if the board has one,
    /// and the LNA that such front ends put before the modem.
    #[serde(default)]
    pub pa: Option<PaConfig>,
}

impl Default for RadioConfig {
    fn default() -> Self {
        Self {
            driver: RadioDriver::Sx1262,
            avoid_interference: false,
            max_tx_power_dbm: None,
            sx1262: Sx1262Config::default(),
            sim: SimConfig::default(),
            pa: None,
        }
    }
}

impl RadioConfig {
    /// The highest TX power in dBm a host may set before `max_tx_power_dbm` lowers it: the PA's
    /// `max_output_dbm` with `[radio.pa]` (firmware `PA_MAX_OUTPUT`), otherwise the driver's
    /// maximum, [`RadioDriver::max_tx_power_dbm`].
    pub fn tx_power_limit_dbm(&self) -> u8 {
        self.pa
            .as_ref()
            .map_or(self.driver.max_tx_power_dbm(), PaConfig::max_output_dbm)
    }

    /// The `rnode-sx126x` driver configuration: `[radio.sx1262]`, with the LNA gain of
    /// `[radio.pa]`.
    pub fn sx1262_driver_config(&self) -> Result<rnode_sx126x::Config, ConfigError> {
        Ok(rnode_sx126x::Config {
            lna_gain_db: self.pa.as_ref().map_or(0, PaConfig::lna_gain_db),
            ..self.sx1262.driver_config()?
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RadioDriver {
    Sx1262,
    Sim,
}

impl RadioDriver {
    /// The most TX power in dBm the driver's modem can be set to: without `[radio.pa]`, the
    /// default and the upper bound of `radio.max_tx_power_dbm`. Both drivers clamp to 22 dBm, the
    /// SX1262 without an external PA (firmware `sx126x::setTxPower()`, and the `CMD_TXPOWER`
    /// handler's `txp > 22` clamp for SX1262 boards without `HAS_LORA_PA`); the simulator stands
    /// in for an SX1262.
    pub fn max_tx_power_dbm(self) -> u8 {
        match self {
            Self::Sx1262 | Self::Sim => 22,
        }
    }
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
    /// The `rnode-sx126x` driver configuration of this section alone. Unset timeouts inherit the
    /// driver's defaults, and the LNA gain is the driver's default of 0: it comes from
    /// `[radio.pa]`, which [`RadioConfig::sx1262_driver_config`] adds.
    pub fn driver_config(&self) -> Result<rnode_sx126x::Config, ConfigError> {
        let defaults = rnode_sx126x::Config::default();
        Ok(rnode_sx126x::Config {
            tcxo: self.tcxo_voltage.as_deref().map(parse_tcxo_voltage).transpose()?,
            dio2_as_rf_switch: self.dio2_as_rf_switch,
            ocp: self.ocp,
            busy_timeout_ms: self.busy_timeout_ms.unwrap_or(defaults.busy_timeout_ms),
            tx_timeout_ms: self.tx_timeout_ms.unwrap_or(defaults.tx_timeout_ms),
            lna_gain_db: defaults.lna_gain_db,
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

/// The PA models of `[radio.pa] model`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaModel {
    /// The GC1109 front end of the Heltec WiFi LoRa 32 V4 (firmware `LORA_PA_GC1109`).
    Gc1109,
    /// The KCT8103L front end of the Heltec WiFi LoRa 32 V4 (firmware `LORA_PA_KCT8103L`).
    Kct8103l,
    /// A gain table from `gain_db`.
    Custom,
}

impl<'de> Deserialize<'de> for PaModel {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "gc1109" => Ok(Self::Gc1109),
            "kct8103l" => Ok(Self::Kct8103l),
            "custom" => Ok(Self::Custom),
            _ => Err(serde::de::Error::custom(
                "radio.pa.model must be \"gc1109\", \"kct8103l\" or \"custom\"",
            )),
        }
    }
}

impl fmt::Display for PaModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Gc1109 => "gc1109",
            Self::Kct8103l => "kct8103l",
            Self::Custom => "custom",
        })
    }
}

/// Most entries of a custom `radio.pa.gain_db`: one for each modem output from 0 to 22 dBm, the
/// SX1262's range.
pub const PA_GAIN_POINTS_MAX: usize = 23;
/// Highest gain in dB a custom `radio.pa.gain_db` entry may have: more than any LoRa front end
/// adds, to catch typos.
pub const PA_GAIN_DB_MAX: i64 = 40;
/// Highest `radio.pa.lna_gain_db`. The weakest RSSI the SX1262 reports is −127 dBm; less 30 dB it
/// is −157 dBm, the lowest RSSI the host's byte can carry (`RSSI_OFFSET`, 157). A higher gain would
/// wrap weak signals around to strong ones, in the firmware as here.
pub const LNA_GAIN_DB_MAX: i64 = 30;

/// `[radio.pa]`: an external power amplifier after the modem (firmware `HAS_LORA_PA`) and the
/// gain of the LNA before it on receive (firmware `HAS_LORA_LNA`). It is checked as it is read, so
/// a `PaConfig` always describes a PA the node can map TX power through.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "PaToml")]
pub struct PaConfig {
    model: PaModel,
    external_pa: ExternalPa,
    lna_gain_db: u8,
}

/// `[radio.pa]` as written.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PaToml {
    model: PaModel,
    #[serde(default)]
    gain_db: Option<Vec<i64>>,
    #[serde(default)]
    max_output_dbm: Option<i64>,
    #[serde(default)]
    lna_gain_db: Option<i64>,
}

impl TryFrom<PaToml> for PaConfig {
    type Error = String;

    fn try_from(toml: PaToml) -> Result<Self, Self::Error> {
        let model = toml.model;
        let (gain_db, default_max_output_dbm, default_lna_gain_db) = match model {
            PaModel::Gc1109 | PaModel::Kct8103l => {
                if toml.gain_db.is_some() {
                    return Err(format!(
                        "radio.pa.gain_db is only for model = \"custom\"; model = \"{model}\" uses the firmware's \
                         gain table"
                    ));
                }
                let (pa, lna_gain_db) = match model {
                    // Firmware `LORA_LNA_GAIN` and `LORA_LNA_KCT8103L_GAIN` (`Boards.h`).
                    PaModel::Gc1109 => (ExternalPa::gc1109(), 17),
                    _ => (ExternalPa::kct8103l(), 21),
                };
                (pa.gain_db().to_vec(), Some(pa.max_output_dbm()), lna_gain_db)
            }
            PaModel::Custom => {
                let gain_db = toml
                    .gain_db
                    .ok_or("radio.pa.gain_db is required for model = \"custom\"")?;
                (custom_gain_table(&gain_db)?, None, 0)
            }
        };
        // Each gain is at most 1 dB below the one before, so the last output is the highest.
        let highest_output_dbm = gain_db.len() - 1 + usize::from(gain_db[gain_db.len() - 1]);
        let max_output_dbm = match toml.max_output_dbm {
            Some(dbm) => u8::try_from(dbm)
                .ok()
                .filter(|&dbm| usize::from(dbm) <= highest_output_dbm)
                .ok_or_else(|| {
                    format!(
                        "radio.pa.max_output_dbm = {dbm} is out of range: 0 to {highest_output_dbm} dBm, the \
                         highest output of the {model} gain table"
                    )
                })?,
            None => default_max_output_dbm.ok_or("radio.pa.max_output_dbm is required for model = \"custom\"")?,
        };
        let lna_gain_db = match toml.lna_gain_db {
            Some(db) => u8::try_from(db)
                .ok()
                .filter(|&db| i64::from(db) <= LNA_GAIN_DB_MAX)
                .ok_or_else(|| format!("radio.pa.lna_gain_db = {db} is out of range: 0 to {LNA_GAIN_DB_MAX} dB"))?,
            None => default_lna_gain_db,
        };
        let external_pa = ExternalPa::new(gain_db, max_output_dbm).map_err(|e| format!("radio.pa: {e}"))?;
        Ok(Self {
            model,
            external_pa,
            lna_gain_db,
        })
    }
}

/// A custom `radio.pa.gain_db`: 1 to [`PA_GAIN_POINTS_MAX`] entries of 0 to [`PA_GAIN_DB_MAX`] dB,
/// each equal to the one before or 1 dB lower. Then the PA's output, modem output plus gain, rises
/// by 1 or 0 dB with each dB of modem output, as in the firmware's tables, and the firmware's
/// mapping (`rnode_core::pa`) hits every power from the table's lowest output to its highest. With
/// a gain that grows, it would set the modem below the PA's minimum for the powers the table skips;
/// with one that drops faster, it would miss powers that a later entry reaches, and report less
/// than the modem then puts out.
fn custom_gain_table(gain_db: &[i64]) -> Result<Vec<u8>, String> {
    if !(1..=PA_GAIN_POINTS_MAX).contains(&gain_db.len()) {
        return Err(format!(
            "radio.pa.gain_db must have 1 to {PA_GAIN_POINTS_MAX} entries, the gain in dB at modem outputs from \
             0 dBm up; it has {}",
            gain_db.len()
        ));
    }
    let mut table = Vec::with_capacity(gain_db.len());
    for (i, &gain) in gain_db.iter().enumerate() {
        let gain = u8::try_from(gain)
            .ok()
            .filter(|&gain| i64::from(gain) <= PA_GAIN_DB_MAX)
            .ok_or_else(|| format!("radio.pa.gain_db[{i}] = {gain} is out of range: 0 to {PA_GAIN_DB_MAX} dB"))?;
        if let Some(&previous) = table.last() {
            if gain > previous || gain + 1 < previous {
                return Err(format!(
                    "radio.pa.gain_db[{i}] = {gain} after {previous}: each gain must equal the one before or be \
                     1 dB lower, as in the firmware's tables, so that the PA's output rises by 1 or 0 dB per dB \
                     of modem output; RNode_Firmware's mapping, which the daemon follows, gets some powers wrong \
                     otherwise"
                ));
            }
        }
        table.push(gain);
    }
    Ok(table)
}

impl PaConfig {
    /// `model`.
    pub fn model(&self) -> PaModel {
        self.model
    }

    /// The gain table and maximum output the node maps TX power through
    /// (`rnode_core::NodeConfig::external_pa`).
    pub fn external_pa(&self) -> &ExternalPa {
        &self.external_pa
    }

    /// `max_output_dbm`: the highest TX power in dBm a host may set through the PA.
    pub fn max_output_dbm(&self) -> u8 {
        self.external_pa.max_output_dbm()
    }

    /// `lna_gain_db`: the gain in dB that the SX1262 driver subtracts from the RSSI it reads.
    pub fn lna_gain_db(&self) -> u8 {
        self.lna_gain_db
    }
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
    #[serde(default)]
    pub crc_error: f32,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            air_dir: default_air_dir(),
            node_name: default_node_name(),
            rssi_dbm: default_rssi_dbm(),
            snr_db: default_snr_db(),
            loss: 0.0,
            crc_error: 0.0,
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
    ///
    /// With `[radio.pa]` the node maps TX power through the PA's gain table with either driver;
    /// the LNA's noise-floor rule (`NodeConfig::external_lna`) only applies to the SX1262, as the
    /// simulated radio reports `rssi_dbm` as configured, with no LNA gain to vary.
    pub fn node_config(&self) -> rnode_core::NodeConfig {
        let limit = self.radio.tx_power_limit_dbm();
        rnode_core::NodeConfig {
            avoid_interference: self.radio.avoid_interference,
            // `validate()` keeps the setting within 0..=limit; clamping keeps a `Config` built
            // without it within the limit too, and makes the cast lossless.
            max_tx_power: self
                .radio
                .max_tx_power_dbm
                .map_or(limit, |dbm| dbm.clamp(0, i64::from(limit)) as u8),
            external_pa: self.radio.pa.as_ref().map(|pa| pa.external_pa().clone()),
            external_lna: self.radio.driver == RadioDriver::Sx1262
                && self.radio.pa.as_ref().is_some_and(|pa| pa.lna_gain_db() > 0),
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
                if !(0.0..=1.0).contains(&self.radio.sim.crc_error) {
                    return Err(ConfigError::Invalid(
                        "radio.sim.crc_error must be between 0.0 and 1.0".into(),
                    ));
                }
            }
        }
        self.validate_max_tx_power()?;
        self.pty.slave_permissions()?;
        if let Some(spi) = &self.spi {
            if spi.speed_hz == 0 {
                return Err(ConfigError::Invalid("spi.speed_hz must be greater than zero".into()));
            }
        }
        Ok(())
    }

    /// Check `radio.max_tx_power_dbm` against the range given in [`RadioConfig::max_tx_power_dbm`].
    fn validate_max_tx_power(&self) -> Result<(), ConfigError> {
        let Some(dbm) = self.radio.max_tx_power_dbm else {
            return Ok(());
        };
        let max = self.radio.tx_power_limit_dbm();
        if (0..=i64::from(max)).contains(&dbm) {
            return Ok(());
        }
        Err(ConfigError::Invalid(match &self.radio.pa {
            None => format!(
                "radio.max_tx_power_dbm = {dbm} is out of range; the {} driver allows 0 to {max} dBm",
                self.radio.driver
            ),
            Some(pa) => format!(
                "radio.max_tx_power_dbm = {dbm} is out of range; with the {} PA of [radio.pa] it allows 0 to \
                 {max} dBm, its max_output_dbm",
                pa.model()
            ),
        }))
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
