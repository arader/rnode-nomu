use std::fs;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rnode_linux::config::{Config, RadioDriver};

use super::{CLIENT_PY, Children, KISS_BOUNDARY_PY, SERVER_PY, Scenario, create_private_dir, daemon_config};

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
    children
        .terminate_all(Duration::from_secs(3))
        .map_err(|e| e.to_string())
}

/// A daemon finishes the packet on the air before it exits after SIGTERM (#51), so the harness
/// must wait at least as long as the longest packet takes on the simulated air, plus the 3 s the
/// processes need to exit. The simulated air times a frame with Semtech's formula: (preamble +
/// 4.25 + 8 + ceil((8 x 255 - 4 SF + 28 + 16) / (4 SF)) x 5) symbols of 2^SF / 125 kHz, with a
/// 24-symbol preamble at SF7 and 18 symbols at SF11 (firmware `LORA_PREAMBLE_SYMBOLS_MIN`). The
/// longest packet, 508 bytes, takes two such 255-byte frames.
#[test]
fn sigterm_wait_outlasts_the_longest_packet_on_the_simulated_air() {
    for (sf, frame_ms) in [(7, 416.0), (11, 4_345.856)] {
        let wait_ms = Scenario { name: "test", sf }.sigterm_wait().as_secs_f64() * 1000.0;
        assert!(wait_ms >= 2.0 * frame_ms + 3000.0, "SF{sf}: {wait_ms} ms");
        assert!(
            wait_ms <= 2.0 * (2.0 * frame_ms) + 3000.0,
            "SF{sf}: {wait_ms} ms is excessive"
        );
    }
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

/// Issue #60: the probe's `expect()` reports a payload that arrives without RSSI and SNR right
/// before it as a `RuntimeError`, also when another data frame came just before, where it used to
/// fail with a `TypeError`; `expect_absent()` reports any data frame. The probe runs with fake
/// ports and a stub `serial` module, so no pyserial is needed. Skipped without `python3`.
#[test]
fn kiss_boundary_probe_reports_missing_metadata_and_stray_data() {
    let driver = r#"
import sys, time, types
sys.modules['serial'] = types.ModuleType('serial')
probe = {'__name__': 'kiss_boundary'}
exec(compile(sys.stdin.read(), 'kiss_boundary.py', 'exec'), probe)
RSSI, SNR, DATA = probe['CMD_STAT_RSSI'], probe['CMD_STAT_SNR'], probe['CMD_DATA']

class Port:
    def __init__(self, frames): self.frames = frames
    def read_frames(self, deadline):
        frames, self.frames = self.frames, []
        if not frames: time.sleep(0.01)
        return frames

def outcome(check, frames):
    try:
        check(Port(frames), b'payload', 0.1)
        return 'ok'
    except RuntimeError as e:
        return f'RuntimeError: {e}'

expect, absent = probe['expect'], probe['expect_absent']
print(outcome(expect, [(RSSI, b'\x60'), (SNR, b'\x1c'), (DATA, b'payload')]))
print(outcome(expect, [(RSSI, b'\x60'), (DATA, b'stray'), (DATA, b'payload')]))
print(outcome(expect, [(DATA, b'payload')]))
print(outcome(expect, [(DATA, b'stray')]))
print(outcome(absent, [(RSSI, b'\x60')]))
print(outcome(absent, [(RSSI, b'\x60'), (DATA, b'stray')]))
"#;
    let Some(output) = python("probe-check", driver, KISS_BOUNDARY_PY) else {
        eprintln!("skipping the KISS probe check: python3 is not on PATH");
        return;
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        stdout.lines().collect::<Vec<_>>(),
        [
            "ok",
            "RuntimeError: missing RSSI/SNR immediately before data len 7: [(35, b'`'), (0, 5)]",
            "RuntimeError: missing RSSI/SNR immediately before data len 7: []",
            "RuntimeError: missing payload len 7, saw [(0, 5)]",
            "ok",
            "RuntimeError: over-MTU payload produced CMD_DATA frames: [(35, b'`'), (0, 5)]",
        ]
    );
}

/// Issue #60: the harness creates the air directory private to its owner in one step, with any
/// missing parents, and refuses an existing directory that others can open rather than change it.
#[test]
fn create_private_dir_creates_it_private_and_refuses_an_open_one() {
    let dir = test_dir("private-dir");
    let mode = |path: &Path| fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777;

    let air = dir.join("run").join("air");
    create_private_dir(&air).unwrap();
    assert_eq!(mode(&air), 0o700);
    create_private_dir(&air).unwrap();

    let open = dir.join("open");
    fs::create_dir(&open).unwrap();
    fs::set_permissions(&open, fs::Permissions::from_mode(0o755)).unwrap();
    let error = create_private_dir(&open).unwrap_err();
    assert!(error.to_string().contains("mode 0755"), "{error}");
    assert_eq!(mode(&open), 0o755, "left as it was");

    let link = dir.join("link");
    symlink(&air, &link).unwrap();
    assert!(create_private_dir(&link).is_err(), "a symlink to a private directory");
}

/// Compiles `source` with `python3`, or returns `None` when there is no `python3` to run.
fn python_compile(name: &str, source: &str) -> Option<Output> {
    python(
        name,
        "import sys; compile(sys.stdin.buffer.read(), sys.argv[1], 'exec')",
        source,
    )
}

/// Runs `code` with `python3 -I`, `name` as its argument and `stdin` as its input, or returns `None`
/// when there is no `python3` to run.
fn python(name: &str, code: &str, stdin: &str) -> Option<Output> {
    let mut python = match Command::new("python3")
        .args(["-I", "-c", code, name])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(python) => python,
        Err(e) if e.kind() == ErrorKind::NotFound => return None,
        Err(e) => panic!("start python3: {e}"),
    };
    let mut input = python.stdin.take().expect("python3 stdin is piped");
    input.write_all(stdin.as_bytes()).expect("write the input to python3");
    drop(input);
    Some(python.wait_with_output().expect("wait for python3"))
}
