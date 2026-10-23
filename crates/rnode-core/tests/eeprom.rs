//! The emulated EEPROM (#66): the image the node sends for `CMD_ROM_READ`, checked against values
//! computed independently from RNode_Firmware's `ROM.h` and Python's `hashlib.md5`, the refused
//! writes and wipe, and the firmware-update and signing commands the node ignores.

mod common;

use log::{Level, LevelFilter};

use common::log_capture::capture;
use common::{cmd, init, lines, node, plain, split_frames};
use rnode_core::eeprom::{self, DeviceInfo, InfoBlock};
use rnode_core::kiss::*;
use rnode_core::{MemoryStorage, Node, NodeConfig, RadioSettings, SimMedium, SimRadio};

/// The bytes a string of hex digits spells.
fn hex(digits: &str) -> Vec<u8> {
    let digits: Vec<u8> = digits.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    digits
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

/// Settings as `eeprom_conf_save()` would save them for the radio of `init()`.
const SETTINGS: RadioSettings = RadioSettings {
    frequency: 914_875_000,
    bandwidth: 125_000,
    spreading_factor: 8,
    coding_rate: 5,
    tx_power: 2,
};

/// A node with `storage` and this EEPROM serial number.
fn node_with(storage: MemoryStorage, serial_number: u32) -> Node<SimRadio, MemoryStorage> {
    let config = NodeConfig {
        serial_number,
        ..NodeConfig::default()
    };
    Node::new(SimMedium::new().radio(), storage, config, 1)
}

/// A node that booted from `storage`, with the replies of its start-up taken.
fn booted(storage: MemoryStorage, serial_number: u32) -> Node<SimRadio, MemoryStorage> {
    let mut n = node_with(storage, serial_number);
    n.boot(0);
    n.take_host_output();
    n
}

/// The image `n` sends for `CMD_ROM_READ`, unescaped.
fn read_image(n: &mut Node<SimRadio, MemoryStorage>) -> Vec<u8> {
    n.handle_host_input(&plain(CMD_ROM_READ, &[0x00]), 0);
    let frames = split_frames(&n.take_host_output());
    assert_eq!(frames.len(), 1, "{frames:02X?}");
    assert_eq!(frames[0].0, CMD_ROM_READ);
    frames[0].1.clone()
}

/// The checksum is the MD5 of product, model, hardware revision, serial number and manufacturing
/// time (`eeprom_checksum_valid()`, rnodeconf `parse_eeprom()`). The digests come from Python:
/// `hashlib.md5(bytes.fromhex("f0fe01000000016ad95200")).hexdigest()`.
#[test]
fn checksum_is_the_md5_that_python_computes() {
    for (serial, checksummed, md5) in [
        (1, "f0fe01000000016ad95200", "e66334e392f7f68f0413253fe51421a8"),
        (
            0x1234_5678,
            "f0fe01123456786ad95200",
            "5f65390f3b323ac6c7d0642bfcd124fa",
        ),
        (
            0xC0DB_00F2,
            "f0fe01c0db00f26ad95200",
            "f526bac4384a0a1edb148676c034121d",
        ),
    ] {
        let info = DeviceInfo::homebrew(serial);
        assert_eq!(info.checksummed_bytes().to_vec(), hex(checksummed), "{serial:#X}");
        assert_eq!(info.checksum().to_vec(), hex(md5), "{serial:#X}");
    }
    // 2026-10-22 00:00:00 UTC: `calendar.timegm((2026, 10, 22, 0, 0, 0))`.
    assert_eq!(eeprom::MANUFACTURED, 0x6AD9_5200);
}

#[test]
fn image_without_saved_settings_and_with_avoidance_off() {
    let image = eeprom::image(&InfoBlock::unsigned(DeviceInfo::homebrew(1)), None, false);
    let expected = [
        // 0x00-0x0A: product 0xF0 and model 0xFE (Boards.h PRODUCT_HMBRW, MODEL_FE), revision 1,
        // serial number 1 and the manufacturing time, big-endian.
        hex("f0 fe 01 00000001 6ad95200"),
        // 0x0B-0x1A: the MD5 of 0x00-0x0A.
        hex("e66334e392f7f68f0413253fe51421a8"),
        // 0x1B-0x9A: no signature, as erased EEPROM.
        vec![0xFF; 128],
        // 0x9B: INFO_LOCK_BYTE, provisioned.
        vec![0x73],
        // 0x9C-0xC7: no TNC settings (0xA7 not CONF_OK_BYTE), interference avoidance off at 0xB9,
        // Bluetooth, display and Wi-Fi unset.
        vec![0xFF; 44],
    ]
    .concat();
    assert_eq!(image.len(), 200);
    assert_eq!(image.to_vec(), expected);
}

#[test]
fn image_with_saved_settings_and_avoidance_on() {
    let image = eeprom::image(
        &InfoBlock::unsigned(DeviceInfo::homebrew(0x1234_5678)),
        Some(&SETTINGS),
        true,
    );
    let expected = [
        hex("f0 fe 01 12345678 6ad95200"),
        hex("5f65390f3b323ac6c7d0642bfcd124fa"),
        vec![0xFF; 128],
        vec![0x73],
        // 0x9C-0xA7 as eeprom_conf_save() writes them: SF 8, CR 5, 2 dBm, 125000 Hz and
        // 914875000 Hz big-endian, then CONF_OK_BYTE.
        hex("08 05 02 0001e848 3687e278 73"),
        // 0xA8-0xB8: unused, Bluetooth, display and NeoPixel settings.
        vec![0xFF; 17],
        // 0xB9: 0x00, interference avoidance on (firmware setup()).
        vec![0x00],
        // 0xBA-0xC7: Wi-Fi mode, channel and the rest.
        vec![0xFF; 14],
    ]
    .concat();
    assert_eq!(image.to_vec(), expected);
}

/// `kiss_dump_eeprom()` escapes every byte (`escaped_serial_write()`). Here the serial number,
/// the checksum and the saved frequency hold `FEND` and `FESC`; the expected frame was built with
/// Python.
#[test]
fn rom_read_reply_is_the_image_kiss_escaped() {
    let settings = RadioSettings {
        frequency: 0x36C0_DB00,
        bandwidth: 125_000,
        spreading_factor: 7,
        coding_rate: 5,
        tx_power: 14,
    };
    let mut n = node_with(MemoryStorage::with_settings(settings), 0xC0DB_00F2);
    n.handle_host_input(&plain(CMD_ROM_READ, &[0x00]), 0);
    let out = n.take_host_output();
    let expected = [
        hex("c0 51 f0fe01 dbdcdbdd00f2 6ad95200 f526bac4384a0a1edbdd148676dbdc34121d"),
        vec![0xFF; 128],
        hex("73 07050e 0001e848 36dbdcdbdd00 73"),
        vec![0xFF; 32],
        vec![FEND],
    ]
    .concat();
    assert_eq!(out, expected);
    assert_eq!(
        split_frames(&out),
        [(
            CMD_ROM_READ,
            eeprom::image(
                &InfoBlock::unsigned(DeviceInfo::homebrew(0xC0DB_00F2)),
                Some(&settings),
                false
            )
            .to_vec()
        )]
    );

    // serial_callback() dumps the EEPROM once for each byte after the command, so an empty frame
    // gets nothing and a frame with two bytes two images.
    n.handle_host_input(&[FEND, CMD_ROM_READ, FEND], 0);
    assert!(n.take_host_output().is_empty());
    n.handle_host_input(&plain(CMD_ROM_READ, &[0x00, 0x00]), 0);
    assert_eq!(n.take_host_output(), [&expected[..], &expected[..]].concat());
}

/// The image shows TNC mode (`ADDR_CONF_OK` = `CONF_OK_BYTE`) exactly when `boot()` brings the
/// radio up from the saved settings, as the firmware's `eeprom_have_conf()` decides both.
#[test]
fn rom_read_shows_tnc_mode_exactly_when_the_saved_settings_start_the_radio() {
    let cases = [
        (SETTINGS, true),
        (
            RadioSettings {
                tx_power: 0,
                ..SETTINGS
            },
            true,
        ),
        (
            RadioSettings {
                tx_power: 127,
                ..SETTINGS
            },
            true,
        ),
        // boot() raises a spreading factor of 0 to 5.
        (
            RadioSettings {
                spreading_factor: 0,
                ..SETTINGS
            },
            true,
        ),
        (
            RadioSettings {
                tx_power: 0xFF,
                ..SETTINGS
            },
            false,
        ),
        (
            RadioSettings {
                tx_power: 0x80,
                ..SETTINGS
            },
            false,
        ),
        (
            RadioSettings {
                tx_power: 0xFE,
                ..SETTINGS
            },
            false,
        ),
        (
            RadioSettings {
                frequency: 0,
                ..SETTINGS
            },
            false,
        ),
        (
            RadioSettings {
                bandwidth: 0,
                ..SETTINGS
            },
            false,
        ),
    ];
    for (settings, tnc) in cases {
        let mut n = booted(MemoryStorage::with_settings(settings), 1);
        assert_eq!(n.is_radio_online(), tnc, "{settings:?}");
        let image = read_image(&mut n);
        let expected_tnc = if tnc {
            [
                &[settings.spreading_factor, settings.coding_rate, settings.tx_power][..],
                &settings.bandwidth.to_be_bytes(),
                &settings.frequency.to_be_bytes(),
                &[0x73],
            ]
            .concat()
        } else {
            vec![0xFF; 12]
        };
        assert_eq!(image[0x9C..=0xA7], expected_tnc[..], "{settings:?}");
    }
}

/// The image always reflects what the node keeps now: `CMD_CONF_SAVE`, `CMD_CONF_DELETE` and
/// `CMD_DIS_IA` show in the next read, as they do in the firmware's EEPROM.
#[test]
fn rom_read_follows_saved_settings_and_interference_avoidance() {
    let mut n = node(1);
    init(&mut n);
    n.take_host_output();
    let erased = read_image(&mut n);
    assert_eq!(erased[0x9C..], [0xFF; 44]);

    n.handle_host_input(&plain(CMD_CONF_SAVE, &[0x00]), 0);
    n.handle_host_input(&cmd(CMD_DIS_IA, &[0x00]), 0);
    let saved = read_image(&mut n);
    // init(): 867.2 MHz, 125 kHz, 14 dBm, SF7, CR 4/5.
    assert_eq!(saved[0x9C..=0xA7], hex("07 05 0e 0001e848 33b06c00 73")[..]);
    assert_eq!(saved[0xB9], 0x00);
    assert_eq!(saved[..0x9C], erased[..0x9C]);

    n.handle_host_input(&plain(CMD_CONF_DELETE, &[0x00]), 0);
    n.handle_host_input(&cmd(CMD_DIS_IA, &[0x01]), 0);
    assert_eq!(read_image(&mut n), erased);
}

/// `rnodeconf --config` also reads the config sector, which holds Wi-Fi settings on ESP32 only:
/// `kiss_dump_config()` sends it empty on other platforms, once per payload byte.
#[test]
fn cfg_read_gets_an_empty_config_sector() {
    let mut n = node(1);
    n.handle_host_input(&plain(CMD_CFG_READ, &[0x00]), 0);
    assert_eq!(n.take_host_output(), [FEND, CMD_CFG_READ, FEND]);
    n.handle_host_input(&[FEND, CMD_CFG_READ, FEND], 0);
    assert!(n.take_host_output().is_empty());
}

/// Firmware `eeprom_write()` answers a write to a locked EEPROM with `ERROR_EEPROM_LOCKED`; the
/// node's EEPROM is always locked. Nothing changes, and each write logs one warning.
#[test]
fn rom_write_is_refused_and_changes_nothing() {
    let mut n = booted(MemoryStorage::with_settings(SETTINGS), 1);
    let before = read_image(&mut n);
    let cap = capture(LevelFilter::Trace);
    for (addr, byte) in [(0x00, 0xF0), (0x9B, 0x73), (0xA7, 0x00), (0xC0, 0xDB), (0xC8, 0x00)] {
        n.handle_host_input(&cmd(CMD_ROM_WRITE, &[addr, byte]), 0);
        assert_eq!(
            n.take_host_output(),
            [FEND, CMD_ERROR, ERROR_EEPROM_LOCKED, FEND],
            "{addr:#04X}"
        );
    }
    let warnings: Vec<(Level, String)> = cap.take().into_iter().map(|r| (r.level, r.message)).collect();
    assert_eq!(
        warnings,
        ["0x00", "0x9B", "0xA7", "0xC0", "0xC8"].map(|addr| (
            Level::Warn,
            format!("refusing CMD_ROM_WRITE to EEPROM address {addr}: the EEPROM is locked; sent ERROR_EEPROM_LOCKED")
        ))
    );
    // The firmware fires once two bytes have arrived, after unescaping, and ignores the rest.
    n.handle_host_input(&plain(CMD_ROM_WRITE, &[0x00]), 0);
    assert!(n.take_host_output().is_empty());
    n.handle_host_input(&plain(CMD_ROM_WRITE, &[0x00, 0x01, 0x02]), 0);
    assert_eq!(n.take_host_output(), plain(CMD_ERROR, &[ERROR_EEPROM_LOCKED]));

    assert_eq!(n.storage().settings(), Some(SETTINGS));
    assert!(n.is_radio_online());
    assert_eq!(read_image(&mut n), before);
}

/// `rnodeconf --eeprom-wipe` sends `CMD_UNLOCK_ROM 0xF8`; firmware `unlock_rom()` would erase the
/// EEPROM, saved settings included, and reboot. The node refuses with `ERROR_EEPROM_LOCKED`, so
/// rnodeconf reports an error, and keeps its settings and its radio. Other bytes do nothing, as in
/// the firmware.
#[test]
fn eeprom_wipe_is_refused_and_keeps_the_saved_settings() {
    let mut n = booted(MemoryStorage::with_settings(SETTINGS), 1);
    let before = read_image(&mut n);
    let cap = capture(LevelFilter::Trace);
    n.handle_host_input(&plain(CMD_UNLOCK_ROM, &[ROM_UNLOCK_BYTE]), 0);
    assert_eq!(n.take_host_output(), [FEND, CMD_ERROR, ERROR_EEPROM_LOCKED, FEND]);
    assert_eq!(
        lines(&cap.take()),
        [(
            Level::Warn,
            "refusing to wipe the EEPROM (CMD_UNLOCK_ROM): the saved radio settings are kept, CMD_CONF_DELETE \
             deletes them; sent ERROR_EEPROM_LOCKED"
        )]
    );
    for byte in [0x00, 0xF7, 0xFF] {
        n.handle_host_input(&plain(CMD_UNLOCK_ROM, &[byte]), 0);
    }
    assert!(n.take_host_output().is_empty());
    assert!(cap.take().is_empty());
    assert_eq!(n.storage().settings(), Some(SETTINGS));
    assert!(n.is_radio_online());
    assert_eq!(read_image(&mut n), before);
}

/// Firmware updates and device signing do not apply to rnode-linux: `CMD_FW_UPD`, `CMD_FW_HASH`
/// and `CMD_DEV_SIG` get no reply and one warning each, when the firmware would act on them
/// (`CMD_FW_HASH` at `DEV_HASH_LEN`, 32 bytes, `CMD_DEV_SIG` at `DEV_SIG_LEN`, 64). The device and
/// firmware hash queries stay unanswered, as on firmware without them, and are not logged.
#[test]
fn firmware_update_and_signing_commands_are_ignored_with_a_warning() {
    let mut n = node(1);
    init(&mut n);
    n.take_host_output();
    let cap = capture(LevelFilter::Trace);
    n.handle_host_input(&plain(CMD_FW_UPD, &[0x01]), 0);
    n.handle_host_input(&cmd(CMD_FW_HASH, &[0xC0; 32]), 0);
    n.handle_host_input(&cmd(CMD_DEV_SIG, &[0xDB; 64]), 0);
    assert!(n.take_host_output().is_empty());
    assert_eq!(
        lines(&cap.take()),
        [
            (
                Level::Warn,
                "ignoring CMD_FW_UPD: firmware updates do not apply to rnode-linux"
            ),
            (
                Level::Warn,
                "ignoring CMD_FW_HASH: firmware updates do not apply to rnode-linux, which has no firmware hash"
            ),
            (
                Level::Warn,
                "ignoring CMD_DEV_SIG: device signing does not apply to rnode-linux"
            ),
        ]
    );

    n.handle_host_input(&cmd(CMD_FW_HASH, &[0x11; 31]), 0);
    n.handle_host_input(&cmd(CMD_DEV_SIG, &[0x22; 63]), 0);
    n.handle_host_input(&plain(CMD_DEV_HASH, &[0x01]), 0);
    n.handle_host_input(&plain(CMD_HASHES, &[0x01]), 0);
    n.handle_host_input(&plain(CMD_HASHES, &[0x02]), 0);
    assert!(n.take_host_output().is_empty());
    assert!(cap.take().is_empty());
    assert!(n.is_radio_online());
}

/// The firmware stores a target firmware hash once `DEV_HASH_LEN` (32) unescaped bytes arrived,
/// and a device signature at `DEV_SIG_LEN` (64), the size of its command buffer.
#[test]
fn fw_hash_and_dev_sig_fire_at_the_firmware_lengths() {
    let mut decoder = Decoder::new();
    let mut events = Vec::new();
    let mut input = cmd(CMD_FW_HASH, &[FEND; 40]);
    input.extend(cmd(CMD_DEV_SIG, &[FESC; 64]));
    for b in input {
        decoder.push(b, &mut events);
    }
    assert_eq!(
        events,
        [
            HostEvent::Command {
                cmd: CMD_FW_HASH,
                payload: vec![FEND; 32]
            },
            HostEvent::Command {
                cmd: CMD_DEV_SIG,
                payload: vec![FESC; 64]
            },
        ]
    );
}
