//! Test-only: captures this thread's `log` lines, so a test can assert the
//! decision lines a log query on the phone depends on.

use std::cell::RefCell;
use std::sync::Once;

thread_local! {
    static LINES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

struct Capture;

impl log::Log for Capture {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, r: &log::Record) {
        LINES.with(|l| l.borrow_mut().push(r.args().to_string()));
    }
    fn flush(&self) {}
}

static CAPTURE: Capture = Capture;
static INSTALL: Once = Once::new();

/// Starts capturing on this thread, dropping what was captured before.
pub(crate) fn start() {
    INSTALL.call_once(|| {
        if log::set_logger(&CAPTURE).is_ok() {
            log::set_max_level(log::LevelFilter::Trace);
        }
    });
    LINES.with(|l| l.borrow_mut().clear());
}

/// The lines this thread logged since [`start`].
pub(crate) fn lines() -> Vec<String> {
    LINES.with(|l| l.borrow().clone())
}
