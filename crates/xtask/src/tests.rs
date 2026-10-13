use std::fs;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rnode_linux::config::{Config, RadioDriver};

use super::{CLIENT_PY, Children, KISS_BOUNDARY_PY, SERVER_PY, daemon_config};

/// The daemon rejects unknown keys (`deny_unknown_fields`), so the config the harness writes
/// must stay loadable by the rnode-linux it drives.
#[test]
fn daemon_config_loads_with_the_rnode_linux_parser() {
    let air = Path::new("target/rns-e2e/runs/r1-2/sf7/air");
    let pty = Path::new("target/rns-e2e/runs/r1-2/sf7/b/pty");
    let settings = Path::new("target/rns-e2e/runs/r1-2/sf7/b/radio-settings.toml");
    let config = Config::from_toml(&daemon_config(air, "b", pty, settings, -64, 6.5)).unwrap();

    assert_eq!(config.radio.driver, RadioDriver::Sim);
    assert!(!config.radio.avoid_interference);
    assert_eq!(config.radio.sim.air_dir, air);
    assert_eq!(config.radio.sim.node_name, "b");
    assert_eq!(config.radio.sim.rssi_dbm, -64);
    assert_eq!(config.radio.sim.snr_db, 6.5);
    assert_eq!(config.radio.sim.loss, 0.0);
    assert_eq!(config.pty.symlink.as_deref(), Some(pty));
    assert_eq!(config.storage.radio_settings.as_deref(), Some(settings));
    assert_eq!(config.logging.log_level, "debug");
    assert_eq!(config.warnings(), Vec::<String>::new());
}

#[test]
fn daemon_config_quotes_awkward_paths_and_whole_snr_values() {
    let awkward = Path::new("runs/\"quoted\" \\ back\tslash\u{1}/air");
    let config = Config::from_toml(&daemon_config(awkward, "a", awkward, awkward, -58, 9.0)).unwrap();

    assert_eq!(config.radio.sim.air_dir, awkward);
    assert_eq!(config.pty.symlink.as_deref(), Some(awkward));
    assert_eq!(config.storage.radio_settings.as_deref(), Some(awkward));
    assert_eq!(config.radio.sim.snr_db, 9.0);
}

/// Once a daemon has exited, the PTY its symlink pointed at is gone, so a symlink it left behind
/// dangles. `terminate_all()` must still report it.
#[test]
fn terminate_all_reports_a_dangling_pty_symlink() {
    let error = terminate_exited_daemon("dangling-pty-symlink", |link| {
        symlink(link.with_file_name("closed-pty"), link).unwrap();
    })
    .unwrap_err();
    assert!(error.contains("did not remove PTY symlink"), "{error}");
}

#[test]
fn terminate_all_accepts_a_removed_pty_symlink() {
    terminate_exited_daemon("removed-pty-symlink", |_| {}).unwrap();
}

/// Runs `terminate_all()` on a stand-in daemon that has already exited cleanly, after
/// `leave_link` has had the chance to leave something at its PTY symlink path.
fn terminate_exited_daemon(name: &str, leave_link: impl FnOnce(&Path)) -> Result<(), String> {
    let dir = test_dir(name);
    let link = dir.join("pty");
    leave_link(&link);
    let mut children = Children::default();
    children
        .spawn_logged("daemon", &mut Command::new("true"), &dir.join("daemon.log"), Some(link))
        .unwrap();
    // Let it exit by itself, so that only the symlink can be at fault.
    let deadline = Instant::now() + Duration::from_secs(10);
    while children.0[0].child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the stand-in daemon did not exit");
        thread::sleep(Duration::from_millis(10));
    }
    children.terminate_all().map_err(|e| e.to_string())
}

/// A fresh, empty directory under this crate's `target/`.
fn test_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("xtask-tests")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// The harness's Python only runs by hand (`cargo xtask rns-e2e`), so check here that it at least
/// compiles. `compile()` parses and compiles without running or importing anything. Skipped when
/// `python3` is not on PATH; the CI builder image has it.
#[test]
fn harness_python_compiles() {
    let scripts = [
        ("kiss_boundary.py", KISS_BOUNDARY_PY),
        ("rns_server.py", SERVER_PY),
        ("rns_client.py", CLIENT_PY),
        (
            "tests/e2e/kiss_probe.py",
            include_str!("../../../tests/e2e/kiss_probe.py"),
        ),
    ];
    for (name, source) in scripts {
        let Some(output) = python_compile(name, source) else {
            eprintln!("skipping the Python syntax check: python3 is not on PATH");
            return;
        };
        assert!(
            output.status.success(),
            "{name} does not compile:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// Compiles `source` with `python3`, or returns `None` when there is no `python3` to run.
fn python_compile(name: &str, source: &str) -> Option<Output> {
    let check = "import sys; compile(sys.stdin.buffer.read(), sys.argv[1], 'exec')";
    let mut python = match Command::new("python3")
        .args(["-I", "-c", check, name])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(python) => python,
        Err(e) if e.kind() == ErrorKind::NotFound => return None,
        Err(e) => panic!("start python3: {e}"),
    };
    let mut stdin = python.stdin.take().expect("python3 stdin is piped");
    stdin.write_all(source.as_bytes()).expect("write the script to python3");
    drop(stdin);
    Some(python.wait_with_output().expect("wait for python3"))
}
