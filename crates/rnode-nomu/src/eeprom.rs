//! The serial number in the EEPROM image that `rnodeconf` reads (`rnode_core::eeprom`).
//!
//! An RNode's serial number is assigned when its EEPROM is provisioned. rnode-nomu has no EEPROM,
//! so it derives a serial number that stays the same across restarts: from the machine ID, which
//! identifies the installation, and the PTY symlink path, which tells several daemons on one
//! machine apart. `[eeprom] serial_number` in the config replaces it.

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use md5::{Digest, Md5};

/// Where the machine ID is looked for, in this order: systemd's file, then the D-Bus copy that
/// systems without systemd keep (machine-id(5)).
pub const MACHINE_ID_PATHS: [&str; 2] = ["/etc/machine-id", "/var/lib/dbus/machine-id"];

/// What the hashed data starts with, so that the serial number is specific to this daemon. It keeps
/// the project's former name, rnode-linux, so that the serial numbers daemons derived before the
/// rename stay the same. machine-id(5) asks applications not to expose the machine ID, but to hash
/// it with a key of their own; only 4 bytes of this MD5 digest are used, which cannot give the
/// machine ID away.
const SERIAL_NUMBER_KEY: &[u8] = b"rnode-linux EEPROM serial number\0";

/// The machine ID, from the first of [`MACHINE_ID_PATHS`] that holds one; see [`machine_id_from`].
pub fn machine_id() -> Option<String> {
    machine_id_from(MACHINE_ID_PATHS.iter().map(Path::new))
}

/// The contents of the first of `paths` that can be read and is not blank, without surrounding
/// whitespace, or `None` when no file has one (a container or another system without a machine
/// ID).
pub fn machine_id_from<'a>(paths: impl IntoIterator<Item = &'a Path>) -> Option<String> {
    paths.into_iter().find_map(|path| {
        let text = fs::read_to_string(path).ok()?;
        let id = text.trim();
        (!id.is_empty()).then(|| id.to_owned())
    })
}

/// The serial number of the daemon whose PTY symlink is `pty_symlink`, on the machine whose ID is
/// `machine_id`: the first 4 bytes, big-endian, of the MD5 of `rnode-linux EEPROM serial number`
/// and a zero byte, the machine ID, another zero byte and the symlink path as the config gives it.
/// Without a machine ID the serial number depends on the symlink path alone, and without a symlink
/// on the machine ID alone.
pub fn derive_serial_number(machine_id: Option<&str>, pty_symlink: Option<&Path>) -> u32 {
    let mut md5 = Md5::new();
    md5.update(SERIAL_NUMBER_KEY);
    md5.update(machine_id.unwrap_or_default().as_bytes());
    md5.update([0]);
    md5.update(pty_symlink.map_or(&[][..], |path| path.as_os_str().as_bytes()));
    let digest = md5.finalize();
    u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]])
}
