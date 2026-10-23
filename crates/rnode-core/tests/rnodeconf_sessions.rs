//! Golden rnodeconf sessions (#66): the bytes Reticulum's `rnodeconf` 1.5.5 (`e40191b3`) wrote to
//! an rnode-linux PTY, replayed into a node in the order they ran, with the replies RNode_Firmware
//! v1.86 gives to them.
//!
//! The writes were recorded with a logging proxy on a PTY pair between rnodeconf and a simulated
//! rnode-linux daemon with `[eeprom] serial_number = 42`, starting without saved settings. Each
//! write is listed with when it was made, in ms after rnodeconf started; one write can hold several
//! frames. While its radio was on, the daemon also pushed `CMD_STAT_CHTM`, which comes from ticks
//! that a replay does not run.

mod common;

use common::{cmd, plain};
use rnode_core::kiss::*;
use rnode_core::phy::{Modulation, PhyParams};
use rnode_core::{MemoryStorage, Node, NodeConfig, RadioSettings, SimMedium, SimRadio};

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
