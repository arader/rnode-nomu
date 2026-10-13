use std::cmp::min;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use rnode_core::Node;
use rnode_core::radio::Radio;
use rnode_core::storage::Storage;

use crate::pty::{PendingHostOutput, PtyError, PtyPair};

pub const POLL_TIMEOUT_CAP_MS: u64 = 50;

pub trait IrqSource {
    fn fd(&self) -> Option<BorrowedFd<'_>>;
    fn drain(&mut self) -> std::io::Result<()>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct NoIrq;

impl IrqSource for NoIrq {
    fn fd(&self) -> Option<BorrowedFd<'_>> {
        None
    }
    fn drain(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Generic readable-fd IRQ source for drivers that expose a pollable event fd.
#[derive(Debug)]
pub struct FdIrqSource {
    fd: OwnedFd,
}

impl FdIrqSource {
    pub fn new(fd: OwnedFd) -> Self {
        Self { fd }
    }
}

impl IrqSource for FdIrqSource {
    fn fd(&self) -> Option<BorrowedFd<'_>> {
        Some(self.fd.as_fd())
    }

    fn drain(&mut self) -> std::io::Result<()> {
        let mut buf = [0_u8; 64];
        loop {
            match nix::unistd::read(&self.fd, &mut buf) {
                Ok(0) => return Ok(()),
                Ok(_) => {}
                Err(nix::Error::EAGAIN) => return Ok(()),
                Err(e) => return Err(std::io::Error::other(e)),
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EventLoopError {
    #[error("pty: {0}")]
    Pty(#[from] PtyError),
    #[error("poll: {0}")]
    Poll(#[from] nix::Error),
    #[error("irq: {0}")]
    Irq(#[from] std::io::Error),
}

pub fn monotonic_ms(start: Instant) -> u64 {
    start.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}

pub fn cap_timeout_ms(due_ms: u64) -> u16 {
    min(due_ms, POLL_TIMEOUT_CAP_MS) as u16
}

pub fn compute_timeout_ms<R: Radio, S: Storage>(node: &Node<R, S>, now_ms: u64) -> u16 {
    cap_timeout_ms(node.next_wakeup_ms(now_ms).saturating_sub(now_ms))
}

pub fn run<R, S, I>(
    mut node: Node<R, S>,
    mut irq: I,
    mut pty: PtyPair,
    shutdown: &AtomicBool,
) -> Result<(), EventLoopError>
where
    R: Radio,
    S: Storage,
    I: IrqSource,
{
    let start = Instant::now();
    node.boot(0);
    let mut read_buf = [0_u8; 4096];
    let mut output = PendingHostOutput::default();

    while !shutdown.load(Ordering::Relaxed) {
        let now = monotonic_ms(start);
        let timeout = compute_timeout_ms(&node, now);
        let pty_ready;
        let mut irq_ready = false;

        if let Some(irq_fd) = irq.fd() {
            let mut fds = [
                PollFd::new(pty.master_fd(), PollFlags::POLLIN),
                PollFd::new(irq_fd, PollFlags::POLLIN),
            ];
            poll_until_ready(&mut fds, timeout)?;
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            pty_ready = fds[0]
                .revents()
                .unwrap_or(PollFlags::empty())
                .contains(PollFlags::POLLIN);
            irq_ready = fds[1]
                .revents()
                .unwrap_or(PollFlags::empty())
                .contains(PollFlags::POLLIN);
        } else {
            let mut fds = [PollFd::new(pty.master_fd(), PollFlags::POLLIN)];
            poll_until_ready(&mut fds, timeout)?;
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            pty_ready = fds[0]
                .revents()
                .unwrap_or(PollFlags::empty())
                .contains(PollFlags::POLLIN);
        }

        let now = monotonic_ms(start);
        if pty_ready {
            loop {
                let n = pty.read(&mut read_buf)?;
                if n == 0 {
                    break;
                }
                node.handle_host_input(&read_buf[..n], now);
            }
        }
        if irq_ready {
            irq.drain()?;
            node.service_radio(now);
        }

        // Safety net for missed/level-stuck IRQs (#2) and regular protocol work. A shutdown request
        // ends a TX burst after the packet on the air (#51).
        node.service_radio(now);
        node.tick_with(now, || shutdown.load(Ordering::Relaxed));
        let host_output = node.take_host_output();
        output.flush_bytes_to(&host_output, &pty)?;
    }

    node.shutdown();
    if let Err(e) = pty.remove_symlink() {
        log::warn!("{e}");
    }
    Ok(())
}

fn poll_until_ready(fds: &mut [PollFd<'_>], timeout: u16) -> Result<(), EventLoopError> {
    match poll(fds, PollTimeout::from(timeout)) {
        Ok(_) => Ok(()),
        Err(nix::Error::EINTR) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub fn sleep_backoff() {
    std::thread::sleep(Duration::from_millis(1));
}
