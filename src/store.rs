//! SQLite storage: schema, object store, repo state, branch reachability.

use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

use crate::{bad, now, ZResult, ZF_DB, ZF_DIR};

pub fn db_path(root: &Path) -> PathBuf {
    root.join(ZF_DIR).join(ZF_DB)
}

pub fn open_db(root: &Path) -> ZResult<Connection> {
    let conn = Connection::open(db_path(root))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;")?;
    Ok(conn)
}

pub fn init_schema(conn: &Connection) -> ZResult<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS objects (
            hash       TEXT PRIMARY KEY,
            content    TEXT NOT NULL,
            size       INTEGER NOT NULL,
            created_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS changesets (
            id         TEXT PRIMARY KEY,
            message    TEXT NOT NULL,
            author     TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            status     TEXT NOT NULL CHECK (status IN ('pending','committed')),
            parent     TEXT,
            branch     TEXT NOT NULL DEFAULT 'main'
        );
        CREATE TABLE IF NOT EXISTS changeset_files (
            changeset_id TEXT NOT NULL,
            path         TEXT NOT NULL,
            old_hash     TEXT,
            new_hash     TEXT NOT NULL,
            PRIMARY KEY (changeset_id, path)
        );
        CREATE TABLE IF NOT EXISTS file_versions (
            path         TEXT NOT NULL,
            version      INTEGER NOT NULL,
            hash         TEXT NOT NULL,
            changeset_id TEXT NOT NULL,
            created_at   INTEGER NOT NULL,
            PRIMARY KEY (path, version)
        );
        CREATE TABLE IF NOT EXISTS blame (
            path         TEXT NOT NULL,
            line_no      INTEGER NOT NULL,
            changeset_id TEXT NOT NULL,
            author       TEXT NOT NULL,
            committed_at INTEGER NOT NULL,
            PRIMARY KEY (path, line_no)
        );
        CREATE TABLE IF NOT EXISTS working_copy (
            path       TEXT PRIMARY KEY,
            content    TEXT NOT NULL,
            hash       TEXT NOT NULL,
            updated_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS branches (
            name        TEXT PRIMARY KEY,
            head        TEXT,
            created_at  INTEGER NOT NULL,
            from_branch TEXT
        );
        CREATE TABLE IF NOT EXISTS repo_state (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_cf_changeset ON changeset_files(changeset_id);
        CREATE INDEX IF NOT EXISTS idx_fv_path ON file_versions(path);
        CREATE INDEX IF NOT EXISTS idx_cs_status ON changesets(status, created_at);
        "#,
    )?;
    Ok(())
}

pub fn hash_content(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    hex::encode(h.finalize())
}

pub fn get_state(conn: &Connection, key: &str) -> ZResult<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT value FROM repo_state WHERE key = ?1",
            params![key],
            |row| row.get::<_, String>(0),
        )
        .optional()?)
}

pub fn set_state(conn: &Connection, key: &str, value: &str) -> ZResult<()> {
    conn.execute(
        "INSERT INTO repo_state(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

pub fn current_branch(conn: &Connection) -> ZResult<String> {
    Ok(get_state(conn, "current_branch")?.unwrap_or_else(|| "main".to_string()))
}

/// Insert content into the object store. Idempotent: identical content maps to
/// one row.
pub fn put_object(conn: &Connection, content: &str) -> ZResult<String> {
    let h = hash_content(content);
    conn.execute(
        "INSERT OR IGNORE INTO objects(hash, content, size, created_at) VALUES(?1, ?2, ?3, ?4)",
        params![h, content, content.len() as i64, now()],
    )?;
    Ok(h)
}

pub fn get_object(conn: &Connection, hash: &str) -> ZResult<String> {
    conn.query_row(
        "SELECT content FROM objects WHERE hash = ?1",
        params![hash],
        |row| row.get::<_, String>(0),
    )
    .optional()?
    .ok_or_else(|| bad(format!("object {} not found (store may be corrupted)", hash)))
}

/// Every changeset id reachable from `branch`'s head, newest first.
/// Returns an empty vec for an empty branch.
pub fn reachable_from(conn: &Connection, branch: &str) -> ZResult<Vec<String>> {
    let head: Option<String> = conn
        .query_row("SELECT head FROM branches WHERE name = ?1", params![branch], |r| {
            r.get::<_, Option<String>>(0)
        })
        .optional()?
        .ok_or_else(|| bad(format!("no such branch: {}", branch)))?;

    let mut chain = Vec::new();
    let mut cur = head;
    let mut guard = 0usize;
    while let Some(id) = cur {
        // A cycle would otherwise spin forever.
        if chain.contains(&id) {
            return Err(bad(format!("branch {} has a cycle at changeset {}", branch, id)));
        }
        chain.push(id.clone());
        cur = conn
            .query_row(
                "SELECT parent FROM changesets WHERE id = ?1",
                params![id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        guard += 1;
        if guard > 1_000_000 {
            return Err(bad("changeset chain too long; database likely corrupted"));
        }
    }
    Ok(chain)
}

/// Latest committed content hash for `path` on `branch`, following the parent
/// chain rather than taking the globally newest version.
pub fn latest_hash(conn: &Connection, path: &str, branch: &str) -> ZResult<Option<String>> {
    let chain = reachable_from(conn, branch)?;
    if chain.is_empty() {
        return Ok(None);
    }
    let mut stmt = conn.prepare(
        "SELECT hash FROM file_versions WHERE path = ?1 AND changeset_id = ?2 ORDER BY version DESC LIMIT 1",
    )?;
    for id in chain {
        let h: Option<String> = stmt
            .query_row(params![path, id], |r| r.get::<_, String>(0))
            .optional()?;
        if h.is_some() {
            return Ok(h);
        }
    }
    Ok(None)
}

/// Branch head, or `None` for an empty branch.
pub fn branch_head(conn: &Connection, branch: &str) -> ZResult<Option<String>> {
    conn.query_row(
        "SELECT head FROM branches WHERE name = ?1",
        params![branch],
        |r| r.get::<_, Option<String>>(0),
    )
    .optional()?
    .ok_or_else(|| bad(format!("no such branch: {}", branch)))
}

/// Allocate the next changeset sequence number atomically.
///
/// A read-then-write counter races: two concurrent agents both read N, both
/// write N+1 and then hash the same inputs, so one insert hits the primary key.
/// The increment has to happen inside SQLite itself, which is what RETURNING
/// gives us here.
pub fn next_changeset_seq(conn: &Connection) -> ZResult<u64> {
    let v: i64 = conn.query_row(
        "INSERT INTO repo_state(key, value) VALUES('changeset_seq', '1')
         ON CONFLICT(key) DO UPDATE SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)
         RETURNING CAST(value AS INTEGER)",
        [],
        |r| r.get(0),
    )?;
    Ok(v.max(1) as u64)
}

/// Allocate a changeset id that is not already taken.
///
/// The id mixes a persisted monotonic sequence with message, author and
/// timestamp. Without the sequence, two agents writing the same message in the
/// same second would produce the same id and the second insert would be lost.
/// Collisions are still checked against the table before returning.
pub fn new_changeset_id(conn: &Connection, message: &str, author: &str) -> ZResult<String> {
    let seq = next_changeset_seq(conn)?;

    for attempt in 0..16u32 {
        let seed = format!("{}|{}|{}|{}|{}", message, author, now(), seq, attempt);
        let id = format!("cs{}", &hash_content(&seed)[..12]);
        let taken: Option<String> = conn
            .query_row("SELECT id FROM changesets WHERE id = ?1", params![id], |r| r.get(0))
            .optional()?;
        if taken.is_none() {
            return Ok(id);
        }
    }
    Err(bad("could not allocate a unique changeset id after 16 attempts"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn mem_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO branches(name, head, created_at, from_branch) VALUES('main', NULL, 0, NULL)",
            [],
        )
        .unwrap();
        conn
    }

    #[test]
    fn hash_is_stable_and_distinct() {
        assert_eq!(hash_content("abc"), hash_content("abc"));
        assert_ne!(hash_content("abc"), hash_content("abd"));
        assert_eq!(hash_content("").len(), 64);
    }

    #[test]
    fn object_store_roundtrip_and_dedup() {
        let conn = mem_db();
        let a = put_object(&conn, "hello").unwrap();
        let b = put_object(&conn, "hello").unwrap();
        assert_eq!(a, b);
        assert_eq!(get_object(&conn, &a).unwrap(), "hello");
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM objects", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "duplicate content must not create a second object");
    }

    #[test]
    fn missing_object_is_an_error_not_empty_string() {
        let conn = mem_db();
        let err = get_object(&conn, "deadbeef").unwrap_err().to_string();
        assert!(err.contains("not found"), "got: {}", err);
    }

    #[test]
    fn repo_state_upsert() {
        let conn = mem_db();
        set_state(&conn, "current_branch", "main").unwrap();
        assert_eq!(current_branch(&conn).unwrap(), "main");
        set_state(&conn, "current_branch", "dev").unwrap();
        assert_eq!(current_branch(&conn).unwrap(), "dev");
    }

    #[test]
    fn unknown_branch_is_an_error() {
        let conn = mem_db();
        assert!(reachable_from(&conn, "nope").is_err());
        assert!(branch_head(&conn, "nope").is_err());
    }

    #[test]
    fn ids_are_unique_across_calls() {
        let conn = mem_db();
        let a = new_changeset_id(&conn, "m", "a").unwrap();
        // Same message/author/second must still produce a fresh id.
        let b = new_changeset_id(&conn, "m", "a").unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), 14, "cs + 12 hex");
    }

    #[test]
    fn latest_hash_follows_reachability_not_globally_newest() {
        let conn = mem_db();
        let h1 = put_object(&conn, "v1\n").unwrap();
        let h2 = put_object(&conn, "v2\n").unwrap();

        for (id, parent, hash, ver) in [("cs1", None, &h1, 1i64), ("cs2", Some("cs1"), &h2, 2)] {
            conn.execute(
                "INSERT INTO changesets(id, message, author, created_at, status, parent, branch)
                 VALUES(?1, 'm', 'a', 1, 'committed', ?2, 'main')",
                params![id, parent],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO file_versions(path, version, hash, changeset_id, created_at)
                 VALUES('f.txt', ?1, ?2, ?3, 1)",
                params![ver, hash, id],
            )
            .unwrap();
        }

        // main at cs2 -> sees v2
        conn.execute("UPDATE branches SET head='cs2' WHERE name='main'", []).unwrap();
        assert_eq!(latest_hash(&conn, "f.txt", "main").unwrap().as_deref(), Some(h2.as_str()));

        // branch pinned at cs1 -> sees v1 even though v2 exists
        conn.execute(
            "INSERT INTO branches(name, head, created_at, from_branch) VALUES('pinned', 'cs1', 0, 'main')",
            [],
        )
        .unwrap();
        assert_eq!(latest_hash(&conn, "f.txt", "pinned").unwrap().as_deref(), Some(h1.as_str()));

        assert_eq!(latest_hash(&conn, "absent.txt", "main").unwrap(), None);
    }
}
