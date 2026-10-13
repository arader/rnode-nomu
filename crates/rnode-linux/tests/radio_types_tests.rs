use rnode_linux::radio::types::{BandwidthCode, TcxoVoltage};

// --- TcxoVoltage tests ---

#[test]
fn tcxo_from_str_valid() {
    assert!(matches!(TcxoVoltage::from_str("1.6V"), Some(TcxoVoltage::V1_6)));
    assert!(matches!(TcxoVoltage::from_str("1.7V"), Some(TcxoVoltage::V1_7)));
    assert!(matches!(TcxoVoltage::from_str("1.8V"), Some(TcxoVoltage::V1_8)));
    assert!(matches!(TcxoVoltage::from_str("2.2V"), Some(TcxoVoltage::V2_2)));
    assert!(matches!(TcxoVoltage::from_str("2.4V"), Some(TcxoVoltage::V2_4)));
    assert!(matches!(TcxoVoltage::from_str("2.7V"), Some(TcxoVoltage::V2_7)));
    assert!(matches!(TcxoVoltage::from_str("3.0V"), Some(TcxoVoltage::V3_0)));
    assert!(matches!(TcxoVoltage::from_str("3.3V"), Some(TcxoVoltage::V3_3)));
}

#[test]
fn tcxo_from_str_invalid() {
    assert!(TcxoVoltage::from_str("5V").is_none());
    assert!(TcxoVoltage::from_str("").is_none());
    assert!(TcxoVoltage::from_str("1.8").is_none());
}

#[test]
fn tcxo_register_values() {
    assert_eq!(TcxoVoltage::V1_6.register_value(), 0x00);
    assert_eq!(TcxoVoltage::V1_7.register_value(), 0x01);
    assert_eq!(TcxoVoltage::V1_8.register_value(), 0x02);
    assert_eq!(TcxoVoltage::V2_2.register_value(), 0x03);
    assert_eq!(TcxoVoltage::V3_3.register_value(), 0x07);
}

// --- BandwidthCode tests ---

#[test]
fn bandwidth_from_hz_exact() {
    assert_eq!(BandwidthCode::from_hz(7_800).code(), 0x00);
    assert_eq!(BandwidthCode::from_hz(125_000).code(), 0x04);
    assert_eq!(BandwidthCode::from_hz(500_000).code(), 0x06);
}

#[test]
fn bandwidth_from_hz_rounds_up() {
    // Values between thresholds should round to the next lower bandwidth
    assert_eq!(BandwidthCode::from_hz(8_000).code(), 0x08); // 10.4kHz
    assert_eq!(BandwidthCode::from_hz(100_000).code(), 0x04); // 125kHz
    assert_eq!(BandwidthCode::from_hz(200_000).code(), 0x05); // 250kHz
}

#[test]
fn bandwidth_from_hz_very_large() {
    assert_eq!(BandwidthCode::from_hz(1_000_000).code(), 0x06); // 500kHz max
}

#[test]
fn bandwidth_to_hz_roundtrip() {
    let codes = [
        BandwidthCode::Bw7_8,
        BandwidthCode::Bw10_4,
        BandwidthCode::Bw15_6,
        BandwidthCode::Bw20_8,
        BandwidthCode::Bw31_25,
        BandwidthCode::Bw41_7,
        BandwidthCode::Bw62_5,
        BandwidthCode::Bw125,
        BandwidthCode::Bw250,
        BandwidthCode::Bw500,
    ];
    for bw in codes {
        let hz = bw.to_hz();
        let back = BandwidthCode::from_hz(hz);
        assert_eq!(back.code(), bw.code(), "Round-trip failed for {:?}", bw);
    }
}

#[test]
fn bandwidth_to_hz_values() {
    assert_eq!(BandwidthCode::Bw7_8.to_hz(), 7_800);
    assert_eq!(BandwidthCode::Bw125.to_hz(), 125_000);
    assert_eq!(BandwidthCode::Bw500.to_hz(), 500_000);
}

#[test]
fn bandwidth_code_values_match_sx1262() {
    // Verify the register codes match the SX1262 datasheet
    assert_eq!(BandwidthCode::Bw7_8.code(), 0x00);
    assert_eq!(BandwidthCode::Bw10_4.code(), 0x08);
    assert_eq!(BandwidthCode::Bw15_6.code(), 0x01);
    assert_eq!(BandwidthCode::Bw20_8.code(), 0x09);
    assert_eq!(BandwidthCode::Bw31_25.code(), 0x02);
    assert_eq!(BandwidthCode::Bw41_7.code(), 0x0A);
    assert_eq!(BandwidthCode::Bw62_5.code(), 0x03);
    assert_eq!(BandwidthCode::Bw125.code(), 0x04);
    assert_eq!(BandwidthCode::Bw250.code(), 0x05);
    assert_eq!(BandwidthCode::Bw500.code(), 0x06);
}
