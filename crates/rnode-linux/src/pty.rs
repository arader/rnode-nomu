use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};

use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::pty::openpty;
use nix::sys::stat::{Mode, fchmod};
use nix::sys::termios::{SetArg, cfmakeraw, tcgetattr, tcsetattr};
use nix::unistd::{Gid, fchown, read, ttyname, write};

/// PTY pair used for the host KISS session.
#[derive(Debug)]
pub struct PtyPair {
    master: OwnedFd,
    slave: OwnedFd,
    slave_path: PathBuf,
    symlink_path: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    #[error("pty operation failed: {0}")]
    Nix(#[from] nix::Error),
    #[error("pty io failed: {0}")]
    Io(#[from] io::Error),
    #[error("symlink {link} -> {target} failed: {source}")]
    Symlink {
        link: PathBuf,
        target: PathBuf,
        source: io::Error,
    },
    #[error("remove symlink {path} failed: {source}")]
    RemoveSymlink { path: PathBuf, source: io::Error },
    #[error("give PTY slave to group {gid} failed (is the daemon a member?): {source}")]
    SlaveGroup { gid: u32, source: nix::Error },
    #[error("set PTY slave mode {mode:04o} failed: {source}")]
    SlaveMode { mode: u32, source: nix::Error },
}

/// Ownership and permission bits for the PTY slave, applied before it is published.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PtyPermissions {
    /// Group for `fchown(slave, -1, gid)`; `None` keeps the group `openpty()` chose.
    pub gid: Option<u32>,
    /// Permission bits for `fchmod(slave, mode)`; `None` keeps the mode `openpty()` chose.
    pub mode: Option<u32>,
}

impl PtyPair {
    /// Create a raw slave PTY, keep the slave fd open, and make the master non-blocking.
    pub fn open(symlink: Option<&Path>) -> Result<Self, PtyError> {
        Self::open_with_permissions(symlink, PtyPermissions::default())
    }

    /// Like [`PtyPair::open`], applying `permissions` to the slave right after `openpty()`, before
    /// the symlink is published, so a host running as another user can open it.
    pub fn open_with_permissions(symlink: Option<&Path>, permissions: PtyPermissions) -> Result<Self, PtyError> {
        let pair = openpty(None, None)?;
        if let Some(gid) = permissions.gid {
            fchown(&pair.slave, None, Some(Gid::from_raw(gid)))
                .map_err(|source| PtyError::SlaveGroup { gid, source })?;
        }
        if let Some(mode) = permissions.mode {
            let bits = Mode::from_bits_truncate((mode & 0o7777) as nix::libc::mode_t);
            fchmod(&pair.slave, bits).map_err(|source| PtyError::SlaveMode { mode, source })?;
        }
        let mut termios = tcgetattr(&pair.slave)?;
        cfmakeraw(&mut termios);
        tcsetattr(&pair.slave, SetArg::TCSANOW, &termios)?;

        let raw_flags = fcntl(&pair.master, FcntlArg::F_GETFL)?;
        let flags = OFlag::from_bits_truncate(raw_flags) | OFlag::O_NONBLOCK;
        fcntl(&pair.master, FcntlArg::F_SETFL(flags))?;

        let slave_path = ttyname(&pair.slave)?;
        let symlink_path = if let Some(link) = symlink {
            if link.exists() || link.is_symlink() {
                match std::fs::remove_file(link) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(source) => {
                        return Err(PtyError::RemoveSymlink {
                            path: link.into(),
                            source,
                        });
                    }
                }
            }
            std::os::unix::fs::symlink(&slave_path, link).map_err(|source| PtyError::Symlink {
                link: link.into(),
                target: slave_path.clone(),
                source,
            })?;
            Some(link.into())
        } else {
            None
        };

        Ok(Self {
            master: pair.master,
            slave: pair.slave,
            slave_path,
            symlink_path,
        })
    }

    pub fn slave_path(&self) -> &Path {
        &self.slave_path
    }

    pub fn master_fd(&self) -> BorrowedFd<'_> {
        self.master.as_fd()
    }

    pub fn master_raw_fd(&self) -> RawFd {
        self.master.as_raw_fd()
    }

    pub fn read(&self, buf: &mut [u8]) -> Result<usize, PtyError> {
        match read(&self.master, buf) {
            Ok(n) => Ok(n),
            // Holding the slave fd open means Linux reports temporary idleness as EAGAIN;
            // a real EIO would still be returned below if the invariant ever changes.
            Err(nix::Error::EAGAIN) => Ok(0),
            Err(e) => Err(e.into()),
        }
    }

    pub fn write_nonblocking(&self, data: &[u8]) -> Result<usize, PtyError> {
        match write(&self.master, data) {
            Ok(n) => Ok(n),
            // See read(): with the daemon-owned slave open, EAGAIN means the kernel PTY buffer is full.
            Err(nix::Error::EAGAIN) => Ok(0),
            Err(e) => Err(e.into()),
        }
    }

    pub fn slave_fd_for_tests(&self) -> BorrowedFd<'_> {
        self.slave.as_fd()
    }

    pub fn remove_symlink(&mut self) -> Result<(), PtyError> {
        if let Some(path) = self.symlink_path.take() {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(source) => return Err(PtyError::RemoveSymlink { path, source }),
            }
        }
        Ok(())
    }
}

impl Drop for PtyPair {
    fn drop(&mut self) {
        let _ = self.remove_symlink();
    }
}

/// Output buffer for a single partially written KISS frame.
///
/// Output queued in the PTY while no host is attached is stale by the time one connects;
/// RNodeInterface discards it because pyserial 3.5's `Serial.open()` flushes input with
/// `tcflush(fd, TCIFLUSH)` (`serialposix.py`, `_reset_input_buffer()`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PendingHostOutput {
    tail: VecDeque<u8>,
    warned_stall: bool,
    dropped_frames: u64,
}

impl PendingHostOutput {
    pub fn len(&self) -> usize {
        self.tail.len()
    }

    pub fn buffered_bytes(&self) -> Vec<u8> {
        self.tail.iter().copied().collect()
    }

    pub fn has_warned_drop_for_tests(&self) -> bool {
        self.warned_stall
    }

    pub fn dropped_frames_for_tests(&self) -> u64 {
        self.dropped_frames
    }

    pub fn is_empty(&self) -> bool {
        self.tail.is_empty()
    }

    pub fn flush_bytes_to(&mut self, data: &[u8], pty: &PtyPair) -> Result<(), PtyError> {
        self.flush_bytes_with(data, |bytes| {
            pty.write_nonblocking(bytes)
                .map_err(|e| io::Error::other(e.to_string()))
        })?;
        Ok(())
    }

    pub fn flush_bytes_with<F>(&mut self, data: &[u8], mut write: F) -> io::Result<()>
    where
        F: FnMut(&[u8]) -> io::Result<usize>,
    {
        self.flush_with(&mut write)?;
        for (index, frame) in kiss_frames(data).iter().enumerate() {
            if !self.tail.is_empty() {
                self.drop_frame();
                continue;
            }
            match write(frame)? {
                0 => self.drop_frame(),
                n if n >= frame.len() => self.mark_progress(),
                n => {
                    self.mark_progress();
                    self.tail.extend(&frame[n..]);
                    for _ in index + 1..kiss_frames(data).len() {
                        self.drop_frame();
                    }
                    break;
                }
            }
        }
        Ok(())
    }

    pub fn flush_with<F>(&mut self, mut write: F) -> io::Result<()>
    where
        F: FnMut(&[u8]) -> io::Result<usize>,
    {
        while !self.tail.is_empty() {
            let contiguous = self.tail.make_contiguous();
            let len = contiguous.len();
            let written = write(contiguous)?;
            if written == 0 {
                break;
            }
            self.mark_progress();
            self.tail.drain(..written.min(len));
        }
        Ok(())
    }

    fn drop_frame(&mut self) {
        self.dropped_frames = self.dropped_frames.saturating_add(1);
        if !self.warned_stall {
            log::warn!("dropping host output because the PTY is not draining");
            self.warned_stall = true;
        }
    }

    fn mark_progress(&mut self) {
        self.warned_stall = false;
    }
}

fn kiss_frames(bytes: &[u8]) -> Vec<&[u8]> {
    const FEND: u8 = 0xC0;
    let mut frames = Vec::new();
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == FEND {
            let start = idx;
            idx += 1;
            while idx < bytes.len() {
                if bytes[idx] == FEND {
                    frames.push(&bytes[start..=idx]);
                    idx += 1;
                    break;
                }
                idx += 1;
            }
            if idx == bytes.len() && bytes.last() != Some(&FEND) {
                frames.push(&bytes[start..]);
            }
        } else {
            let start = idx;
            while idx < bytes.len() && bytes[idx] != FEND {
                idx += 1;
            }
            if idx < bytes.len() {
                frames.push(&bytes[start..=idx]);
                idx += 1;
            } else {
                frames.push(&bytes[start..]);
            }
        }
    }
    frames.into_iter().filter(|frame| !frame.is_empty()).collect()
}
