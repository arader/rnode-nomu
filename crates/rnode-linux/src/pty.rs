// Copyright (C) 2024, Mark Qvist
// Ported to Rust for Linux by RNode contributors.
// Licensed under the GNU General Public License v3.0 or later.

//! PTY (pseudo-terminal) creation and I/O.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;

use log::info;

use crate::error::{RNodeError, Result};

/// A PTY pair (master + slave) for KISS serial communication.
#[allow(dead_code)]
pub struct PtyPair {
    pub master: OwnedFd,
    pub slave_path: String,
    symlink_path: Option<String>,
}

impl PtyPair {
    /// Create a new PTY pair and optionally symlink the slave device.
    pub fn open(symlink: Option<&str>) -> Result<Self> {
        let mut master_fd: RawFd = -1;
        let mut slave_fd: RawFd = -1;

        let ret = unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ret != 0 {
            return Err(RNodeError::Pty(format!(
                "openpty failed: {}",
                std::io::Error::last_os_error()
            )));
        }

        let master = unsafe { OwnedFd::from_raw_fd(master_fd) };
        let slave = unsafe { OwnedFd::from_raw_fd(slave_fd) };

        // Get the slave path via ttyname
        let slave_path = get_ttyname(slave.as_raw_fd())?;

        // Set master to non-blocking
        let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 {
            return Err(RNodeError::Pty(format!(
                "fcntl F_GETFL: {}",
                std::io::Error::last_os_error()
            )));
        }
        let ret =
            unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) };
        if ret < 0 {
            return Err(RNodeError::Pty(format!(
                "fcntl F_SETFL: {}",
                std::io::Error::last_os_error()
            )));
        }

        // Create symlink if requested
        let symlink_path = if let Some(link) = symlink {
            if Path::new(link).exists() || Path::new(link).is_symlink() {
                let _ = std::fs::remove_file(link);
            }
            std::os::unix::fs::symlink(&slave_path, link).map_err(|e| {
                RNodeError::Pty(format!("symlink {} -> {}: {}", link, slave_path, e))
            })?;
            info!("PTY symlink: {} -> {}", link, slave_path);
            Some(link.to_string())
        } else {
            None
        };

        info!("PTY slave: {}", slave_path);

        // Drop the slave fd — the host process will open it via the path
        drop(slave);

        Ok(Self {
            master,
            slave_path,
            symlink_path,
        })
    }

    /// Get the raw fd of the master side (for poll/epoll).
    pub fn master_fd(&self) -> RawFd {
        self.master.as_raw_fd()
    }

    /// Read bytes from the master side. Returns Ok(0) on EAGAIN (non-blocking).
    pub fn read(&self, buf: &mut [u8]) -> Result<usize> {
        let n = unsafe {
            libc::read(
                self.master.as_raw_fd(),
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
            )
        };
        if n >= 0 {
            Ok(n as usize)
        } else {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                Ok(0)
            } else {
                Err(RNodeError::Pty(format!("read: {}", err)))
            }
        }
    }

    /// Write bytes to the master side.
    pub fn write(&self, data: &[u8]) -> Result<usize> {
        let n = unsafe {
            libc::write(
                self.master.as_raw_fd(),
                data.as_ptr() as *const libc::c_void,
                data.len(),
            )
        };
        if n >= 0 {
            Ok(n as usize)
        } else {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                Ok(0)
            } else {
                Err(RNodeError::Pty(format!("write: {}", err)))
            }
        }
    }

    /// Write all bytes, retrying on partial writes.
    pub fn write_all(&self, data: &[u8]) -> Result<()> {
        let mut offset = 0;
        while offset < data.len() {
            let n = self.write(&data[offset..])?;
            if n == 0 {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            offset += n;
        }
        Ok(())
    }
}

impl Drop for PtyPair {
    fn drop(&mut self) {
        if let Some(ref link) = self.symlink_path {
            let _ = std::fs::remove_file(link);
        }
    }
}

/// Get the tty device path for a file descriptor.
fn get_ttyname(fd: RawFd) -> Result<String> {
    let mut buf = vec![0u8; 256];
    let ret = unsafe { libc::ttyname_r(fd, buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if ret != 0 {
        return Err(RNodeError::Pty(format!(
            "ttyname_r failed: {}",
            std::io::Error::from_raw_os_error(ret)
        )));
    }
    let cstr = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr() as *const libc::c_char) };
    Ok(cstr.to_string_lossy().to_string())
}
