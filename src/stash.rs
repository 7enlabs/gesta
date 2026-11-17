//! Saving work in progress without committing it.
//!
//! A stash holds the difference between the working tree and the branch head as
//! object hashes, then puts the working tree back to the head. Nothing about the
//! branch, its history or its blame changes, which is the whole point: a stash is
//! not a commit and must not look like one.
//!
//! Content goes through the same object store as a commit, so a stash costs one
//! copy of the bytes that already exist and nothing more.

use std::path::Path;

use rusqlite::params;
use rusqlite::Connection;

use crate::repo;
use crate::store::{get_object, put_object_against};
use crate::now;
use crate::worktree::{read_file, ReadOutcome};
use crate::{bad, ZResult};

/// One stash: what was saved, when, and under what message.
pub struct Stash {
    pub id: i64,
    pub message: String,
    pub created_at: i64,
    pub files: Vec<(String, String)>,
}

fn init_schema(conn: &Connection) -> ZResult<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS stashes (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            message    TEXT NOT NULL,
            author     TEXT NOT NULL,
            created_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS stash_files (
            stash_id INTEGER NOT NULL,
            path     TEXT NOT NULL,
            hash     TEXT NOT NULL,
            PRIMARY KEY (stash_id, path)
        );",
    )?;
    Ok(())
}

/// Save the working tree and reset it to the branch head.
///
/// Returns the number of files saved, or zero when there was nothing to save.
pub fn save(conn: &Connection, root: &Path, message: &str, author: &str) -> ZResult<(i64, usize)> {
    init_schema(conn)?;
    let branch = crate::store::current_branch(conn)?;
    let head = crate::store::branch_head(conn, &branch)?;
    let committed = repo::committed_tree(conn, &branch)?;
    let d = repo::diff_worktree(conn, root, &branch)?;

    let mut paths: Vec<String> = Vec::new();
    paths.extend(d.added.iter().cloned());
    paths.extend(d.modified.iter().cloned());
    paths.sort();

    if paths.is_empty() {
        return Ok((0, 0));
    }

    // One transaction, so a failure half way does not leave a stash pointing at
    // content that was never stored.
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO stashes(message, author, created_at) VALUES(?1, ?2, ?3)",
        params![message, author, now()],
    )?;
    let stash_id = tx.last_insert_rowid();

    let mut saved = 0usize;
    for rel in &paths {
        let full = root.join(rel);
        let content = match read_file(&full) {
            ReadOutcome::Text(c) => c,
            // A binary file cannot be stashed as text. Reporting it is better
            // than silently dropping it from the stash.
            ReadOutcome::Binary => {
                return Err(bad(format!(
                    "cannot stash {}: binary files are not tracked",
                    rel
                )))
            }
            ReadOutcome::Unreadable(e) => {
                return Err(bad(format!("cannot read {}: {}", rel, e)))
            }
        };
        // Base the delta on whatever this path held at head, so a stash of a
        // slightly edited large file costs a delta rather than a whole copy.
        let base = committed.get(rel).map(|s| s.as_str());
        let hash = put_object_against(&tx, &content, base)?;
        tx.execute(
            "INSERT INTO stash_files(stash_id, path, hash) VALUES(?1, ?2, ?3)",
            params![stash_id, rel, hash],
        )?;
        saved += 1;
    }
    tx.commit()?;

    // Only after the stash is durable does the working tree go back to head.
    // The other order loses the work if the save fails.
    let _ = head;
    repo::restore_worktree(conn, root, &branch)?;

    // restore_worktree only removes paths that were tracked at some point, and
    // a file added since the last commit was never tracked. Those are exactly
    // the files this stash just saved, so leaving them on disk would duplicate
    // the work: the stash holds a copy and the working tree still shows it as
    // new. Deleting them here is safe because the content is already stored.
    for rel in &d.added {
        let target = root.join(rel);
        if target.is_file() {
            std::fs::remove_file(&target)?;
        }
    }

    Ok((stash_id, saved))
}

fn load(conn: &Connection, id: Option<i64>) -> ZResult<Stash> {
    init_schema(conn)?;
    let row: Option<(i64, String, i64)> = match id {
        Some(id) => conn
            .query_row(
                "SELECT id, message, created_at FROM stashes WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .ok(),
        None => conn
            .query_row(
                "SELECT id, message, created_at FROM stashes ORDER BY id DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .ok(),
    };
    let (id, message, created_at) =
        row.ok_or_else(|| bad("no stash to apply".to_string()))?;

    let files = {
        let mut stmt =
            conn.prepare("SELECT path, hash FROM stash_files WHERE stash_id = ?1 ORDER BY path")?;
        let rows = stmt.query_map(params![id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    Ok(Stash {
        id,
        message,
        created_at,
        files,
    })
}

pub fn list(conn: &Connection) -> ZResult<Vec<Stash>> {
    init_schema(conn)?;
    let ids: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM stashes ORDER BY id DESC")?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut out = Vec::new();
    for id in ids {
        out.push(load(conn, Some(id))?);
    }
    Ok(out)
}

/// Write a stash back onto the working tree.
///
/// A stash does not overwrite a path that the working tree has changed since, for
/// the same reason a merge refuses: the write would destroy work with no record
/// of it. The stash is kept in that case and the caller gets the list of paths
/// that blocked it.
pub fn pop(conn: &Connection, root: &Path, id: Option<i64>) -> ZResult<(Stash, usize, Vec<String>)> {
    let stash = load(conn, id)?;
    let branch = crate::store::current_branch(conn)?;
    let d = repo::diff_worktree(conn, root, &branch)?;
    let dirty: std::collections::HashSet<&String> = d
        .added
        .iter()
        .chain(d.modified.iter())
        .chain(d.deleted.iter())
        .collect();

    let mut blocked: Vec<String> = stash
        .files
        .iter()
        .map(|(p, _)| p.clone())
        .filter(|p| dirty.contains(p))
        .collect();
    blocked.sort();
    if !blocked.is_empty() {
        return Ok((stash, 0, blocked));
    }

    let mut written = 0usize;
    for (rel, hash) in &stash.files {
        let content = get_object(conn, hash)?;
        let target = root.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, content.as_bytes())?;
        written += 1;
    }

    drop_stash(conn, stash.id)?;
    Ok((stash, written, Vec::new()))
}

pub fn drop_stash(conn: &Connection, id: i64) -> ZResult<()> {
    conn.execute("DELETE FROM stash_files WHERE stash_id = ?1", params![id])?;
    conn.execute("DELETE FROM stashes WHERE id = ?1", params![id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::tests::commit_files;
    use crate::store::tests::mem_db;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("gesta-stash-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn repo_with_file(tag: &str) -> std::path::PathBuf {
        let d = tmp(tag);
        std::fs::write(d.join("f.txt"), "base\n").unwrap();
        d
    }

    #[test]
    fn saving_a_stash_empties_the_working_tree() {
        let d = repo_with_file("save");
        let conn = mem_db();
        commit_files(&conn, "c1", "first", &[("f.txt", "base\n")]);

        std::fs::write(d.join("f.txt"), "changed\n").unwrap();
        std::fs::write(d.join("new.txt"), "added\n").unwrap();

        let (id, n) = save(&conn, &d, "wip", "t").unwrap();
        assert_eq!(id, 1);
        assert_eq!(n, 2, "both the changed and the new file are saved");
        assert_eq!(std::fs::read_to_string(d.join("f.txt")).unwrap(), "base\n");
        assert!(!d.join("new.txt").exists(), "an added file is removed too");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn popping_restores_both_the_edit_and_the_new_file() {
        let d = repo_with_file("pop");
        let conn = mem_db();
        commit_files(&conn, "c1", "first", &[("f.txt", "base\n")]);

        std::fs::write(d.join("f.txt"), "edited\n").unwrap();
        std::fs::write(d.join("new.txt"), "added\n").unwrap();
        save(&conn, &d, "wip", "t").unwrap();

        let (_s, written, blocked) = pop(&conn, &d, None).unwrap();
        assert!(blocked.is_empty());
        assert_eq!(written, 2);
        assert_eq!(std::fs::read_to_string(d.join("f.txt")).unwrap(), "edited\n");
        assert_eq!(std::fs::read_to_string(d.join("new.txt")).unwrap(), "added\n");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn popping_refuses_when_the_working_tree_has_changed() {
        // Writing over a locally changed path would destroy work that was never
        // recorded anywhere. The stash stays so the caller can try again.
        let d = repo_with_file("refuse");
        let conn = mem_db();
        commit_files(&conn, "c1", "first", &[("f.txt", "base\n")]);

        std::fs::write(d.join("f.txt"), "stashed\n").unwrap();
        save(&conn, &d, "wip", "t").unwrap();
        std::fs::write(d.join("f.txt"), "local only\n").unwrap();

        let (_s, written, blocked) = pop(&conn, &d, None).unwrap();
        assert_eq!(written, 0, "nothing is written when a path conflicts");
        assert_eq!(blocked, vec!["f.txt".to_string()]);
        assert_eq!(
            std::fs::read_to_string(d.join("f.txt")).unwrap(),
            "local only\n",
            "the local change is untouched"
        );
        assert_eq!(list(&conn).unwrap().len(), 1, "the stash is kept");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn saving_with_nothing_to_save_reports_zero() {
        let d = repo_with_file("empty");
        let conn = mem_db();
        commit_files(&conn, "c1", "first", &[("f.txt", "base\n")]);

        let (id, n) = save(&conn, &d, "wip", "t").unwrap();
        assert_eq!((id, n), (0, 0));
        let _ = std::fs::remove_dir_all(&d);
    }
}
