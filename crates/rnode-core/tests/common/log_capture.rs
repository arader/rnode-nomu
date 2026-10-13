//! A `log::Log` for tests that captures records per thread.
//!
//! [`capture`] installs the logger once per test binary and raises the global maximum level to
//! `Trace`. Whether a record is enabled, and where it is kept, is then decided by the calling
//! thread's own level and buffer. Cargo runs tests in parallel threads of one process, so a test
//! neither sees another test's records nor changes its level, and `log::log_enabled!` answers
//! per thread. Threads that never call [`capture`] log nothing.
//!
//! It uses only `std` and the public `log` API, so every integration test crate can include it
//! through `tests/common/`.

use std::cell::{Cell, RefCell};
use std::marker::PhantomData;
use std::sync::Once;

use log::{Level, LevelFilter, Log, Metadata, Record};

/// A log record captured on the current thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    /// The record's level.
    pub level: Level,
    /// The record's target: the module path of the log call unless it set one.
    pub target: String,
    /// The formatted message.
    pub message: String,
}

thread_local! {
    static LEVEL: Cell<LevelFilter> = const { Cell::new(LevelFilter::Off) };
    static RECORDS: RefCell<Vec<Captured>> = const { RefCell::new(Vec::new()) };
}

#[derive(Debug)]
struct ThreadLogger;

impl Log for ThreadLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= LEVEL.try_with(Cell::get).unwrap_or(LevelFilter::Off)
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let captured = Captured {
            level: record.level(),
            target: record.target().to_owned(),
            message: record.args().to_string(),
        };
        let _ = RECORDS.try_with(|records| records.borrow_mut().push(captured));
    }

    fn flush(&self) {}
}

static LOGGER: ThreadLogger = ThreadLogger;
static INSTALL: Once = Once::new();

/// Capture the current thread's records at `level` and more severe, discarding anything captured
/// before, until the returned guard is dropped. Use one guard per thread at a time.
pub fn capture(level: LevelFilter) -> Capture {
    INSTALL.call_once(|| {
        log::set_logger(&LOGGER).expect("no other logger may be installed in this test binary");
        log::set_max_level(LevelFilter::Trace);
    });
    LEVEL.with(|current| current.set(level));
    RECORDS.with(|records| records.borrow_mut().clear());
    Capture { _thread: PhantomData }
}

/// Capturing on the current thread; see [`capture`].
#[derive(Debug)]
pub struct Capture {
    /// Not `Send`: the guard controls the thread-local state of the thread that created it.
    _thread: PhantomData<*const ()>,
}

impl Capture {
    /// Capture `level` and more severe from now on.
    pub fn set_level(&self, level: LevelFilter) {
        LEVEL.with(|current| current.set(level));
    }

    /// Remove and return the records captured so far.
    pub fn take(&self) -> Vec<Captured> {
        RECORDS.with(|records| std::mem::take(&mut *records.borrow_mut()))
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = LEVEL.try_with(|current| current.set(LevelFilter::Off));
        let _ = RECORDS.try_with(|records| records.borrow_mut().clear());
    }
}
