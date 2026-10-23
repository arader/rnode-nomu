//! An external power amplifier (PA) after the modem: firmware `HAS_LORA_PA`.
//!
//! On a board with a PA between the modem and the antenna, the TX power the host sets with
//! `CMD_TXPOWER` is the power after the PA, the target power. RNode_Firmware v1.86 describes such a
//! PA with a gain table, `tx_gain[]` in `Utilities.h`: the gain in dB the PA adds at each modem
//! output from 0 dBm up, one entry per dB (`PA_GAIN_POINTS` entries, `Boards.h`), and a maximum
//! output, `PA_MAX_OUTPUT`, to which `serial_callback()` clamps `CMD_TXPOWER`. Its
//! `setTXPower()` programs the modem output that reaches the target,
//! `map_target_power_to_modem_output()`, and stores the power that this output gives,
//! `map_modem_output_to_target_power()`, as the TX power that `kiss_indicate_txpower()` reports.
//!
//! [`ExternalPa`] holds such a table and ports both functions exactly, quirks included:
//!
//! - A target below the PA's minimum, its output at a modem output of 0 dBm, maps to a negative
//!   modem output: the target minus that minimum (−11 dBm for a target of 0 dBm through the
//!   GC1109). The modem clamps it to its own range, −9 dBm and up on the SX1262.
//! - Mapping back clamps the modem output to the table, so such a target is reported as the PA's
//!   minimum (11 dBm), not as the power the PA puts out.
//! - A target above the table's highest output maps past the table's end, with the gain of its
//!   last entry; mapping back reports the output of that last entry.
//! - The search takes the first modem output whose output reaches the target. A target that falls
//!   between two outputs of the table, which only a table whose gain grows from one entry to the
//!   next has, maps to a negative modem output as if it were below the minimum; so does a target
//!   that an earlier output overshoots, in a table whose output falls as the drive rises.
//! - The search starts from −9 dBm, which only an empty table would return; [`ExternalPa::new`]
//!   refuses one.
//!
//! The firmware has the tables of the Heltec WiFi LoRa 32 V4 (`BOARD_HELTEC32_V4`), whose front end
//! is a GC1109 or a KCT8103L: [`ExternalPa::gc1109`] and [`ExternalPa::kct8103l`].

use core::fmt;

/// Firmware `PA_GC1109_VALUES` (`Boards.h`, `BOARD_HELTEC32_V4`): the gain in dB of the GC1109
/// front end at modem outputs of 0 to 21 dBm.
pub const GC1109_GAIN_DB: [u8; 22] = [
    11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 10, 10, 9, 9, 8, 7,
];

/// Firmware `PA_KCT8103L_VALUES` (`Boards.h`, `BOARD_HELTEC32_V4`): the gain in dB of the KCT8103L
/// front end at modem outputs of 0 to 21 dBm.
pub const KCT8103L_GAIN_DB: [u8; 22] = [
    13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 12, 12, 11, 11, 10, 9, 8, 7,
];

/// Firmware `PA_MAX_OUTPUT` of the Heltec V4: the highest TX power in dBm a host may set through
/// either front end.
pub const HELTEC32_V4_MAX_OUTPUT_DBM: u8 = 28;

/// The highest power in dBm an [`ExternalPa`] may put out or allow. A node never reports or saves
/// more: [`crate::Node::boot`] refuses saved settings with a TX power from 128 dBm up.
pub const MAX_OUTPUT_DBM: u8 = 127;

/// An external PA: its gain table and the highest TX power a host may set through it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalPa {
    /// The gain in dB at each modem output from 0 dBm up (firmware `tx_gain[]`).
    gain_db: Vec<u8>,
    /// Firmware `PA_MAX_OUTPUT`.
    max_output_dbm: u8,
}

/// Why [`ExternalPa::new`] refused a gain table or a maximum output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalPaError {
    /// The gain table has no entries.
    EmptyGainTable,
    /// The PA's output at a modem output, that output plus its gain, is above [`MAX_OUTPUT_DBM`].
    OutputTooHigh {
        /// The modem output in dBm, the index into the table.
        modem_output_dbm: usize,
        /// The PA's output in dBm there.
        output_dbm: usize,
    },
    /// The maximum output is above [`MAX_OUTPUT_DBM`].
    MaxOutputTooHigh(u8),
}

impl fmt::Display for ExternalPaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::EmptyGainTable => f.write_str("the PA gain table is empty"),
            Self::OutputTooHigh {
                modem_output_dbm,
                output_dbm,
            } => write!(
                f,
                "the PA puts out {output_dbm} dBm at a modem output of {modem_output_dbm} dBm, more than \
                 {MAX_OUTPUT_DBM} dBm"
            ),
            Self::MaxOutputTooHigh(dbm) => {
                write!(
                    f,
                    "the PA's maximum output of {dbm} dBm is more than {MAX_OUTPUT_DBM} dBm"
                )
            }
        }
    }
}

impl std::error::Error for ExternalPaError {}

impl ExternalPa {
    /// A PA with `gain_db[n]` dB of gain at a modem output of `n` dBm, through which a host may set
    /// at most `max_output_dbm`. Every output, `n + gain_db[n]`, and `max_output_dbm` must be at
    /// most [`MAX_OUTPUT_DBM`].
    pub fn new(gain_db: impl Into<Vec<u8>>, max_output_dbm: u8) -> Result<Self, ExternalPaError> {
        let gain_db = gain_db.into();
        if gain_db.is_empty() {
            return Err(ExternalPaError::EmptyGainTable);
        }
        for (modem_output_dbm, &gain) in gain_db.iter().enumerate() {
            let output_dbm = modem_output_dbm + usize::from(gain);
            if output_dbm > usize::from(MAX_OUTPUT_DBM) {
                return Err(ExternalPaError::OutputTooHigh {
                    modem_output_dbm,
                    output_dbm,
                });
            }
        }
        if max_output_dbm > MAX_OUTPUT_DBM {
            return Err(ExternalPaError::MaxOutputTooHigh(max_output_dbm));
        }
        Ok(Self {
            gain_db,
            max_output_dbm,
        })
    }

    /// The Heltec V4's GC1109 front end: [`GC1109_GAIN_DB`], up to
    /// [`HELTEC32_V4_MAX_OUTPUT_DBM`].
    pub fn gc1109() -> Self {
        Self {
            gain_db: GC1109_GAIN_DB.to_vec(),
            max_output_dbm: HELTEC32_V4_MAX_OUTPUT_DBM,
        }
    }

    /// The Heltec V4's KCT8103L front end: [`KCT8103L_GAIN_DB`], up to
    /// [`HELTEC32_V4_MAX_OUTPUT_DBM`].
    pub fn kct8103l() -> Self {
        Self {
            gain_db: KCT8103L_GAIN_DB.to_vec(),
            max_output_dbm: HELTEC32_V4_MAX_OUTPUT_DBM,
        }
    }

    /// The gain in dB at each modem output from 0 dBm up.
    pub fn gain_db(&self) -> &[u8] {
        &self.gain_db
    }

    /// The highest TX power in dBm a host may set (firmware `PA_MAX_OUTPUT`).
    pub fn max_output_dbm(&self) -> u8 {
        self.max_output_dbm
    }

    /// The modem output in dBm for a TX power of `target_dbm` after the PA: firmware
    /// `map_target_power_to_modem_output()`, quirks included (see the module docs). It can be
    /// negative, and outside the modem's range; the modem clamps it.
    pub fn modem_output_dbm(&self, target_dbm: u8) -> i32 {
        let target_dbm = i32::from(target_dbm);
        let last = self.gain_db.len() - 1;
        let mut modem_output_dbm = -9;
        for (i, &gain) in self.gain_db.iter().enumerate() {
            // `new()` keeps the table at 128 entries or fewer, so `i` fits.
            let i_dbm = i as i32;
            let effective_output_dbm = i_dbm + i32::from(gain);
            match effective_output_dbm.cmp(&target_dbm) {
                core::cmp::Ordering::Greater => {
                    modem_output_dbm = -(effective_output_dbm - target_dbm);
                    break;
                }
                core::cmp::Ordering::Equal => {
                    modem_output_dbm = i_dbm;
                    break;
                }
                core::cmp::Ordering::Less if i == last => {
                    modem_output_dbm = i_dbm + (target_dbm - effective_output_dbm);
                    break;
                }
                core::cmp::Ordering::Less => {}
            }
        }
        modem_output_dbm
    }

    /// The TX power in dBm after the PA at a modem output of `modem_output_dbm`: firmware
    /// `map_modem_output_to_target_power()`, which clamps the modem output to the table first.
    pub fn target_power_dbm(&self, modem_output_dbm: i32) -> u8 {
        let last = self.gain_db.len() - 1;
        let index = usize::try_from(modem_output_dbm).map_or(0, |dbm| dbm.min(last));
        // `new()` keeps every output, the index plus its gain, at or below MAX_OUTPUT_DBM.
        (index + usize::from(self.gain_db[index])) as u8
    }
}
