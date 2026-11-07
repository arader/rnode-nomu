//! The Debian package that release builds make with cargo-deb (`[package.metadata.deb]` in
//! Cargo.toml): it installs the repository's systemd unit with the package's binary path, and
//! creates the account the unit runs as.

use std::fs;
use std::path::{Path, PathBuf};

fn crate_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(name)
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// A unit's settings: its lines without comments and blank lines.
fn directives(unit: &str) -> Vec<&str> {
    unit.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect()
}

/// The package's unit is contrib/rnode-nomu.service, which a manual install uses with the binary in
/// /usr/local/bin, starting /usr/bin/rnode-nomu instead.
#[test]
fn package_unit_is_the_contrib_unit_starting_usr_bin() {
    let contrib = read(&crate_path("../../contrib/rnode-nomu.service"));
    let package = read(&crate_path("debian/rnode-nomu.service"));
    let expected: Vec<String> = directives(&contrib)
        .into_iter()
        .map(|line| line.replace("ExecStart=/usr/local/bin/rnode-nomu ", "ExecStart=/usr/bin/rnode-nomu "))
        .collect();
    assert!(
        expected
            .iter()
            .any(|line| line.starts_with("ExecStart=/usr/bin/rnode-nomu ")),
        "contrib/rnode-nomu.service does not start /usr/local/bin/rnode-nomu: {expected:?}"
    );
    assert_eq!(directives(&package), expected);
}

/// The sysusers file creates the account and group the unit runs as, and postinst adds that
/// account to the device groups.
#[test]
fn package_creates_the_account_its_unit_runs_as() {
    let unit = read(&crate_path("debian/rnode-nomu.service"));
    let sysusers = read(&crate_path("debian/rnode-nomu.sysusers"));
    let users: Vec<&str> = directives(&sysusers)
        .into_iter()
        .filter_map(|line| line.strip_prefix("u "))
        .filter_map(|rest| rest.split_whitespace().next())
        .collect();
    let [user] = users[..] else {
        panic!("rnode-nomu.sysusers should create one account: {users:?}");
    };
    for key in ["User=", "Group="] {
        let value = directives(&unit).into_iter().find_map(|line| line.strip_prefix(key));
        assert_eq!(value, Some(user), "{key}");
    }
    let postinst = read(&crate_path("debian/postinst"));
    assert!(
        postinst.contains(&format!("usermod -a -G \"$group\" {user}\n")),
        "debian/postinst does not add {user} to the groups"
    );
}

/// Every file the package takes from the repository exists, and every pattern matches one. Files
/// under target/release/ come from the release build.
#[test]
fn package_files_exist() {
    let manifest: toml::Table = read(&crate_path("Cargo.toml")).parse().expect("Cargo.toml");
    let deb = &manifest["package"]["metadata"]["deb"];
    let mut sources = vec![
        deb["license-file"].as_str().expect("license-file"),
        deb["maintainer-scripts"].as_str().expect("maintainer-scripts"),
    ];
    for asset in deb["assets"].as_array().expect("assets") {
        sources.push(asset[0].as_str().expect("asset source"));
    }
    for source in sources
        .into_iter()
        .filter(|source| !source.starts_with("target/release/"))
    {
        let (dir, name) = source.rsplit_once('/').unwrap_or((".", source));
        let matches = match name.split_once('*') {
            None => usize::from(crate_path(source).exists()),
            Some((prefix, suffix)) => fs::read_dir(crate_path(dir))
                .unwrap_or_else(|e| panic!("{source}: {e}"))
                .map(|entry| entry.expect("directory entry").file_name())
                .filter(|file| {
                    let file = file.to_string_lossy();
                    file.len() >= prefix.len() + suffix.len() && file.starts_with(prefix) && file.ends_with(suffix)
                })
                .count(),
        };
        assert!(matches > 0, "the package's {source} matches no file");
    }
}
