//! Operation log and undo.
//!
//! Every command that moves a ref records a snapshot of all refs afterwards.
//! Undo restores the previous snapshot. Objects never move, so this is cheap:
//! no content is copied, only the pointer table.

use rusqlite::{params, Connection, OptionalExtension};

use crate::{bad, now, ZResult};

/// Snapshot of every ref the log cares about.
pub struct RepoSnapshot {
    pub branches: Vec<(String, Option<String>)>,
    pub current_branch: String,
    pub tags: Vec<(String, String)>,
}

impl RepoSnapshot {
    pub fn capture(conn: &Connection) -> ZResult<RepoSnapshot> {
        let branches: Vec<(String, Option<String>)> = {
            let mut stmt = conn.prepare("SELECT name, head FROM branches ORDER BY name")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        let tags: Vec<(String, String)> = {
            let mut stmt = conn.prepare("SELECT name, changeset_id FROM tags ORDER BY name")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        let current_branch = crate::store::current_branch(conn)?;
        Ok(RepoSnapshot { branches, current_branch, tags })
    }

    pub fn restore(&self, conn: &Connection) -> ZResult<()> {
        for (name, head) in &self.branches {
            conn.execute(
                "INSERT INTO branches(name, head, created_at, from_branch) VALUES(?1, ?2, ?3, NULL)
                 ON CONFLICT(name) DO UPDATE SET head = excluded.head",
                params![name, head, now()],
            )?;
            // Undo moves a head, so the derived tree has to follow. Missing this
            // made `fsck` report a stale tree after any undo, and `status` then
            // reported already-committed files as new.
            crate::store::refresh_branch_tree(conn, name)?;
        }
        // Branches created after the snapshot are removed.
        let keep: Vec<String> = self.branches.iter().map(|(n, _)| n.clone()).collect();
        let existing: Vec<String> = {
            let mut stmt = conn.prepare("SELECT name FROM branches")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        for name in existing {
            if !keep.contains(&name) {
                conn.execute("DELETE FROM branches WHERE name = ?1", params![name])?;
                conn.execute(
                    "DELETE FROM branch_tree WHERE branch = ?1",
                    params![name],
                )?;
            }
        }
        conn.execute("DELETE FROM tags", [])?;
        for (name, cs) in &self.tags {
            conn.execute(
                "INSERT INTO tags(name, changeset_id, message, created_at) VALUES(?1, ?2, NULL, ?3)",
                params![name, cs, now()],
            )?;
        }
        crate::store::set_state(conn, "current_branch", &self.current_branch)?;
        Ok(())
    }

    fn encode(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("B\t{}\n", self.current_branch));
        for (n, h) in &self.branches {
            s.push_str(&format!("R\t{}\t{}\n", n, h.clone().unwrap_or_else(|| "-".into())));
        }
        for (n, c) in &self.tags {
            s.push_str(&format!("T\t{}\t{}\n", n, c));
        }
        s
    }

    fn decode(s: &str) -> ZResult<RepoSnapshot> {
        let mut snap = RepoSnapshot { branches: Vec::new(), current_branch: "main".into(), tags: Vec::new() };
        for line in s.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            match f.as_slice() {
                ["B", b] => snap.current_branch = b.to_string(),
                ["R", n, h] => snap
                    .branches
                    .push((n.to_string(), if *h == "-" { None } else { Some(h.to_string()) })),
                ["T", n, c] => snap.tags.push((n.to_string(), c.to_string())),
                _ => {}
            }
        }
        Ok(snap)
    }
}

/// Record an operation. Call AFTER the mutation, with the resulting state.
pub fn record(conn: &Connection, kind: &str, target: Option<&str>, detail: Option<&str>) -> ZResult<i64> {
    let snap = RepoSnapshot::capture(conn)?;
    let parent: Option<i64> = conn
        .query_row("SELECT MAX(id) FROM operations", [], |r| r.get::<_, Option<i64>>(0))
        .optional()?
        .flatten();
    conn.execute(
        "INSERT INTO operations(parent_op, kind, target, detail, snapshot, created_at)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
        params![parent, kind, target, detail, snap.encode(), now()],
    )?;
    Ok(conn.last_insert_rowid())
}

pub struct Operation {
    pub id: i64,
    pub kind: String,
    pub target: Option<String>,
    pub detail: Option<String>,
    pub created_at: i64,
}

pub fn list(conn: &Connection, limit: usize) -> ZResult<Vec<Operation>> {
    let mut stmt = conn.prepare(
        "SELECT id, kind, target, detail, created_at FROM operations
         ORDER BY id DESC LIMIT ?1",
    )?;
    let rows = stmt
        .query_map(params![limit as i64], |r| {
            Ok(Operation {
                id: r.get(0)?,
                kind: r.get(1)?,
                target: r.get(2)?,
                detail: r.get(3)?,
                created_at: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Undo the most recent operation. Returns a description of what was undone.
///
/// The snapshot of the *previous* operation is what gets restored, so a chain of
/// `undo` calls walks backwards through history one step at a time.
pub fn undo(conn: &Connection) -> ZResult<String> {
    let last: Option<(i64, String, Option<String>, String)> = conn
        .query_row(
            "SELECT id, kind, target, snapshot FROM operations ORDER BY id DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;

    let Some((id, kind, target, _snap)) = last else {
        return Err(bad("nothing to undo: no operations recorded yet"));
    };

    // Restore the state recorded by the operation BEFORE this one.
    let prev_snapshot: Option<String> = conn
        .query_row(
            "SELECT snapshot FROM operations WHERE id < ?1 ORDER BY id DESC LIMIT 1",
            params![id],
            |r| r.get::<_, String>(0),
        )
        .optional()?;

    let tx = conn.unchecked_transaction()?;
    match prev_snapshot {
        Some(s) => RepoSnapshot::decode(&s)?.restore(&tx)?,
        None => {
            // Undoing the very first operation: go back to an empty repo.
            let empty = RepoSnapshot { branches: vec![("main".into(), None)], current_branch: "main".into(), tags: Vec::new() };
            empty.restore(&tx)?;
        }
    }
    // The undone operation leaves the log so a second undo walks further back.
    tx.execute("DELETE FROM operations WHERE id = ?1", params![id])?;
    tx.commit()?;

    let what = match &target {
        Some(t) => format!("{} {}", kind, t),
        None => kind.clone(),
    };
    Ok(format!("undid {} (operation {})", what, id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::mem_db;

    #[test]
    fn snapshot_roundtrips_through_encode() {
        let snap = RepoSnapshot {
            branches: vec![("main".into(), Some("cs1".into())), ("dev".into(), None)],
            current_branch: "dev".into(),
            tags: vec![("v1.0".into(), "cs1".into())],
        };
        let back = RepoSnapshot::decode(&snap.encode()).unwrap();
        assert_eq!(back.current_branch, "dev");
        assert_eq!(back.branches.len(), 2);
        assert_eq!(back.tags, vec![("v1.0".to_string(), "cs1".to_string())]);
    }

    #[test]
    fn record_and_undo_restores_the_previous_head() {
        let conn = mem_db();
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('cs1','m','a',1,'committed',NULL,'main')",
            [],
        )
        .unwrap();

        record(&conn, "commit", Some("cs1"), None).unwrap();
        conn.execute("UPDATE branches SET head='cs1' WHERE name='main'", []).unwrap();
        record(&conn, "commit", Some("cs1"), None).unwrap();

        assert_eq!(crate::store::branch_head(&conn, "main").unwrap().as_deref(), Some("cs1"));

        let msg = undo(&conn).unwrap();
        assert!(msg.contains("commit"));

        // After undoing the second op we are back to the state the first op left.
        assert_eq!(crate::store::branch_head(&conn, "main").unwrap(), None);

        undo(&conn).unwrap();
        assert_eq!(crate::store::branch_head(&conn, "main").unwrap(), None);
    }

    #[test]
    fn undo_without_operations_is_an_error() {
        let conn = mem_db();
        let err = undo(&conn).unwrap_err().to_string();
        assert!(err.contains("nothing to undo"), "got: {}", err);
    }

    #[test]
    fn undo_restores_branch_creation() {
        let conn = mem_db();
        record(&conn, "branch create", Some("dev"), None).unwrap();
        conn.execute(
            "INSERT INTO branches(name,head,created_at,from_branch) VALUES('dev',NULL,0,'main')",
            [],
        )
        .unwrap();
        record(&conn, "branch create", Some("dev"), None).unwrap();

        undo(&conn).unwrap();
        let exists: Option<String> = conn
            .query_row("SELECT name FROM branches WHERE name='dev'", [], |r| r.get(0))
            .optional()
            .unwrap();
        assert_eq!(exists, None, "a branch created after the snapshot is removed by undo");
    }

    #[test]
    fn operations_are_listed_newest_first() {
        let conn = mem_db();
        record(&conn, "commit", Some("cs1"), None).unwrap();
        record(&conn, "branch create", Some("dev"), None).unwrap();
        let ops = list(&conn, 10).unwrap();
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].kind, "branch create");
        assert_eq!(ops[1].kind, "commit");
    }
}
