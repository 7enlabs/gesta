//! Does stat-ing the tree in parallel actually help here?
//!
//! This is a measurement, not a feature. On a filesystem where stat goes through several layers the
//! kernel can serve them concurrently; on one where it is a cheap memory hit, thread handoff costs
//! more than it saves. Only a run on the machine answers which one this is, and the answer decides
//! whether the walk is worth threading.
//!
//! Run: bench/stat_parallel.rs as a standalone binary via `rustc -O`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

fn collect(root: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![std::path::PathBuf::from(root)];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(t) = e.file_type() else { continue };
            if t.is_dir() {
                let name = e.file_name();
                if name == ".gesta" {
                    continue;
                }
                stack.push(e.path());
            } else {
                out.push(e.path().to_string_lossy().to_string());
            }
        }
    }
    out
}

fn stat_serial(paths: &[String]) -> u64 {
    let mut n = 0u64;
    for p in paths {
        if std::fs::metadata(p).is_ok() {
            n += 1;
        }
    }
    n
}

fn stat_parallel(paths: &[String], workers: usize) -> u64 {
    let queue = Arc::new(Mutex::new(VecDeque::from(paths.to_vec())));
    let done = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for _ in 0..workers {
        let q = Arc::clone(&queue);
        let d = Arc::clone(&done);
        handles.push(std::thread::spawn(move || {
            loop {
                // One path at a time under the lock, rather than a slice per thread: a slice is
                // faster but a queue shows the cost of the coordination honestly, and the cost is
                // part of what this measures.
                let next = {
                    let mut g = q.lock().unwrap();
                    g.pop_front()
                };
                let Some(p) = next else { break };
                if std::fs::metadata(&p).is_ok() {
                    d.fetch_add(1, Ordering::Relaxed);
                }
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    done.load(Ordering::Relaxed) as u64
}

fn stat_chunked(paths: &[String], workers: usize) -> u64 {
    let chunk = (paths.len() + workers - 1) / workers.max(1);
    let mut handles = Vec::new();
    for w in 0..workers {
        let lo = (w * chunk).min(paths.len());
        let hi = ((w + 1) * chunk).min(paths.len());
        if lo >= hi {
            continue;
        }
        // A slice, so the threads share one allocation and no lock is on the hot path.
        let part = paths[lo..hi].to_vec();
        handles.push(std::thread::spawn(move || stat_serial(&part)));
    }
    handles.into_iter().map(|h| h.join().unwrap_or(0)).sum()
}

fn main() {
    let root = std::env::args().nth(1).unwrap_or_else(|| ".".to_string());
    let paths = collect(&root);
    println!("{} files under {}", paths.len(), root);

    for _ in 0..2 {
        let t = Instant::now();
        let n = stat_serial(&paths);
        println!("serial:          {:7.1} ms  ({} stat ok)", t.elapsed().as_secs_f64() * 1000.0, n);
    }
    for workers in [2usize, 4, 8, 16] {
        let t = Instant::now();
        let n = stat_chunked(&paths, workers);
        println!(
            "chunked x{:2}:     {:7.1} ms  ({} stat ok)",
            workers,
            t.elapsed().as_secs_f64() * 1000.0,
            n
        );
    }
    let t = Instant::now();
    let n = stat_parallel(&paths, 8);
    println!("queue   x 8:     {:7.1} ms  ({} stat ok)", t.elapsed().as_secs_f64() * 1000.0, n);
}
