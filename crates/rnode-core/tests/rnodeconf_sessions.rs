//! Golden rnodeconf sessions (#66): the bytes Reticulum's `rnodeconf` 1.5.5 (`e40191b3`) wrote to
//! an rnode-linux PTY, replayed into a node in the order they ran, with the replies RNode_Firmware
//! v1.86 gives to them.
//!
//! The writes were recorded with a logging proxy on a PTY pair between rnodeconf and simulated
//! rnode-linux daemons, starting without saved settings: one with `[eeprom] serial_number = 42`
//! ([`SESSIONS`]), and one with `[storage] eeprom` set and no file yet, which rnodeconf signed with
//! a throwaway key ([`SIGNING_SESSIONS`]). Each write is listed with when it was made, in ms after
//! rnodeconf started; one write can hold several frames, and the 156 writes of `--rom` are listed
//! as one. While its radio was on, the daemon also pushed `CMD_STAT_CHTM`, which comes from ticks
//! that a replay does not run.

mod common;

use common::{cmd, plain};
use rnode_core::eeprom::InfoBlock;
use rnode_core::kiss::*;
use rnode_core::phy::{Modulation, PhyParams};
use rnode_core::{MemoryStorage, Node, NodeConfig, RadioSettings, SimMedium, SimRadio, StoredInfo};

/// One rnodeconf run: its options, and its writes as (ms after it started, bytes in hex).
struct Session {
    options: &'static str,
    writes: &'static [(u64, &'static str)],
}

/// The sessions in the order they ran.
const SESSIONS: [Session; 9] = [
    Session {
        options: "-i",
        writes: &[
            (2990, "c00873c05000c04800c04900c04700c05601c06001c06002c0"),
            (3747, "c05100c0"),
            (4375, "c00affc0"),
        ],
    },
    Session {
        options: "-c",
        writes: &[
            (2885, "c00873c05000c04800c04900c04700c05601c06001c06002c0"),
            (3636, "c05100c0"),
            (4253, "c06d00c0"),
            (4858, "c00affc0"),
        ],
    },
    Session {
        options: "-T --freq 914875000 --bw 125000 --txp 2 --sf 8 --cr 5",
        writes: &[
            (2817, "c00873c05000c04800c04900c04700c05601c06001c06002c0"),
            (3572, "c05100c0"),
            (4182, "c0013687e278c0c0020001e848c0c00302c0c00408c0c00505c0"),
            (4182, "c00601c0"),
            (4691, "c05300c0"),
        ],
    },
    Session {
        options: "-i",
        writes: &[
            (2681, "c00873c05000c04800c04900c04700c05601c06001c06002c0"),
            (3437, "c05100c0"),
            (4048, "c00affc0"),
        ],
    },
    Session {
        options: "-x",
        writes: &[
            (2700, "c00873c05000c04800c04900c04700c05601c06001c06002c0"),
            (3459, "c05100c0"),
            (4072, "c06900c0"),
        ],
    },
    Session {
        options: "-c",
        writes: &[
            (2689, "c00873c05000c04800c04900c04700c05601c06001c06002c0"),
            (3445, "c05100c0"),
            (4059, "c06d00c0"),
            (4668, "c00affc0"),
        ],
    },
    Session {
        options: "-X",
        writes: &[
            (2680, "c00873c05000c04800c04900c04700c05601c06001c06002c0"),
            (3433, "c05100c0"),
            (4048, "c06901c0"),
        ],
    },
    Session {
        options: "-N",
        writes: &[
            (2703, "c00873c05000c04800c04900c04700c05601c06001c06002c0"),
            (3461, "c05100c0"),
            (4078, "c05400c0"),
        ],
    },
    Session {
        options: "--eeprom-wipe",
        writes: &[
            (2706, "c00873c05000c04800c04900c04700c05601c06001c06002c0"),
            (3466, "c059f8c0"),
            (16468, "c055f8c0"),
        ],
    },
];

/// The sessions that signed an unprovisioned EEPROM with a local key, in the order they ran, after
/// `rnodeconf -k`, which does not open the port.
const SIGNING_SESSIONS: [Session; 3] = [
    Session {
        options: "-i",
        writes: &[
            (2662, "c00873c05000c04800c04900c04700c05601c06001c06002c0"),
            (3420, "c05100c0"),
        ],
    },
    Session {
        options: "--rom --product f0 --model fe --hwrev 1",
        writes: &[
            (2675, "c00873c05000c04800c04900c04700c05601c06001c06002c0"),
            (3436, "c05100c0"),
            (7067, ROM_WRITES),
            (8409, "c055f8c0"),
            (10410, "c05100c0"),
            (11020, "c055f8c0"),
        ],
    },
    Session {
        options: "-i",
        writes: &[
            (2667, "c00873c05000c04800c04900c04700c05601c06001c06002c0"),
            (3421, "c05100c0"),
            (4041, "c00affc0"),
        ],
    },
];

/// The 156 `CMD_ROM_WRITE` frames of `--rom`, one for each address from `0x00` to the info lock
/// byte at `0x9B`, 6 ms apart: product, model, revision, serial number 1, the time, the checksum,
/// the signature, then `0x73`. FEND and FESC are escaped.
const ROM_WRITES: &str = concat!(
    "c05200f0c0c05201fec0c0520201c0c0520300c0c0520400c0c0520500c0c0520601c0c052076ac0c05208dbddc0c052",
    "0902c0c0520acbc0c0520b30c0c0520c62c0c0520d7bc0c0520ed9c0c0520f7ec0c0521032c0c052114dc0c0521252c0",
    "c0521377c0c05214b1c0c052152cc0c05216e5c0c052173ec0c05218e2c0c052192cc0c0521a92c0c0521b60c0c0521c",
    "89c0c0521dffc0c0521e41c0c0521ffbc0c052204fc0c05221f5c0c05222abc0c05223dbddc0c052248dc0c0522561c0",
    "c05226b0c0c0522762c0c05228f0c0c052297ac0c0522ab0c0c0522b4ec0c0522c47c0c0522d30c0c0522e37c0c0522f",
    "e0c0c05230d1c0c052312bc0c052322dc0c0523349c0c052343ec0c0523519c0c052362ac0c0523708c0c05238dfc0c0",
    "5239efc0c0523addc0c0523b2ac0c0523c3dc0c0523d94c0c0523e3fc0c0523f53c0c052403fc0c0524146c0c052424c",
    "c0c05243c4c0c0524465c0c0524577c0c05246d7c0c05247f1c0c05248cbc0c05249e7c0c0524a7bc0c0524bd6c0c052",
    "4c2ac0c0524d85c0c0524e13c0c0524f42c0c0525040c0c0525189c0c05252a9c0c0525372c0c0525477c0c05255d0c0",
    "c0525601c0c052575bc0c0525887c0c0525962c0c0525a57c0c0525b08c0c0525cd3c0c0525d0ac0c0525e2dc0c0525f",
    "07c0c05260a4c0c052612fc0c052621ec0c0526320c0c05264ccc0c0526564c0c0526682c0c0526751c0c052682fc0c0",
    "5269a1c0c0526ae7c0c0526b90c0c0526ce3c0c0526d5dc0c0526e79c0c0526f92c0c05270a3c0c05271f2c0c0527257",
    "c0c052737ac0c0527418c0c0527546c0c05276d9c0c052776cc0c0527832c0c0527956c0c0527a55c0c0527bf0c0c052",
    "7c0dc0c0527d91c0c0527e30c0c0527fdbdcc0c05280d3c0c05281a8c0c0528237c0c0528331c0c0528443c0c0528514",
    "c0c05286e6c0c0528720c0c05288bbc0c0528905c0c0528a2ec0c0528baac0c0528cd1c0c0528d89c0c0528ec4c0c052",
    "8fdbdcc0c0529024c0c05291dac0c0529287c0c05293a6c0c0529417c0c05295edc0c0529628c0c05297bac0c05298b7",
    "c0c052996cc0c0529abcc0c0529b73c0",
);

/// The info block those writes hold, as the daemon stored it.
const SIGNED_INFO_BLOCK: &str = concat!(
    "f0fe01000000016adb02cb30627bd97e324d5277b12ce53ee22c926089ff41fb4ff5abdb8d61b062f07ab04e473037e0",
    "d12b2d493e192a08dfefdd2a3d943f533f464cc46577d7f1cbe77bd62a8513424089a97277d0015b87625708d30a2d07",
    "a42f1e20cc6482512fa1e790e35d7992a3f2577a1846d96c325655f00d9130c0d3a837314314e620bb052eaad189c4c0",
    "24da87a617ed28bab76cbc73",
);

/// The settings `-T` configures and saves.
const TNC: RadioSettings = RadioSettings {
    frequency: 914_875_000,
    bandwidth: 125_000,
    spreading_factor: 8,
    coding_rate: 5,
    tx_power: 2,
};

fn hex(digits: &str) -> Vec<u8> {
    let digits: Vec<u8> = digits.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    digits
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

/// The replies to `detect()`'s write: `kiss_indicate_detect()`, firmware version 1.86, platform
/// `0x60`, MCU `0x61` and board `0x60`; nothing for `CMD_DEV_HASH` and `CMD_HASHES`, which only
/// ESP32 and nRF52 firmware answers.
fn detect_replies() -> Vec<u8> {
    [
        plain(CMD_DETECT, &[DETECT_RESP]),
        plain(CMD_FW_VERSION, &[0x01, 0x56]),
        plain(CMD_PLATFORM, &[0x60]),
        plain(CMD_MCU, &[0x61]),
        plain(CMD_BOARD, &[0x60]),
    ]
    .concat()
}

/// The `CMD_ROM_READ` reply: the EEPROM image with serial number 42, KISS-escaped. The checksum
/// is `hashlib.md5(bytes.fromhex("f0fe010000002a6ad95200")).hexdigest()`.
fn rom_read_reply(tnc: bool, avoid_interference: bool) -> Vec<u8> {
    let image = [
        hex("f0fe01 0000002a 6ad95200"),
        hex("e5f4efcda3fec6a9207011c36c93f8c9"),
        vec![0xFF; 128],
        vec![0x73],
        if tnc {
            hex("08 05 02 0001e848 3687e278 73")
        } else {
            vec![0xFF; 12]
        },
        vec![0xFF; 17],
        vec![if avoid_interference { 0x00 } else { 0xFF }],
        vec![0xFF; 14],
    ]
    .concat();
    cmd(CMD_ROM_READ, &image)
}

/// The `CMD_CFG_READ` reply of firmware without Wi-Fi: an empty frame.
fn cfg_read_reply() -> Vec<u8> {
    vec![FEND, CMD_CFG_READ, FEND]
}

/// Replays `session` into `n`, starting at `start_ms`, and returns the reply to each write.
fn replay(n: &mut Node<SimRadio, MemoryStorage>, session: &Session, start_ms: u64) -> Vec<Vec<u8>> {
    session
        .writes
        .iter()
        .map(|&(at_ms, bytes)| {
            n.handle_host_input(&hex(bytes), start_ms + at_ms);
            n.take_host_output()
        })
        .collect()
}

#[test]
fn rnodeconf_sessions_get_the_firmware_replies() {
    let config = NodeConfig {
        serial_number: 42,
        ..NodeConfig::default()
    };
    let mut n = Node::new(SimMedium::new().radio(), MemoryStorage::default(), config, 1);
    n.boot(0);
    assert!(n.take_host_output().is_empty());
    let mut sessions = SESSIONS.iter().zip((1..).map(|i| 60_000 * i));
    // Replays the next session, which must be the one run with these options.
    let mut next = |n: &mut Node<SimRadio, MemoryStorage>, options: &str| {
        let (session, start_ms) = sessions.next().unwrap();
        assert_eq!(session.options, options);
        replay(n, session, start_ms)
    };

    // Device info, without saved settings: normal (host-controlled) mode. CMD_LEAVE has no reply.
    assert_eq!(
        next(&mut n, "-i"),
        [detect_replies(), rom_read_reply(false, false), vec![]]
    );
    // The configuration: CMD_CFG_READ gets an empty config sector, which only ESP32 firmware
    // fills (kiss_dump_config()).
    assert_eq!(
        next(&mut n, "-c"),
        [detect_replies(), rom_read_reply(false, false), cfg_read_reply(), vec![]]
    );

    // TNC mode: the radio settings, each echoed, the radio on, then CMD_CONF_SAVE.
    let phy = PhyParams::new(Modulation {
        bandwidth_hz: 125_000,
        spreading_factor: 8,
        coding_rate: 5,
    });
    assert_eq!(
        next(&mut n, "-T --freq 914875000 --bw 125000 --txp 2 --sf 8 --cr 5"),
        [
            detect_replies(),
            rom_read_reply(false, false),
            [
                cmd(CMD_FREQUENCY, &914_875_000u32.to_be_bytes()),
                cmd(CMD_BANDWIDTH, &125_000u32.to_be_bytes()),
                plain(CMD_TXPOWER, &[2]),
                plain(CMD_SF, &[8]),
                plain(CMD_CR, &[5]),
            ]
            .concat(),
            [cmd(CMD_STAT_PHYPRM, &phy.stats_payload()), plain(CMD_RADIO_STATE, &[1])].concat(),
            vec![],
        ]
    );
    assert_eq!(n.storage().settings(), Some(TNC));
    assert!(n.is_radio_online());

    assert_eq!(
        next(&mut n, "-i"),
        [detect_replies(), rom_read_reply(true, false), vec![]]
    );

    // Interference avoidance: CMD_DIS_IA has no reply, and shows in the next image.
    assert_eq!(
        next(&mut n, "-x"),
        [detect_replies(), rom_read_reply(true, false), vec![]]
    );
    assert!(n.avoid_interference());
    assert_eq!(
        next(&mut n, "-c"),
        [detect_replies(), rom_read_reply(true, true), cfg_read_reply(), vec![]]
    );
    assert_eq!(
        next(&mut n, "-X"),
        [detect_replies(), rom_read_reply(true, true), vec![]]
    );
    assert!(!n.avoid_interference());

    // Normal mode deletes the saved settings; the radio stays on until the next reset.
    assert_eq!(
        next(&mut n, "-N"),
        [detect_replies(), rom_read_reply(true, false), vec![]]
    );
    assert_eq!(n.storage().settings(), None);
    assert!(n.is_radio_online());

    // The wipe is refused with ERROR_EEPROM_LOCKED, which rnodeconf prints as "hardware error
    // (code 03)"; the CMD_RESET it sends 13 s later resets the node, which has nothing saved.
    assert_eq!(
        next(&mut n, "--eeprom-wipe"),
        [detect_replies(), plain(CMD_ERROR, &[ERROR_EEPROM_LOCKED]), vec![]]
    );
    assert!(!n.is_radio_online());
    assert!(sessions.next().is_none(), "a session was not checked");
}

/// `rnodeconf --rom` signs an unprovisioned EEPROM with the user's key: it reads the EEPROM, which
/// rnodeconf calls invalid, writes the info block and locks it, resets the device and reads the
/// EEPROM back, which must now hold the signed info block.
#[test]
fn rnodeconf_rom_signs_an_unprovisioned_eeprom() {
    let mut n = Node::new(
        SimMedium::new().radio(),
        MemoryStorage::keeping_info(),
        NodeConfig::default(),
        1,
    );
    n.boot(0);
    assert!(n.take_host_output().is_empty());
    let block = InfoBlock::from_bytes(&hex(SIGNED_INFO_BLOCK)).unwrap();
    let unprovisioned = cmd(CMD_ROM_READ, &[0xFF; 200]);
    let signed = cmd(CMD_ROM_READ, &[&block.as_bytes()[..], &[0xFF; 44]].concat());
    let [info, rom, signed_info] = &SIGNING_SESSIONS;

    // "EEPROM is invalid, no further information available"
    assert_eq!(replay(&mut n, info, 60_000), [detect_replies(), unprovisioned.clone()]);
    assert_eq!(
        replay(&mut n, rom, 120_000),
        [detect_replies(), unprovisioned, vec![], vec![], signed.clone(), vec![]]
    );
    assert_eq!(n.storage().info(), &StoredInfo::Locked(block));
    // "Device signature : Validated - Local signature"
    assert_eq!(replay(&mut n, signed_info, 180_000), [detect_replies(), signed, vec![]]);
}
