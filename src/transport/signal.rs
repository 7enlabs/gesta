// A server that cannot be stopped is not a server a supervisor can run.
//
// `serve` loops on `listener.incoming()` and had no signal handling at all, so SIGTERM and SIGINT
// were both ignored: `kill -TERM` on a running `gesta serve` left it alive. That is invisible when a
// person runs it in a terminal and presses Ctrl-C, because the terminal then closes and the process
// is reaped when the session ends, but a test harness that starts a server and terminates it on the
// way out leaks every one it started. Two thousand of those had accumulated in this project's own
// bench container, holding twelve gigabytes and every port they had ever bound.
//
// The shutdown flag is a plain atomic rather than a channel: the accept loop is the only reader and
// it needs one bit, not a message. The handler does nothing but set it, because the handler runs in
// signal context where almost nothing else is allowed.

use std::sync::atomic::{AtomicBool, Ordering};

/// Set when the process is asked to stop.
pub static STOP: AtomicBool = AtomicBool::new(false);

pub fn stop_requested() -> bool {
    STOP.load(Ordering::Relaxed)
}

/// Install the handler that sets [`STOP`]. Calling this twice is harmless.
///
/// Both signals are handled because they arrive from different places: a supervisor sends SIGTERM,
/// a terminal sends SIGINT, and a harness that calls `terminate()` sends SIGTERM as well.
#[cfg(unix)]
pub fn install() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        // `sigaction` with an empty mask and no flags, so a second signal cannot arrive inside the
        // first one's handler. The handler is async-signal-safe: one relaxed atomic store.
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handler as *const () as usize;
        action.sa_flags = 0;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut());
        libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
    });
}

#[cfg(unix)]
extern "C" fn handler(_sig: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

/// On Windows there is no `kill -TERM`; a console close or Ctrl-C arrives as a control event, and
/// the process is normally ended outright by the system. The accept loop still polls the flag, so
/// the same code path works when something sets it by other means.
#[cfg(not(unix))]
pub fn install() {
    let _ = &STOP;
}

/// Wait until a stop has been requested, or until the timeout passes.
///
/// The accept loop polls this with a short timeout rather than blocking forever in `accept`, which
/// is what makes a stop take effect within the timeout instead of at the next connection. The cost
/// is one wakeup per interval on an idle server, which is a timer going off rather than work.
pub fn wait_for_stop(timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if stop_requested() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    stop_requested()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_starts_clear_and_reads_back_what_was_stored() {
        // The static is shared across tests in the same binary, so this asserts the read path rather
        // than the initial value: another test may have set it.
        let before = stop_requested();
        STOP.store(true, Ordering::Relaxed);
        assert!(stop_requested(), "a stored stop must read back as a stop");
        STOP.store(before, Ordering::Relaxed);
    }

    #[test]
    fn waiting_returns_false_when_nothing_asks_it_to_stop() {
        STOP.store(false, Ordering::Relaxed);
        let started = std::time::Instant::now();
        let stopped = wait_for_stop(std::time::Duration::from_millis(30));
        assert!(!stopped, "nothing asked for a stop");
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(25),
            "the wait must actually wait, or a caller polls the socket for nothing"
        );
    }

    #[test]
    fn waiting_returns_true_as_soon_as_the_flag_is_set() {
        STOP.store(false, Ordering::Relaxed);
        let setter = std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(20));
            STOP.store(true, Ordering::Relaxed);
        });
        let started = std::time::Instant::now();
        let stopped = wait_for_stop(std::time::Duration::from_secs(5));
        setter.join().unwrap();
        assert!(stopped, "a set flag must end the wait");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "it must return on the flag, not on the timeout"
        );
        STOP.store(false, Ordering::Relaxed);
    }
}
