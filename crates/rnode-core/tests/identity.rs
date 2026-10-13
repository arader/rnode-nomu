//! The identity a node reports to the host.

use rnode_core::identity::*;

#[test]
fn default_identity_does_not_collide_with_firmware_platforms() {
    let id = DeviceIdentity::default();
    assert!(![0x70, 0x80, 0x90].contains(&id.platform));
    assert!(![0x71, 0x81, 0x91, 0x92].contains(&id.mcu));
    assert_eq!((id.fw_major, id.fw_minor), (1, 0x56));
}
