//! Changeset lifecycle, blame derivation, merge, checkout.

use rusqlite::{params, Connection, OptionalExtension};
use std::fs;
use std::path::Path;

use crate::merge::{diff_hunks, merge3, MergeResult};
use crate::store::{
    branch_head, current_branch, get_object, hash_at_point, hash_content, latest_hash,
    merge_base, new_changeset_id, put_object,
};
use crate::worktree::{read_file, rel_path, scan_files, IgnoreRules, ReadOutcome};
use crate::{bad, now, ZResult, ZF_DIR};

/// Files loaded from the repository's ignore configuration.
pub fn load_ignore_rules(root: &Path, conn: &Connection) -> ZResult<IgnoreRules> {
    let mut extra: Vec<String> = Vec::new();

    for name in [".zenignore", ".gitignore"] {
        if let Ok(text) = fs::read_to_string(root.join(name)) {
            for line in text.lines() {
                let l = line.trim();
                if !l.is_empty() && !l.starts_with('#') {
                    extra.push(l.to_string());
                }
            }
        }
    }
    if let Ok(text) = fs::read_to_string(root.join(ZF_DIR).join("ignore")) {
        for line in text.lines() {
            let l = line.trim();
            if !l.is_empty() && !l.starts_with('#') {
                extra.push(l.to_string());
            }
        }
    }
    let mut stmt = conn.prepare("SELECT pattern FROM ignore_rules ORDER BY pattern")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    for r in rows {
        extra.push(r?);
    }
    Ok(IgnoreRules::with_defaults(&extra))
}

/// One file's diff: path, removed lines, added lines.
pub type FileDiff = (String, Vec<String>, Vec<String>);

/// What a diff of the working tree against a branch found.
pub struct WorktreeDelta {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
    pub unchanged: usize,
    pub skipped_binary: Vec<String>,
}

/// Compare the working tree to `branch`.
pub fn diff_worktree(conn: &Connection, root: &Path, branch: &str) -> ZResult<WorktreeDelta> {
    let rules = load_ignore_rules(root, conn)?;
    let mut d = WorktreeDelta {
        added: Vec::new(),
        modified: Vec::new(),
        deleted: Vec::new(),
        unchanged: 0,
        skipped_binary: Vec::new(),
    };
    let mut seen = std::collections::HashSet::new();

    for p in scan_files(root, &rules)? {
        let rel = rel_path(root, &p);
        match read_file(&p) {
            ReadOutcome::Text(content) => {
                seen.insert(rel.clone());
                let new_hash = hash_content(&content);
                match latest_hash(conn, &rel, branch)? {
                    None => d.added.push(rel),
                    Some(old) if old != new_hash => d.modified.push(rel),
                    Some(_) => d.unchanged += 1,
                }
            }
            ReadOutcome::Binary => d.skipped_binary.push(rel),
            ReadOutcome::Unreadable(e) => {
                return Err(bad(format!("cannot read {}: {}", rel, e)));
            }
        }
    }

    // Files tracked on the branch but absent from disk.
    let head = branch_head(conn, branch)?;
    if let Some(head) = head {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT path FROM file_versions WHERE changeset_id = ?1 ORDER BY path",
        )?;
        let tracked: Vec<String> = stmt
            .query_map(params![head], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for path in tracked {
            if !seen.contains(&path) && !root.join(&path).exists() {
                d.deleted.push(path);
            }
        }
    }

    Ok(d)
}

/// A newly created changeset.
pub struct NewChangeset {
    pub id: String,
    pub files: Vec<(String, Option<String>, String)>,
    pub deleted: Vec<String>,
    pub skipped: Vec<String>,
}

/// Snapshot the working tree as a pending changeset. `None` when nothing
/// changed. Deletions of tracked files are recorded too.
pub fn create_changeset(
    conn: &Connection,
    root: &Path,
    message: &str,
    author: &str,
) -> ZResult<Option<NewChangeset>> {
    let branch = current_branch(conn)?;
    let rules = load_ignore_rules(root, conn)?;

    let mut changes: Vec<(String, Option<String>, String, String)> = Vec::new();
    let mut deleted: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for p in scan_files(root, &rules)? {
        let rel = rel_path(root, &p);
        match read_file(&p) {
            ReadOutcome::Text(content) => {
                seen.insert(rel.clone());
                let new_hash = hash_content(&content);
                let old_hash = latest_hash(conn, &rel, &branch)?;
                if old_hash.as_deref() != Some(new_hash.as_str()) {
                    changes.push((rel, old_hash, new_hash, content));
                }
            }
            ReadOutcome::Binary => skipped.push(rel),
            ReadOutcome::Unreadable(e) => {
                return Err(bad(format!("cannot read {}: {}", rel, e)));
            }
        }
    }

    if let Some(head) = branch_head(conn, &branch)? {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT path FROM file_versions WHERE changeset_id = ?1 ORDER BY path",
        )?;
        let tracked: Vec<String> = stmt
            .query_map(params![head], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for path in tracked {
            if !seen.contains(&path) && !root.join(&path).exists() {
                deleted.push(path);
            }
        }
    }

    if changes.is_empty() && deleted.is_empty() {
        return Ok(None);
    }

    let id = new_changeset_id(conn, message, author)?;
    let parent = branch_head(conn, &branch)?;

    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO changesets(id, message, author, created_at, status, parent, branch)
         VALUES(?1, ?2, ?3, ?4, 'pending', ?5, ?6)",
        params![id, message, author, now(), parent, branch],
    )?;

    let mut files = Vec::new();
    for (rel, old, new, content) in &changes {
        put_object(&tx, content)?;
        tx.execute(
            "INSERT INTO changeset_files(changeset_id, path, old_hash, new_hash)
             VALUES(?1, ?2, ?3, ?4)",
            params![id, rel, old, new],
        )?;
        files.push((rel.clone(), old.clone(), new.clone()));
    }
    for path in &deleted {
        tx.execute(
            "INSERT INTO changeset_files(changeset_id, path, old_hash, new_hash)
             VALUES(?1, ?2, ?3, NULL)",
            params![id, path, latest_hash(&tx, path, &branch)?],
        )?;
    }
    tx.commit()?;

    Ok(Some(NewChangeset { id, files, deleted, skipped }))
}

/// Commit one pending changeset. Returns files written; `0` means it was
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

    let files: Vec<(String, Option<String>, Option<String>)> = {
        let mut stmt = conn.prepare(
            "SELECT path, old_hash, new_hash FROM changeset_files
             WHERE changeset_id = ?1 ORDER BY path",
        )?;
        let rows = stmt.query_map(params![id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };

    // One transaction: a half-written commit leaves orphan versions and a
    // pending changeset with content already recorded.
    let tx = conn.unchecked_transaction()?;

    // Re-parent onto the live head so a pending changeset never forks history
    // or orphans an earlier one.
    let head_now = branch_head(&tx, &branch)?;
    if head_now.as_deref() != Some(id) {
        tx.execute("UPDATE changesets SET parent = ?1 WHERE id = ?2", params![head_now, id])?;
    }

    let mut written = 0usize;
    let mut touched: Vec<(String, String)> = Vec::new();
    for (path, _old, new) in &files {
        match new {
            Some(hash) => {
                let latest: Option<String> = tx
                    .query_row(
                        "SELECT hash FROM file_versions WHERE path = ?1
                         ORDER BY version DESC LIMIT 1",
                        params![path],
                        |r| r.get(0),
                    )
                    .optional()?;

                if latest.as_deref() != Some(hash.as_str()) {
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

                // The committed content is already in `objects`; duplicating it
                // into a working-copy table only doubled the disk for small
                // repositories and nothing ever read it back.
                touched.push((path.clone(), hash.clone()));
            }
            None => {
                // Deletion: record it as a version with no hash so history can
                // still answer "which changeset removed this file".
                tx.execute("DELETE FROM blame WHERE path = ?1", params![path])?;
                written += 1;
            }
        }
    }

    tx.execute("UPDATE changesets SET status = 'committed' WHERE id = ?1", params![id])?;
    tx.execute("UPDATE branches SET head = ?1 WHERE name = ?2", params![id, branch])?;

    recompute_blame(&tx, &touched)?;
    tx.commit()?;
    Ok(written)
}

/// Rebuild blame for the given files.
///
/// Lines are matched by sequence (longest common subsequence against each older
/// version), not by content set, so a line that duplicates another line keeps
/// its own author and a moved line keeps the author who wrote it.
fn recompute_blame(conn: &Connection, files: &[(String, String)]) -> ZResult<()> {
    for (path, hash) in files {
        let current = get_object(conn, hash)?;
        let cur_lines: Vec<&str> = current.lines().collect();

        let versions: Vec<(String, String, String, i64)> = {
            let mut stmt = conn.prepare(
                "SELECT fv.hash, fv.changeset_id, cs.author, cs.created_at
                 FROM file_versions fv JOIN changesets cs ON cs.id = fv.changeset_id
                 WHERE fv.path = ?1 ORDER BY fv.version ASC",
            )?;
            let rows = stmt.query_map(params![path], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };

        conn.execute("DELETE FROM blame WHERE path = ?1", params![path])?;

        // Owner per current line. Start with the oldest version and let later
        // versions fill only the lines still unowned, so the first version that
        // introduced a line keeps it.
        let mut owner: Vec<Option<(String, String, i64)>> = vec![None; cur_lines.len()];
        let mut remaining = cur_lines.len();

        for (vh, cs_id, author, ts) in &versions {
            if remaining == 0 {
                break;
            }
            let Ok(old) = get_object(conn, vh) else { continue };
            let old_lines: Vec<&str> = old.lines().collect();

            // Lines present in both, in order.
            let mut matched_old = vec![false; old_lines.len()];
            let mut j = 0usize;
            for (i, line) in cur_lines.iter().enumerate() {
                if owner[i].is_some() {
                    continue;
                }
                // Look ahead in the old version for this exact line.
                while j < old_lines.len() && &old_lines[j] != line {
                    j += 1;
                }
                if j < old_lines.len() {
                    matched_old[j] = true;
                    owner[i] = Some((cs_id.clone(), author.clone(), *ts));
                    remaining -= 1;
                    j += 1;
                }
            }
        }

        // Anything still unowned belongs to the newest version that introduced it.
        if let Some((_, cs_id, author, ts)) = versions.last() {
            for slot in owner.iter_mut() {
                if slot.is_none() {
                    *slot = Some((cs_id.clone(), author.clone(), *ts));
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

/// Append a conflict copy of a file so a failed merge never loses the other side.
pub fn write_conflict_copy(root: &Path, path: &str, theirs: &str) -> ZResult<String> {
    let target = root.join(format!("{}.theirs", path));
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&target, theirs)?;
    Ok(format!("{}.theirs", path))
}

/// Result of a merge attempt.
pub struct MergeOutcome {
    pub applied: Vec<String>,
    pub conflicts: Vec<String>,
    pub merge_changeset: Option<String>,
}

/// Merge `source` into `target` with a real three-way merge.
///
/// Writes merged content to disk, records a merge changeset with two parents,
/// and refuses to move the target head when any file conflicts.
pub fn merge_branch(
    conn: &Connection,
    root: &Path,
    source: &str,
    target: &str,
    author: &str,
) -> ZResult<MergeOutcome> {
    let src_head = branch_head(conn, source)?;
    let tgt_head = branch_head(conn, target)?;

    let Some(src_head) = src_head else {
        return Ok(MergeOutcome { applied: Vec::new(), conflicts: Vec::new(), merge_changeset: None });
    };
    if Some(src_head.clone()) == tgt_head {
        return Ok(MergeOutcome { applied: Vec::new(), conflicts: Vec::new(), merge_changeset: None });
    }

    // Fast-forward when the target is already an ancestor of the source.
    if let Some(t) = &tgt_head {
        let tgt_anc = crate::store::ancestors(conn, t)?;
        if tgt_anc.contains(&src_head) {
            return Ok(MergeOutcome { applied: Vec::new(), conflicts: Vec::new(), merge_changeset: None });
        }
        let src_anc = crate::store::ancestors(conn, &src_head)?;
        if src_anc.contains(t) {
            // Target is behind: move the head and materialise the files.
            let tx = conn.unchecked_transaction()?;
            tx.execute("UPDATE branches SET head = ?1 WHERE name = ?2", params![src_head, target])?;
            tx.commit()?;
            return Ok(MergeOutcome {
                applied: vec![src_head],
                conflicts: Vec::new(),
                merge_changeset: None,
            });
        }
    }

    let base = match &tgt_head {
        Some(t) => merge_base(conn, t, &src_head)?,
        None => None,
    };

    // Every path touched on either side relative to the base.
    let mut paths: Vec<String> = Vec::new();
    let src_anc = crate::store::ancestors(conn, &src_head)?;
    let tgt_anc: Vec<String> = match &tgt_head {
        Some(t) => crate::store::ancestors(conn, t)?,
        None => Vec::new(),
    };
    let base_set: std::collections::HashSet<String> = match &base {
        Some(b) => crate::store::ancestors(conn, b)?.into_iter().collect(),
        None => std::collections::HashSet::new(),
    };

    let collect = |ids: &[String], exclude: &std::collections::HashSet<String>| -> ZResult<Vec<String>> {
        let mut out = std::collections::HashSet::new();
        let mut stmt = conn.prepare("SELECT path FROM changeset_files WHERE changeset_id = ?1")?;
        for id in ids {
            if exclude.contains(id) {
                continue;
            }
            let rows = stmt.query_map(params![id], |r| r.get::<_, String>(0))?;
            for r in rows {
                out.insert(r?);
            }
        }
        Ok(out.into_iter().collect())
    };

    let mut all: std::collections::HashSet<String> = std::collections::HashSet::new();
    for p in collect(&src_anc, &base_set)? {
        all.insert(p);
    }
    for p in collect(&tgt_anc, &base_set)? {
        all.insert(p);
    }
    paths.extend(all);
    paths.sort();

    let mut merged_files: Vec<(String, String)> = Vec::new();
    let mut conflicts: Vec<String> = Vec::new();

    for path in &paths {
        let base_text = match &base {
            Some(b) => match hash_at_point(conn, path, b)? {
                Some(h) => get_object(conn, &h)?,
                None => String::new(),
            },
            None => String::new(),
        };
        let our_text = match &tgt_head {
            Some(t) => match hash_at_point(conn, path, t)? {
                Some(h) => get_object(conn, &h)?,
                None => String::new(),
            },
            None => String::new(),
        };
        let their_text = match hash_at_point(conn, path, &src_head)? {
            Some(h) => get_object(conn, &h)?,
            None => String::new(),
        };

        // A path absent on one side and present on the other is an add.
        let result = merge3(&base_text, &our_text, &their_text);
        if result.is_conflict() {
            // Record the path and preserve their side before writing anything,
            // so a refused merge still leaves the other version recoverable.
            let theirs = match &result {
                MergeResult::Conflict { theirs, .. } => theirs.clone(),
                MergeResult::Clean(_) => String::new(),
            };
            // `ours` is what the file holds now; the caller needs both sides to
            // show a useful conflict message.
            let _ = result.ours();
            write_conflict_copy(root, path, &theirs)?;
            merged_files.push((path.clone(), result.text().to_string()));
            conflicts.push(path.clone());
        } else if result.text() != our_text {
            merged_files.push((path.clone(), result.text().to_string()));
        }
    }

    if !conflicts.is_empty() {
        // Write what we produced so the user can inspect it, but leave the head
        // alone: a conflicted merge is not a merge.
        for (path, text) in &merged_files {
            let target_path = root.join(path);
            if let Some(parent) = target_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&target_path, text)?;
        }
        return Ok(MergeOutcome { applied: Vec::new(), conflicts, merge_changeset: None });
    }

    // Record the merge as a changeset with two parents.
    let id = new_changeset_id(conn, &format!("merge {} into {}", source, target), author)?;
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO changesets(id, message, author, created_at, status, parent, parent2, branch)
         VALUES(?1, ?2, ?3, ?4, 'committed', ?5, ?6, ?7)",
        params![
            id,
            format!("merge {} into {}", source, target),
            author,
            now(),
            tgt_head,
            src_head,
            target
        ],
    )?;

    for (path, text) in &merged_files {
        let h = put_object(&tx, text)?;
        tx.execute(
            "INSERT INTO changeset_files(changeset_id, path, old_hash, new_hash)
             VALUES(?1, ?2, NULL, ?3)",
            params![id, path, h],
        )?;
        let next_ver: i64 = tx.query_row(
            "SELECT COALESCE(MAX(version), 0) + 1 FROM file_versions WHERE path = ?1",
            params![path],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO file_versions(path, version, hash, changeset_id, created_at)
             VALUES(?1, ?2, ?3, ?4, ?5)",
            params![path, next_ver, h, id, now()],
        )?;
    }
    tx.execute("UPDATE branches SET head = ?1 WHERE name = ?2", params![id, target])?;
    tx.commit()?;

    // Materialise the merged tree on disk.
    for (path, text) in &merged_files {
        let target_path = root.join(path);
        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&target_path, text)?;
    }

    let touched: Vec<(String, String)> = merged_files
        .iter()
        .map(|(p, t)| (p.clone(), hash_content(t)))
        .collect();
    recompute_blame(conn, &touched)?;

    Ok(MergeOutcome { applied: vec![id.clone()], conflicts: Vec::new(), merge_changeset: Some(id) })
}

/// Diff two changesets (or a changeset against its first parent).
pub fn changeset_diff(conn: &Connection, id: &str) -> ZResult<Vec<FileDiff>> {
    let parent: Option<String> = conn
        .query_row("SELECT parent FROM changesets WHERE id = ?1", params![id], |r| {
            r.get::<_, Option<String>>(0)
        })
        .optional()?
        .flatten();

    let files: Vec<(String, Option<String>, Option<String>)> = {
        let mut stmt = conn.prepare(
            "SELECT path, old_hash, new_hash FROM changeset_files
             WHERE changeset_id = ?1 ORDER BY path",
        )?;
        let rows = stmt.query_map(params![id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };

    let mut out = Vec::new();
    for (path, _old, new) in files {
        let old_text = match &parent {
            Some(p) => match hash_at_point(conn, &path, p)? {
                Some(h) => get_object(conn, &h)?,
                None => String::new(),
            },
            None => String::new(),
        };
        let new_text = match new {
            Some(h) => get_object(conn, &h)?,
            None => String::new(),
        };
        let hunks = diff_hunks(&old_text, &new_text);
        let a: Vec<&str> = old_text.lines().collect();
        let b: Vec<&str> = new_text.lines().collect();
        let mut removed = Vec::new();
        let mut added = Vec::new();
        for h in hunks {
            for l in &a[h.old_start..h.old_start + h.old_len] {
                removed.push(l.to_string());
            }
            for l in &b[h.new_start..h.new_start + h.new_len] {
                added.push(l.to_string());
            }
        }
        out.push((path, removed, added));
    }
    Ok(out)
}

/// Write a committed file back to disk.
pub fn checkout_file(
    conn: &Connection,
    root: &Path,
    rel: &str,
    changeset: Option<&str>,
) -> ZResult<usize> {
    let hash = match changeset {
        Some(cs) => hash_at_point(conn, rel, cs)?
            .ok_or_else(|| bad(format!("{} not tracked as of changeset {}", rel, cs)))?,
        None => {
            let branch = current_branch(conn)?;
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

/// The full file tree as of `changeset`: every path reachable from it paired
/// with its newest version. This is what a checkout materialises.
pub fn tree_at(conn: &Connection, changeset: &str) -> ZResult<Vec<(String, String)>> {
    let reachable = crate::store::ancestors(conn, changeset)?;
    if reachable.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = vec!["?"; reachable.len()].join(",");
    let sql = format!(
        "SELECT path, hash FROM (
            SELECT path, hash, version,
                   ROW_NUMBER() OVER (PARTITION BY path ORDER BY version DESC) AS rn
            FROM file_versions WHERE changeset_id IN ({})
         ) WHERE rn = 1 ORDER BY path",
        placeholders
    );
    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::ToSql> =
        reachable.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
    let files = stmt
        .query_map(params.as_slice(), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(files)
}

/// Write every file of `branch` to disk. Used after a push and after a clone so
/// the working tree never lags behind the branch head.
pub fn materialise(conn: &Connection, root: &Path, branch: &str) -> ZResult<usize> {
    let head = match branch_head(conn, branch)? {
        Some(h) => h,
        None => return Ok(0),
    };
    let files = tree_at(conn, &head)?;
    for (path, hash) in &files {
        let content = get_object(conn, hash)?;
        let target = root.join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&target, content.as_bytes())?;
    }
    Ok(files.len())
}

/// Restore every tracked file of `branch` to its committed content, and remove
/// tracked files that should not exist. Untracked files are left alone.
pub fn restore_worktree(conn: &Connection, root: &Path, branch: &str) -> ZResult<(usize, usize)> {
    let head = match branch_head(conn, branch)? {
        Some(h) => h,
        None => return Ok((0, 0)),
    };

    // The tree at `head` is the newest version of every path reachable from it,
    // not just the paths that `head` itself touched. Reading file_versions for
    // the head changeset alone loses every file added by an earlier changeset.
    let files = tree_at(conn, &head)?;

    let mut written = 0usize;
    for (path, hash) in &files {
        let content = get_object(conn, hash)?;
        let target = root.join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&target, content.as_bytes())?;
        written += 1;
    }

    // Tracked on an older changeset but gone from this one: delete it.
    let present: std::collections::HashSet<&String> = files.iter().map(|(p, _)| p).collect();
    let mut removed = 0usize;
    let mut all_stmt = conn.prepare("SELECT DISTINCT path FROM file_versions")?;
    let all: Vec<String> = all_stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for path in all {
        if !present.contains(&path) {
            let target = root.join(&path);
            if target.is_file() {
                fs::remove_file(&target)?;
                removed += 1;
            }
        }
    }
    Ok((written, removed))
}



#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::mem_db;

    fn commit_files(conn: &Connection, id: &str, msg: &str, files: &[(&str, &str)]) {
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES(?1,?2,'a',1,'pending',NULL,'main')",
            params![id, msg],
        )
        .unwrap();
        for (path, content) in files {
            let h = put_object(conn, content).unwrap();
            conn.execute(
                "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
                 VALUES(?1,?2,NULL,?3)",
                params![id, path, h],
            )
            .unwrap();
        }
        commit_one(conn, id).unwrap();
    }

    #[test]
    fn commit_creates_versions_head_and_blame() {
        let conn = mem_db();
        commit_files(&conn, "cs1", "first", &[("f.txt", "alpha\nbeta\n")]);

        let versions: i64 = conn
            .query_row("SELECT COUNT(*) FROM file_versions WHERE path='f.txt'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(versions, 1);
        let blamed: i64 = conn
            .query_row("SELECT COUNT(*) FROM blame WHERE path='f.txt'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(blamed, 2);
        assert_eq!(branch_head(&conn, "main").unwrap().as_deref(), Some("cs1"));
    }

    #[test]
    fn commit_is_idempotent() {
        let conn = mem_db();
        commit_files(&conn, "cs1", "m", &[("f.txt", "x\n")]);
        assert_eq!(commit_one(&conn, "cs1").unwrap(), 0);
        let versions: i64 = conn.query_row("SELECT COUNT(*) FROM file_versions", [], |r| r.get(0)).unwrap();
        assert_eq!(versions, 1);
    }

    #[test]
    fn committing_unchanged_content_adds_no_version() {
        let conn = mem_db();
        commit_files(&conn, "cs1", "m", &[("f.txt", "same\n")]);
        commit_files(&conn, "cs2", "m2", &[("f.txt", "same\n")]);
        let versions: i64 = conn
            .query_row("SELECT COUNT(*) FROM file_versions WHERE path='f.txt'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(versions, 1, "identical content must not add a version");
    }

    #[test]
    fn blame_attributes_by_sequence_not_content() {
        let conn = mem_db();
        commit_files(&conn, "cs1", "v1", &[("f.txt", "keep\nold\nend\n")]);
        commit_files(&conn, "cs2", "v2", &[("f.txt", "keep\nnew\nend\n")]);

        let line1: String = conn
            .query_row("SELECT changeset_id FROM blame WHERE path='f.txt' AND line_no=1", [], |r| r.get(0))
            .unwrap();
        let line2: String = conn
            .query_row("SELECT changeset_id FROM blame WHERE path='f.txt' AND line_no=2", [], |r| r.get(0))
            .unwrap();
        assert_eq!(line1, "cs1", "unchanged line keeps its original changeset");
        assert_eq!(line2, "cs2", "changed line belongs to the new changeset");
    }

    #[test]
    fn blame_does_not_credit_a_duplicate_line_to_an_earlier_author() {
        let conn = mem_db();
        // agentA writes two identical lines.
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('cs1','a','agentA',1,'pending',NULL,'main')",
            [],
        )
        .unwrap();
        let h1 = put_object(&conn, "return None\nother\nreturn None\n").unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('cs1','f.txt',NULL,?1)",
            params![h1],
        )
        .unwrap();
        commit_one(&conn, "cs1").unwrap();

        // agentB appends a third identical line.
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('cs2','b','agentB',2,'pending','cs1','main')",
            [],
        )
        .unwrap();
        let h2 = put_object(&conn, "return None\nother\nreturn None\nreturn None\n").unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('cs2','f.txt',NULL,?1)",
            params![h2],
        )
        .unwrap();
        commit_one(&conn, "cs2").unwrap();

        let authors: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT author FROM blame WHERE path='f.txt' ORDER BY line_no")
                .unwrap();
            let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
            rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
        };
        assert_eq!(authors, vec!["agentA", "agentA", "agentA", "agentB"],
            "the appended duplicate belongs to agentB");
    }

    #[test]
    fn merge_of_disjoint_edits_moves_head_and_writes_disk() {
        let tmp = std::env::temp_dir().join(format!("zf_merge_ok_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let conn = mem_db();
        fs::write(tmp.join("shared.txt"), "one\ntwo\nthree\n").unwrap();
        commit_files(&conn, "cs1", "base", &[("shared.txt", "one\ntwo\nthree\n")]);

        conn.execute(
            "INSERT INTO branches(name,head,created_at,from_branch) VALUES('dev','cs1',0,'main')",
            [],
        )
        .unwrap();

        // ours on main
        fs::write(tmp.join("shared.txt"), "ONE\ntwo\nthree\n").unwrap();
        commit_files(&conn, "cs2", "ours", &[("shared.txt", "ONE\ntwo\nthree\n")]);
        conn.execute("UPDATE changesets SET parent='cs1' WHERE id='cs2'", []).unwrap();

        // theirs on dev
        fs::write(tmp.join("shared.txt"), "one\ntwo\nTHREE\n").unwrap();
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('cs3','theirs','a',2,'pending','cs1','dev')",
            [],
        )
        .unwrap();
        let h = put_object(&conn, "one\ntwo\nTHREE\n").unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('cs3','shared.txt',NULL,?1)",
            params![h],
        )
        .unwrap();
        commit_one(&conn, "cs3").unwrap();

        let out = merge_branch(&conn, &tmp, "dev", "main", "a").unwrap();
        assert!(out.conflicts.is_empty(), "disjoint edits must not conflict");
        assert!(out.merge_changeset.is_some());

        let on_disk = fs::read_to_string(tmp.join("shared.txt")).unwrap();
        assert_eq!(on_disk, "ONE\ntwo\nTHREE\n", "merged content must reach the working copy");
        let head = branch_head(&conn, "main").unwrap().unwrap();
        assert_eq!(Some(head), out.merge_changeset);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn conflicting_merge_writes_theirs_and_leaves_head_alone() {
        let tmp = std::env::temp_dir().join(format!("zf_merge_bad_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let conn = mem_db();
        fs::write(tmp.join("s.txt"), "base\n").unwrap();
        commit_files(&conn, "cs1", "base", &[("s.txt", "base\n")]);
        conn.execute(
            "INSERT INTO branches(name,head,created_at,from_branch) VALUES('dev','cs1',0,'main')",
            [],
        )
        .unwrap();

        fs::write(tmp.join("s.txt"), "OURS\n").unwrap();
        commit_files(&conn, "cs2", "ours", &[("s.txt", "OURS\n")]);
        conn.execute("UPDATE changesets SET parent='cs1' WHERE id='cs2'", []).unwrap();

        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('cs3','theirs','a',2,'pending','cs1','dev')",
            [],
        )
        .unwrap();
        let h = put_object(&conn, "THEIRS\n").unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('cs3','s.txt',NULL,?1)",
            params![h],
        )
        .unwrap();
        commit_one(&conn, "cs3").unwrap();

        let before_head = branch_head(&conn, "main").unwrap();
        let out = merge_branch(&conn, &tmp, "dev", "main", "a").unwrap();

        assert_eq!(out.conflicts, vec!["s.txt".to_string()], "must report the conflict");
        assert_eq!(branch_head(&conn, "main").unwrap(), before_head, "head must not move");
        assert!(tmp.join("s.txt.theirs").exists(), "theirs must be preserved on disk");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn changing_branch_never_orphans_a_changeset() {
        let conn = mem_db();
        commit_files(&conn, "cs1", "one", &[("a.txt", "a\n")]);
        commit_files(&conn, "cs2", "two", &[("b.txt", "b\n")]);
        let r = crate::store::reachable_from(&conn, "main").unwrap();
        assert!(r.contains(&"cs1".to_string()));
        assert!(r.contains(&"cs2".to_string()));
        let parent: Option<String> = conn
            .query_row("SELECT parent FROM changesets WHERE id='cs2'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(parent.as_deref(), Some("cs1"));
    }
}
