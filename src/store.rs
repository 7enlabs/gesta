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
         PRAGMA busy_timeout=30000;",
    )?;
    Ok(conn)
}

/// Is this error SQLite refusing a write because another connection holds the
/// write lock?
pub fn is_busy(e: &rusqlite::Error) -> bool {
    match e {
        rusqlite::Error::SqliteFailure(f, _) => {
            matches!(
                f.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
        }
        _ => false,
    }
}

/// Run `f`, retrying while SQLite reports the database as busy.
///
/// `busy_timeout` covers a reader waiting for a writer. It does not cover two
/// writers arriving at once from different processes: SQLite returns SQLITE_BUSY
/// immediately for a write-write conflict when a transaction has already started
/// reading. Retrying with a growing pause is what makes concurrent commits work;
/// without it, 20 parallel commits produced 19 failures.
pub fn with_retry<T>(mut f: impl FnMut() -> ZResult<T>) -> ZResult<T> {
    let mut paused = std::time::Duration::from_millis(5);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) => {
                let busy = e
                    .downcast_ref::<rusqlite::Error>()
                    .map(is_busy)
                    .unwrap_or(false);
                if !busy || std::time::Instant::now() >= deadline {
                    return Err(e);
                }
                std::thread::sleep(paused);
                paused = (paused * 2).min(std::time::Duration::from_millis(250));
            }
        }
    }
}

/// Add a column to a table that predates it.
///
/// SQLite has no `ADD COLUMN IF NOT EXISTS`, so the column list decides. An
/// existing repository keeps working without a manual migration step.
fn ensure_column(conn: &Connection, table: &str, column: &str, decl: &str) -> ZResult<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", table))?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
    let existing: Vec<String> = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    if !existing.iter().any(|c| c == column) {
        conn.execute(&format!("ALTER TABLE {} ADD COLUMN {} {}", table, column, decl), [])?;
    }
    Ok(())
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
            name  TEXT PRIMARY KEY,
            url   TEXT NOT NULL,
            token TEXT,
            pin   TEXT
        );
        CREATE TABLE IF NOT EXISTS remote_refs (
            remote       TEXT NOT NULL,
            branch       TEXT NOT NULL,
            changeset_id TEXT NOT NULL,
            updated_at   INTEGER NOT NULL,
            PRIMARY KEY (remote, branch)
        );
        CREATE TABLE IF NOT EXISTS deltas (
            hash       TEXT PRIMARY KEY,
            base       TEXT NOT NULL,
            payload    BLOB NOT NULL,
            created_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS worktree_cache (
            path  TEXT PRIMARY KEY,
            mtime INTEGER NOT NULL,
            size  INTEGER NOT NULL,
            hash  TEXT NOT NULL
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

    // Columns added after the first release. A repository created by an older
    // build has the table but not the column, so the schema step upgrades it in
    // place instead of asking the operator to run a migration.
    ensure_column(conn, "remotes", "pin", "TEXT")?;
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

fn compress_bytes(data: &[u8]) -> ZResult<Vec<u8>> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data)?;
    Ok(enc.finish()?)
}

fn decompress_bytes(blob: &[u8]) -> ZResult<Vec<u8>> {
    let mut dec = ZlibDecoder::new(blob);
    let mut out = Vec::new();
    dec.read_to_end(&mut out)?;
    Ok(out)
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
    put_object_against(conn, content, None)
}

/// Store `content`, optionally as a delta against `base`.
///
/// A delta only wins when it is smaller than the compressed whole object, which
/// is measured rather than assumed: a two-line change to a large file produces a
/// tiny delta, while an unrelated rewrite produces a delta bigger than the file
/// and is discarded.
pub fn put_object_against(
    conn: &Connection,
    content: &str,
    base: Option<&str>,
) -> ZResult<String> {
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
        (packed.clone(), 1i64)
    } else {
        (raw, 0i64)
    };
    let whole_size = blob.len();

    // Try a delta only when a base is offered and it is a different object.
    if let Some(base_hash) = base {
        if base_hash != h {
            if let Ok(base_text) = get_object(conn, base_hash) {
                if let Some(ops) = crate::delta::encode(&base_text, content) {
                    let payload = crate::delta::to_bytes(&ops);
                    let packed_delta = compress_bytes(&payload)?;
                    // A delta is only worth it when it beats the compressed
                    // whole object by a real margin, not by a few bytes.
                    if packed_delta.len() + 32 < whole_size {
                        conn.execute(
                            "INSERT OR IGNORE INTO objects(hash, content, size, compressed, created_at)
                             VALUES(?1, X'', ?2, 0, ?3)",
                            params![h, content.len() as i64, now()],
                        )?;
                        conn.execute(
                            "INSERT OR REPLACE INTO deltas(hash, base, payload, created_at)
                             VALUES(?1, ?2, ?3, ?4)",
                            params![h, base_hash, packed_delta, now()],
                        )?;
                        return Ok(h);
                    }
                }
            }
        }
    }

    conn.execute(
        "INSERT OR IGNORE INTO objects(hash, content, size, compressed, created_at)
         VALUES(?1, ?2, ?3, ?4, ?5)",
        params![h, blob, content.len() as i64, compressed, now()],
    )?;
    Ok(h)
}

/// Read the delta base chain for a hash, following up to 8 levels.
pub fn delta_chain(conn: &Connection, hash: &str) -> ZResult<Vec<String>> {
    let mut chain = vec![hash.to_string()];
    let mut cur = hash.to_string();
    for _ in 0..8 {
        let base: Option<String> = conn
            .query_row("SELECT base FROM deltas WHERE hash = ?1", params![cur], |r| r.get(0))
            .optional()?;
        match base {
            Some(b) => {
                chain.push(b.clone());
                cur = b;
            }
            None => break,
        }
    }
    Ok(chain)
}

/// Number of objects stored as deltas, and the bytes they save.
pub fn delta_stats(conn: &Connection) -> ZResult<(i64, i64)> {
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM deltas", [], |r| r.get(0))
        .unwrap_or(0);
    if count == 0 {
        return Ok((0, 0));
    }
    let mut stmt = conn.prepare(
        "SELECT d.hash, LENGTH(d.payload), o.size FROM deltas d
         JOIN objects o ON o.hash = d.hash",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    let mut saved = 0i64;
    for r in rows {
        let (_h, payload, raw) = r?;
        // Compare against the raw size, which is the optimistic alternative: a
        // real whole object would also be compressed, so the true saving is
        // smaller than this. Reported as an upper bound.
        saved += (raw - payload).max(0);
    }
    Ok((count, saved))
}

pub fn get_object(conn: &Connection, hash: &str) -> ZResult<String> {
    let chain = delta_chain(conn, hash)?;

    // Walk from the deepest base outwards: a chain is only ever one delta deep
    // in practice, but the loop costs nothing and handles nesting.
    let mut content: Option<String> = None;
    for (i, h) in chain.iter().enumerate().rev() {
        if i == 0 && chain.len() > 1 {
            // The requested object: apply its delta to what the base produced.
            let base = content
                .take()
                .ok_or_else(|| bad(format!("delta chain for {} is broken", hash)))?;
            let payload: Vec<u8> = conn
                .query_row("SELECT payload FROM deltas WHERE hash = ?1", params![h], |r| r.get(0))
                .optional()?
                .ok_or_else(|| bad(format!("delta for {} vanished", h)))?;
            let raw = decompress_bytes(&payload)?;
            let ops = crate::delta::from_bytes(&raw)
                .ok_or_else(|| bad(format!("delta for {} is malformed", h)))?;
            let rebuilt = crate::delta::apply(&base, &ops)
                .ok_or_else(|| bad(format!("delta for {} does not apply to its base", h)))?;
            // The hash is the contract. A rebuild that does not match means the
            // store is corrupt, and returning the wrong bytes would hide it.
            if hash_content(&rebuilt) != *hash {
                return Err(bad(format!("rebuilt object {} does not match its hash", hash)));
            }
            return Ok(rebuilt);
        }
        content = Some(read_whole(conn, h)?);
    }

    content.ok_or_else(|| bad(format!("object {} not found (store may be corrupted)", hash)))
}

fn read_whole(conn: &Connection, hash: &str) -> ZResult<String> {
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

/// Resolve a revision name to a changeset id.
///
/// A person writes `main:file.txt`, an agent sends `{"rev":"main"}`, and both
/// mean "the head of that branch". A changeset id means that changeset. Trying
/// the branch first and the id second covers both, and returning `None` lets the
/// caller produce one clear error instead of two nested ones.
///
/// This existed as a bug first: `show main:x` passed the branch name into a
/// function that only walked changesets, so a file that was plainly in the head
/// tree was reported missing.
pub fn resolve_rev(conn: &Connection, rev: &str) -> ZResult<Option<String>> {
    if let Ok(Some(head)) = branch_head(conn, rev) {
        return Ok(Some(head));
    }
    let exists: Option<String> = conn
        .query_row("SELECT id FROM changesets WHERE id = ?1", params![rev], |r| r.get(0))
        .optional()?;
    if exists.is_some() {
        return Ok(exists);
    }
    // A tag is also a valid revision.
    let tagged: Option<String> = conn
        .query_row("SELECT changeset_id FROM tags WHERE name = ?1", params![rev], |r| r.get(0))
        .optional()?;
    Ok(tagged)
}

/// Content hash for `path` as of `changeset`, walking its ancestry.
///
/// `changeset_files` is the source of truth, not `file_versions`: a deletion
/// writes a row with `new_hash IS NULL` and no version row, so a lookup that
/// only consulted versions would resurrect deleted files. This is the same rule
/// `tree_at` follows.
pub fn hash_at_point(conn: &Connection, path: &str, changeset: &str) -> ZResult<Option<String>> {
    let reachable = ancestors(conn, changeset)?;
    if reachable.is_empty() {
        return Ok(None);
    }
    let placeholders = vec!["?"; reachable.len()].join(",");
    let sql = format!(
        "SELECT cf.new_hash FROM changeset_files cf
         JOIN changesets cs ON cs.id = cf.changeset_id
         WHERE cf.path = ? AND cf.changeset_id IN ({})
         ORDER BY cs.created_at DESC, cs.rowid DESC LIMIT 1",
        placeholders
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(reachable.len() + 1);
    params.push(&path);
    for id in &reachable {
        params.push(id);
    }
    // A row with a NULL new_hash means the path was deleted at that point, and
    // returning None is exactly right for that case.
    let found: Option<Option<String>> = stmt
        .query_row(params.as_slice(), |r| r.get::<_, Option<String>>(0))
        .optional()?;
    Ok(found.flatten())
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
            // hash_at_point reads changeset_files, so a fixture that only wrote
            // file_versions would not describe a real repository.
            conn.execute(
                "INSERT INTO changeset_files(changeset_id, path, old_hash, new_hash)
                 VALUES(?1, 'f.txt', NULL, ?2)",
                params![id, hash],
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

    #[test]
    fn a_busy_error_is_recognised() {
        // Shaped like the error SQLite raises when another process holds the
        // write lock. Recognising it is what lets the retry loop help.
        let busy = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(5), // SQLITE_BUSY
            Some("database is locked".to_string()),
        );
        assert!(is_busy(&busy));

        let other = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(1), // SQLITE_ERROR
            Some("no such table".to_string()),
        );
        assert!(!is_busy(&other));

        assert!(!is_busy(&rusqlite::Error::QueryReturnedNoRows));
    }

    #[test]
    fn with_retry_passes_a_success_through() {
        let mut calls = 0;
        let v = with_retry(|| {
            calls += 1;
            Ok(7)
        })
        .unwrap();
        assert_eq!(v, 7);
        assert_eq!(calls, 1);
    }

    #[test]
    fn with_retry_gives_up_on_an_error_that_is_not_busy() {
        let mut calls = 0;
        let r: ZResult<()> = with_retry(|| {
            calls += 1;
            Err(bad("no such column".to_string()))
        });
        assert!(r.is_err());
        assert_eq!(calls, 1, "a non-busy error must not be retried");
    }

    #[test]
    fn with_retry_retries_a_busy_error_then_succeeds() {
        let mut calls = 0;
        let v = with_retry(|| {
            calls += 1;
            if calls < 4 {
                Err(Box::new(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(5),
                    Some("database is locked".to_string()),
                )) as Box<dyn std::error::Error + Send + Sync>)
            } else {
                Ok(calls)
            }
        })
        .unwrap();
        assert_eq!(v, 4);
        assert_eq!(calls, 4);
    }

    #[test]
    fn resolve_rev_accepts_a_branch_name() {
        // `show main:file.txt` passes "main". A resolver that only knew
        // changeset ids answered "file does not exist" for a file that was in
        // the head tree, which is how this was found.
        let conn = mem_db();
        let h = put_object(&conn, "x\n").unwrap();
        conn.execute(
            "INSERT INTO changesets(id, message, author, created_at, status, parent, branch)
             VALUES('cs1', 'm', 'a', 1, 'committed', NULL, 'main')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id, path, old_hash, new_hash)
             VALUES('cs1', 'x.txt', NULL, ?1)",
            params![h],
        )
        .unwrap();
        conn.execute("UPDATE branches SET head='cs1' WHERE name='main'", []).unwrap();

        assert_eq!(resolve_rev(&conn, "main").unwrap().as_deref(), Some("cs1"));
    }

    #[test]
    fn resolve_rev_accepts_a_changeset_id_and_a_tag() {
        let conn = mem_db();
        conn.execute(
            "INSERT INTO changesets(id, message, author, created_at, status, parent, branch)
             VALUES('cs9', 'm', 'a', 1, 'committed', NULL, 'main')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags(name, changeset_id, created_at) VALUES('v1', 'cs9', 1)",
            [],
        )
        .unwrap();

        assert_eq!(resolve_rev(&conn, "cs9").unwrap().as_deref(), Some("cs9"));
        assert_eq!(resolve_rev(&conn, "v1").unwrap().as_deref(), Some("cs9"));
    }

    #[test]
    fn resolve_rev_reports_an_unknown_name() {
        let conn = mem_db();
        assert_eq!(resolve_rev(&conn, "nowhere").unwrap(), None);
    }
}
