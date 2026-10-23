//! The RNode EEPROM that `rnodeconf` reads with `CMD_ROM_READ`.
//!
//! RNode_Firmware keeps its device information and TNC settings in the top
//! [`EEPROM_RESERVED`] bytes of the microcontroller's EEPROM (`ROM.h`), and `kiss_dump_eeprom()`
//! sends all of them, KISS-escaped, in reply to `CMD_ROM_READ` (`eeprom_dump_all()`). Reticulum's
//! `rnodeconf` reads them before almost everything it does, and gives up if no reply comes, if the
//! info lock byte is not [`INFO_LOCK_BYTE`], or if the checksum does not match.
//!
//! rnode-linux has no EEPROM, so the node builds an image from what it has: an info block with
//! the device information, and the TNC settings and interference-avoidance setting the node keeps
//! elsewhere ([`image`]). The built-in info block ([`InfoBlock::unsigned`]) identifies a homebrew
//! RNode ([`PRODUCT_HMBRW`], [`MODEL_FE`]). Both codes are in rnodeconf's tables, and neither names
//! a firmware file, so `rnodeconf --update` stops before it would flash anything. Its signature is
//! all `0xFF`, which no key verifies, so rnodeconf reports the device as unverified.

use core::fmt;

use md5::{Digest, Md5};

use crate::storage::RadioSettings;

/// Size of the image: the bytes the firmware reserves at the top of the EEPROM and sends in reply
/// to `CMD_ROM_READ` (`EEPROM_RESERVED`).
pub const EEPROM_RESERVED: usize = 200;

/// Product code (`ADDR_PRODUCT`).
pub const ADDR_PRODUCT: usize = 0x00;
/// Model code: band, maximum TX power and firmware file in rnodeconf's tables (`ADDR_MODEL`).
pub const ADDR_MODEL: usize = 0x01;
/// Hardware revision (`ADDR_HW_REV`).
pub const ADDR_HW_REV: usize = 0x02;
/// Serial number, 4 bytes big-endian (`ADDR_SERIAL`).
pub const ADDR_SERIAL: usize = 0x03;
/// Manufacturing time in Unix seconds, 4 bytes big-endian (`ADDR_MADE`).
pub const ADDR_MADE: usize = 0x07;
/// MD5 of the [`CHECKSUMMED_SIZE`] bytes from [`ADDR_PRODUCT`] (`ADDR_CHKSUM`).
pub const ADDR_CHKSUM: usize = 0x0B;
/// Signature over the checksum, [`SIGNATURE_LEN`] bytes (`ADDR_SIGNATURE`).
pub const ADDR_SIGNATURE: usize = 0x1B;
/// [`INFO_LOCK_BYTE`] when the device is provisioned (`ADDR_INFO_LOCK`).
pub const ADDR_INFO_LOCK: usize = 0x9B;
/// TNC spreading factor (`ADDR_CONF_SF`).
pub const ADDR_CONF_SF: usize = 0x9C;
/// TNC coding rate (`ADDR_CONF_CR`).
pub const ADDR_CONF_CR: usize = 0x9D;
/// TNC TX power in dBm (`ADDR_CONF_TXP`).
pub const ADDR_CONF_TXP: usize = 0x9E;
/// TNC bandwidth in Hz, 4 bytes big-endian (`ADDR_CONF_BW`).
pub const ADDR_CONF_BW: usize = 0x9F;
/// TNC frequency in Hz, 4 bytes big-endian (`ADDR_CONF_FREQ`).
pub const ADDR_CONF_FREQ: usize = 0xA3;
/// [`CONF_OK_BYTE`] when TNC settings are saved (`ADDR_CONF_OK`).
pub const ADDR_CONF_OK: usize = 0xA7;
/// Interference avoidance: `0x00` on, anything else off (`ADDR_CONF_DIA`; firmware `setup()`).
pub const ADDR_CONF_DIA: usize = 0xB9;

/// How many bytes from [`ADDR_PRODUCT`] the checksum covers (`CHECKSUMMED_SIZE`).
pub const CHECKSUMMED_SIZE: usize = 0x0B;
/// Length of the MD5 checksum at [`ADDR_CHKSUM`].
pub const CHECKSUM_LEN: usize = 16;
/// Length of the signature at [`ADDR_SIGNATURE`]: an RSA-1024 signature.
pub const SIGNATURE_LEN: usize = 128;
/// Length of the info block, from [`ADDR_PRODUCT`] to [`ADDR_INFO_LOCK`] (the range firmware
/// `eeprom_dump_info()` sends, and `rnodeconf --rom` writes).
pub const INFO_LEN: usize = ADDR_INFO_LOCK + 1;

/// The info lock byte of a provisioned device (`INFO_LOCK_BYTE`).
pub const INFO_LOCK_BYTE: u8 = 0x73;
/// The TNC settings byte when settings are saved (`CONF_OK_BYTE`).
pub const CONF_OK_BYTE: u8 = 0x73;
/// What erased EEPROM reads as.
pub const ERASED: u8 = 0xFF;

/// The firmware's product code for homebrew RNodes (`PRODUCT_HMBRW`, `Boards.h`).
pub const PRODUCT_HMBRW: u8 = 0xF0;
/// The firmware's model code for a homebrew board (`MODEL_FE`, `Boards.h`). rnodeconf lists it as
/// "(Band capabilities unknown)" with a maximum of 17 dBm, a figure it only displays, and with no
/// firmware file.
pub const MODEL_FE: u8 = 0xFE;
/// Hardware revision in the built-in info block. The firmware accepts any revision but `0x00` and
/// `0xFF` (`eeprom_hwrev_valid()`).
pub const HW_REV: u8 = 0x01;
/// Manufacturing time in the built-in info block: 2026-10-22 00:00:00 UTC, when rnode-linux began
/// to emulate the EEPROM. It is the same on every installation; there is no manufacturing to
/// record.
pub const MANUFACTURED: u32 = 1_792_627_200;

/// The device information at the start of the EEPROM: what `rnodeconf --info` shows, and what the
/// checksum covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Product code ([`ADDR_PRODUCT`]).
    pub product: u8,
    /// Model code ([`ADDR_MODEL`]).
    pub model: u8,
    /// Hardware revision ([`ADDR_HW_REV`]).
    pub hw_rev: u8,
    /// Serial number ([`ADDR_SERIAL`]).
    pub serial: u32,
    /// Manufacturing time in Unix seconds ([`ADDR_MADE`]).
    pub made: u32,
}

impl DeviceInfo {
    /// The device information rnode-linux reports by default: a homebrew RNode ([`PRODUCT_HMBRW`],
    /// [`MODEL_FE`], revision [`HW_REV`]) with this serial number, made at [`MANUFACTURED`].
    pub const fn homebrew(serial: u32) -> Self {
        Self {
            product: PRODUCT_HMBRW,
            model: MODEL_FE,
            hw_rev: HW_REV,
            serial,
            made: MANUFACTURED,
        }
    }

    /// The [`CHECKSUMMED_SIZE`] bytes at the start of the EEPROM.
    pub fn checksummed_bytes(&self) -> [u8; CHECKSUMMED_SIZE] {
        let mut bytes = [0; CHECKSUMMED_SIZE];
        bytes[ADDR_PRODUCT] = self.product;
        bytes[ADDR_MODEL] = self.model;
        bytes[ADDR_HW_REV] = self.hw_rev;
        bytes[ADDR_SERIAL..ADDR_MADE].copy_from_slice(&self.serial.to_be_bytes());
        bytes[ADDR_MADE..ADDR_CHKSUM].copy_from_slice(&self.made.to_be_bytes());
        bytes
    }

    /// The checksum at [`ADDR_CHKSUM`]: the MD5 of [`Self::checksummed_bytes`], as firmware
    /// `eeprom_checksum_valid()` and rnodeconf's `parse_eeprom()` compute it.
    pub fn checksum(&self) -> [u8; CHECKSUM_LEN] {
        Md5::digest(self.checksummed_bytes()).into()
    }
}

/// The info block: the EEPROM from [`ADDR_PRODUCT`] to [`ADDR_INFO_LOCK`], with the device
/// information, its checksum and signature, and the info lock byte.
#[derive(Clone, PartialEq, Eq)]
pub struct InfoBlock([u8; INFO_LEN]);

impl InfoBlock {
    /// The provisioned, unsigned info block for `info`: its checksum, a signature of
    /// [`SIGNATURE_LEN`] `0xFF` bytes, as erased EEPROM reads, and the info lock byte.
    pub fn unsigned(info: DeviceInfo) -> Self {
        let mut bytes = [ERASED; INFO_LEN];
        bytes[..CHECKSUMMED_SIZE].copy_from_slice(&info.checksummed_bytes());
        bytes[ADDR_CHKSUM..ADDR_SIGNATURE].copy_from_slice(&info.checksum());
        bytes[ADDR_INFO_LOCK] = INFO_LOCK_BYTE;
        Self(bytes)
    }

    /// The bytes of the block, [`ADDR_PRODUCT`] first.
    pub fn as_bytes(&self) -> &[u8; INFO_LEN] {
        &self.0
    }
}

impl fmt::Debug for InfoBlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("InfoBlock(")?;
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        f.write_str(")")
    }
}

/// The [`EEPROM_RESERVED`]-byte image the node sends in reply to `CMD_ROM_READ`.
///
/// - `0x00`–`0x9B` are `info`.
/// - `0x9C`–`0xA7` hold `tnc`, the saved TNC settings, as `eeprom_conf_save()` writes them, with
///   [`CONF_OK_BYTE`] at [`ADDR_CONF_OK`]. Without saved settings they read as erased, `0xFF`,
///   like the EEPROM of a device that never saved any: `eeprom_have_conf()` is false then, and
///   rnodeconf reports normal (host-controlled) mode.
/// - [`ADDR_CONF_DIA`] is `0x00` when interference avoidance is on, as the firmware stores it
///   (`dia_conf_save()`), and `0xFF`, erased, when it is off, the firmware's default.
/// - Everything else reads as erased, `0xFF`: Bluetooth and Wi-Fi off, and the display, NeoPixel
///   and blanking settings unset.
pub fn image(info: &InfoBlock, tnc: Option<&RadioSettings>, avoid_interference: bool) -> [u8; EEPROM_RESERVED] {
    let mut image = [ERASED; EEPROM_RESERVED];
    image[..INFO_LEN].copy_from_slice(info.as_bytes());
    if let Some(settings) = tnc {
        image[ADDR_CONF_SF] = settings.spreading_factor;
        image[ADDR_CONF_CR] = settings.coding_rate;
        image[ADDR_CONF_TXP] = settings.tx_power;
        image[ADDR_CONF_BW..ADDR_CONF_FREQ].copy_from_slice(&settings.bandwidth.to_be_bytes());
        image[ADDR_CONF_FREQ..ADDR_CONF_OK].copy_from_slice(&settings.frequency.to_be_bytes());
        image[ADDR_CONF_OK] = CONF_OK_BYTE;
    }
    if avoid_interference {
        image[ADDR_CONF_DIA] = 0x00;
    }
    image
}
