//! Content search over the working tree and over history.
//!
//! An agent asks "where is this string" far more often than a person does, and the answer it
//! needs is a path and a line number, not a page of context. Searching history matters because
//! the string that was deleted is usually the one being looked for.

use crate::store::get_object;
use crate::{bad, ZResult};
use rusqlite::Connection;
use std::collections::BTreeMap;
use std::path::Path;

/// One match: where it is and what the line says.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub path: String,
    pub line: usize,
    /// The revision this match was read at. Empty means the working tree.
    pub rev: String,
    pub text: String,
}

/// A search that has been compiled once and run against many revisions.
pub struct Pattern {
    needle: String,
    ignore_case: bool,
    /// Match whole words only, so `hash` does not find `hashes`.
    word: bool,
}

impl Pattern {
    pub fn new(needle: &str, ignore_case: bool, word: bool) -> ZResult<Pattern> {
        if needle.is_empty() {
            return Err(bad("an empty pattern matches every line, so it is refused"));
        }
        Ok(Pattern {
            needle: if ignore_case {
                needle.to_lowercase()
            } else {
                needle.to_string()
            },
            ignore_case,
            word,
        })
    }

    pub fn matches(&self, line: &str) -> bool {
        let hay = if self.ignore_case {
            line.to_lowercase()
        } else {
            line.to_string()
        };
        if !self.word {
            return hay.contains(&self.needle);
        }
        let mut from = 0;
        while let Some(at) = hay[from..].find(&self.needle) {
            let start = from + at;
            let end = start + self.needle.len();
            let before_ok = start == 0
                || !hay[..start]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_');
            let after_ok = end >= hay.len()
                || !hay[end..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_');
            if before_ok && after_ok {
                return true;
            }
            from = start + 1;
            if from >= hay.len() {
                return false;
            }
        }
        false
    }

    /// Every matching line of one file body.
    pub fn scan(&self, path: &str, body: &str, rev: &str) -> Vec<Hit> {
        if body.contains('\0') {
            return Vec::new();
        }
        let mut out = Vec::new();
        for (n, line) in body.lines().enumerate() {
            if self.matches(line) {
                out.push(Hit {
                    path: path.to_string(),
                    line: n + 1,
                    rev: rev.to_string(),
                    text: line.trim_end().to_string(),
                });
            }
        }
        out
    }
}

/// Search the files on disk, skipping anything the repository ignores.
pub fn search_worktree(
    conn: &Connection,
    root: &Path,
    pattern: &Pattern,
    path_filter: Option<&str>,
) -> ZResult<Vec<Hit>> {
    let rules = crate::repo::load_ignore_rules(root, conn)?;
    let scope = crate::sparse::SparseScope::load(conn)?;
    let scanned = crate::cache::scan_with_cache(conn, root, &rules)?;
    if let Some(first) = scanned.unreadable.first() {
        return Err(bad(format!(
            "cannot read {} ({} more)",
            first,
            scanned.unreadable.len() - 1
        )));
    }

    let mut hits = Vec::new();
    for f in scanned.files {
        if !scope.contains(&f.rel) {
            continue;
        }
        if let Some(filter) = path_filter {
            if !f.rel.starts_with(filter) {
                continue;
            }
        }
        let body = match std::fs::read(root.join(&f.rel)) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
            Err(_) => continue,
        };
        hits.extend(pattern.scan(&f.rel, &body, ""));
    }
    Ok(hits)
}

/// Search every distinct version of every tracked path, newest first.
///
/// The same body is reached through many changesets, so each object hash is read once. Reading
/// per changeset instead cost a repeated read of the whole history, which is the difference
/// between a search that finishes and one that does not on a long repository.
pub fn search_history(
    conn: &Connection,
    pattern: &Pattern,
    path_filter: Option<&str>,
    limit: i64,
) -> ZResult<Vec<Hit>> {
    let mut stmt = conn.prepare(
        "SELECT fv.path, fv.hash, fv.changeset_id
         FROM file_versions fv
         ORDER BY fv.created_at DESC, fv.version DESC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map(rusqlite::params![limit], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;

    let mut seen: BTreeMap<(String, String), ()> = BTreeMap::new();
    let mut hits = Vec::new();
    for row in rows {
        let (path, hash, cs) = row?;
        if let Some(filter) = path_filter {
            if !path.starts_with(filter) {
                continue;
            }
        }
        if seen.insert((path.clone(), hash.clone()), ()).is_some() {
            continue;
        }
        let body = match get_object(conn, &hash) {
            Ok(b) => b,
            Err(_) => continue,
        };
        hits.extend(pattern.scan(&path, &body, &cs));
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_string_is_found_on_its_line() {
        let p = Pattern::new("needle", false, false).unwrap();
        let hits = p.scan("a.txt", "first\nhas a needle here\nlast\n", "");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line, 2);
        assert_eq!(hits[0].path, "a.txt");
    }

    #[test]
    fn case_is_ignored_only_when_asked() {
        let exact = Pattern::new("Needle", false, false).unwrap();
        assert!(exact.scan("a", "a needle\n", "").is_empty());
        let loose = Pattern::new("Needle", true, false).unwrap();
        assert_eq!(loose.scan("a", "a needle\n", "").len(), 1);
    }

    #[test]
    fn a_whole_word_match_does_not_find_a_longer_identifier() {
        let p = Pattern::new("hash", false, true).unwrap();
        assert!(p.matches("let hash = 1;"));
        assert!(!p.matches("let hashes = 1;"), "a prefix must not match");
        assert!(!p.matches("let rehash = 1;"), "a suffix must not match");
        assert!(p.matches("call(hash)"));
    }

    #[test]
    fn an_empty_pattern_is_refused_because_it_matches_everything() {
        assert!(Pattern::new("", false, false).is_err());
    }

    #[test]
    fn a_binary_file_is_skipped_rather_than_reported() {
        let p = Pattern::new("a", false, false).unwrap();
        assert!(p.scan("blob.bin", "a\0b\n", "").is_empty());
    }

    #[test]
    fn history_reads_each_object_once_across_many_changesets() {
        let conn = crate::store::tests::mem_db();
        crate::repo::tests::commit_files(&conn, "g1", "one", &[("a.txt", "needle here\n")]);
        crate::repo::tests::commit_files(&conn, "g2", "two", &[("a.txt", "needle here\n")]);
        let p = Pattern::new("needle", false, false).unwrap();
        let hits = search_history(&conn, &p, None, 500).unwrap();
        // One body, reached through two changesets, reported once per distinct revision.
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(paths, vec!["a.txt"], "the same body was reported twice");
    }

    #[test]
    fn history_finds_a_string_that_no_longer_exists_on_disk() {
        let conn = crate::store::tests::mem_db();
        crate::repo::tests::commit_files(&conn, "h1", "old", &[("gone.txt", "the lost token\n")]);
        let p = Pattern::new("lost token", false, false).unwrap();
        let hits = search_history(&conn, &p, None, 500).unwrap();
        assert_eq!(hits.len(), 1, "a deleted string must still be findable");
        assert_eq!(hits[0].rev, "h1");
    }
}
