//! SPI: how embedded-hal operations become a Linux spidev transaction, and the SPI trace recorder.

mod common;

use embedded_hal::spi::Operation;
use rnode_nomu::spi::{SpiRecorder, SpiSegmentKind, build_segment_plan};

use common::test_dir;

#[test]
fn spi_segment_plan_preserves_delay_order_for_linux_transaction() {
    let mut read = [0; 2];
    let ops = [
        Operation::Write(&[0x9f]),
        Operation::DelayNs(1_500),
        Operation::Read(&mut read),
    ];
    let plan = build_segment_plan(&ops);
    assert_eq!(plan.len(), 3);
    assert_eq!(plan[0].kind, SpiSegmentKind::Write);
    assert_eq!(plan[0].op_index, Some(0));
    assert_eq!(plan[0].tx, vec![0x9f]);
    assert_eq!(plan[1].kind, SpiSegmentKind::DelayUs(2));
    assert_eq!(plan[1].op_index, None);
    assert_eq!(plan[2].kind, SpiSegmentKind::Read);
    assert_eq!(plan[2].op_index, Some(2));
    assert_eq!(plan[2].tx, vec![0, 0]);
}

#[test]
fn spi_segment_plan_covers_transfer_variants() {
    let mut transfer_read = [0; 4];
    let mut inplace = [0xAA, 0xBB];
    let ops = [
        Operation::Transfer(&mut transfer_read, &[0x01, 0x02]),
        Operation::TransferInPlace(&mut inplace),
    ];
    let plan = build_segment_plan(&ops);
    assert_eq!(plan[0].kind, SpiSegmentKind::Transfer);
    assert_eq!(plan[0].tx, vec![0x01, 0x02, 0x00, 0x00]);
    assert_eq!(plan[0].rx_len, 4);
    assert_eq!(plan[1].kind, SpiSegmentKind::InPlace);
    assert_eq!(plan[1].tx, vec![0xAA, 0xBB]);
    assert_eq!(plan[1].rx_len, 2);
}

#[test]
fn recorder_create_on_unwritable_path_returns_error() {
    let dir = test_dir("directory-is-not-a-file");
    assert!(SpiRecorder::create(&dir).is_err());
}
