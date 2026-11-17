//! SQLite storage: schema, compressed object store, refs, graph queries.

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::{bad, now, ZResult, ZF_DB, ZF_DIR};

pub fn db_path(root: &Path) -> PathBuf {
    root.join(ZF_DIR).join(ZF_DB)
}

pub fn open_db(root: &Path) -> ZResult<Connection> {
    let conn = Connection::open(db_path(root))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA foreign_keys=ON;
         PRAGMA busy_timeout=10000;",
    )?;
    Ok(conn)
}

pub fn init_schema(conn: &Connection) -> ZResult<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS objects (
            hash       TEXT PRIMARY KEY,
            content    BLOB NOT NULL,
            size       INTEGER NOT NULL,
            compressed INTEGER NOT NULL DEFAULT 1,
            created_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS changesets (
            id         TEXT PRIMARY KEY,
            message    TEXT NOT NULL,
            author     TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            status     TEXT NOT NULL CHECK (status IN ('pending','committed')),
            parent     TEXT,
            parent2    TEXT,
            branch     TEXT NOT NULL DEFAULT 'main'
        );
        CREATE TABLE IF NOT EXISTS changeset_files (
            changeset_id TEXT NOT NULL,
            path         TEXT NOT NULL,
            old_hash     TEXT,
            new_hash     TEXT,
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
        -- No working_copy table: committed content already lives in `objects`
        -- and duplicating it doubled disk usage for nothing.
        CREATE TABLE IF NOT EXISTS branches (
            name        TEXT PRIMARY KEY,
            head        TEXT,
            created_at  INTEGER NOT NULL,
            from_branch TEXT
        );
        CREATE TABLE IF NOT EXISTS tags (
            name         TEXT PRIMARY KEY,
            changeset_id TEXT NOT NULL,
            message      TEXT,
            created_at   INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS ignore_rules (
            pattern TEXT PRIMARY KEY
        );
        CREATE TABLE IF NOT EXISTS operations (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            parent_op  INTEGER,
            kind       TEXT NOT NULL,
            target     TEXT,
            detail     TEXT,
            snapshot   TEXT NOT NULL,
            created_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS remotes (
            name TEXT PRIMARY KEY,
            url  TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS remote_refs (
            remote       TEXT NOT NULL,
            branch       TEXT NOT NULL,
            changeset_id TEXT NOT NULL,
            updated_at   INTEGER NOT NULL,
            PRIMARY KEY (remote, branch)
        );
        CREATE TABLE IF NOT EXISTS repo_state (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_cf_changeset ON changeset_files(changeset_id);
        CREATE INDEX IF NOT EXISTS idx_fv_path ON file_versions(path);
        CREATE INDEX IF NOT EXISTS idx_cs_status ON changesets(status, created_at);
        CREATE INDEX IF NOT EXISTS idx_cs_parent ON changesets(parent);
        CREATE INDEX IF NOT EXISTS idx_blame_cs ON blame(changeset_id);
        "#,
    )?;
    Ok(())
}

/// Upgrade a database created before v0.3 (uncompressed TEXT objects, no
/// operation log, no tags, no remotes). Returns true when it migrated.
pub fn migrate(conn: &Connection) -> ZResult<bool> {
    let obj_cols = table_columns(conn, "objects")?;
    if obj_cols.is_empty() {
        return Ok(false); // fresh database; init_schema creates the full shape
    }
    if obj_cols.iter().any(|c| c == "compressed") {
        return Ok(false);
    }

    conn.execute_batch("ALTER TABLE objects ADD COLUMN compressed INTEGER NOT NULL DEFAULT 0")?;
    if !table_columns(conn, "changesets")?.iter().any(|c| c == "parent2") {
        conn.execute_batch("ALTER TABLE changesets ADD COLUMN parent2 TEXT")?;
    }
    init_schema(conn)?;
    Ok(true)
}

pub fn table_columns(conn: &Connection, table: &str) -> ZResult<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", table))?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// object store.

pub fn hash_bytes(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

pub fn hash_content(s: &str) -> String {
    hash_bytes(s.as_bytes())
}

fn compress(content: &str) -> ZResult<Vec<u8>> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(content.as_bytes())?;
    Ok(enc.finish()?)
}

fn decompress(blob: &[u8]) -> ZResult<String> {
    let mut dec = ZlibDecoder::new(blob);
    let mut out = String::new();
    dec.read_to_string(&mut out)?;
    Ok(out)
}

/// Insert content into the object store. Idempotent: identical content maps to
/// one row.
///
/// Compression is applied only when it actually saves bytes. A zlib stream
/// carries a fixed header and trailer, so for a short file compressing makes the
/// row bigger than the text itself; small objects are stored raw instead.
pub fn put_object(conn: &Connection, content: &str) -> ZResult<String> {
    let h = hash_content(content);
    let exists: Option<i64> = conn
        .query_row("SELECT 1 FROM objects WHERE hash = ?1", params![h], |r| r.get(0))
        .optional()?;
    if exists.is_some() {
        return Ok(h);
    }

    let raw = content.as_bytes().to_vec();
    let packed = compress(content)?;
    let (blob, compressed) = if packed.len() < raw.len() {
        (packed, 1i64)
    } else {
        (raw, 0i64)
    };

    conn.execute(
        "INSERT OR IGNORE INTO objects(hash, content, size, compressed, created_at)
         VALUES(?1, ?2, ?3, ?4, ?5)",
        params![h, blob, content.len() as i64, compressed, now()],
    )?;
    Ok(h)
}

pub fn get_object(conn: &Connection, hash: &str) -> ZResult<String> {
    let row: Option<(Vec<u8>, i64)> = conn
        .query_row(
            "SELECT content, compressed FROM objects WHERE hash = ?1",
            params![hash],
            |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()?;
    let (blob, compressed) =
        row.ok_or_else(|| bad(format!("object {} not found (store may be corrupted)", hash)))?;
    if compressed == 1 {
        decompress(&blob)
    } else {
        String::from_utf8(blob)
            .map_err(|e| bad(format!("object {} is not valid UTF-8: {}", hash, e)))
    }
}

/// (raw bytes of all content, bytes actually stored) for measuring compression.
pub fn object_store_bytes(conn: &Connection) -> ZResult<(i64, i64)> {
    Ok(conn.query_row(
        "SELECT COALESCE(SUM(size),0), COALESCE(SUM(LENGTH(content)),0) FROM objects",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}

// repo state.

pub fn get_state(conn: &Connection, key: &str) -> ZResult<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM repo_state WHERE key = ?1", params![key], |row| {
            row.get::<_, String>(0)
        })
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

// graph.

fn parents_of(conn: &Connection, id: &str) -> ZResult<(Option<String>, Option<String>)> {
    Ok(conn
        .query_row(
            "SELECT parent, parent2 FROM changesets WHERE id = ?1",
            params![id],
            |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .optional()?
        .unwrap_or((None, None)))
}

/// Every changeset id reachable from `branch`'s head, following both parents so
/// a merge keeps its full history.
pub fn reachable_from(conn: &Connection, branch: &str) -> ZResult<Vec<String>> {
    match branch_head(conn, branch)? {
        Some(head) => Ok(ancestors(conn, &head)?),
        None => Ok(Vec::new()),
    }
}

/// `id` plus all its ancestors, in discovery order (newest first).
pub fn ancestors(conn: &Connection, id: &str) -> ZResult<Vec<String>> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = vec![id.to_string()];
    while let Some(cur) = stack.pop() {
        if !seen.insert(cur.clone()) {
            continue;
        }
        out.push(cur.clone());
        let (p1, p2) = parents_of(conn, &cur)?;
        if let Some(p) = p1 {
            stack.push(p);
        }
        if let Some(p) = p2 {
            stack.push(p);
        }
    }
    Ok(out)
}

/// Best common ancestor of two changesets: the common ancestor that every other
/// common ancestor also reaches. This is what a three-way merge diffs against.
pub fn merge_base(conn: &Connection, a: &str, b: &str) -> ZResult<Option<String>> {
    let anc_a: HashSet<String> = ancestors(conn, a)?.into_iter().collect();
    let common: Vec<String> = ancestors(conn, b)?
        .into_iter()
        .filter(|x| anc_a.contains(x))
        .collect();
    if common.is_empty() {
        return Ok(None);
    }
    if common.len() == 1 {
        return Ok(Some(common[0].clone()));
    }

    let cache: std::collections::HashMap<String, HashSet<String>> = common
        .iter()
        .map(|c| Ok((c.clone(), ancestors(conn, c)?.into_iter().collect())))
        .collect::<ZResult<_>>()?;

    // A merge base is a common ancestor that no other common ancestor reaches.
    let mut best: Option<(String, i64)> = None;
    for cand in &common {
        let reached_by_another = common
            .iter()
            .any(|other| other != cand && cache[other].contains(cand));
        if reached_by_another {
            continue;
        }
        let ts: i64 = conn
            .query_row("SELECT created_at FROM changesets WHERE id = ?1", params![cand], |r| r.get(0))
            .unwrap_or(0);
        if best.as_ref().map(|(_, t)| ts > *t).unwrap_or(true) {
            best = Some((cand.clone(), ts));
        }
    }
    Ok(best.map(|(id, _)| id))
}

/// Committed content hash for `path` introduced by exactly this changeset.
pub fn hash_at(conn: &Connection, path: &str, changeset: &str) -> ZResult<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT hash FROM file_versions WHERE path = ?1 AND changeset_id = ?2
             ORDER BY version DESC LIMIT 1",
            params![path, changeset],
            |r| r.get::<_, String>(0),
        )
        .optional()?)
}

/// Content hash for `path` as of `changeset`, walking its ancestry.
pub fn hash_at_point(conn: &Connection, path: &str, changeset: &str) -> ZResult<Option<String>> {
    let mut seen = HashSet::new();
    let mut stack = vec![changeset.to_string()];
    while let Some(cur) = stack.pop() {
        if !seen.insert(cur.clone()) {
            continue;
        }
        if let Some(h) = hash_at(conn, path, &cur)? {
            return Ok(Some(h));
        }
        let (p1, p2) = parents_of(conn, &cur)?;
        if let Some(p) = p1 {
            stack.push(p);
        }
        if let Some(p) = p2 {
            stack.push(p);
        }
    }
    Ok(None)
}

/// Latest committed content hash for `path` on `branch`.
pub fn latest_hash(conn: &Connection, path: &str, branch: &str) -> ZResult<Option<String>> {
    match branch_head(conn, branch)? {
        Some(head) => hash_at_point(conn, path, &head),
        None => Ok(None),
    }
}

/// Branch head, or `None` for an empty branch.
pub fn branch_head(conn: &Connection, branch: &str) -> ZResult<Option<String>> {
    conn.query_row("SELECT head FROM branches WHERE name = ?1", params![branch], |r| {
        r.get::<_, Option<String>>(0)
    })
    .optional()?
    .ok_or_else(|| bad(format!("no such branch: {}", branch)))
}

// id allocation.

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
    fn object_store_roundtrip_is_compressed_and_deduped() {
        let conn = mem_db();
        let text = "line of text\n".repeat(500);
        let a = put_object(&conn, &text).unwrap();
        let b = put_object(&conn, &text).unwrap();
        assert_eq!(a, b);
        assert_eq!(get_object(&conn, &a).unwrap(), text);

        let n: i64 = conn.query_row("SELECT COUNT(*) FROM objects", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "duplicate content must not create a second object");

        let (raw, stored) = object_store_bytes(&conn).unwrap();
        assert!(
            stored * 3 < raw,
            "repetitive text must compress at least 3x, got {} -> {}",
            raw,
            stored
        );
    }

    #[test]
    fn tiny_objects_are_stored_uncompressed_when_zlib_would_grow_them() {
        let conn = mem_db();
        let tiny = "a\n";
        let h = put_object(&conn, tiny).unwrap();
        assert_eq!(get_object(&conn, &h).unwrap(), tiny);

        let (flag, stored): (i64, i64) = conn
            .query_row(
                "SELECT compressed, LENGTH(content) FROM objects WHERE hash = ?1",
                params![h],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(flag, 0, "a 2-byte object must not be stored compressed");
        assert!(
            stored <= tiny.len() as i64 + 2,
            "stored size {} should not exceed the raw size {}",
            stored,
            tiny.len()
        );
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
        let b = new_changeset_id(&conn, "m", "a").unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), 14, "cs + 12 hex");
    }

    fn add_cs(conn: &Connection, id: &str, p1: Option<&str>, p2: Option<&str>) {
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,parent2,branch)
             VALUES(?1,'m','a',1,'committed',?2,?3,'main')",
            params![id, p1, p2],
        )
        .unwrap();
    }

    #[test]
    fn reachability_follows_both_parents_of_a_merge() {
        let conn = mem_db();
        add_cs(&conn, "cs1", None, None);
        add_cs(&conn, "cs2", Some("cs1"), None);
        add_cs(&conn, "cs3", Some("cs1"), None);
        add_cs(&conn, "cs4", Some("cs2"), Some("cs3"));
        conn.execute("UPDATE branches SET head='cs4' WHERE name='main'", []).unwrap();

        let r = reachable_from(&conn, "main").unwrap();
        for id in ["cs1", "cs2", "cs3", "cs4"] {
            assert!(r.contains(&id.to_string()), "{} must stay reachable, got {:?}", id, r);
        }
    }

    #[test]
    fn merge_base_finds_the_fork_point() {
        let conn = mem_db();
        add_cs(&conn, "cs1", None, None);
        add_cs(&conn, "cs2", Some("cs1"), None);
        add_cs(&conn, "cs3", Some("cs2"), None);
        add_cs(&conn, "cs4", Some("cs2"), None);
        assert_eq!(merge_base(&conn, "cs3", "cs4").unwrap().as_deref(), Some("cs2"));
        assert_eq!(merge_base(&conn, "cs2", "cs3").unwrap().as_deref(), Some("cs2"));
    }

    #[test]
    fn merge_base_of_unrelated_history_is_none() {
        let conn = mem_db();
        add_cs(&conn, "cs1", None, None);
        add_cs(&conn, "cs2", None, None);
        assert_eq!(merge_base(&conn, "cs1", "cs2").unwrap(), None);
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

        conn.execute("UPDATE branches SET head='cs2' WHERE name='main'", []).unwrap();
        assert_eq!(latest_hash(&conn, "f.txt", "main").unwrap().as_deref(), Some(h2.as_str()));

        conn.execute(
            "INSERT INTO branches(name, head, created_at, from_branch) VALUES('pinned', 'cs1', 0, 'main')",
            [],
        )
        .unwrap();
        assert_eq!(latest_hash(&conn, "f.txt", "pinned").unwrap().as_deref(), Some(h1.as_str()));
        assert_eq!(latest_hash(&conn, "absent.txt", "main").unwrap(), None);
    }
}
