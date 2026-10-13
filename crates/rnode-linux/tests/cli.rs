//! The command line (issue #54): fatal errors name each cause once, `--version` names the
//! firmware version reported to hosts, and `--spilog` is refused with the simulated radio.

mod common;

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::Context;
use rnode_linux::cli::ErrorChain;
use rnode_linux::config::Config;
use rnode_linux::spi::SpiRecorder;

use common::test_dir;

/// Run the `rnode-linux` binary with `args`; fail if it is still running after 10 s.
fn rnode_linux(args: &[&OsStr]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rnode-linux"))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "rnode-linux {args:?} did not exit; stderr: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

/// The message of the error reading `path` gives, as it does not exist.
fn not_found(path: &Path) -> String {
    fs::read(path).unwrap_err().to_string()
}

#[test]
fn error_chain_names_each_cause_once() {
    let dir = test_dir("cli-error-chain");
    let missing = dir.join("missing.toml");
    let cause = not_found(&missing);
    let path = missing.display();

    // `ConfigError::Read` puts its io error in its message and also returns it as its source.
    let error = Config::load(&missing)
        .with_context(|| format!("load {path}"))
        .unwrap_err();
    assert_eq!(
        format!("{error:#}"),
        format!("load {path}: failed to read {path}: {cause}: {cause}"),
        "what anyhow prints"
    );
    assert_eq!(
        ErrorChain(&*error).to_string(),
        format!("load {path}: failed to read {path}: {cause}")
    );

    // `LinuxSpiError::Io` does the same, one level down.
    let csv = dir.join("no-such-dir").join("spi.csv");
    let error = anyhow::Error::new(SpiRecorder::create(&csv).unwrap_err()).context("open SPI log");
    assert_eq!(
        ErrorChain(&*error).to_string(),
        format!("open SPI log: spi io: {}", not_found(&csv))
    );

    // A source that its error's message leaves out is printed.
    let error = anyhow::Error::new(io::Error::other("the cause")).context("open PTY");
    assert_eq!(ErrorChain(&*error).to_string(), "open PTY: the cause");
}

#[test]
fn fatal_errors_name_each_cause_once() {
    let dir = test_dir("cli-fatal");
    let missing = dir.join("missing.toml");
    let output = rnode_linux(&["--config".as_ref(), missing.as_os_str()]);
    assert_eq!(output.status.code(), Some(1));
    let path = missing.display();
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!("fatal: load {path}: failed to read {path}: {}\n", not_found(&missing))
    );
}

#[test]
fn version_names_the_firmware_version_reported_to_hosts() {
    for flag in ["--version", "-V"] {
        let output = rnode_linux(&[flag.as_ref()]);
        assert!(output.status.success(), "{flag}: {output:?}");
        // Firmware `Config.h`: MAJ_VERS 0x01, MIN_VERS 0x56.
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!(
                "rnode-linux {} (reports RNode firmware 1.86 to hosts)\n",
                env!("CARGO_PKG_VERSION")
            ),
            "{flag}"
        );
    }
}

#[test]
fn spilog_is_refused_with_the_simulated_radio() {
    let dir = test_dir("cli-spilog");
    let config = dir.join("sim.toml");
    fs::write(&config, "[radio]\ndriver = \"sim\"\n").unwrap();
    let csv = dir.join("spi.csv");
    let output = rnode_linux(&[
        "--config".as_ref(),
        config.as_os_str(),
        "--spilog".as_ref(),
        csv.as_os_str(),
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "fatal: --spilog records SPI transactions, but radio.driver = \"sim\" has no SPI bus\n"
    );
    assert!(!csv.exists());
}
