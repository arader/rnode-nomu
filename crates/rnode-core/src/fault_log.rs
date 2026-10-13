//! Rate-limited logging for the radio calls the node makes every few milliseconds (#53).
//!
//! While the radio is online, DCD and RSSI are read every [`crate::node::STATUS_INTERVAL_MS`]
//! (3 ms), and the platform polls the receive path about as often. A persistent fault (an
//! unplugged module, an SPI error, BUSY stuck high) would otherwise log a warning per call, some
//! 330 a second, and bury the first and most useful one. Each of these calls therefore has a
//! [`FaultLog`]:
//!
//! - the first failure is logged at warn level, with its error;
//! - while the call keeps failing, at most one warning per [`FAULT_LOG_INTERVAL_MS`] counts the
//!   failures since the last line and repeats the latest error;
//! - once the call has gone [`FAULT_LOG_INTERVAL_MS`] without failing, its next success logs a
//!   recovery line at info level.
//!
//! A call that fails intermittently thus stays in one episode and is summarised, instead of
//! logging a warning and a recovery for every failure. Logging never changes what the node does.

use core::fmt;

use crate::packet_log::plural;

/// How often a failing call is summarised, and how long it must work before it counts as
/// recovered.
pub(crate) const FAULT_LOG_INTERVAL_MS: u64 = 10_000;

/// The log state of one periodic radio call; see the module docs.
#[derive(Debug, Clone)]
pub(crate) struct FaultLog {
    /// What failed, as the start of each line: `radio DCD`.
    what: &'static str,
    episode: Option<Episode>,
}

/// A run of failures with less than [`FAULT_LOG_INTERVAL_MS`] between them.
#[derive(Debug, Clone, Copy)]
struct Episode {
    first_ms: u64,
    last_ms: u64,
    /// When the last warning was logged.
    reported_ms: u64,
    failures: usize,
    /// Failures since the last warning.
    unreported: usize,
}

impl FaultLog {
    pub(crate) fn new(what: &'static str) -> Self {
        Self { what, episode: None }
    }

    /// The call failed at `now_ms` with `error`.
    pub(crate) fn failed(&mut self, now_ms: u64, error: &dyn fmt::Display) {
        let what = self.what;
        let Some(episode) = &mut self.episode else {
            log::warn!("{what} failed: {error}");
            self.episode = Some(Episode {
                first_ms: now_ms,
                last_ms: now_ms,
                reported_ms: now_ms,
                failures: 1,
                unreported: 0,
            });
            return;
        };
        episode.last_ms = now_ms;
        episode.failures = episode.failures.saturating_add(1);
        episode.unreported = episode.unreported.saturating_add(1);
        let since_report = now_ms.saturating_sub(episode.reported_ms);
        if since_report >= FAULT_LOG_INTERVAL_MS {
            log::warn!(
                "{what} failed {} more time{} in the last {}: {error}",
                episode.unreported,
                plural(episode.unreported),
                Elapsed(since_report)
            );
            episode.reported_ms = now_ms;
            episode.unreported = 0;
        }
    }

    /// The call succeeded at `now_ms`.
    pub(crate) fn succeeded(&mut self, now_ms: u64) {
        let Some(episode) = self.episode else {
            return;
        };
        if now_ms.saturating_sub(episode.last_ms) < FAULT_LOG_INTERVAL_MS {
            return;
        }
        if episode.failures == 1 {
            log::info!("{} recovered after 1 failure", self.what);
        } else {
            log::info!(
                "{} recovered after {} failures in {}",
                self.what,
                episode.failures,
                Elapsed(episode.last_ms.saturating_sub(episode.first_ms))
            );
        }
        self.episode = None;
    }
}

/// A duration in ms, as `12 ms` below a second and `10.0 s` (rounded to 0.1 s) from there.
#[derive(Debug, Clone, Copy)]
struct Elapsed(u64);

impl fmt::Display for Elapsed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 < 1_000 {
            return write!(f, "{} ms", self.0);
        }
        let tenths = self.0.saturating_add(50) / 100;
        write!(f, "{}.{} s", tenths / 10, tenths % 10)
    }
}
