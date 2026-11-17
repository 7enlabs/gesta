//! Changeset lifecycle, blame derivation, and line diffing.

use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashSet;
use std::fs;
use std::path::Path;

use crate::store::{
    get_object, hash_content, latest_hash, new_changeset_id, put_object, reachable_from,
};
use crate::worktree::{read_text, rel_path, scan_files};
use crate::{bad, now, ZResult};

/// Result of comparing the working tree against a branch head.
pub struct WorktreeDelta {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub unchanged: usize,
    /// Files skipped because they are not valid UTF-8. Reported, never silently dropped.
    pub skipped_binary: Vec<String>,
}

pub fn diff_worktree(conn: &Connection, root: &Path, branch: &str) -> ZResult<WorktreeDelta> {
    let mut d = WorktreeDelta {
        added: Vec::new(),
        modified: Vec::new(),
        unchanged: 0,
        skipped_binary: Vec::new(),
    };
    for p in scan_files(root)? {
        let rel = rel_path(root, &p);
        let Some(content) = read_text(&p) else {
            d.skipped_binary.push(rel);
            continue;
        };
        let new_hash = hash_content(&content);
        match latest_hash(conn, &rel, branch)? {
            None => d.added.push(rel),
            Some(old) if old != new_hash => d.modified.push(rel),
            Some(_) => d.unchanged += 1,
        }
    }
    Ok(d)
}

/// A freshly created changeset: its id, the files it captured, and the files
/// that were skipped because they are not UTF-8.
pub struct NewChangeset {
    pub id: String,
    pub files: Vec<(String, Option<String>, String)>,
    /// Skipped files are reported to the user; they are never silently ignored.
    pub skipped: Vec<String>,
}

/// Snapshot the working tree as a pending changeset. Returns the id and the
/// files captured. If nothing changed, returns `None`.
pub fn create_changeset(
    conn: &Connection,
    root: &Path,
    message: &str,
    author: &str,
) -> ZResult<Option<NewChangeset>> {
    let branch = crate::store::current_branch(conn)?;

    let mut changes: Vec<(String, Option<String>, String, String)> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    for p in scan_files(root)? {
        let rel = rel_path(root, &p);
        let content = match read_text(&p) {
            Some(c) => c,
            None => {
                // Not UTF-8 (or unreadable): record it so the caller can report,
                // instead of dropping the file without a trace.
                skipped.push(rel);
                continue;
            }
        };
        let new_hash = hash_content(&content);
        let old_hash = latest_hash(conn, &rel, &branch)?;
        if old_hash.as_deref() != Some(new_hash.as_str()) {
            changes.push((rel, old_hash, new_hash, content));
        }
    }

    if changes.is_empty() {
        return Ok(None);
    }

    let id = new_changeset_id(conn, message, author)?;
    let parent = crate::store::branch_head(conn, &branch)?;

    // One transaction: a half-written changeset would leave orphan objects and
    // files with no owner if the process dies mid-loop.
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO changesets(id, message, author, created_at, status, parent, branch)
         VALUES(?1, ?2, ?3, ?4, 'pending', ?5, ?6)",
        params![id, message, author, now(), parent, branch],
    )?;

    for (rel, old, new, content) in &changes {
        put_object(&tx, content)?;
        tx.execute(
            "INSERT INTO changeset_files(changeset_id, path, old_hash, new_hash)
             VALUES(?1, ?2, ?3, ?4)",
            params![id, rel, old, new],
        )?;
    }
    tx.commit()?;

    let summary = changes
        .into_iter()
        .map(|(rel, old, new, _)| (rel, old, new))
        .collect();
    Ok(Some(NewChangeset {
        id,
        files: summary,
        skipped,
    }))
}

/// Commit one pending changeset. Returns files committed; `0` means it was
/// already committed.
pub fn commit_one(conn: &Connection, id: &str) -> ZResult<usize> {
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT status, COALESCE(branch, 'main') FROM changesets WHERE id = ?1",
            params![id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?;
    let (status, branch) = row.ok_or_else(|| bad(format!("no such changeset: {}", id)))?;
    if status == "committed" {
        return Ok(0);
    }

    let mut stmt =
        conn.prepare("SELECT path, new_hash FROM changeset_files WHERE changeset_id = ?1 ORDER BY path")?;
    let files: Vec<(String, String)> = stmt
        .query_map(params![id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    // Everything below lands in one transaction. Without it a crash between the
    // version insert and the status update leaves the changeset pending with
    // file_versions already written, so a retry doubles the version numbers.
    let tx = conn.unchecked_transaction()?;

    // Re-parent onto the head as it stands at commit time, not at creation time.
    // A pending changeset can be created while another one is already pending;
    // keeping the stale parent would fork history and orphan the earlier
    // changeset the moment the branch head moves past it.
    let head_now = crate::store::branch_head(&tx, &branch)?;
    if head_now.as_deref() != Some(id) {
        tx.execute(
            "UPDATE changesets SET parent = ?1 WHERE id = ?2",
            params![head_now, id],
        )?;
    }

    let mut written = 0usize;
    for (path, hash) in &files {
        // Skip the version bump when the content is already the latest one.
        // Two concurrent agents snapshotting the same tree would otherwise
        // record a new "version" per commit for files nobody touched, and the
        // history of a file would inflate with no content change behind it.
        let latest: Option<String> = tx
            .query_row(
                "SELECT hash FROM file_versions WHERE path = ?1
                 ORDER BY version DESC LIMIT 1",
                params![path],
                |r| r.get(0),
            )
            .optional()?;

        if latest.as_deref() != Some(hash.as_str()) {
            // Propagate SQL errors instead of silently restarting numbering at 1.
            // Numbering is per path, so it stays contiguous after a skip.
            let next_ver: i64 = tx.query_row(
                "SELECT COALESCE(MAX(version), 0) + 1 FROM file_versions WHERE path = ?1",
                params![path],
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT INTO file_versions(path, version, hash, changeset_id, created_at)
                 VALUES(?1, ?2, ?3, ?4, ?5)",
                params![path, next_ver, hash, id, now()],
            )?;
            written += 1;
        }

        let content = get_object(&tx, hash)?;
        tx.execute(
            "INSERT INTO working_copy(path, content, hash, updated_at) VALUES(?1, ?2, ?3, ?4)
             ON CONFLICT(path) DO UPDATE SET content = excluded.content,
                                             hash = excluded.hash,
                                             updated_at = excluded.updated_at",
            params![path, content, hash, now()],
        )?;
    }

    tx.execute("UPDATE changesets SET status = 'committed' WHERE id = ?1", params![id])?;
    tx.execute("UPDATE branches SET head = ?1 WHERE name = ?2", params![id, branch])?;

    recompute_blame(&tx, &files)?;
    tx.commit()?;
    Ok(written)
}

/// Rebuild blame for the given files. A line is owned by the earliest version
/// that contains it; lines never committed stay unattributed.
fn recompute_blame(conn: &Connection, files: &[(String, String)]) -> ZResult<()> {
    for (path, hash) in files {
        let current = get_object(conn, hash)?;
        let lines: Vec<&str> = current.lines().collect();

        let mut stmt = conn.prepare(
            "SELECT fv.hash, fv.changeset_id, cs.author, cs.created_at
             FROM file_versions fv JOIN changesets cs ON cs.id = fv.changeset_id
             WHERE fv.path = ?1 ORDER BY fv.version ASC",
        )?;
        let versions: Vec<(String, String, String, i64)> = stmt
            .query_map(params![path], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);

        conn.execute("DELETE FROM blame WHERE path = ?1", params![path])?;

        let mut owner: Vec<Option<(String, String, i64)>> = vec![None; lines.len()];
        let mut remaining = lines.len();
        for (vh, cs_id, author, ts) in &versions {
            if remaining == 0 {
                break;
            }
            let Ok(old) = get_object(conn, vh) else { continue };
            let old_lines: HashSet<&str> = old.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                if owner[i].is_none() && old_lines.contains(line) {
                    owner[i] = Some((cs_id.clone(), author.clone(), *ts));
                    remaining -= 1;
                }
            }
        }

        for (i, slot) in owner.iter().enumerate() {
            if let Some((cs_id, author, ts)) = slot {
                conn.execute(
                    "INSERT INTO blame(path, line_no, changeset_id, author, committed_at)
                     VALUES(?1, ?2, ?3, ?4, ?5)",
                    params![path, (i + 1) as i64, cs_id, author, ts],
                )?;
            }
        }
    }
    Ok(())
}

/// Line-level diff between two texts, preserving order and repeated lines.
/// Returns (removed, added) as ordered lists.
pub fn line_diff(old: &str, new: &str) -> (Vec<String>, Vec<String>) {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();

    let mut new_counts: std::collections::HashMap<&str, i32> = std::collections::HashMap::new();
    for l in &new_lines {
        *new_counts.entry(l).or_insert(0) += 1;
    }
    let mut removed = Vec::new();
    for l in &old_lines {
        match new_counts.get_mut(l) {
            Some(c) if *c > 0 => *c -= 1,
            _ => removed.push(l.to_string()),
        }
    }

    let mut old_counts: std::collections::HashMap<&str, i32> = std::collections::HashMap::new();
    for l in &old_lines {
        *old_counts.entry(l).or_insert(0) += 1;
    }
    let mut added = Vec::new();
    for l in &new_lines {
        match old_counts.get_mut(l) {
            Some(c) if *c > 0 => *c -= 1,
            _ => added.push(l.to_string()),
        }
    }

    (removed, added)
}

/// Write a committed file back to disk.
pub fn checkout_file(
    conn: &Connection,
    root: &Path,
    rel: &str,
    changeset: Option<&str>,
) -> ZResult<usize> {
    let hash: String = match changeset {
        Some(cs) => conn
            .query_row(
                "SELECT new_hash FROM changeset_files WHERE changeset_id = ?1 AND path = ?2",
                params![cs, rel],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| bad(format!("{} not changed in changeset {}", rel, cs)))?,
        None => {
            let branch = crate::store::current_branch(conn)?;
            latest_hash(conn, rel, &branch)?
                .ok_or_else(|| bad(format!("no committed version of {}", rel)))?
        }
    };

    let content = get_object(conn, &hash)?;
    let target = root.join(rel);
    if let Some(parent) = target.as_path().parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&target, content.as_bytes())?;
    Ok(content.len())
}

/// Fast-forward `target` with changesets reachable from `source` but not from
/// `target`. Returns the ids applied.
pub fn merge_branch(conn: &Connection, source: &str, target: &str) -> ZResult<Vec<String>> {
    let src_head = crate::store::branch_head(conn, source)?;
    let tgt_head = crate::store::branch_head(conn, target)?;

    if src_head.is_none() {
        return Ok(Vec::new());
    }
    if src_head == tgt_head {
        return Ok(Vec::new());
    }

    let src_chain = reachable_from(conn, source)?;
    let tgt_set: HashSet<String> = reachable_from(conn, target)?.into_iter().collect();

    // Walk source history oldest-first, keeping only what target has not seen.
    let mut to_apply: Vec<String> = src_chain
        .into_iter()
        .rev()
        .filter(|id| !tgt_set.contains(id))
        .collect();

    if to_apply.is_empty() {
        return Ok(Vec::new());
    }

    // Parent rewiring and the head move must be atomic, or the branch head can
    // point at a changeset whose parent chain was only partly rewritten.
    let tx = conn.unchecked_transaction()?;
    let mut parent = tgt_head;
    for id in &to_apply {
        tx.execute(
            "UPDATE changesets SET parent = ?1 WHERE id = ?2",
            params![parent, id],
        )?;
        parent = Some(id.clone());
    }
    tx.execute("UPDATE branches SET head = ?1 WHERE name = ?2", params![parent, target])?;
    tx.commit()?;

    Ok(std::mem::take(&mut to_apply))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::mem_db;

    #[test]
    fn line_diff_preserves_order_and_duplicates() {
        let (rem, add) = line_diff("a\nb\na\n", "a\nc\na\n");
        assert_eq!(rem, vec!["b"]);
        assert_eq!(add, vec!["c"]);

        // Duplicate lines: two "x" -> one "x" means one removal.
        let (rem, add) = line_diff("x\nx\n", "x\n");
        assert_eq!(rem, vec!["x"]);
        assert!(add.is_empty(), "no additions expected, got {:?}", add);
    }

    #[test]
    fn line_diff_handles_pure_insert_and_delete() {
        let (rem, add) = line_diff("", "a\nb\n");
        assert!(rem.is_empty());
        assert_eq!(add, vec!["a", "b"]);

        let (rem, add) = line_diff("a\nb\n", "");
        assert_eq!(rem, vec!["a", "b"]);
        assert!(add.is_empty());
    }

    #[test]
    fn commit_creates_versions_head_and_blame() {
        let conn = mem_db();
        let content = "alpha\nbeta\n";
        let h = put_object(&conn, content).unwrap();
        conn.execute(
            "INSERT INTO changesets(id, message, author, created_at, status, parent, branch)
             VALUES('cs1', 'first', 'agent', 1, 'pending', NULL, 'main')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id, path, old_hash, new_hash)
             VALUES('cs1', 'f.txt', NULL, ?1)",
            params![h],
        )
        .unwrap();

        assert_eq!(commit_one(&conn, "cs1").unwrap(), 1);

        let versions: i64 = conn
            .query_row("SELECT COUNT(*) FROM file_versions WHERE path='f.txt'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(versions, 1);

        let blamed: i64 = conn
            .query_row("SELECT COUNT(*) FROM blame WHERE path='f.txt'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(blamed, 2);

        let head: Option<String> = conn
            .query_row("SELECT head FROM branches WHERE name='main'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(head.as_deref(), Some("cs1"));
    }

    #[test]
    fn commit_is_idempotent() {
        let conn = mem_db();
        let h = put_object(&conn, "x\n").unwrap();
        conn.execute(
            "INSERT INTO changesets(id, message, author, created_at, status, parent, branch)
             VALUES('cs1', 'm', 'a', 1, 'pending', NULL, 'main')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id, path, old_hash, new_hash)
             VALUES('cs1', 'f.txt', NULL, ?1)",
            params![h],
        )
        .unwrap();

        assert_eq!(commit_one(&conn, "cs1").unwrap(), 1);
        assert_eq!(commit_one(&conn, "cs1").unwrap(), 0);

        let versions: i64 = conn
            .query_row("SELECT COUNT(*) FROM file_versions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(versions, 1);
    }

    #[test]
    fn blame_attributes_lines_to_first_introducing_version() {
        let conn = mem_db();

        let v1 = put_object(&conn, "keep\nold\n").unwrap();
        conn.execute(
            "INSERT INTO changesets(id, message, author, created_at, status, parent, branch)
             VALUES('cs1','v1','agent',1,'committed',NULL,'main')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO file_versions(path, version, hash, changeset_id, created_at)
             VALUES('f.txt',1,?1,'cs1',1)",
            params![v1],
        )
        .unwrap();
        conn.execute("UPDATE branches SET head='cs1' WHERE name='main'", []).unwrap();

        let v2 = put_object(&conn, "keep\nnew\n").unwrap();
        conn.execute(
            "INSERT INTO changesets(id, message, author, created_at, status, parent, branch)
             VALUES('cs2','v2','agent2',2,'committed','cs1','main')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO file_versions(path, version, hash, changeset_id, created_at)
             VALUES('f.txt',2,?1,'cs2',2)",
            params![v2],
        )
        .unwrap();
        conn.execute("UPDATE branches SET head='cs2' WHERE name='main'", []).unwrap();

        recompute_blame(&conn, &[("f.txt".to_string(), v2.clone())]).unwrap();

        let line1: String = conn
            .query_row("SELECT changeset_id FROM blame WHERE path='f.txt' AND line_no=1", [], |r| {
                r.get(0)
            })
            .unwrap();
        let line2: String = conn
            .query_row("SELECT changeset_id FROM blame WHERE path='f.txt' AND line_no=2", [], |r| {
                r.get(0)
            })
            .unwrap();

        assert_eq!(line1, "cs1", "unchanged line keeps its original changeset");
        assert_eq!(line2, "cs2", "changed line belongs to the new changeset");
    }

    #[test]
    fn merge_fast_forwards_and_is_idempotent() {
        let conn = mem_db();
        conn.execute(
            "INSERT INTO changesets(id, message, author, created_at, status, parent, branch)
             VALUES('cs1','m','a',1,'committed',NULL,'main')",
            [],
        )
        .unwrap();
        conn.execute("UPDATE branches SET head='cs1' WHERE name='main'", []).unwrap();
        conn.execute(
            "INSERT INTO branches(name, head, created_at, from_branch) VALUES('dev','cs1',0,'main')",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO changesets(id, message, author, created_at, status, parent, branch)
             VALUES('cs2','m','a',2,'committed','cs1','dev')",
            [],
        )
        .unwrap();
        conn.execute("UPDATE branches SET head='cs2' WHERE name='dev'", []).unwrap();

        let applied = merge_branch(&conn, "dev", "main").unwrap();
        assert_eq!(applied, vec!["cs2"]);

        let head = crate::store::branch_head(&conn, "main").unwrap();
        assert_eq!(head.as_deref(), Some("cs2"));

        let again = merge_branch(&conn, "dev", "main").unwrap();
        assert!(again.is_empty(), "second merge is a no-op");
    }

    #[test]
    fn merge_of_empty_branch_does_nothing() {
        let conn = mem_db();
        conn.execute(
            "INSERT INTO branches(name, head, created_at, from_branch) VALUES('empty',NULL,0,'main')",
            [],
        )
        .unwrap();
        let applied = merge_branch(&conn, "empty", "main").unwrap();
        assert!(applied.is_empty());
    }

    #[test]
    fn commit_rolls_back_when_an_object_is_missing() {
        let conn = mem_db();
        let good = put_object(&conn, "a\n").unwrap();
        let missing = "0".repeat(64);

        conn.execute(
            "INSERT INTO changesets(id, message, author, created_at, status, parent, branch)
             VALUES('csbad', 'm', 'a', 1, 'pending', NULL, 'main')",
            [],
        )
        .unwrap();
        for (path, hash) in [("a.txt", &good), ("z.txt", &missing)] {
            conn.execute(
                "INSERT INTO changeset_files(changeset_id, path, old_hash, new_hash)
                 VALUES('csbad', ?1, NULL, ?2)",
                params![path, hash],
            )
            .unwrap();
        }

        let err = commit_one(&conn, "csbad").unwrap_err();
        assert!(err.to_string().contains("not found"), "got: {}", err);

        // The first file's version insert must have been rolled back.
        let versions: i64 = conn
            .query_row("SELECT COUNT(*) FROM file_versions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(versions, 0, "partial commit must not leave file versions");

        let status: String = conn
            .query_row("SELECT status FROM changesets WHERE id='csbad'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "pending", "changeset stays pending after a failed commit");

        let head: Option<String> = conn
            .query_row("SELECT head FROM branches WHERE name='main'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(head, None, "branch head must not move");
    }

    #[test]
    fn create_changeset_reports_binary_files_instead_of_dropping_them() {
        let tmp = std::env::temp_dir().join(format!("zf_create_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join(".zenforge")).unwrap();

        let conn = mem_db();
        std::fs::write(tmp.join("good.txt"), "text\n").unwrap();
        std::fs::write(tmp.join("blob.bin"), [0xffu8, 0xfe, 0x00]).unwrap();

        let created = create_changeset(&conn, &tmp, "m", "a").unwrap().unwrap();
        assert_eq!(created.files.len(), 1, "only the text file is tracked");
        assert_eq!(created.files[0].0, "good.txt");
        assert_eq!(created.skipped, vec!["blob.bin".to_string()]);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn create_changeset_returns_none_on_clean_tree() {
        let tmp = std::env::temp_dir().join(format!("zf_clean_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join(".zenforge")).unwrap();
        std::fs::write(tmp.join("a.txt"), "x\n").unwrap();

        let conn = mem_db();
        let first = create_changeset(&conn, &tmp, "m", "a").unwrap();
        assert!(first.is_some());
        commit_one(&conn, &first.unwrap().id).unwrap();

        let second = create_changeset(&conn, &tmp, "m2", "a").unwrap();
        assert!(second.is_none(), "no changes must yield no changeset");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn committing_unchanged_content_does_not_add_a_version() {
        let conn = mem_db();
        let h = put_object(&conn, "same\n").unwrap();

        for cs in ["cs1", "cs2"] {
            conn.execute(
                "INSERT INTO changesets(id, message, author, created_at, status, parent, branch)
                 VALUES(?1, 'm', 'a', 1, 'pending', NULL, 'main')",
                params![cs],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO changeset_files(changeset_id, path, old_hash, new_hash)
                 VALUES(?1, 'f.txt', NULL, ?2)",
                params![cs, h],
            )
            .unwrap();
        }

        assert_eq!(commit_one(&conn, "cs1").unwrap(), 1, "first write creates v1");
        assert_eq!(commit_one(&conn, "cs2").unwrap(), 0, "identical content adds nothing");

        let versions: i64 = conn
            .query_row("SELECT COUNT(*) FROM file_versions WHERE path='f.txt'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(versions, 1, "history must not inflate without a content change");
    }

    #[test]
    fn version_numbers_stay_contiguous_after_skips() {
        let conn = mem_db();
        let a = put_object(&conn, "a\n").unwrap();
        let b = put_object(&conn, "b\n").unwrap();

        // cs1 writes a (v1), cs2 rewrites the same content (skip), cs3 writes b (v2).
        for (cs, hash) in [("cs1", &a), ("cs2", &a), ("cs3", &b)] {
            conn.execute(
                "INSERT INTO changesets(id, message, author, created_at, status, parent, branch)
                 VALUES(?1, 'm', 'a', 1, 'pending', NULL, 'main')",
                params![cs],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO changeset_files(changeset_id, path, old_hash, new_hash)
                 VALUES(?1, 'f.txt', NULL, ?2)",
                params![cs, hash],
            )
            .unwrap();
            commit_one(&conn, cs).unwrap();
        }

        let vers: Vec<i64> = {
            let mut stmt = conn
                .prepare("SELECT version FROM file_versions WHERE path='f.txt' ORDER BY version")
                .unwrap();
            let rows = stmt.query_map([], |r| r.get::<_, i64>(0)).unwrap();
            rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
        };
        assert_eq!(vers, vec![1, 2], "versions stay contiguous when a write is skipped");
    }

    #[test]
    fn committing_a_pending_changeset_never_orphans_an_earlier_one() {
        let conn = mem_db();
        let ha = put_object(&conn, "a\n").unwrap();
        let hb = put_object(&conn, "b\n").unwrap();

        // Both snapshots were taken while main was empty, so both recorded a
        // NULL parent. Committing them in order must still chain them.
        for (cs, path, hash) in [("cs1", "a.txt", &ha), ("cs2", "b.txt", &hb)] {
            conn.execute(
                "INSERT INTO changesets(id, message, author, created_at, status, parent, branch)
                 VALUES(?1, 'm', 'a', 1, 'pending', NULL, 'main')",
                params![cs],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO changeset_files(changeset_id, path, old_hash, new_hash)
                 VALUES(?1, ?2, NULL, ?3)",
                params![cs, path, hash],
            )
            .unwrap();
        }

        commit_one(&conn, "cs1").unwrap();
        commit_one(&conn, "cs2").unwrap();

        let reachable = reachable_from(&conn, "main").unwrap();
        assert!(reachable.contains(&"cs1".to_string()), "cs1 still on the branch");
        assert!(reachable.contains(&"cs2".to_string()), "cs2 is the head");

        let parent_of_cs2: Option<String> = conn
            .query_row("SELECT parent FROM changesets WHERE id='cs2'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(parent_of_cs2.as_deref(), Some("cs1"), "cs2 re-parented onto the live head");
    }

    #[test]
    fn committing_an_older_pending_changeset_keeps_the_newer_one() {
        let conn = mem_db();
        let ha = put_object(&conn, "a\n").unwrap();
        let hb = put_object(&conn, "b\n").unwrap();

        for (cs, path, hash) in [("cs_new", "b.txt", &hb), ("cs_old", "a.txt", &ha)] {
            conn.execute(
                "INSERT INTO changesets(id, message, author, created_at, status, parent, branch)
                 VALUES(?1, 'm', 'a', 1, 'pending', NULL, 'main')",
                params![cs],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO changeset_files(changeset_id, path, old_hash, new_hash)
                 VALUES(?1, ?2, NULL, ?3)",
                params![cs, path, hash],
            )
            .unwrap();
        }

        commit_one(&conn, "cs_new").unwrap();
        commit_one(&conn, "cs_old").unwrap(); // committed out of order

        let reachable = reachable_from(&conn, "main").unwrap();
        assert!(
            reachable.contains(&"cs_new".to_string()),
            "the newer changeset must stay reachable, got {:?}",
            reachable
        );
        assert!(reachable.contains(&"cs_old".to_string()));
    }
}