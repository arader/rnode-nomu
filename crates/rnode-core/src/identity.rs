//! What the node reports about itself to the host (`CMD_FW_VERSION`, `CMD_PLATFORM`,
//! `CMD_MCU`, `CMD_BOARD`).

/// Platform code reported for rnode-linux. The firmware and Reticulum know `0x90` (AVR),
/// `0x80` (ESP32) and `0x70` (NRF52); Reticulum only special-cases ESP32 and NRF52, so a
/// distinct value is safe and keeps hosts from mistaking this daemon for a microcontroller.
pub const PLATFORM_LINUX: u8 = 0x60;

/// MCU code reported for rnode-linux (the firmware uses `0x91`/`0x92` AVR, `0x81` ESP32,
/// `0x71` NRF52).
pub const MCU_LINUX: u8 = 0x61;

/// Board code reported for rnode-linux: a LoRa modem on a Linux host's SPI bus. Distinct from
/// the firmware's board codes (`0x31`–`0x51`).
pub const BOARD_LINUX_SPI: u8 = 0x60;

/// Firmware version reported to the host: the RNode_Firmware release whose behaviour this
/// implementation follows.
pub const FW_VERSION: (u8, u8) = (1, 0x56);

/// The identity a node reports to the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceIdentity {
    /// `CMD_PLATFORM` value.
    pub platform: u8,
    /// `CMD_MCU` value.
    pub mcu: u8,
    /// `CMD_BOARD` value.
    pub board: u8,
    /// `CMD_FW_VERSION` major version.
    pub fw_major: u8,
    /// `CMD_FW_VERSION` minor version.
    pub fw_minor: u8,
}

impl Default for DeviceIdentity {
    fn default() -> Self {
        Self {
            platform: PLATFORM_LINUX,
            mcu: MCU_LINUX,
            board: BOARD_LINUX_SPI,
            fw_major: FW_VERSION.0,
            fw_minor: FW_VERSION.1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_identity_does_not_collide_with_firmware_platforms() {
        let id = DeviceIdentity::default();
        assert!(![0x70, 0x80, 0x90].contains(&id.platform));
        assert!(![0x71, 0x81, 0x91, 0x92].contains(&id.mcu));
        assert_eq!((id.fw_major, id.fw_minor), (1, 0x56));
    }
}
