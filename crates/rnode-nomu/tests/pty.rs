//! The host's PTY: raw mode, slave permissions, and `PendingHostOutput` when the host stalls or
//! flushes its input.

mod common;

use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::stat::Mode;
use nix::sys::termios::{ControlFlags, FlushArg, InputFlags, LocalFlags, OutputFlags, tcflush, tcgetattr};
use nix::unistd::{getegid, mkfifo};
use rnode_nomu::pty::{PendingHostOutput, PtyError, PtyPair, PtyPermissions};
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

/// Issue #52: `[pty] symlink` replaces a symlink, stale or dangling, and refuses everything else,
/// keeping it as it was.
#[test]
#[serial]
fn pty_symlink_replaces_only_a_symlink() {
    let dir = test_dir("pty-symlink");
    let refused = |path: &Path| match PtyPair::open(Some(path)).unwrap_err() {
        PtyError::NotASymlink { path: reported, kind } => {
            assert_eq!(reported, path);
            kind
        }
        other => panic!("{}: unexpected error {other}", path.display()),
    };

    let file = dir.join("file");
    fs::write(&file, b"precious").unwrap();
    assert_eq!(refused(&file), "regular file");
    assert_eq!(fs::read(&file).unwrap(), b"precious");
    assert_eq!(
        PtyPair::open(Some(&file)).unwrap_err().to_string(),
        format!(
            "refusing to replace {} with the PTY symlink: it is a regular file, not a symlink",
            file.display()
        )
    );

    let subdir = dir.join("dir");
    fs::create_dir(&subdir).unwrap();
    assert_eq!(refused(&subdir), "directory");
    assert!(subdir.is_dir());

    let fifo = dir.join("fifo");
    mkfifo(&fifo, Mode::from_bits_truncate(0o600)).unwrap();
    assert_eq!(refused(&fifo), "FIFO");
    assert!(fs::symlink_metadata(&fifo).unwrap().file_type().is_fifo());

    // Relative, because Unix socket paths are limited to about 104 bytes.
    let socket = PathBuf::from(format!("target/pty-symlink-{}.sock", std::process::id()));
    let _ = fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).unwrap();
    assert_eq!(refused(&socket), "socket");
    assert!(fs::symlink_metadata(&socket).unwrap().file_type().is_socket());
    drop(listener);
    fs::remove_file(&socket).unwrap();

    // A symlink left by an earlier run is replaced; what it pointed to is untouched.
    let link = dir.join("rnode0");
    let target = dir.join("target");
    fs::write(&target, b"target").unwrap();
    symlink(&target, &link).unwrap();
    let pty = PtyPair::open(Some(&link)).unwrap();
    assert_eq!(fs::read_link(&link).unwrap(), pty.slave_path());
    assert_eq!(fs::read(&target).unwrap(), b"target");
    drop(pty);
    assert!(fs::symlink_metadata(&link).is_err(), "removed on exit");

    let dangling = dir.join("gone");
    symlink(&dangling, &link).unwrap();
    let pty = PtyPair::open(Some(&link)).unwrap();
    assert_eq!(fs::read_link(&link).unwrap(), pty.slave_path());
}

/// Issue #52: on exit the symlink is removed only while it still points to this daemon's slave;
/// a path another process has taken over in the meantime is left alone.
#[test]
#[serial]
fn pty_symlink_is_removed_on_exit_only_while_it_is_ours() {
    let dir = test_dir("pty-symlink-exit");
    let link = dir.join("rnode0");

    let mut pty = PtyPair::open(Some(&link)).unwrap();
    pty.remove_symlink().unwrap();
    assert!(fs::symlink_metadata(&link).is_err());
    pty.remove_symlink().unwrap();

    // Another daemon published its own PTY under the same name.
    let mut pty = PtyPair::open(Some(&link)).unwrap();
    let other = dir.join("other-pty");
    fs::remove_file(&link).unwrap();
    symlink(&other, &link).unwrap();
    pty.remove_symlink().unwrap();
    assert_eq!(fs::read_link(&link).unwrap(), other);
    drop(pty);
    assert_eq!(fs::read_link(&link).unwrap(), other);

    // Replaced by something that is not a symlink: dropping the PTY leaves it too.
    fs::remove_file(&link).unwrap();
    let pty = PtyPair::open(Some(&link)).unwrap();
    fs::remove_file(&link).unwrap();
    fs::write(&link, b"someone else's").unwrap();
    drop(pty);
    assert_eq!(fs::read(&link).unwrap(), b"someone else's");
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
fn pending_host_output_drops_the_frames_after_a_partial_write() {
    let mut output = PendingHostOutput::default();
    let mut offered = Vec::new();
    output
        .flush_bytes_with(&[0xC0, 0x01, 0xC0, 0xC0, 0x02, 0xC0, 0xC0, 0x03, 0xC0], |bytes| {
            offered.push(bytes.to_vec());
            Ok(bytes.len().min(2))
        })
        .unwrap();
    assert_eq!(offered, [[0xC0, 0x01, 0xC0]], "the later frames are not offered");
    assert_eq!(output.buffered_bytes(), [0xC0]);
    assert_eq!(output.dropped_frames_for_tests(), 2);
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

/// A writer for a host that reads `budget` more bytes and then stops: the PTY takes that much
/// and is full after it.
fn trickle(budget: usize) -> impl FnMut(&[u8]) -> std::io::Result<usize> {
    let mut left = budget;
    move |bytes| {
        let n = bytes.len().min(left);
        left -= n;
        Ok(n)
    }
}

/// Issue #56: while a host reads only a few bytes at a time, the stall warning stays latched: a
/// partial write of the pending tail or of a new frame does not re-arm it. It re-arms once the
/// tail has drained, so the next stall warns again.
#[test]
fn pending_host_output_warning_stays_latched_until_the_tail_drains() {
    let frame = [0xC0, 0x00, 1, 2, 3, 4, 5, 6, 7, 8, 0xC0];
    let mut output = PendingHostOutput::default();

    // A stall: the frame is dropped with a warning.
    output.flush_bytes_with(&frame, trickle(0)).unwrap();
    assert_eq!(output.dropped_frames_for_tests(), 1);
    assert_eq!(output.stall_warnings_for_tests(), 1);

    // The host reads 4 bytes: the next frame goes out partly, the one after it is dropped, and
    // the stall is still the same one.
    output.flush_bytes_with(&[frame, frame].concat(), trickle(4)).unwrap();
    assert_eq!(output.len(), 7);
    assert_eq!(output.dropped_frames_for_tests(), 2);
    assert_eq!(output.stall_warnings_for_tests(), 1);

    // A few bytes at a time, with nothing new to send: the tail shrinks, the warning stays latched.
    for left in [4, 1] {
        output.flush_with(trickle(3)).unwrap();
        assert_eq!(output.len(), left);
        assert!(output.has_warned_drop_for_tests(), "{left} bytes left");
    }
    output.flush_bytes_with(&frame, trickle(0)).unwrap();
    assert_eq!(output.dropped_frames_for_tests(), 3);
    assert_eq!(output.stall_warnings_for_tests(), 1);

    // The last byte of the tail goes out: the PTY is draining again.
    output.flush_with(trickle(3)).unwrap();
    assert!(output.is_empty());
    assert!(!output.has_warned_drop_for_tests());
    output.flush_bytes_with(&frame, trickle(0)).unwrap();
    assert_eq!(output.dropped_frames_for_tests(), 4);
    assert_eq!(output.stall_warnings_for_tests(), 2);
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
