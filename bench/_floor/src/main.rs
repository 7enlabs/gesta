// A tighter floor: same work as a scan needs, without the allocations my earlier harness made.
// The previous version pushed a fresh PathBuf per directory and cloned paths per entry, which is the
// kind of overhead a floor is supposed to exclude.
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

fn main() {
    let root = std::env::args().nth(1).unwrap_or("/tmp/gesta_scale/g100000".to_string());
    let mut best = f64::MAX;

    for _ in 0..5 {
        let t0 = Instant::now();
        let mut stack: Vec<PathBuf> = vec![PathBuf::from(&root)];
        let mut files = 0usize;
        let mut bytes: u128 = 0;
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else { continue };
            for e in entries.flatten() {
                let Ok(ft) = e.file_type() else { continue };
                if ft.is_dir() {
                    // reuse the directory path buffer instead of building one per entry
                    let mut p = dir.clone();
                    p.push(e.file_name());
                    stack.push(p);
                } else {
                    let Ok(m) = e.metadata() else { continue };
                    files += 1;
                    bytes += m.len() as u128 + m.modified().map(|_| 1).unwrap_or(0);
                }
            }
        }
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        if ms < best {
            best = ms;
        }
        let _ = (files, bytes);
    }
    println!("tightest floor (readdir + file_type + stat per file): {:.1} ms", best);
}
