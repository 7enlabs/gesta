//! One call that answers where this session stands.
//!
//! An agent starting work has to know what it inherited: which branch, whether a rebase or
//! merge stopped half way, what is stashed, what is uncommitted, what is pending. Asking that
//! with four commands costs four round trips and four blocks of output to hold in context. This
//! puts the answer in one small object, and every field is something a decision depends on.

use crate::json::Json;
use crate::ZResult;
use rusqlite::Connection;
use std::path::Path;

/// What to do next, stated as one of a few known states.
///
/// A free text summary would be read once and forgotten. A named state can be branched on by a
/// program, which is the difference between an agent that checks and an agent that guesses.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Next {
    /// A rebase stopped on a conflict. It must be resolved or aborted before anything else.
    FinishRebase,
    /// Tracked files differ from the head. Commit them or they are lost on the next switch.
    CommitWork,
    /// A changeset was snapshotted but never committed, and it still matches the disk.
    CommitPending,
    /// Nothing to do: the tree matches the head and no operation is half finished.
    Ready,
}

impl Next {
    pub fn as_str(self) -> &'static str {
        match self {
            Next::FinishRebase => "finish-rebase",
            Next::CommitWork => "commit-work",
            Next::CommitPending => "commit-pending",
            Next::Ready => "ready",
        }
    }

    /// What an agent should do, in the words it needs. Empty when nothing is owed.
    pub fn advice(self) -> &'static str {
        match self {
            Next::FinishRebase => {
                "a rebase stopped on a conflict: resolve the files, then `gesta rebase --continue`, or `gesta rebase --abort` to put the branch back"
            }
            Next::CommitWork => {
                "the working tree differs from the head: `gesta changeset create` then `gesta changeset commit`, or `gesta stash save` to set it aside"
            }
            Next::CommitPending => {
                "a pending changeset describes the disk: `gesta changeset commit <id>`, or discard it"
            }
            Next::Ready => "",
        }
    }
}

/// Everything a session needs to know before its first edit.
pub struct Brief {
    pub branch: String,
    pub head: Option<String>,
    pub next: Next,
    pub pending: Vec<String>,
    /// Changesets a stopped rebase still has to replay.
    pub rebase_pending: usize,
    pub stashes: usize,
    pub conflicts: Vec<String>,
    /// Files that differ from the head, by kind.
    pub changed: usize,
    /// Stale pending snapshots, which a commit would record wrongly.
    pub stale: Vec<String>,
}

pub fn brief(conn: &Connection, root: &Path) -> ZResult<Brief> {
    let branch = crate::store::current_branch(conn)?;
    let head = crate::store::branch_head(conn, &branch)?;

    let pending: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT id FROM changesets WHERE status = 'pending' ORDER BY created_at, rowid",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let rebase_pending = crate::rebase::load(conn)?.map(|s| s.pending.len()).unwrap_or(0);
    let stashes = crate::stash::list(conn)?.len();

    let delta = crate::repo::diff_worktree(conn, root, &branch)?;
    let changed = delta.added.len() + delta.modified.len() + delta.deleted.len();
    // A `.theirs` file left beside a path is what a stopped merge writes, so its presence is
    // the honest signal for an unresolved conflict. The delta does not carry one.
    let conflicts: Vec<String> = delta
        .modified
        .iter()
        .chain(delta.added.iter())
        .filter(|p| root.join(format!("{}.theirs", p)).exists())
        .cloned()
        .collect();

    // A stale snapshot is one whose recorded content no longer matches the disk, which is what
    // a lost update looks like from the outside.
    let mut stale = Vec::new();
    // Paths a live snapshot already describes. A snapshot that covers the whole working diff is
    // the thing to commit, not something to snapshot again over the top of.
    let mut covered: std::collections::HashSet<String> = std::collections::HashSet::new();
    for id in &pending {
        let mut stmt = conn.prepare(
            "SELECT cf.path, cf.new_hash FROM changeset_files cf WHERE cf.changeset_id = ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })?;
        let mut is_stale = false;
        for row in rows {
            let (path, recorded) = row?;
            let on_disk = std::fs::read(root.join(&path))
                .ok()
                .map(|b| crate::store::hash_content(&String::from_utf8_lossy(&b)));
            if recorded != on_disk {
                is_stale = true;
            } else {
                covered.insert(path);
            }
        }
        if is_stale {
            stale.push(id.clone());
        }
    }

    // Work the live snapshots do not describe: a new path, or a file that moved after its
    // snapshot. Only when this is empty does committing the snapshot finish the work.
    let uncovered = delta
        .added
        .iter()
        .chain(delta.modified.iter())
        .chain(delta.deleted.iter())
        .filter(|p| !covered.contains(*p))
        .count();

    let live_pending = pending.iter().any(|id| !stale.contains(id));
    let next = if rebase_pending > 0 {
        Next::FinishRebase
    } else if live_pending && uncovered == 0 {
        Next::CommitPending
    } else if changed > 0 {
        Next::CommitWork
    } else if live_pending {
        Next::CommitPending
    } else {
        Next::Ready
    };

    Ok(Brief {
        branch,
        head,
        next,
        pending,
        rebase_pending,
        stashes,
        conflicts,
        changed,
        stale,
    })
}

pub fn to_json(b: &Brief) -> Json {
    Json::obj(vec![
        ("branch", Json::str(b.branch.clone())),
        (
            "head",
            match &b.head {
                Some(h) => Json::str(h.clone()),
                None => Json::Null,
            },
        ),
        ("next", Json::str(b.next.as_str())),
        ("advice", Json::str(b.next.advice())),
        (
            "pending",
            Json::arr(b.pending.iter().map(|s| Json::str(s.clone())).collect()),
        ),
        ("rebase_pending", Json::int(b.rebase_pending as i64)),
        ("stashes", Json::int(b.stashes as i64)),
        (
            "conflicts",
            Json::arr(b.conflicts.iter().map(|s| Json::str(s.clone())).collect()),
        ),
        ("changed", Json::int(b.changed as i64)),
        (
            "stale",
            Json::arr(b.stale.iter().map(|s| Json::str(s.clone())).collect()),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every test gets its own directory. Using the process id made all of them share one, so
    /// tests running in threads clobbered each other's files and four of them failed at once
    /// for a reason that had nothing to do with the code under test.
    fn repo_with_one_commit(name: &str) -> (rusqlite::Connection, std::path::PathBuf) {
        let conn = crate::store::tests::mem_db();
        let dir = std::env::temp_dir().join(format!("gesta-brief-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        crate::repo::tests::commit_files(&conn, "b1", "base", &[("a.txt", "one\n")]);
        crate::store::set_branch_head(&conn, "main", "b1").unwrap();
        crate::store::set_state(&conn, "current_branch", "main").unwrap();
        (conn, dir)
    }

    #[test]
    fn a_clean_tree_is_ready() {
        let (conn, dir) = repo_with_one_commit("clean");
        let b = brief(&conn, &dir).unwrap();
        assert_eq!(b.next, Next::Ready);
        assert_eq!(b.changed, 0);
        assert!(b.next.advice().is_empty());
        assert_eq!(b.head.as_deref(), Some("b1"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_edit_on_disk_asks_for_a_commit() {
        let (conn, dir) = repo_with_one_commit("edited");
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        let b = brief(&conn, &dir).unwrap();
        assert_eq!(b.next, Next::CommitWork);
        assert_eq!(b.changed, 1);
        assert!(b.next.advice().contains("changeset create"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pending_snapshot_that_still_matches_the_disk_asks_to_commit_it() {
        let (conn, dir) = repo_with_one_commit("snapshot");
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        crate::repo::create_changeset(&conn, &dir, "snap", "a").unwrap();
        let b = brief(&conn, &dir).unwrap();
        assert_eq!(b.pending.len(), 1);
        assert_eq!(b.next, Next::CommitPending, "a matching snapshot is worth committing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_snapshot_the_disk_has_moved_past_is_marked_stale() {
        let (conn, dir) = repo_with_one_commit("stale");
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        crate::repo::create_changeset(&conn, &dir, "snap", "a").unwrap();
        std::fs::write(dir.join("a.txt"), "three\n").unwrap();
        let b = brief(&conn, &dir).unwrap();
        assert_eq!(b.stale.len(), 1, "the stale snapshot was not flagged");
        assert_eq!(b.next, Next::CommitWork, "new work outranks a stale snapshot");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stopped_rebase_outranks_everything_else() {
        let (conn, dir) = repo_with_one_commit("rebase");
        // The rebase table is made on first use, so touch it before writing a row by hand.
        crate::rebase::load(&conn).unwrap();
        conn.execute(
            "INSERT INTO rebase(id, branch, onto, pending, current, done, original_head, created_at)
             VALUES(1, 'side', 'main', '[\"x\",\"y\"]', 'x', '[]', 'b1', 1)",
            [],
        )
        .unwrap();
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        let b = brief(&conn, &dir).unwrap();
        assert_eq!(b.next, Next::FinishRebase);
        assert_eq!(b.rebase_pending, 2);
        assert!(b.next.advice().contains("rebase --continue"));
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn a_snapshot_that_covers_the_diff_is_committed_before_snapshotting_again() {
        let (conn, dir) = repo_with_one_commit("covers");
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        crate::repo::create_changeset(&conn, &dir, "snap", "a").unwrap();
        let b = brief(&conn, &dir).unwrap();
        assert_eq!(
            b.next,
            Next::CommitPending,
            "committing the snapshot is the next step, not making another one"
        );
        assert!(b.next.advice().contains("commit"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_work_beyond_a_snapshot_asks_for_a_commit_not_a_lost_edit() {
        let (conn, dir) = repo_with_one_commit("beyond");
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        crate::repo::create_changeset(&conn, &dir, "snap", "a").unwrap();
        // A file the snapshot never saw.
        std::fs::write(dir.join("b.txt"), "new\n").unwrap();
        let b = brief(&conn, &dir).unwrap();
        assert_eq!(
            b.next,
            Next::CommitWork,
            "the snapshot does not describe b.txt, so committing it would drop the file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
