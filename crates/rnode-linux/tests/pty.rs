//! The host's PTY: raw mode, slave permissions, and `PendingHostOutput` when the host stalls or
//! flushes its input.

mod common;

use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::time::Duration;

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::termios::{ControlFlags, FlushArg, InputFlags, LocalFlags, OutputFlags, tcflush, tcgetattr};
use nix::unistd::getegid;
use rnode_linux::pty::{PendingHostOutput, PtyPair, PtyPermissions};
use serial_test::serial;

use common::test_dir;

#[test]
#[serial]
fn pty_is_raw_idle_poll_times_out_and_data_round_trips() {
    let dir = test_dir("pty");
    let link = dir.join("rnode0");
    let pty = PtyPair::open(Some(&link)).unwrap();
    assert_eq!(fs::read_link(&link).unwrap(), pty.slave_path());

    // Raw before any host configures it: no echo, line editing, signals or CR/LF translation.
    let termios = tcgetattr(pty.slave_fd_for_tests()).unwrap();
    for flag in [
        LocalFlags::ECHO,
        LocalFlags::ICANON,
        LocalFlags::ISIG,
        LocalFlags::IEXTEN,
    ] {
        assert!(!termios.local_flags.contains(flag), "{flag:?}");
    }
    for flag in [
        InputFlags::ICRNL,
        InputFlags::INLCR,
        InputFlags::IGNCR,
        InputFlags::IXON,
    ] {
        assert!(!termios.input_flags.contains(flag), "{flag:?}");
    }
    assert!(!termios.output_flags.contains(OutputFlags::OPOST));
    assert_eq!(termios.control_flags & ControlFlags::CSIZE, ControlFlags::CS8);

    // No host attached: poll() blocks instead of spinning on POLLHUP.
    let mut fds = [PollFd::new(pty.master_fd(), PollFlags::POLLIN)];
    assert_eq!(poll(&mut fds, PollTimeout::from(20_u16)).unwrap(), 0);

    let mut slave = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(pty.slave_path())
        .unwrap();
    slave.write_all(b"abc").unwrap();
    std::thread::sleep(Duration::from_millis(10));
    let mut buf = [0_u8; 8];
    assert_eq!(pty.read(&mut buf).unwrap(), 3);
    assert_eq!(&buf[..3], b"abc");

    pty.write_nonblocking(b"xyz").unwrap();
    let mut out = [0_u8; 8];
    assert_eq!(slave.read(&mut out).unwrap(), 3);
    assert_eq!(&out[..3], b"xyz");

    // A host that disconnects leaves the master idle, not hung up.
    drop(slave);
    let mut fds = [PollFd::new(pty.master_fd(), PollFlags::POLLIN)];
    assert_eq!(poll(&mut fds, PollTimeout::from(20_u16)).unwrap(), 0);
}

/// Issue #47: the slave gets the configured group and mode before its symlink is published.
#[test]
#[serial]
fn pty_slave_gets_configured_group_and_mode() {
    let slave_stat = |pty: &PtyPair| {
        let fd = pty.slave_fd_for_tests().try_clone_to_owned().unwrap();
        File::from(fd).metadata().unwrap()
    };
    let default_gid = slave_stat(&PtyPair::open(None).unwrap()).gid();

    let dir = test_dir("pty-permissions");
    let link = dir.join("rnode0");
    let egid = getegid().as_raw();
    let permissions = PtyPermissions {
        gid: Some(egid),
        mode: Some(0o660),
    };
    let pty = PtyPair::open_with_permissions(Some(&link), permissions).unwrap();
    let stat = slave_stat(&pty);
    assert_eq!(stat.mode() & 0o7777, 0o660);
    assert_eq!(stat.gid(), egid);
    assert_eq!(fs::read_link(&link).unwrap(), pty.slave_path());

    let mode_only = PtyPair::open_with_permissions(
        None,
        PtyPermissions {
            gid: None,
            mode: Some(0o600),
        },
    )
    .unwrap();
    let stat = slave_stat(&mode_only);
    assert_eq!(stat.mode() & 0o7777, 0o600);
    assert_eq!(stat.gid(), default_gid);
}

#[test]
fn pending_host_output_drops_new_frames_when_writer_stalls() {
    let mut output = PendingHostOutput::default();
    output
        .flush_bytes_with(&[0xC0, 0x01, 0xC0, 0xC0, 0x02, 0xC0], |_| Ok(0))
        .unwrap();
    assert!(output.is_empty());
    assert_eq!(output.dropped_frames_for_tests(), 2);
    assert!(output.has_warned_drop_for_tests());
}

#[test]
fn pending_host_output_finishes_partial_tail_before_next_frame() {
    let mut output = PendingHostOutput::default();
    output
        .flush_bytes_with(&[0xC0, 0x01, 0xC0], |bytes| Ok(bytes.len().min(2)))
        .unwrap();
    assert_eq!(output.buffered_bytes(), vec![0xC0]);

    let mut writes = Vec::new();
    output
        .flush_bytes_with(&[0xC0, 0x02, 0xC0], |bytes| {
            writes.push(bytes.to_vec());
            Ok(bytes.len())
        })
        .unwrap();
    assert_eq!(writes, vec![vec![0xC0], vec![0xC0, 0x02, 0xC0]]);
    assert!(output.is_empty());
    assert_eq!(output.dropped_frames_for_tests(), 0);
}

#[test]
fn pending_host_output_warning_rearms_after_successful_write() {
    let mut output = PendingHostOutput::default();
    output.flush_bytes_with(&[0xC0, 0x01, 0xC0], |_| Ok(0)).unwrap();
    assert!(output.has_warned_drop_for_tests());
    output.flush_bytes_with(&[0xC0, 0x02, 0xC0], |_| Ok(0)).unwrap();
    assert!(output.has_warned_drop_for_tests());
    assert_eq!(output.dropped_frames_for_tests(), 2);

    output
        .flush_bytes_with(&[0xC0, 0x03, 0xC0], |bytes| Ok(bytes.len()))
        .unwrap();
    assert!(!output.has_warned_drop_for_tests());
    output.flush_bytes_with(&[0xC0, 0x04, 0xC0], |_| Ok(0)).unwrap();
    assert!(output.has_warned_drop_for_tests());
    assert_eq!(output.dropped_frames_for_tests(), 3);
}

#[test]
fn pending_host_output_flushes_to_real_pty() {
    let pty = PtyPair::open(None).unwrap();
    let mut slave = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(pty.slave_path())
        .unwrap();
    let mut output = PendingHostOutput::default();
    output.flush_bytes_to(&[0xC0, 0x42, 0xC0], &pty).unwrap();
    let mut buf = [0_u8; 3];
    slave.read_exact(&mut buf).unwrap();
    assert_eq!(buf, [0xC0, 0x42, 0xC0]);
    assert!(output.is_empty());
}

/// A host that stops reading stalls the real PTY, and the frames offered then are dropped. After
/// the host flushes its input (pyserial's `Serial.open()` does `tcflush(TCIFLUSH)`), it reads only
/// the unwritten remainder of the frame that was in flight, then fresh frames: nothing dropped is
/// replayed. The PTY is filled through `PendingHostOutput` up to its first drop, and the test then
/// checks exactly what the host reads. It never assumes the PTY stays full: Linux moves PTY data
/// from the flip buffer into the line discipline asynchronously, so space can free up at any time.
#[test]
#[serial]
fn pending_host_output_real_pty_drops_stale_frames_after_host_flush() {
    let pty = PtyPair::open(None).unwrap();
    let mut host = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(pty.slave_path())
        .unwrap();
    let mut stale = vec![0x55; 256];
    stale[0] = 0xC0;
    stale[255] = 0xC0;
    let fresh = [0xC0, 0x99, 0x01, 0xC0];

    let mut output = PendingHostOutput::default();
    for _ in 0..16_384 {
        if output.dropped_frames_for_tests() > 0 {
            break;
        }
        output.flush_bytes_to(&stale, &pty).unwrap();
    }
    assert_eq!(output.dropped_frames_for_tests(), 1, "the PTY never filled up");
    let in_flight = output.buffered_bytes();
    assert!(
        in_flight.len() < stale.len() && stale.ends_with(&in_flight),
        "{in_flight:02X?}"
    );

    tcflush(host.as_fd(), FlushArg::TCIFLUSH).unwrap();
    output.flush_bytes_to(&fresh, &pty).unwrap();
    assert!(output.is_empty());
    assert_eq!(output.dropped_frames_for_tests(), 1);

    let mut expected = in_flight;
    expected.extend_from_slice(&fresh);
    let mut received = vec![0_u8; expected.len()];
    host.read_exact(&mut received).unwrap();
    assert_eq!(received, expected);
    let mut fds = [PollFd::new(host.as_fd(), PollFlags::POLLIN)];
    assert_eq!(
        poll(&mut fds, PollTimeout::ZERO).unwrap(),
        0,
        "stale output was replayed"
    );
}
