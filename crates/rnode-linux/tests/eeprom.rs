//! The serial number of the EEPROM image that `rnodeconf` reads (#66): derived from the machine ID
//! and the PTY symlink path, or set with `[eeprom] serial_number`.

mod common;

use std::fs;
use std::path::Path;

use rnode_linux::config::{Config, ConfigError};
use rnode_linux::eeprom::{MACHINE_ID_PATHS, derive_serial_number, machine_id_from};

use common::test_dir;

const MACHINE_ID: &str = "0123456789abcdef0123456789abcdef";

/// The first 4 bytes of `hashlib.md5(b"rnode-linux EEPROM serial number\0" + machine_id + b"\0"
/// + symlink).digest()`, computed with Python.
#[test]
fn derived_serial_numbers_are_the_python_md5_prefixes() {
    let symlink = Path::new("/run/rnode/rnode0");
    for (machine_id, symlink, serial) in [
        (Some(MACHINE_ID), Some(symlink), 0x4981_38E1),
        (Some(MACHINE_ID), Some(Path::new("/run/rnode/rnode1")), 0x24C0_2422),
        (None, Some(symlink), 0xA9AC_6387),
        (Some(MACHINE_ID), None, 0x16A1_2BA3),
        (None, None, 0xE595_1076),
    ] {
        assert_eq!(
            derive_serial_number(machine_id, symlink),
            serial,
            "{machine_id:?} {symlink:?}"
        );
    }
}

#[test]
fn the_machine_id_comes_from_the_first_file_that_holds_one() {
    assert_eq!(MACHINE_ID_PATHS, ["/etc/machine-id", "/var/lib/dbus/machine-id"]);
    let dir = test_dir("eeprom-machine-id");
    let missing = dir.join("missing");
    let blank = dir.join("blank");
    let systemd = dir.join("machine-id");
    let dbus = dir.join("dbus-machine-id");
    fs::write(&blank, " \n").unwrap();
    fs::write(&systemd, format!("{MACHINE_ID}\n")).unwrap();
    fs::write(&dbus, "fedcba9876543210fedcba9876543210").unwrap();

    assert_eq!(
        machine_id_from([missing.as_path(), blank.as_path(), systemd.as_path(), dbus.as_path()]).as_deref(),
        Some(MACHINE_ID)
    );
    assert_eq!(
        machine_id_from([missing.as_path(), dbus.as_path()]).as_deref(),
        Some("fedcba9876543210fedcba9876543210")
    );
    assert_eq!(machine_id_from([missing.as_path(), blank.as_path()]), None);
}

#[test]
fn eeprom_serial_number_replaces_the_derived_one() {
    let sim = "[radio]\ndriver = \"sim\"\n[pty]\nsymlink = \"/run/rnode/rnode0\"\n";
    let derived = Config::from_toml(sim).unwrap();
    assert_eq!(derived.eeprom.serial_number, None);
    assert_eq!(derived.serial_number(Some(MACHINE_ID)), 0x4981_38E1);
    assert_eq!(derived.serial_number(None), 0xA9AC_6387);
    // The node configuration from the file alone leaves the serial number to the daemon.
    assert_eq!(derived.node_config().serial_number, 0);

    for (value, serial) in [
        ("0", 0),
        ("42", 42),
        ("0xC0FFEE", 0x00C0_FFEE),
        ("4294967295", u32::MAX),
    ] {
        let set = Config::from_toml(&format!("{sim}[eeprom]\nserial_number = {value}\n")).unwrap();
        assert_eq!(set.eeprom.serial_number, Some(serial), "{value}");
        assert_eq!(set.serial_number(Some(MACHINE_ID)), serial, "{value}");
        assert_eq!(set.serial_number(None), serial, "{value}");
    }
    for value in ["-1", "4294967296", "\"42\"", "1.5"] {
        let err = Config::from_toml(&format!("{sim}[eeprom]\nserial_number = {value}\n")).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{value}: {err}");
        assert!(err.to_string().contains("serial_number"), "{value}: {err}");
    }
    let err = Config::from_toml(&format!("{sim}[eeprom]\nserial = 42\n")).unwrap_err();
    assert!(err.to_string().contains("unknown field `serial`"), "{err}");
}
