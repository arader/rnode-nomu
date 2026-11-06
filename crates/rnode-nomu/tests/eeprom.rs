//! The EEPROM image that `rnodeconf` reads (#66): the serial number, derived from the machine ID
//! and the PTY symlink path or set with `[eeprom] serial_number`, and the file for an info block
//! signed with `rnodeconf --rom`, `[storage] eeprom`.

mod common;

use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rnode_nomu::config::{Config, ConfigError};
use rnode_nomu::eeprom::{MACHINE_ID_PATHS, derive_serial_number, machine_id_from};

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

#[test]
fn storage_eeprom_names_the_info_block_file() {
    let sim = "[radio]\ndriver = \"sim\"\n[storage]\nradio_settings = \"settings.toml\"\n";
    assert_eq!(Config::from_toml(sim).unwrap().storage.eeprom, None);
    let cfg = Config::from_toml(&format!("{sim}eeprom = \"/var/lib/rnode/eeprom.bin\"\n")).unwrap();
    assert_eq!(
        cfg.storage.eeprom.as_deref(),
        Some(Path::new("/var/lib/rnode/eeprom.bin"))
    );
    assert_eq!(cfg.warnings(), Vec::<String>::new());

    // The stored info block, or the one rnodeconf --rom writes, has its own serial number.
    let both = Config::from_toml(&format!(
        "{sim}eeprom = \"/var/lib/rnode/eeprom.bin\"\n[eeprom]\nserial_number = 42\n"
    ))
    .unwrap();
    assert_eq!(
        both.warnings(),
        [
            "eeprom.serial_number has no effect while storage.eeprom is set: the EEPROM then holds the serial \
          number rnodeconf --rom writes"
        ]
    );
}

/// A daemon whose EEPROM file is not a valid info block warns, keeps running with an unprovisioned
/// EEPROM, and leaves the file alone.
#[test]
fn the_daemon_warns_about_an_eeprom_file_it_cannot_use_and_runs_on() {
    let dir = test_dir("eeprom-corrupt-file");
    fs::DirBuilder::new().mode(0o700).create(dir.join("air")).unwrap();
    let eeprom = dir.join("eeprom.bin");
    fs::write(&eeprom, b"not an info block").unwrap();
    let config = dir.join("daemon.toml");
    fs::write(
        &config,
        format!(
            "[radio]\ndriver = \"sim\"\n[radio.sim]\nair_dir = \"air\"\n[storage]\neeprom = {}\n",
            toml::Value::String(eeprom.to_str().unwrap().to_owned())
        ),
    )
    .unwrap();
    let log = dir.join("daemon.log");
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_rnode-nomu"))
        .arg("--config")
        .arg(&config)
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(fs::File::create(&log).unwrap())
        .spawn()
        .unwrap();
    let expected = format!(
        "WARN  rnode_nomu::storage] ignoring EEPROM file {}: it is 17 bytes long, not 156; the EEPROM reads as \
         unprovisioned",
        eeprom.display()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let text = loop {
        let text = fs::read_to_string(&log).unwrap();
        if text.contains(&expected) || Instant::now() > deadline {
            break text;
        }
        thread::sleep(Duration::from_millis(20));
    };
    let running = daemon.try_wait().unwrap().is_none();
    daemon.kill().unwrap();
    daemon.wait().unwrap();
    assert!(text.contains(&expected), "{text}");
    assert!(running, "the daemon exited: {text}");
    assert_eq!(fs::read(&eeprom).unwrap(), b"not an info block");
}
