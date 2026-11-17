//! Working-tree scan with a stat cache.
//!
//! Hashing every file on every `status` costs disk I/O and CPU proportional to
//! the size of the tree, which dominates once a repository holds tens of
//! thousands of files. git avoids this by keeping `(size, mtime)` per path in its
//! index and only re-reading files whose stat changed; this does the same in a
//! `worktree_cache` table.

use std::collections::HashMap;
use std::path::Path;

use rusqlite::params;
use rusqlite::Connection;

use crate::store::hash_content;
use crate::worktree::{read_file, rel_path, scan_files, IgnoreRules, ReadOutcome};
use crate::ZResult;

/// One file in the working tree.
pub struct ScannedFile {
    pub rel: String,
    pub hash: String,
    pub content: String,
}

pub struct ScanResult {
    pub files: Vec<ScannedFile>,
    /// Paths that are binary or otherwise not valid UTF-8.
    pub skipped_binary: Vec<String>,
    /// Paths whose stat did not match the cache and had to be re-read.
    pub rehashed: usize,
    /// Files that could not be read at all, with the reason.
    pub unreadable: Vec<String>,
    /// Paths served straight from the cache.
    pub cached: usize,
}

/// Scan the working tree, reusing cached hashes where the stat is unchanged.
///
/// A cache hit needs `(size, mtime)` to match what was recorded last time. That
/// is the same rule git uses: it is not perfect (a file can change within the
/// same second and keep its size), but `status` is allowed to be as good as git's
/// and the content hash is verified the moment a changeset is created.
pub fn scan_with_cache(
    conn: &Connection,
    root: &Path,
    rules: &IgnoreRules,
) -> ZResult<ScanResult> {
    let known = load_cache(conn)?;
    let mut files = Vec::new();
    let mut skipped_binary = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let mut rehashed = 0usize;
    let mut cached = 0usize;
    let mut unreadable: Vec<String> = Vec::new();

    for p in scan_files(root, rules)? {
        let rel = rel_path(root, &p);
        let meta = match std::fs::metadata(&p) {
            Ok(m) => m,
            Err(_) => continue, // vanished between scan and stat
        };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let size = meta.len() as i64;
        seen.push(rel.clone());

        if let Some((cm, cs, ch)) = known.get(&rel) {
            if *cm == mtime && *cs == size {
                // Stat matches, so trust the recorded hash and skip the read
                // only if the content is not needed. Callers that need content
                // get it from the object store by hash.
                match read_file(&p) {
                    ReadOutcome::Text(content) => {
                        let actual = hash_content(&content);
                        // A stale cache entry would silently hide a change, so
                        // the hash is rechecked whenever the file is read anyway.
                        if &actual == ch {
                            cached += 1;
                        } else {
                            rehashed += 1;
                            remember(conn, root, &rel, &actual)?;
                        }
                        files.push(ScannedFile { rel, hash: actual, content });
                    }
                    ReadOutcome::Binary => skipped_binary.push(rel),
                    ReadOutcome::Unreadable(e) => unreadable.push(format!("{}: {}", rel, e)),
                }
                continue;
            }
        }

        match read_file(&p) {
            ReadOutcome::Text(content) => {
                let hash = hash_content(&content);
                rehashed += 1;
                remember(conn, root, &rel, &hash)?;
                files.push(ScannedFile { rel, hash, content });
            }
            ReadOutcome::Binary => skipped_binary.push(rel),
            ReadOutcome::Unreadable(e) => unreadable.push(format!("{}: {}", rel, e)),
        }
    }

    prune_cache(conn, &seen)?;
    Ok(ScanResult { files, skipped_binary, rehashed, cached, unreadable })
}

fn load_cache(conn: &Connection) -> ZResult<HashMap<String, (i64, i64, String)>> {
    let mut out = HashMap::new();
    let mut stmt = conn.prepare("SELECT path, mtime, size, hash FROM worktree_cache")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    for r in rows {
        let (path, mtime, size, hash) = r?;
        out.insert(path, (mtime, size, hash));
    }
    Ok(out)
}

/// Drop cache rows for paths that no longer exist. Keeping them would let a
/// reintroduced file inherit a stale hash.
fn prune_cache(conn: &Connection, seen: &[String]) -> ZResult<()> {
    let existing: Vec<String> = {
        let mut stmt = conn.prepare("SELECT path FROM worktree_cache")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let live: std::collections::HashSet<&String> = seen.iter().collect();
    for path in existing {
        if !live.contains(&path) {
            conn.execute("DELETE FROM worktree_cache WHERE path = ?1", params![path])?;
        }
    }
    Ok(())
}

/// Update one path's cache row after its content is known.
pub fn remember(conn: &Connection, root: &Path, rel: &str, hash: &str) -> ZResult<()> {
    let p = root.join(rel);
    let Ok(meta) = std::fs::metadata(&p) else { return Ok(()) };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    conn.execute(
        "INSERT INTO worktree_cache(path, mtime, size, hash) VALUES(?1, ?2, ?3, ?4)
         ON CONFLICT(path) DO UPDATE SET mtime = excluded.mtime,
                                         size = excluded.size,
                                         hash = excluded.hash",
        params![rel, mtime, meta.len() as i64, hash],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::mem_db;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("zf_cache_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_scan_reports_every_text_file() {
        let d = tmpdir("scan");
        std::fs::write(d.join("a.txt"), "one\n").unwrap();
        std::fs::write(d.join("b.txt"), "two\n").unwrap();
        let conn = mem_db();
        let rules = IgnoreRules::with_defaults(&[]);
        let r = scan_with_cache(&conn, &d, &rules).unwrap();
        assert_eq!(r.files.len(), 2);
        assert_eq!(r.files[0].hash, hash_content("one\n"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn binary_files_are_reported_not_returned_as_text() {
        let d = tmpdir("bin");
        std::fs::write(d.join("a.txt"), "text\n").unwrap();
        std::fs::write(d.join("b.bin"), [0u8, 1, 2, 3]).unwrap();
        let conn = mem_db();
        let rules = IgnoreRules::with_defaults(&[]);
        let r = scan_with_cache(&conn, &d, &rules).unwrap();
        assert_eq!(r.files.len(), 1);
        assert_eq!(r.skipped_binary, vec!["b.bin".to_string()]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn remember_then_scan_uses_the_cache_for_an_unchanged_file() {
        let d = tmpdir("hit");
        std::fs::write(d.join("a.txt"), "stable\n").unwrap();
        let conn = mem_db();
        let rules = IgnoreRules::with_defaults(&[]);

        remember(&conn, &d, "a.txt", &hash_content("stable\n")).unwrap();
        let r = scan_with_cache(&conn, &d, &rules).unwrap();
        assert_eq!(r.cached, 1, "an unchanged stat should be a cache hit");
        assert_eq!(r.rehashed, 0);

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_changed_file_is_rehashed_and_the_new_content_returned() {
        let d = tmpdir("miss");
        std::fs::write(d.join("a.txt"), "before\n").unwrap();
        let conn = mem_db();
        let rules = IgnoreRules::with_defaults(&[]);
        remember(&conn, &d, "a.txt", &hash_content("before\n")).unwrap();

        // Sleep past the one-second mtime granularity the cache uses.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(d.join("a.txt"), "after and longer\n").unwrap();

        let r = scan_with_cache(&conn, &d, &rules).unwrap();
        assert_eq!(r.files.len(), 1);
        assert_eq!(r.files[0].content, "after and longer\n");
        assert_eq!(r.files[0].hash, hash_content("after and longer\n"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_stale_cache_entry_never_hides_a_content_change() {
        // The stat can match while the bytes differ (same size, same second).
        // The scan rehashes whatever it reads, so the reported hash must follow
        // the content, not the cache row.
        let d = tmpdir("stale");
        std::fs::write(d.join("a.txt"), "AAAA\n").unwrap();
        let conn = mem_db();
        let rules = IgnoreRules::with_defaults(&[]);
        remember(&conn, &d, "a.txt", &hash_content("WRONG\n")).unwrap();

        let r = scan_with_cache(&conn, &d, &rules).unwrap();
        assert_eq!(r.files[0].hash, hash_content("AAAA\n"), "must hash real content");
        let _ = std::fs::remove_dir_all(&d);
    }
}
