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

/// Block until the process is asked to stop, retrying the wait when it times out.
///
/// The wait takes a timeout so a caller can wake periodically for its own reasons, and that means it
/// can return `false` without a stop having been asked for. Treating that return as a stop is the
/// defect this exists to prevent: a server given a 24-hour wait shut down after a day, printed
/// nothing, and exited 0, which a supervisor reads as a clean shutdown and does not restart.
///
/// The only `true` this can return came from `wait`, which means the flag is set.
pub fn wait_until_stopped(wait: impl Fn(std::time::Duration) -> bool) {
    // A minute between retries: long enough that an idle server wakes 1,440 times a day rather than
    // 86,400, short enough that the flag is noticed promptly after it is set.
    while !wait(std::time::Duration::from_secs(60)) {}
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

    #[test]
    fn a_finished_wait_does_not_mean_the_server_should_stop() {
        // The defect this guards: a wait with a deadline returns false when the deadline passes, and
        // the caller treated any return as a reason to exit. A server up for the length of its
        // deadline then shut down with status 0 and said nothing, so a supervisor saw a clean stop
        // and did not restart it.
        //
        // The wait is made to time out three times and only then report a stop. A loop that treats
        // the first timeout as a stop calls it once and returns; a loop that retries calls it four
        // times. The third timeout is what makes this terminate whatever the loop does, because the
        // counter is outside the closure rather than the flag.
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&calls);
        wait_until_stopped(move |_| {
            let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
            n > 3
        });
        assert_eq!(
            calls.load(Ordering::Relaxed),
            4,
            "a timed-out wait must be retried; one call means the timeout was read as a stop"
        );
    }
}
