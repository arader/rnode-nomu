//! An EEPROM the storage keeps (#66): unprovisioned until the host writes an info block with
//! `CMD_ROM_WRITE` and locks it, as `rnodeconf --rom` does to sign the device with the user's own
//! key, then that info block.

mod common;

use log::{Level, LevelFilter};

use common::log_capture::capture;
use common::{cmd, lines, plain, split_frames};
use rnode_core::eeprom::{DeviceInfo, INFO_LEN, InfoBlock, InfoError};
use rnode_core::kiss::*;
use rnode_core::{MemoryStorage, Node, NodeConfig, RadioSettings, SimMedium, SimRadio, Storage, StoredInfo};

/// The info block `rnodeconf --rom --product f0 --model fe --hwrev 1` writes for serial number 7,
/// with a signature of `0x5A` bytes.
fn signed_block() -> InfoBlock {
    let mut bytes = *InfoBlock::unsigned(DeviceInfo::homebrew(7)).as_bytes();
    bytes[0x1B..0x9B].fill(0x5A);
    InfoBlock::from_bytes(&bytes).unwrap()
}

/// A booted node whose storage keeps an info block, holding `info`.
fn node_holding(info: StoredInfo) -> Node<SimRadio, MemoryStorage> {
    let mut storage = MemoryStorage::default();
    storage.set_info(info);
    let mut n = Node::new(SimMedium::new().radio(), storage, NodeConfig::default(), 1);
    n.boot(0);
    n.take_host_output();
    n
}

/// The image `n` sends for `CMD_ROM_READ`, unescaped.
fn read_image<S: Storage>(n: &mut Node<SimRadio, S>) -> Vec<u8> {
    n.handle_host_input(&plain(CMD_ROM_READ, &[0x00]), 0);
    let frames = split_frames(&n.take_host_output());
    assert_eq!(frames.len(), 1, "{frames:02X?}");
    assert_eq!(frames[0].0, CMD_ROM_READ);
    frames[0].1.clone()
}

/// Writes `bytes` from address 0 on, one `CMD_ROM_WRITE` each, as rnodeconf does, and returns the
/// replies.
fn write_from_0<S: Storage>(n: &mut Node<SimRadio, S>, bytes: &[u8]) -> Vec<u8> {
    for (addr, &byte) in bytes.iter().enumerate() {
        n.handle_host_input(&cmd(CMD_ROM_WRITE, &[addr as u8, byte]), 0);
    }
    n.take_host_output()
}

fn locked_reply() -> Vec<u8> {
    plain(CMD_ERROR, &[ERROR_EEPROM_LOCKED])
}

/// Firmware `validate_status()` only runs a device whose EEPROM is locked, has a valid product,
/// model and hardware revision, and a correct checksum; the node takes the homebrew RNode only.
#[test]
fn info_blocks_are_checked_like_the_firmware_checks_them_at_boot() {
    let good = *signed_block().as_bytes();
    assert_eq!(
        InfoBlock::from_bytes(&good).unwrap().validate(),
        Ok(DeviceInfo::homebrew(7))
    );
    assert_eq!(signed_block().device_info(), DeviceInfo::homebrew(7));

    let changed = |at: usize, byte: u8| {
        let mut bytes = good;
        bytes[at] = byte;
        InfoBlock::from_bytes(&bytes)
    };
    let cases = [
        (
            InfoBlock::from_bytes(&good[..INFO_LEN - 1]),
            InfoError::Length(155),
            "it is 155 bytes long, not 156",
        ),
        (
            InfoBlock::from_bytes(&[&good[..], &[0x73]].concat()),
            InfoError::Length(157),
            "it is 157 bytes long, not 156",
        ),
        (
            changed(0x9B, 0xFF),
            InfoError::NotLocked(0xFF),
            "its info lock byte is 0xFF, not 0x73",
        ),
        (
            changed(0x00, 0x03),
            InfoError::Device {
                product: 0x03,
                model: 0xFE,
            },
            "product 0x03 and model 0xFE are not the homebrew RNode (0xF0, 0xFE) that rnode-linux is",
        ),
        (
            changed(0x01, 0xFF),
            InfoError::Device {
                product: 0xF0,
                model: 0xFF,
            },
            "product 0xF0 and model 0xFF are not the homebrew RNode (0xF0, 0xFE) that rnode-linux is",
        ),
        (
            changed(0x02, 0x00),
            InfoError::HwRev(0x00),
            "hardware revision 0x00 is not valid",
        ),
        (
            changed(0x02, 0xFF),
            InfoError::HwRev(0xFF),
            "hardware revision 0xFF is not valid",
        ),
        // A serial number that does not match the checksum, and a checksum that does not match.
        (
            changed(0x06, 0x08),
            InfoError::Checksum,
            "its checksum is not the MD5 of its device information",
        ),
        (
            changed(0x1A, good[0x1A] ^ 0x01),
            InfoError::Checksum,
            "its checksum is not the MD5 of its device information",
        ),
    ];
    for (result, error, message) in cases {
        assert_eq!(result, Err(error));
        assert_eq!(error.to_string(), message);
    }
    // The signature is rnodeconf's to check.
    assert!(changed(0x50, 0x00).is_ok());
    assert_eq!(InfoBlock::erased().as_bytes(), &[0xFF; INFO_LEN]);
    assert_eq!(InfoBlock::erased().validate(), Err(InfoError::NotLocked(0xFF)));
}

/// Without a stored info block, the image reads erased up to the info lock byte, so rnodeconf
/// reports the EEPROM as invalid and `--rom` writes one. The settings around it stay live.
#[test]
fn an_unprovisioned_eeprom_reads_erased_and_takes_writes_to_the_info_block() {
    let settings = RadioSettings {
        frequency: 914_875_000,
        bandwidth: 125_000,
        spreading_factor: 8,
        coding_rate: 5,
        tx_power: 2,
    };
    let mut storage = MemoryStorage::with_settings(settings);
    storage.set_info(StoredInfo::Empty);
    let mut n = Node::new(SimMedium::new().radio(), storage, NodeConfig::default(), 1);
    n.boot(0);
    n.take_host_output();
    let image = read_image(&mut n);
    assert_eq!(image[..0x9C], [0xFF; 0x9C]);
    assert_eq!(
        image[0x9C..=0xA7],
        [8, 5, 2, 0x00, 0x01, 0xE8, 0x48, 0x36, 0x87, 0xE2, 0x78, 0x73]
    );

    let cap = capture(LevelFilter::Trace);
    // FEND and FESC arrive escaped, as rnodeconf's write_eeprom() sends them.
    assert!(write_from_0(&mut n, &[0xF0, 0xFE, 0x01]).is_empty());
    n.handle_host_input(&cmd(CMD_ROM_WRITE, &[0x10, FEND]), 0);
    n.handle_host_input(&cmd(CMD_ROM_WRITE, &[0x9A, FESC]), 0);
    // An info lock byte other than INFO_LOCK_BYTE is written, and locks nothing.
    n.handle_host_input(&cmd(CMD_ROM_WRITE, &[0x9B, 0x00]), 0);
    assert!(n.take_host_output().is_empty());
    assert!(cap.take().is_empty());

    let image = read_image(&mut n);
    let mut expected = [0xFF; 0x9C];
    expected[..3].copy_from_slice(&[0xF0, 0xFE, 0x01]);
    expected[0x10] = FEND;
    expected[0x9A] = FESC;
    expected[0x9B] = 0x00;
    assert_eq!(image[..0x9C], expected);
    assert_eq!(
        image[0x9C..=0xA7],
        [8, 5, 2, 0x00, 0x01, 0xE8, 0x48, 0x36, 0x87, 0xE2, 0x78, 0x73]
    );
    assert_eq!(n.storage().info(), &StoredInfo::Empty);
}

/// The settings beyond the info block come from the saved settings and the node's
/// configuration, so writes there are refused, where the firmware takes addresses up to 199.
#[test]
fn writes_beyond_the_info_block_are_refused() {
    let mut n = node_holding(StoredInfo::Empty);
    let cap = capture(LevelFilter::Trace);
    for addr in [0x9C, 0xA7, 0xB9, 0xC7, 0xC8, 0xFF] {
        n.handle_host_input(&cmd(CMD_ROM_WRITE, &[addr, 0x73]), 0);
        assert_eq!(n.take_host_output(), locked_reply(), "{addr:#04X}");
    }
    let warnings: Vec<String> = cap.take().into_iter().map(|r| r.message).collect();
    assert_eq!(
        warnings,
        ["0x9C", "0xA7", "0xB9", "0xC7", "0xC8", "0xFF"].map(|addr| format!(
            "refusing CMD_ROM_WRITE to EEPROM address {addr}: only the info block, 0x00 to 0x9B, can be written; \
             sent ERROR_EEPROM_LOCKED"
        ))
    );
    assert_eq!(read_image(&mut n), [0xFF; 200]);
}

/// The info lock byte locks a valid info block: the storage keeps it, the node reports it, and
/// the EEPROM takes no more writes, as `eeprom_write()` refuses them once `eeprom_info_locked()`.
#[test]
fn the_info_lock_byte_locks_a_valid_info_block_and_the_storage_keeps_it() {
    let mut n = node_holding(StoredInfo::Empty);
    let block = signed_block();
    let cap = capture(LevelFilter::Info);
    assert!(write_from_0(&mut n, block.as_bytes()).is_empty());
    assert_eq!(n.storage().info(), &StoredInfo::Locked(block.clone()));
    assert_eq!(
        lines(&cap.take()),
        [(
            Level::Info,
            "host provisioned the EEPROM: product 0xF0, model 0xFE, hardware revision 1, serial number 0x00000007"
        )]
    );
    assert_eq!(read_image(&mut n)[..INFO_LEN], block.as_bytes()[..]);

    for (addr, byte) in [(0x00, 0x00), (0x9B, 0x00), (0x9C, 0x00)] {
        n.handle_host_input(&cmd(CMD_ROM_WRITE, &[addr, byte]), 0);
        assert_eq!(n.take_host_output(), locked_reply(), "{addr:#04X}");
    }
    assert_eq!(
        cap.take().len(),
        3,
        "one warning for each refused write: the EEPROM is locked"
    );
    assert_eq!(n.storage().info(), &StoredInfo::Locked(block.clone()));
    assert_eq!(read_image(&mut n)[..INFO_LEN], block.as_bytes()[..]);
}

/// An info block the node would not load, here another product and model, is not locked: the
/// node refuses the info lock byte, and the host can write a valid one without a restart.
#[test]
fn an_invalid_info_block_is_not_locked() {
    let mut n = node_holding(StoredInfo::Empty);
    let mut bytes = *signed_block().as_bytes();
    bytes[0x00] = 0x03;
    bytes[0x01] = 0xA9;
    let cap = capture(LevelFilter::Trace);
    assert_eq!(write_from_0(&mut n, &bytes), locked_reply());
    assert_eq!(
        lines(&cap.take()),
        [(
            Level::Warn,
            "refusing to lock the EEPROM: product 0x03 and model 0xA9 are not the homebrew RNode (0xF0, 0xFE) that \
             rnode-linux is; it stays unprovisioned; sent ERROR_EEPROM_LOCKED"
        )]
    );
    assert_eq!(n.storage().info(), &StoredInfo::Empty);
    let mut expected = bytes;
    expected[0x9B] = 0xFF;
    assert_eq!(read_image(&mut n)[..INFO_LEN], expected[..]);

    assert!(write_from_0(&mut n, signed_block().as_bytes()).is_empty());
    assert_eq!(n.storage().info(), &StoredInfo::Locked(signed_block()));
}

/// The bytes written into an unprovisioned EEPROM survive `CMD_RESET`, as EEPROM survives the
/// firmware's reboot. The node loads the info block again at the reset and for every
/// `CMD_ROM_READ`, so one stored or deleted meanwhile shows at once.
#[test]
fn the_info_block_is_loaded_again_for_each_read_and_at_reset() {
    let mut n = node_holding(StoredInfo::Empty);
    assert!(write_from_0(&mut n, &[0xF0, 0xFE]).is_empty());
    n.handle_host_input(&plain(CMD_RESET, &[CMD_RESET_BYTE]), 0);
    assert!(n.take_host_output().is_empty());
    assert_eq!(read_image(&mut n)[..3], [0xF0, 0xFE, 0xFF]);

    // A stored info block replaces what was written, which is then forgotten.
    n.storage_mut().set_info(StoredInfo::Locked(signed_block()));
    assert_eq!(read_image(&mut n)[..INFO_LEN], signed_block().as_bytes()[..]);
    n.handle_host_input(&cmd(CMD_ROM_WRITE, &[0x02, 0x02]), 0);
    assert_eq!(n.take_host_output(), locked_reply());

    // Deleting it makes the EEPROM unprovisioned again: rnodeconf --rom can sign it anew.
    n.storage_mut().set_info(StoredInfo::Empty);
    assert_eq!(read_image(&mut n)[..INFO_LEN], [0xFF; INFO_LEN]);
    assert!(write_from_0(&mut n, signed_block().as_bytes()).is_empty());
    assert_eq!(n.storage().info(), &StoredInfo::Locked(signed_block()));

    // A reset loads it too.
    n.storage_mut().set_info(StoredInfo::Empty);
    n.handle_host_input(&plain(CMD_RESET, &[CMD_RESET_BYTE]), 0);
    assert!(write_from_0(&mut n, &[0xF0]).is_empty());
}

/// `rnodeconf --eeprom-wipe` is refused whatever the EEPROM holds; to provision again, the user
/// deletes the stored info block.
#[test]
fn the_wipe_is_refused_in_every_eeprom_state() {
    for info in [
        StoredInfo::NotKept,
        StoredInfo::Empty,
        StoredInfo::Locked(signed_block()),
    ] {
        let mut n = node_holding(info.clone());
        let before = read_image(&mut n);
        n.handle_host_input(&plain(CMD_UNLOCK_ROM, &[ROM_UNLOCK_BYTE]), 0);
        assert_eq!(n.take_host_output(), locked_reply(), "{info:?}");
        assert_eq!(n.storage().info(), &info);
        assert_eq!(read_image(&mut n), before, "{info:?}");
    }
}

/// A storage whose info block operations fail.
#[derive(Debug, Default)]
struct FailingStorage {
    settings: MemoryStorage,
    fail_load: bool,
}

impl Storage for FailingStorage {
    type Error = &'static str;

    fn load(&mut self) -> Result<Option<RadioSettings>, Self::Error> {
        Ok(self.settings.load().unwrap())
    }

    fn save(&mut self, settings: &RadioSettings) -> Result<(), Self::Error> {
        self.settings.save(settings).unwrap();
        Ok(())
    }

    fn delete(&mut self) -> Result<(), Self::Error> {
        self.settings.delete().unwrap();
        Ok(())
    }

    fn load_info(&mut self) -> Result<StoredInfo, Self::Error> {
        if self.fail_load {
            Err("disk on fire")
        } else {
            Ok(StoredInfo::Empty)
        }
    }

    fn save_info(&mut self, _info: &InfoBlock) -> Result<(), Self::Error> {
        Err("read-only file system")
    }
}

/// The firmware cannot fail to write its EEPROM. When the storage cannot keep the info block,
/// the node refuses the info lock byte and the EEPROM stays unprovisioned; one it cannot read
/// leaves the EEPROM unprovisioned too.
#[test]
fn storage_errors_leave_the_eeprom_unprovisioned() {
    let mut n = Node::new(
        SimMedium::new().radio(),
        FailingStorage::default(),
        NodeConfig::default(),
        1,
    );
    n.boot(0);
    let cap = capture(LevelFilter::Trace);
    assert_eq!(write_from_0(&mut n, signed_block().as_bytes()), locked_reply());
    assert_eq!(
        lines(&cap.take()),
        [(
            Level::Warn,
            "failed to save the EEPROM info block: read-only file system; the EEPROM stays unprovisioned; sent \
             ERROR_EEPROM_LOCKED"
        )]
    );
    assert_eq!(read_image(&mut n)[0x9B], 0xFF);

    n.storage_mut().fail_load = true;
    assert_eq!(read_image(&mut n)[0x9B], 0xFF);
    assert_eq!(
        lines(&cap.take()),
        [(
            Level::Warn,
            "failed to load the EEPROM info block: disk on fire; the EEPROM reads as unprovisioned"
        )]
    );
}
