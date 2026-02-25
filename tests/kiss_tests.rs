use rnode_linux::kiss::*;

// --- Encoder tests ---

#[test]
fn encode_simple_frame() {
    let frame = encode_frame(CMD_DATA, &[0x01, 0x02, 0x03]);
    assert_eq!(frame, vec![FEND, CMD_DATA, 0x01, 0x02, 0x03, FEND]);
}

#[test]
fn encode_escapes_fend_in_payload() {
    let frame = encode_frame(CMD_DATA, &[0xAA, FEND, 0xBB]);
    assert_eq!(
        frame,
        vec![FEND, CMD_DATA, 0xAA, FESC, TFEND, 0xBB, FEND]
    );
}

#[test]
fn encode_escapes_fesc_in_payload() {
    let frame = encode_frame(CMD_DATA, &[FESC]);
    assert_eq!(frame, vec![FEND, CMD_DATA, FESC, TFESC, FEND]);
}

#[test]
fn encode_escapes_both_special_bytes() {
    let frame = encode_frame(CMD_DATA, &[FEND, FESC]);
    assert_eq!(
        frame,
        vec![FEND, CMD_DATA, FESC, TFEND, FESC, TFESC, FEND]
    );
}

#[test]
fn encode_empty_payload() {
    let frame = encode_frame(CMD_STAT_RX, &[]);
    assert_eq!(frame, vec![FEND, CMD_STAT_RX, FEND]);
}

// --- Indication helper tests ---

#[test]
fn indicate_frequency_encoding() {
    let frame = indicate_frequency(915_000_000);
    // The encoder escapes special bytes, so we verify via round-trip
    assert_eq!(frame[0], FEND);
    assert_eq!(frame[1], CMD_FREQUENCY);
    assert_eq!(*frame.last().unwrap(), FEND);
    // Verify round-trip correctness
    match parse_frame(&frame) {
        Some(KissCommand::SetFrequency(f)) => assert_eq!(f, 915_000_000),
        other => panic!("Expected SetFrequency(915000000), got {:?}", other),
    }
}

#[test]
fn indicate_version_encoding() {
    let frame = indicate_version();
    assert_eq!(frame, vec![FEND, CMD_FW_VERSION, MAJ_VERS, MIN_VERS, FEND]);
}

#[test]
fn indicate_detect_encoding() {
    let frame = indicate_detect();
    assert_eq!(frame, vec![FEND, CMD_DETECT, DETECT_RESP, FEND]);
}

#[test]
fn indicate_ready_vs_not_ready() {
    assert_eq!(indicate_ready(), vec![FEND, CMD_READY, 0x01, FEND]);
    assert_eq!(indicate_not_ready(), vec![FEND, CMD_READY, 0x00, FEND]);
}

#[test]
fn indicate_data_with_escapes() {
    let frame = indicate_data(&[0xC0, 0xDB]);
    // FEND should be escaped in the payload
    assert_eq!(
        frame,
        vec![FEND, CMD_DATA, FESC, TFEND, FESC, TFESC, FEND]
    );
}

// --- Parser tests ---

/// Helper: feed a complete KISS frame and return the parsed command.
fn parse_frame(bytes: &[u8]) -> Option<KissCommand> {
    let mut parser = KissParser::new();
    let mut result = None;
    for &b in bytes {
        if let Some(cmd) = parser.feed(b) {
            result = Some(cmd);
        }
    }
    result
}

#[test]
fn parse_data_frame() {
    let frame = [FEND, CMD_DATA, 0x01, 0x02, 0x03, FEND];
    match parse_frame(&frame) {
        Some(KissCommand::Data(data)) => assert_eq!(data, vec![0x01, 0x02, 0x03]),
        other => panic!("Expected Data, got {:?}", other),
    }
}

#[test]
fn parse_data_with_escape_fend() {
    // Payload contains literal 0xC0 (FEND), encoded as FESC+TFEND
    let frame = [FEND, CMD_DATA, 0xAA, FESC, TFEND, 0xBB, FEND];
    match parse_frame(&frame) {
        Some(KissCommand::Data(data)) => assert_eq!(data, vec![0xAA, FEND, 0xBB]),
        other => panic!("Expected Data, got {:?}", other),
    }
}

#[test]
fn parse_data_with_escape_fesc() {
    let frame = [FEND, CMD_DATA, FESC, TFESC, FEND];
    match parse_frame(&frame) {
        Some(KissCommand::Data(data)) => assert_eq!(data, vec![FESC]),
        other => panic!("Expected Data, got {:?}", other),
    }
}

#[test]
fn parse_empty_data_returns_none() {
    // FEND CMD_DATA FEND with no payload
    let frame = [FEND, CMD_DATA, FEND];
    assert!(parse_frame(&frame).is_none());
}

#[test]
fn parse_frequency_set() {
    // Use a frequency whose BE encoding has no special bytes
    let freq: u32 = 433_000_000; // 0x19D7_EA00, no FEND/FESC bytes
    let bytes = freq.to_be_bytes();
    assert!(bytes.iter().all(|&b| b != FEND && b != FESC));
    let frame = [FEND, CMD_FREQUENCY, bytes[0], bytes[1], bytes[2], bytes[3], FEND];
    match parse_frame(&frame) {
        Some(KissCommand::SetFrequency(f)) => assert_eq!(f, 433_000_000),
        other => panic!("Expected SetFrequency, got {:?}", other),
    }
}

#[test]
fn parse_frequency_query() {
    let frame = [FEND, CMD_FREQUENCY, 0x00, 0x00, 0x00, 0x00, FEND];
    match parse_frame(&frame) {
        Some(KissCommand::SetFrequency(0)) => {}
        other => panic!("Expected SetFrequency(0), got {:?}", other),
    }
}

#[test]
fn parse_bandwidth_set() {
    let bw: u32 = 125_000;
    let bytes = bw.to_be_bytes();
    let frame = [FEND, CMD_BANDWIDTH, bytes[0], bytes[1], bytes[2], bytes[3], FEND];
    match parse_frame(&frame) {
        Some(KissCommand::SetBandwidth(b)) => assert_eq!(b, 125_000),
        other => panic!("Expected SetBandwidth, got {:?}", other),
    }
}

#[test]
fn parse_txpower() {
    let frame = [FEND, CMD_TXPOWER, 14, FEND];
    match parse_frame(&frame) {
        Some(KissCommand::SetTxPower(14)) => {}
        other => panic!("Expected SetTxPower(14), got {:?}", other),
    }
}

#[test]
fn parse_txpower_query() {
    let frame = [FEND, CMD_TXPOWER, 0xFF, FEND];
    match parse_frame(&frame) {
        Some(KissCommand::SetTxPower(0xFF)) => {}
        other => panic!("Expected SetTxPower(0xFF), got {:?}", other),
    }
}

#[test]
fn parse_spreading_factor() {
    let frame = [FEND, CMD_SF, 10, FEND];
    match parse_frame(&frame) {
        Some(KissCommand::SetSf(10)) => {}
        other => panic!("Expected SetSf(10), got {:?}", other),
    }
}

#[test]
fn parse_coding_rate() {
    let frame = [FEND, CMD_CR, 7, FEND];
    match parse_frame(&frame) {
        Some(KissCommand::SetCr(7)) => {}
        other => panic!("Expected SetCr(7), got {:?}", other),
    }
}

#[test]
fn parse_radio_state_on() {
    let frame = [FEND, CMD_RADIO_STATE, 0x01, FEND];
    match parse_frame(&frame) {
        Some(KissCommand::RadioState(1)) => {}
        other => panic!("Expected RadioState(1), got {:?}", other),
    }
}

#[test]
fn parse_radio_state_off() {
    let frame = [FEND, CMD_RADIO_STATE, 0x00, FEND];
    match parse_frame(&frame) {
        Some(KissCommand::RadioState(0)) => {}
        other => panic!("Expected RadioState(0), got {:?}", other),
    }
}

#[test]
fn parse_detect() {
    let frame = [FEND, CMD_DETECT, DETECT_REQ, FEND];
    match parse_frame(&frame) {
        Some(KissCommand::Detect) => {}
        other => panic!("Expected Detect, got {:?}", other),
    }
}

#[test]
fn parse_detect_wrong_byte_ignored() {
    let frame = [FEND, CMD_DETECT, 0x00, FEND];
    assert!(parse_frame(&frame).is_none());
}

#[test]
fn parse_promisc_enable() {
    let frame = [FEND, CMD_PROMISC, 0x01, FEND];
    match parse_frame(&frame) {
        Some(KissCommand::Promisc(1)) => {}
        other => panic!("Expected Promisc(1), got {:?}", other),
    }
}

#[test]
fn parse_implicit() {
    let frame = [FEND, CMD_IMPLICIT, 32, FEND];
    match parse_frame(&frame) {
        Some(KissCommand::SetImplicit(32)) => {}
        other => panic!("Expected SetImplicit(32), got {:?}", other),
    }
}

#[test]
fn parse_leave() {
    let frame = [FEND, CMD_LEAVE, 0xFF, FEND];
    match parse_frame(&frame) {
        Some(KissCommand::Leave) => {}
        other => panic!("Expected Leave, got {:?}", other),
    }
}

#[test]
fn parse_st_alock() {
    // 0x03E8 = 1000 => 10.00%
    let frame = [FEND, CMD_ST_ALOCK, 0x03, 0xE8, FEND];
    match parse_frame(&frame) {
        Some(KissCommand::StAirlock(1000)) => {}
        other => panic!("Expected StAirlock(1000), got {:?}", other),
    }
}

#[test]
fn parse_lt_alock() {
    let frame = [FEND, CMD_LT_ALOCK, 0x01, 0xF4, FEND];
    match parse_frame(&frame) {
        Some(KissCommand::LtAirlock(500)) => {}
        other => panic!("Expected LtAirlock(500), got {:?}", other),
    }
}

#[test]
fn parse_reset() {
    let frame = [FEND, CMD_RESET, CMD_RESET_BYTE, FEND];
    match parse_frame(&frame) {
        Some(KissCommand::Reset) => {}
        other => panic!("Expected Reset, got {:?}", other),
    }
}

#[test]
fn parse_reset_wrong_byte_ignored() {
    let frame = [FEND, CMD_RESET, 0x00, FEND];
    assert!(parse_frame(&frame).is_none());
}

#[test]
fn parse_stat_commands() {
    assert!(matches!(parse_frame(&[FEND, CMD_STAT_RX, 0x00, FEND]), Some(KissCommand::StatRx)));
    assert!(matches!(parse_frame(&[FEND, CMD_STAT_TX, 0x00, FEND]), Some(KissCommand::StatTx)));
    assert!(matches!(parse_frame(&[FEND, CMD_STAT_RSSI, 0x00, FEND]), Some(KissCommand::StatRssi)));
    assert!(matches!(parse_frame(&[FEND, CMD_STAT_SNR, 0x00, FEND]), Some(KissCommand::StatSnr)));
}

#[test]
fn parse_random() {
    assert!(matches!(parse_frame(&[FEND, CMD_RANDOM, 0x00, FEND]), Some(KissCommand::Random)));
}

#[test]
fn parse_info_commands() {
    assert!(matches!(parse_frame(&[FEND, CMD_FW_VERSION, 0x00, FEND]), Some(KissCommand::FwVersion)));
    assert!(matches!(parse_frame(&[FEND, CMD_PLATFORM, 0x00, FEND]), Some(KissCommand::Platform)));
    assert!(matches!(parse_frame(&[FEND, CMD_BOARD, 0x00, FEND]), Some(KissCommand::Board)));
}

// --- Round-trip tests ---

#[test]
fn roundtrip_data_simple() {
    let original = vec![0x01, 0x02, 0x03, 0x04];
    let encoded = indicate_data(&original);
    match parse_frame(&encoded) {
        Some(KissCommand::Data(data)) => assert_eq!(data, original),
        other => panic!("Round-trip failed: {:?}", other),
    }
}

#[test]
fn roundtrip_data_with_special_bytes() {
    let original = vec![0xAA, FEND, 0xBB, FESC, 0xCC];
    let encoded = indicate_data(&original);
    match parse_frame(&encoded) {
        Some(KissCommand::Data(data)) => assert_eq!(data, original),
        other => panic!("Round-trip failed: {:?}", other),
    }
}

#[test]
fn roundtrip_frequency() {
    let freq = 868_000_000u32;
    let encoded = indicate_frequency(freq);
    match parse_frame(&encoded) {
        Some(KissCommand::SetFrequency(f)) => assert_eq!(f, freq),
        other => panic!("Round-trip failed: {:?}", other),
    }
}

// --- Multi-frame parsing ---

#[test]
fn parse_multiple_frames_sequentially() {
    let mut parser = KissParser::new();
    let stream = [
        FEND, CMD_DATA, 0xAA, FEND,
        FEND, CMD_DATA, 0xBB, FEND,
    ];
    let mut results = Vec::new();
    for &b in &stream {
        if let Some(cmd) = parser.feed(b) {
            results.push(cmd);
        }
    }
    assert_eq!(results.len(), 2);
    assert!(matches!(&results[0], KissCommand::Data(d) if d == &[0xAA]));
    assert!(matches!(&results[1], KissCommand::Data(d) if d == &[0xBB]));
}

#[test]
fn bytes_before_first_fend_are_ignored() {
    let mut parser = KissParser::new();
    // Garbage bytes before the frame
    for &b in &[0x55, 0xAA, 0x00] {
        assert!(parser.feed(b).is_none());
    }
    // Now a valid frame
    let frame = [FEND, CMD_DATA, 0x42, FEND];
    let mut result = None;
    for &b in &frame {
        if let Some(cmd) = parser.feed(b) {
            result = Some(cmd);
        }
    }
    assert!(matches!(result, Some(KissCommand::Data(d)) if d == vec![0x42]));
}

#[test]
fn frequency_with_escaped_bytes() {
    // Construct a frequency whose big-endian encoding contains FEND (0xC0)
    // 0x36C00000 = 918_552_576
    let freq: u32 = 0x36C00000;
    let bytes = freq.to_be_bytes(); // [0x36, 0xC0, 0x00, 0x00]
    assert_eq!(bytes[1], FEND);
    // The encoder should escape the 0xC0
    let encoded = indicate_frequency(freq);
    match parse_frame(&encoded) {
        Some(KissCommand::SetFrequency(f)) => assert_eq!(f, freq),
        other => panic!("Escaped frequency round-trip failed: {:?}", other),
    }
}
