//! Remote synchronisation: clone, push, pull over a simple bundle format.
//!
//! The wire format is a zlib-compressed JSON bundle holding the changesets,
//! file versions, objects and branch refs a peer is missing. It is deliberately
//! simple: correctness first, and it works over `file://`, HTTP PUT/GET and
//! plain filesystem paths.

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashSet;
use std::fs;
use std::io::{Read, Write};
use std::path::Path;

use crate::store::{branch_head, get_object, hash_content, put_object, open_db, init_schema};
use crate::{bad, now, repo_dir, ZResult};

/// Everything a peer needs to catch up on one branch.
pub struct Bundle {
    pub branch: String,
    pub head: Option<String>,
    /// Changeset rows, oldest first.
    pub changesets: Vec<ChangesetRow>,
    /// Every object reachable from those changesets.
    pub objects: Vec<(String, String)>,
    /// file_versions rows for those changesets.
    pub versions: Vec<(String, i64, String, String, i64)>,
    /// changeset_files rows.
    pub files: Vec<(String, String, Option<String>, Option<String>)>,
    pub hashes: Vec<String>,
}

/// (message, author, created_at, parent, parent2) as stored in the database.
type ChangesetMeta = (String, String, i64, Option<String>, Option<String>);

pub struct ChangesetRow {
    pub id: String,
    pub message: String,
    pub author: String,
    pub created_at: i64,
    pub parent: Option<String>,
    pub parent2: Option<String>,
}

impl Bundle {
    /// Serialise to the wire format: a zlib-compressed line oriented text block.
    /// Text keeps the format inspectable and avoids a JSON dependency.
    pub fn encode(&self) -> ZResult<Vec<u8>> {
        let mut s = String::new();
        s.push_str("gesta-bundle 1\n");
        s.push_str(&format!("branch\t{}\n", self.branch));
        s.push_str(&format!("head\t{}\n", self.head.clone().unwrap_or_else(|| "-".into())));
        for c in &self.changesets {
            s.push_str(&format!(
                "cs\t{}\t{}\t{}\t{}\t{}\t{}\n",
                c.id,
                c.created_at,
                enc(&c.author),
                enc(&c.message),
                c.parent.clone().unwrap_or_else(|| "-".into()),
                c.parent2.clone().unwrap_or_else(|| "-".into())
            ));
        }
        for (path, ver, hash, cs, ts) in &self.versions {
            s.push_str(&format!("fv\t{}\t{}\t{}\t{}\t{}\n", enc(path), ver, hash, cs, ts));
        }
        for (cs, path, old, new) in &self.files {
            s.push_str(&format!(
                "cf\t{}\t{}\t{}\t{}\n",
                cs,
                enc(path),
                old.clone().unwrap_or_else(|| "-".into()),
                new.clone().unwrap_or_else(|| "-".into())
            ));
        }
        for (hash, content) in &self.objects {
            s.push_str(&format!("ob\t{}\t{}\n", hash, enc_base64(content.as_bytes())));
        }
        s.push_str("end\n");

        let mut enc_stream = ZlibEncoder::new(Vec::new(), Compression::default());
        enc_stream.write_all(s.as_bytes())?;
        Ok(enc_stream.finish()?)
    }

    pub fn decode(bytes: &[u8]) -> ZResult<Bundle> {
        let mut dec = ZlibDecoder::new(bytes);
        let mut text = String::new();
        dec.read_to_string(&mut text)
            .map_err(|e| bad(format!("bundle is not valid repository data: {}", e)))?;

        let mut lines = text.lines();
        match lines.next() {
            // Bundles written before the rename carry the old header. Refusing them
        // would strand every archive made by an earlier build.
        Some("gesta-bundle 1") | Some("zenforge-bundle 1") => {}
            _ => return Err(bad("unrecognised bundle header")),
        }

        let mut b = Bundle {
            branch: "main".into(),
            head: None,
            changesets: Vec::new(),
            objects: Vec::new(),
            versions: Vec::new(),
            files: Vec::new(),
            hashes: Vec::new(),
        };

        for line in lines {
            let f: Vec<&str> = line.split('\t').collect();
            match f.as_slice() {
                ["branch", name] => b.branch = name.to_string(),
                ["head", h] => b.head = if *h == "-" { None } else { Some(h.to_string()) },
                ["cs", id, ts, author, message, p1, p2] => b.changesets.push(ChangesetRow {
                    id: id.to_string(),
                    message: dec_str(message),
                    author: dec_str(author),
                    created_at: ts.parse().unwrap_or(0),
                    parent: if *p1 == "-" { None } else { Some(p1.to_string()) },
                    parent2: if *p2 == "-" { None } else { Some(p2.to_string()) },
                }),
                ["fv", path, ver, hash, cs, ts] => b.versions.push((
                    dec_str(path),
                    ver.parse().unwrap_or(0),
                    hash.to_string(),
                    cs.to_string(),
                    ts.parse().unwrap_or(0),
                )),
                ["cf", cs, path, old, new] => b.files.push((
                    cs.to_string(),
                    dec_str(path),
                    if *old == "-" { None } else { Some(old.to_string()) },
                    if *new == "-" { None } else { Some(new.to_string()) },
                )),
                ["ob", hash, content] => {
                    let bytes = dec_base64(content)
                        .ok_or_else(|| bad(format!("object {} has invalid payload", hash)))?;
                    let s = String::from_utf8(bytes)
                        .map_err(|e| bad(format!("object {} is not UTF-8: {}", hash, e)))?;
                    b.hashes.push(hash.to_string());
                    b.objects.push((hash.to_string(), s));
                }
                ["end"] => break,
                _ => {} // forward compatibility: ignore unknown records
            }
        }
        Ok(b)
    }
}

fn enc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\t', "\\t").replace('\n', "\\n").replace('\r', "\\r")
}

fn dec_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('t') => out.push('\t'),
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn enc_base64(bytes: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

pub fn dec_base64(s: &str) -> Option<Vec<u8>> {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lut = [255u8; 256];
    for (i, c) in T.iter().enumerate() {
        lut[*c as usize] = i as u8;
    }
    let mut out = Vec::new();
    let bytes: Vec<u8> = s.bytes().filter(|b| *b != b'=' && *b != b'\n').collect();
    for chunk in bytes.chunks(4) {
        if chunk.len() < 2 {
            return None;
        }
        let mut n = 0u32;
        for (i, b) in chunk.iter().enumerate() {
            let v = lut[*b as usize];
            if v == 255 {
                return None;
            }
            n |= (v as u32) << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// Collect everything needed to bring a peer up to date on `branch`, given the
/// set of changeset ids the peer already has.
/// Order the wanted changesets so every parent comes before its children.
///
/// Depth first over the parent links, emitting a node after its parents. The
/// visited set also breaks any cycle, which a corrupted database could contain and
/// which must not hang an upload.
fn topo_order(
    conn: &Connection,
    head: &str,
    wanted: &HashSet<String>,
) -> ZResult<Vec<String>> {
    fn visit(
        conn: &Connection,
        id: &str,
        wanted: &HashSet<String>,
        done: &mut HashSet<String>,
        out: &mut Vec<String>,
    ) -> ZResult<()> {
        if done.contains(id) {
            return Ok(());
        }
        done.insert(id.to_string());
        let (p1, p2) = crate::store::parents_of_public(conn, id)?;
        for p in [p1, p2].into_iter().flatten() {
            visit(conn, &p, wanted, done, out)?;
        }
        if wanted.contains(id) {
            out.push(id.to_string());
        }
        Ok(())
    }

    let mut done = HashSet::new();
    let mut out = Vec::new();
    visit(conn, head, wanted, &mut done, &mut out)?;
    Ok(out)
}

pub fn build_bundle(conn: &Connection, branch: &str, have: &HashSet<String>) -> ZResult<Bundle> {
    let head = branch_head(conn, branch)?;
    let mut b = Bundle {
        branch: branch.to_string(),
        head: head.clone(),
        changesets: Vec::new(),
        objects: Vec::new(),
        versions: Vec::new(),
        files: Vec::new(),
        hashes: Vec::new(),
    };
    let Some(head) = head else { return Ok(b) };

    let chain = crate::store::ancestors_bfs(conn, &head)?;
    let wanted: HashSet<String> = chain
        .iter()
        .filter(|id| !have.contains(*id))
        .cloned()
        .collect();
    // Oldest first, and a parent before any child. Reversing a breadth-first walk
    // is close but not sufficient: across a merge the two sides sit at different
    // depths, so a reversed walk can put a child before its own parent. A receiver
    // that applies rows in order would then fail on a parent it has not seen.
    let needed = topo_order(conn, &head, &wanted)?;

    let mut object_hashes: HashSet<String> = HashSet::new();
    for id in &needed {
        let row: Option<ChangesetMeta> = conn
            .query_row(
                "SELECT message, author, created_at, parent, parent2
                 FROM changesets WHERE id = ?1",
                params![id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((message, author, ts, parent, parent2)) = row else { continue };
        b.changesets.push(ChangesetRow {
            id: id.clone(),
            message,
            author,
            created_at: ts,
            parent,
            parent2,
        });

        let mut stmt = conn.prepare(
            "SELECT path, version, hash, changeset_id, created_at FROM file_versions
             WHERE changeset_id = ?1 ORDER BY path",
        )?;
        let rows = stmt.query_map(params![id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?;
        for r in rows {
            let (path, ver, hash, cs, cts) = r?;
            object_hashes.insert(hash.clone());
            b.versions.push((path, ver, hash, cs, cts));
        }

        let mut stmt2 = conn.prepare(
            "SELECT path, old_hash, new_hash FROM changeset_files WHERE changeset_id = ?1 ORDER BY path",
        )?;
        let rows2 = stmt2.query_map(params![id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        for r in rows2 {
            let (path, old, new) = r?;
            if let Some(h) = &new {
                object_hashes.insert(h.clone());
            }
            b.files.push((id.clone(), path, old, new));
        }
    }

    for hash in object_hashes {
        match get_object(conn, &hash) {
            Ok(content) => {
                b.hashes.push(hash.clone());
                b.objects.push((hash, content));
            }
            Err(_) => return Err(bad(format!("cannot bundle missing object {}", hash))),
        }
    }
    Ok(b)
}

/// Apply a bundle into a repository. Idempotent per changeset.
///
/// The branch head is left alone when `move_branch` is false: a pull must merge
/// the fetched history into the local branch, not replace it. Overwriting the
/// head here silently drops the local changesets from the branch.
pub fn apply_bundle_opts(conn: &Connection, b: &Bundle, move_branch: bool) -> ZResult<usize> {
    let tx = conn.unchecked_transaction()?;
    let mut applied = 0usize;

    for (hash, content) in &b.objects {
        let expect = hash_content(content);
        if &expect != hash {
            return Err(bad(format!(
                "object {} fails its own hash check (got {})",
                hash, expect
            )));
        }
        put_object(&tx, content)?;
    }

    for c in &b.changesets {
        let exists: Option<i64> = tx
            .query_row("SELECT 1 FROM changesets WHERE id = ?1", params![c.id], |r| r.get(0))
            .optional()?;
        if exists.is_some() {
            continue;
        }
        tx.execute(
            "INSERT INTO changesets(id, message, author, created_at, status, parent, parent2, branch)
             VALUES(?1, ?2, ?3, ?4, 'committed', ?5, ?6, ?7)",
            params![c.id, c.message, c.author, c.created_at, c.parent, c.parent2, b.branch],
        )?;
        applied += 1;
    }

    for (path, ver, hash, cs, ts) in &b.versions {
        tx.execute(
            "INSERT OR IGNORE INTO file_versions(path, version, hash, changeset_id, created_at)
             VALUES(?1, ?2, ?3, ?4, ?5)",
            params![path, ver, hash, cs, ts],
        )?;
    }

    for (cs, path, old, new) in &b.files {
        tx.execute(
            "INSERT OR IGNORE INTO changeset_files(changeset_id, path, old_hash, new_hash)
             VALUES(?1, ?2, ?3, ?4)",
            params![cs, path, old, new],
        )?;
    }

    tx.execute(
        "INSERT INTO branches(name, head, created_at, from_branch) VALUES(?1, NULL, ?2, NULL)
         ON CONFLICT(name) DO NOTHING",
        params![b.branch, now()],
    )?;
    if move_branch {
        if let Some(head) = &b.head {
            crate::store::set_branch_head(&tx, &b.branch, head)?;
        }
    }

    tx.commit()?;
    Ok(applied)
}

/// Apply a bundle and move the branch head to it (clone and server receive).
pub fn apply_bundle(conn: &Connection, b: &Bundle) -> ZResult<usize> {
    apply_bundle_opts(conn, b, true)
}



/// Normalise a user supplied remote into a filesystem path.
pub fn resolve_remote(url: &str) -> std::path::PathBuf {
    let s = url.strip_prefix("file://").unwrap_or(url);
    std::path::PathBuf::from(s)
}

/// Initialise a bare repository at `path` (just a repository directory with a db).
pub fn init_bare(path: &Path, branch: &str) -> ZResult<()> {
    let zf = repo_dir(path);
    fs::create_dir_all(&zf)?;
    let conn = open_db(path)?;
    init_schema(&conn)?;
    conn.execute(
        "INSERT OR IGNORE INTO branches(name, head, created_at, from_branch) VALUES(?1, NULL, ?2, NULL)",
        params![branch, now()],
    )?;
    Ok(())
}

/// Public base64 decode, used by HTTP Basic auth parsing.
pub fn decode_base64_str(s: &str) -> Option<Vec<u8>> {
    dec_base64(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::commit_one;
    use crate::store::tests::mem_db;

    #[test]
    fn base64_roundtrips_including_padding_cases() {
        for input in [&b""[..], b"a", b"ab", b"abc", b"abcd", b"hello world!"] {
            let e = enc_base64(input);
            let d = dec_base64(&e).unwrap();
            assert_eq!(d, input.to_vec(), "failed for {:?} -> {:?}", input, e);
        }
    }

    #[test]
    fn field_escaping_survives_real_text() {
        let original = "line one\nline two\ttabbed\\backslash\n";
        let round = dec_str(&enc(original));
        assert_eq!(round, original);
    }

    #[test]
    fn bundle_roundtrips_through_encode_decode() {
        let conn = mem_db();
        let h = put_object(&conn, "alpha\nbeta\n").unwrap();
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('cs1','first commit\twith tab','agent',10,'committed',NULL,'main')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('cs1','src/a.rs',NULL,?1)",
            params![h],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO file_versions(path,version,hash,changeset_id,created_at)
             VALUES('src/a.rs',1,?1,'cs1',10)",
            params![h],
        )
        .unwrap();
        conn.execute("UPDATE branches SET head='cs1' WHERE name='main'", []).unwrap();

        let bundle = build_bundle(&conn, "main", &HashSet::new()).unwrap();
        let bytes = bundle.encode().unwrap();
        let back = Bundle::decode(&bytes).unwrap();

        assert_eq!(back.branch, "main");
        assert_eq!(back.head.as_deref(), Some("cs1"));
        assert_eq!(back.changesets.len(), 1);
        assert_eq!(back.changesets[0].message, "first commit\twith tab");
        assert_eq!(back.objects.len(), 1);
        assert_eq!(back.objects[0].1, "alpha\nbeta\n");
    }

    #[test]
    fn push_then_pull_transfers_history_and_objects() {
        let source = mem_db();
        let h = put_object(&source, "content\n").unwrap();
        source
            .execute(
                "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
                 VALUES('cs1','m','a',1,'committed',NULL,'main')",
                [],
            )
            .unwrap();
        source
            .execute(
                "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
                 VALUES('cs1','f.txt',NULL,?1)",
                params![h],
            )
            .unwrap();
        source
            .execute(
                "INSERT INTO file_versions(path,version,hash,changeset_id,created_at)
                 VALUES('f.txt',1,?1,'cs1',1)",
                params![h],
            )
            .unwrap();
        source.execute("UPDATE branches SET head='cs1' WHERE name='main'", []).unwrap();

        let bundle = build_bundle(&source, "main", &HashSet::new()).unwrap();
        let bytes = bundle.encode().unwrap();
        let back = Bundle::decode(&bytes).unwrap();

        // Receiver starts empty.
        let dest = mem_db();
        let applied = apply_bundle(&dest, &back).unwrap();
        assert_eq!(applied, 1);
        assert_eq!(branch_head(&dest, "main").unwrap().as_deref(), Some("cs1"));
        assert_eq!(get_object(&dest, &h).unwrap(), "content\n");
    }

    #[test]
    fn applying_a_bundle_twice_changes_nothing() {
        let source = mem_db();
        let h = put_object(&source, "x\n").unwrap();
        source
            .execute(
                "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
                 VALUES('cs1','m','a',1,'committed',NULL,'main')",
                [],
            )
            .unwrap();
        source
            .execute(
                "INSERT INTO file_versions(path,version,hash,changeset_id,created_at)
                 VALUES('f.txt',1,?1,'cs1',1)",
                params![h],
            )
            .unwrap();
        source.execute("UPDATE branches SET head='cs1' WHERE name='main'", []).unwrap();

        let bundle = build_bundle(&source, "main", &HashSet::new()).unwrap();
        let dest = mem_db();
        assert_eq!(apply_bundle(&dest, &bundle).unwrap(), 1);
        assert_eq!(apply_bundle(&dest, &bundle).unwrap(), 0, "second apply is a no-op");
    }

    #[test]
    fn a_corrupted_object_is_rejected() {
        let mut b = Bundle {
            branch: "main".into(),
            head: None,
            changesets: Vec::new(),
            objects: vec![("0".repeat(64), "tampered".into())],
            versions: Vec::new(),
            files: Vec::new(),
            hashes: Vec::new(),
        };
        b.hashes.push("0".repeat(64));
        let dest = mem_db();
        let err = apply_bundle(&dest, &b).unwrap_err().to_string();
        assert!(err.contains("hash check"), "got: {}", err);
    }

    #[test]
    fn bundle_only_sends_what_the_peer_lacks() {
        let conn = mem_db();
        let h1 = put_object(&conn, "one\n").unwrap();
        let h2 = put_object(&conn, "two\n").unwrap();
        for (id, parent, hash, ver) in [("cs1", None, &h1, 1i64), ("cs2", Some("cs1"), &h2, 2)] {
            conn.execute(
                "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
                 VALUES(?1,'m','a',1,'committed',?2,'main')",
                params![id, parent],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO file_versions(path,version,hash,changeset_id,created_at)
                 VALUES('f.txt',?1,?2,?3,1)",
                params![ver, hash, id],
            )
            .unwrap();
        }
        conn.execute("UPDATE branches SET head='cs2' WHERE name='main'", []).unwrap();

        let mut have = HashSet::new();
        have.insert("cs1".to_string());
        let b = build_bundle(&conn, "main", &have).unwrap();
        assert_eq!(b.changesets.len(), 1, "peer already has cs1");
        assert_eq!(b.changesets[0].id, "cs2");
        assert!(!b.objects.iter().any(|(h, _)| h == &h1), "cs1 content not resent");
    }
    /// A bundle must carry every changeset reachable from the head, in an order a
    /// receiver can apply. A depth-first walk happens to work for two commits and
    /// starts dropping them for longer histories, so the depth is the point of this
    /// test rather than an afterthought.
    #[test]
    fn a_bundle_carries_the_whole_chain_for_a_long_history() {
        let conn = mem_db();
        let mut parent: Option<String> = None;
        let mut ids = Vec::new();
        for i in 0..12 {
            let id = format!("cs{:02}", i);
            conn.execute(
                "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
                 VALUES(?1,?2,'a',?3,'pending',?4,'main')",
                params![id, format!("m{}", i), i as i64, parent],
            )
            .unwrap();
            commit_one(&conn, &id).unwrap();
            parent = Some(id.clone());
            ids.push(id);
        }

        let b = build_bundle(&conn, "main", &HashSet::new()).unwrap();
        assert_eq!(
            b.changesets.len(),
            12,
            "the bundle dropped changesets, it carried {}",
            b.changesets.len()
        );
        // Oldest first, so the receiver applies a parent before its child.
        assert_eq!(b.changesets.first().map(|c| c.id.clone()), Some(ids[0].clone()));
        assert_eq!(b.changesets.last().map(|c| c.id.clone()), Some(ids[11].clone()));
        assert_eq!(b.head.as_deref(), Some(ids[11].as_str()));
    }
    /// Across a merge, a parent can sit deeper than a child on the other side, so
    /// reversing a breadth-first walk can emit a child before its own parent. A
    /// receiver applying rows in that order fails on a parent it has not seen.
    #[test]
    fn the_order_puts_every_parent_before_its_child_across_a_merge() {
        // A merge row in the shape the store uses: two parents. Building it
        // directly keeps this about ordering and nothing else.
        let conn = mem_db();
        for (id, p1, p2, ts) in [
            ("base", None, None, 1i64),
            ("left", Some("base"), None, 2),
            ("right", Some("base"), None, 3),
            ("merge", Some("left"), Some("right"), 4),
        ] {
            conn.execute(
                "INSERT INTO changesets(id,message,author,created_at,status,parent,parent2,branch)
                 VALUES(?1,?2,'a',?3,'committed',?4,?5,'main')",
                rusqlite::params![id, format!("m-{}", id), ts, p1, p2],
            )
            .unwrap();
        }

        let reachable = crate::store::ancestors_bfs(&conn, "merge").unwrap();
        assert_eq!(reachable.len(), 4, "expected four changesets, got {:?}", reachable);
        let wanted: HashSet<String> = reachable.into_iter().collect();
        let order = topo_order(&conn, "merge", &wanted).unwrap();

        let mut pos = std::collections::HashMap::new();
        for (i, id) in order.iter().enumerate() {
            pos.insert(id.clone(), i);
        }
        assert_eq!(order.len(), 4, "every reachable changeset must be listed, got {:?}", order);
        for (i, id) in order.iter().enumerate() {
            let (p1, p2) = crate::store::parents_of_public(&conn, id).unwrap();
            for p in [p1, p2].into_iter().flatten() {
                if let Some(pi) = pos.get(&p) {
                    assert!(*pi < i, "parent {} of {} came after it in {:?}", p, id, order);
                }
            }
        }
        // The merge is last: it depends on both sides.
        assert_eq!(order.last().map(|s| s.as_str()), Some("merge"));
    }
}
