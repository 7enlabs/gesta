//! A temporary directory whose name cannot repeat within one process.
//!
//! Temporary directories used to be named from the clock:
//!
//! ```text
//! std::env::temp_dir().join(format!("gesta-audit-{}-{}", pid, nanos))
//! ```
//!
//! A nanosecond timestamp looks unique and is not. Measured on a Windows host, two back to back
//! `SystemTime::now()` calls in the same process returned the same nanosecond in 9836 of 19999
//! pairs, 49.2% of the time: the system clock granularity is coarser than the unit it reports.
//! Two callers that drew the same name then shared one directory, and the `remove_dir_all` at the
//! end of whichever finished first deleted the other one's files while it was still writing them.
//!
//! The symptom is here to recognise, because it looks nothing like the cause: a `std::fs::write`
//! or `File::create` fails with `NotFound` immediately after a `create_dir_all` that succeeded.
//! That is what failed the Windows CI leg in `a_limited_read_does_not_load_the_whole_file`.
//!
//! A counter that is incremented per process makes the name unique, and the process id keeps it
//! unique across runs. Nothing here depends on the clock.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A fresh directory under the system temp directory, named `<prefix>-<pid>-<seq>`.
///
/// The directory is created, so it is ready to be written into. The caller owns it and is
/// responsible for removing it.
pub fn scratch(prefix: &str) -> PathBuf {
    let seq = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("{prefix}-{}-{seq}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("the scratch directory could not be created");
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_calls_never_return_the_same_path() {
        // This is the property the clock-based name did not have. A thousand calls is far more
        // than any single caller asks for, and a duplicate anywhere in that run would have two
        // callers share a directory and delete each other's files.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            assert!(
                seen.insert(scratch("gesta-scratch-probe")),
                "a repeated path means two callers would share one directory"
            );
        }
        for path in &seen {
            let _ = std::fs::remove_dir_all(path);
        }
    }

    #[test]
    fn the_directory_exists_when_it_is_handed_back() {
        let dir = scratch("gesta-scratch-exists");
        assert!(dir.is_dir(), "the caller should not have to create it");
        std::fs::write(dir.join("f.txt"), "ok\n").unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "ok\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
