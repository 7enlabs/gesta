//! Handlers for remotes, integrity checks, statistics and ignore rules.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use rusqlite::OptionalExtension;

use crate::auth;
use crate::http;
use crate::ops;
use crate::remote;
use crate::repo;
use crate::store::{
    ancestors, branch_head, current_branch, delta_stats, get_object, init_schema,
    object_store_bytes, open_db, set_state,
};
use crate::worktree::IgnoreRules;
use crate::{bad, now, open_repo, repo_dir, ZResult};

// remotes.

pub fn cmd_remote_add(
    name: &str,
    url: &str,
    token: Option<&str>,
    pin: Option<&str>,
) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    if let Some(p) = pin {
        // Reject a malformed pin now, not on the first push over a network.
        #[cfg(feature = "tls")]
        crate::tls::client_config(Some(p))?;
        #[cfg(not(feature = "tls"))]
        let _ = p;
    }
    conn.execute(
        "INSERT INTO remotes(name, url, token, pin) VALUES(?1, ?2, ?3, ?4)
         ON CONFLICT(name) DO UPDATE SET url = excluded.url,
                                         token = COALESCE(excluded.token, remotes.token),
                                         pin = COALESCE(excluded.pin, remotes.pin)",
        rusqlite::params![name, url, token, pin],
    )?;
    println!("remote {} -> {}", name, url);
    if url.starts_with("https://") {
        println!("transport: TLS");
    }
    if token.is_some() {
        println!("token stored in the repository database");
    }
    if pin.is_some() {
        println!("certificate pin stored in the repository database");
    }
    Ok(())
}

/// Pin saved for this remote, if any.
fn remote_pin(conn: &rusqlite::Connection, name: &str) -> ZResult<Option<String>> {
    Ok(conn
        .query_row("SELECT pin FROM remotes WHERE name = ?1", rusqlite::params![name], |r| {
            r.get::<_, Option<String>>(0)
        })
        .optional()?
        .flatten())
}

pub fn cmd_remote_list() -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let all: Vec<(String, String, Option<String>)> = {
        let mut stmt = conn.prepare("SELECT name, url, pin FROM remotes ORDER BY name")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    if all.is_empty() {
        println!("no remotes configured");
    }
    for (n, u, pin) in all {
        match pin {
            Some(p) => println!("{:<10} {} (pin {}...)", n, u, &p[..p.len().min(12)]),
            None => println!("{:<10} {}", n, u),
        }
    }
    Ok(())
}

fn remote_url(conn: &rusqlite::Connection, name: &str) -> ZResult<String> {
    conn.query_row("SELECT url FROM remotes WHERE name = ?1", rusqlite::params![name], |r| {
        r.get::<_, String>(0)
    })
    .optional()?
    .ok_or_else(|| bad(format!("no such remote: {}", name)))
}

/// Token saved for this remote, if any. Kept in the repository database so a
/// later push does not need the secret retyped.
fn remote_token(conn: &rusqlite::Connection, name: &str) -> ZResult<Option<String>> {
    Ok(conn
        .query_row("SELECT token FROM remotes WHERE name = ?1", rusqlite::params![name], |r| {
            r.get::<_, Option<String>>(0)
        })
        .optional()?
        .flatten())
}

pub fn cmd_push(remote_name: &str, branch: &str) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let url = remote_url(&conn, remote_name)?;

    if http::is_http_url(&url) {
        return push_http(&conn, root.as_path(), remote_name, &url, branch);
    }
    let path = remote::resolve_remote(&url);

    // The remote must already exist as a bare repository.
    if !crate::repo_dir(&path).is_dir() {
        remote::init_bare(&path, branch)?;
        println!("created remote repository at {}", path.display());
    }

    let remote_conn = open_db(&path)?;
    init_schema(&remote_conn)?;

    // What does the remote already have?
    let have: HashSet<String> = match branch_head(&remote_conn, branch) {
        Ok(Some(h)) => ancestors(&remote_conn, &h)?.into_iter().collect(),
        _ => HashSet::new(),
    };

    let bundle = remote::build_bundle(&conn, branch, &have)?;
    let n = bundle.changesets.len();

    // Serialise and parse back before applying. This is the real transport
    // path: skipping it would let a broken wire format pass locally and only
    // fail once two machines are involved.
    let wire = bundle.encode()?;
    let received = remote::Bundle::decode(&wire)?;
    let applied = remote::apply_bundle(&remote_conn, &received)?;

    conn.execute(
        "INSERT INTO remote_refs(remote, branch, changeset_id, updated_at) VALUES(?1, ?2, ?3, ?4)
         ON CONFLICT(remote, branch) DO UPDATE SET changeset_id = excluded.changeset_id,
                                                   updated_at = excluded.updated_at",
        rusqlite::params![remote_name, branch, bundle.head.clone().unwrap_or_default(), now()],
    )?;

    println!(
        "pushed {} changeset(s) of {} to {} ({}, {} object(s))",
        applied,
        branch,
        remote_name,
        if n == 0 { "already up to date" } else { "new" },
        bundle.objects.len()
    );
    Ok(())
}

pub fn cmd_pull(remote_name: &str, branch: &str, author: &str) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let url = remote_url(&conn, remote_name)?;

    if http::is_http_url(&url) {
        return pull_http(&conn, &root, remote_name, &url, branch, author);
    }
    let path = remote::resolve_remote(&url);

    let remote_conn = open_db(&path)?;
    let remote_head = branch_head(&remote_conn, branch)?
        .ok_or_else(|| bad(format!("remote {} has no branch {}", remote_name, branch)))?;

    let bundle = remote::build_bundle(&remote_conn, branch, &HashSet::new())?;
    let received = remote::Bundle::decode(&bundle.encode()?)?;
    let applied = remote::apply_bundle_opts(&conn, &received, false)?;

    let current = current_branch(&conn)?;
    if current == branch && branch_head(&conn, &current)?.as_deref() != Some(remote_head.as_str()) {
        merge_fetched(&conn, &root, remote_name, branch, &remote_head, author)?;
    }
    repo::materialise(&conn, &root, &current)?;
    ops::record(&conn, "pull", Some(&remote_head), Some(remote_name))?;
    println!("pulled {} changeset(s) from {} into {}", applied, remote_name, branch);
    Ok(())
}

pub fn cmd_clone(url: &str, dir: &str, token: Option<&str>, pin: Option<&str>) -> ZResult<()> {
    let target = Path::new(dir);
    if crate::repo_dir(target).is_dir() {
        return Err(bad(format!("{} already contains a repository", dir)));
    }
    fs::create_dir_all(target)?;

    if http::is_http_url(url) {
        return clone_http(url, target, token, pin);
    }

    let remote_path = remote::resolve_remote(url);
    if !crate::repo_dir(&remote_path).is_dir() {
        return Err(bad(format!("no repository at {}", remote_path.display())));
    }
    let remote_conn = open_db(&remote_path)?;
    let branch: String = remote_conn
        .query_row("SELECT name FROM branches ORDER BY created_at LIMIT 1", [], |r| {
            r.get::<_, String>(0)
        })
        .optional()?
        .ok_or_else(|| bad("remote has no branches"))?;

    let bundle = remote::build_bundle(&remote_conn, &branch, &HashSet::new())?;
    let received = remote::Bundle::decode(&bundle.encode()?)?;

    let zf = repo_dir(target);
    fs::create_dir_all(&zf)?;
    let conn = open_db(target)?;
    init_schema(&conn)?;
    remote::apply_bundle(&conn, &received)?;
    set_state(&conn, "current_branch", &branch)?;
    conn.execute(
        "INSERT INTO remotes(name, url, pin) VALUES('origin', ?1, ?2)
         ON CONFLICT(name) DO UPDATE SET url = excluded.url, pin = excluded.pin",
        rusqlite::params![url, pin],
    )?;

    let (written, _removed) = repo::restore_worktree(&conn, target, &branch)?;
    println!(
        "cloned {} into {} ({} changeset(s), {} file(s) on disk)",
        url,
        dir,
        received.changesets.len(),
        written
    );
    Ok(())
}

// fsck / stats / ignore.

pub fn cmd_fsck(prune: bool) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let mut problems = 0usize;

    // Every changeset_file must point at objects that exist.
    let refs: Vec<(String, String, Option<String>, Option<String>)> = {
        let mut stmt = conn.prepare(
            "SELECT changeset_id, path, old_hash, new_hash FROM changeset_files ORDER BY changeset_id, path",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (cs, path, old, new) in &refs {
        for h in [old, new].into_iter().flatten() {
            if get_object(&conn, h).is_err() {
                println!("MISSING object {} referenced by {} ({})", h, cs, path);
                problems += 1;
            }
        }
    }

    // Every parent must exist.
    let parents: Vec<(String, Option<String>, Option<String>)> = {
        let mut stmt = conn.prepare("SELECT id, parent, parent2 FROM changesets")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, p1, p2) in &parents {
        for p in [p1, p2].into_iter().flatten() {
            let exists: Option<i64> = conn
                .query_row("SELECT 1 FROM changesets WHERE id = ?1", rusqlite::params![p], |r| r.get(0))
                .optional()?;
            if exists.is_none() {
                println!("MISSING parent {} referenced by {}", p, id);
                problems += 1;
            }
        }
    }

    // Every branch head must exist.
    let branches: Vec<(String, Option<String>)> = {
        let mut stmt = conn.prepare("SELECT name, head FROM branches")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (name, head) in &branches {
        if let Some(h) = head {
            let exists: Option<i64> = conn
                .query_row("SELECT 1 FROM changesets WHERE id = ?1", rusqlite::params![h], |r| r.get(0))
                .optional()?;
            if exists.is_none() {
                println!("MISSING changeset {} is the head of branch {}", h, name);
                problems += 1;
            }
        }
    }

    // The maintained tree must equal a rebuild from history. This is the check
    // that makes the derived table safe: a writer that moved a head without
    // refreshing it would make `status` silently report committed files as
    // changed, and nothing else in the program would notice.
    let branches_with_heads: Vec<(String, String)> = {
        let mut stmt = conn.prepare("SELECT name, head FROM branches WHERE head IS NOT NULL")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (name, head) in &branches_with_heads {
        // Compare as sets of (path, hash). The row counts alone cannot tell a
        // stale tree from a fresh one: one row against one row with a different
        // hash is exactly the drift this is looking for.
        let rebuilt = crate::repo::tree_at(&conn, head)?;
        let mut stored: Vec<(String, String)> = {
            let mut stmt =
                conn.prepare("SELECT path, hash FROM branch_tree WHERE branch = ?1 ORDER BY path")?;
            let rows = stmt.query_map(rusqlite::params![name], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        stored.sort();
        let mut expected = rebuilt.clone();
        expected.sort();
        if stored != expected {
            let missing: Vec<&(String, String)> =
                expected.iter().filter(|e| !stored.contains(e)).collect();
            let extra: Vec<&(String, String)> =
                stored.iter().filter(|s| !expected.contains(s)).collect();
            println!(
                "STALE tree for branch {}: {} row(s) stored, {} in history ({} missing, {} extra)",
                name,
                stored.len(),
                expected.len(),
                missing.len(),
                extra.len()
            );
            for (p, h) in missing.iter().take(3) {
                println!("  missing {} {}", p, &h[..h.len().min(12)]);
            }
            for (p, h) in extra.iter().take(3) {
                println!("  extra   {} {}", p, &h[..h.len().min(12)]);
            }
            problems += 1;
        }
    }

    // Objects nothing references.
    let orphans: i64 = conn.query_row(
        "SELECT COUNT(*) FROM objects o WHERE NOT EXISTS (
            SELECT 1 FROM changeset_files cf WHERE cf.new_hash = o.hash OR cf.old_hash = o.hash
         ) AND NOT EXISTS (
            SELECT 1 FROM file_versions fv WHERE fv.hash = o.hash
         )",
        [],
        |r| r.get(0),
    )?;

    if problems == 0 {
        println!("repository is consistent ({} object(s), {} orphan(s))", count_objects(&conn)?, orphans);
    } else {
        println!("\n{} problem(s) found", problems);
    }

    if prune && orphans > 0 {
        let deleted = conn.execute(
            "DELETE FROM objects WHERE hash IN (
                SELECT o.hash FROM objects o WHERE NOT EXISTS (
                    SELECT 1 FROM changeset_files cf WHERE cf.new_hash = o.hash OR cf.old_hash = o.hash
                ) AND NOT EXISTS (
                    SELECT 1 FROM file_versions fv WHERE fv.hash = o.hash
                ))",
            [],
        )?;
        println!("pruned {} orphan object(s)", deleted);
    }

    if problems > 0 {
        return Err(bad(format!("{} integrity problem(s)", problems)));
    }
    Ok(())
}

fn count_objects(conn: &rusqlite::Connection) -> ZResult<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM objects", [], |r| r.get(0))?)
}

pub fn cmd_stats() -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let (raw, stored) = object_store_bytes(&conn)?;
    let objects = count_objects(&conn)?;
    let changesets: i64 = conn.query_row("SELECT COUNT(*) FROM changesets", [], |r| r.get(0))?;
    let versions: i64 = conn.query_row("SELECT COUNT(*) FROM file_versions", [], |r| r.get(0))?;
    let blame: i64 = conn.query_row("SELECT COUNT(*) FROM blame", [], |r| r.get(0))?;
    let branches: i64 = conn.query_row("SELECT COUNT(*) FROM branches", [], |r| r.get(0))?;
    let tags: i64 = conn.query_row("SELECT COUNT(*) FROM tags", [], |r| r.get(0))?;

    // Delta payloads live in their own table, so the object table alone
    // understates the real cost and reports an absurd compression ratio. The
    // footer adds them back, which is the number a person should read.
    let (deltas, saved) = delta_stats(&conn)?;
    let delta_bytes: i64 = conn
        .query_row("SELECT COALESCE(SUM(LENGTH(payload)), 0) FROM deltas", [], |r| r.get(0))
        .unwrap_or(0);
    let total_stored = stored + delta_bytes;
    let ratio = if raw > 0 { raw as f64 / total_stored.max(1) as f64 } else { 0.0 };
    println!(
        "objects:     {} ({} bytes raw, {} bytes in objects plus {} in deltas = {} stored, {:.1}x)",
        objects, raw, stored, delta_bytes, total_stored, ratio
    );
    println!("changesets:  {}", changesets);
    println!("versions:    {}", versions);
    println!("blame rows:  {}", blame);
    println!("branches:    {}", branches);
    println!("tags:        {}", tags);

    let cache_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM worktree_cache", [], |r| r.get(0))
        .unwrap_or(0);
    println!("stat cache:  {} path(s)", cache_count);

    if deltas > 0 {
        println!(
            "deltas:      {} object(s), at most {} bytes saved against raw size",
            deltas, saved
        );
    } else {
        println!("deltas:      0");
    }
    Ok(())
}

pub fn cmd_ignore_add(pattern: &str) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    conn.execute(
        "INSERT OR IGNORE INTO ignore_rules(pattern) VALUES(?1)",
        rusqlite::params![pattern],
    )?;
    println!("ignoring: {}", pattern);
    Ok(())
}

pub fn cmd_ignore_list() -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let stored: Vec<String> = {
        let mut stmt = conn.prepare("SELECT pattern FROM ignore_rules ORDER BY pattern")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut from_files: Vec<String> = Vec::new();
    for name in [".zenignore", ".gitignore"] {
        if let Ok(text) = fs::read_to_string(root.join(name)) {
            for line in text.lines() {
                let l = line.trim();
                if !l.is_empty() && !l.starts_with('#') {
                    from_files.push(format!("{} ({})", l, name));
                }
            }
        }
    }
    println!("from database:");
    if stored.is_empty() {
        println!("  (none)");
    }
    for p in &stored {
        println!("  {}", p);
    }
    println!("\nfrom files:");
    if from_files.is_empty() {
        println!("  (none)");
    }
    for p in &from_files {
        println!("  {}", p);
    }
    let lifted: Vec<String> = stored.into_iter().chain(
        from_files.iter().map(|s| s.split(" (").next().unwrap_or(s).to_string()),
    ).collect();
    let rules = IgnoreRules::with_defaults(&lifted);
    println!("\n{} active pattern(s)", rules.len());
    Ok(())
}

pub fn cmd_ignore_remove(pattern: &str) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let n = conn.execute(
        "DELETE FROM ignore_rules WHERE pattern = ?1",
        rusqlite::params![pattern],
    )?;
    if n == 0 {
        println!("{} was not in the database (it may come from .zenignore)", pattern);
    } else {
        println!("no longer ignoring: {}", pattern);
    }
    Ok(())
}


/// Point a remote-tracking branch at `head`. Pull merges this branch into the
/// current one, which is how a fetched history becomes part of local work
/// without pretending the remote head is already the local head.
fn set_tracking_branch(conn: &rusqlite::Connection, name: &str, head: &str) -> ZResult<()> {
    conn.execute(
        "INSERT INTO branches(name, head, created_at, from_branch) VALUES(?1, ?2, ?3, 'remote')
         ON CONFLICT(name) DO UPDATE SET head = excluded.head",
        rusqlite::params![name, head, now()],
    )?;
    Ok(())
}

/// Merge a fetched head into `branch` through a tracking branch.
fn merge_fetched(
    conn: &rusqlite::Connection,
    root: &Path,
    remote_name: &str,
    branch: &str,
    remote_head: &str,
    author: &str,
) -> ZResult<bool> {
    let tracking = format!("{}/{}", remote_name, branch);
    set_tracking_branch(conn, &tracking, remote_head)?;
    let out = repo::merge_branch(conn, root, &tracking, branch, author)?;
    if !out.conflicts.is_empty() {
        eprintln!("merge of {} into {} hit {} conflict(s):", tracking, branch, out.conflicts.len());
        for c in &out.conflicts {
            eprintln!("  {} (their version saved as {}.theirs)", c, c);
        }
        return Err(bad("pull needs a manual merge"));
    }
    Ok(!out.applied.is_empty())
}

/// Push over HTTP: ask the server which changesets it has, send only the rest.
fn push_http(
    conn: &rusqlite::Connection,
    root: &Path,
    remote_name: &str,
    url: &str,
    branch: &str,
) -> ZResult<()> {
    let base = url.trim_end_matches('/');
    let token = remote_token(conn, remote_name)?;

    // What does the remote already have? Asking first keeps the upload small;
    // the server also refuses a bundle that would move its branch backwards.
    let pin = remote_pin(conn, remote_name)?;
    let (status, body) = http::request_with_pin(
        "GET",
        &format!("{}/v1/branches", base),
        None,
        token.as_deref(),
        pin.as_deref(),
    )?;
    if status == 401 || status == 403 {
        return Err(bad(format!(
            "remote {} rejected the token ({}): {}",
            remote_name,
            status,
            String::from_utf8_lossy(&body).trim()
        )));
    }
    if status != 200 {
        return Err(bad(format!(
            "remote {} answered {}: {}",
            remote_name,
            status,
            String::from_utf8_lossy(&body).trim()
        )));
    }
    // The server tells us its head for this branch; everything reachable from it
    // is already there, so the bundle only carries the rest.
    let listing = String::from_utf8_lossy(&body).to_string();
    let mut have: HashSet<String> = HashSet::new();
    let remote_head = listing
        .lines()
        .find_map(|l| {
            let mut p = l.split('\t');
            match (p.next(), p.next()) {
                (Some(n), Some(h)) if n == branch && h != "-" => Some(h.to_string()),
                _ => None,
            }
        });
    if let Some(h) = &remote_head {
        have.insert(h.clone());
    }

    let bundle = remote::build_bundle(conn, branch, &have)?;
    let wire = bundle.encode()?;
    let object_count = bundle.objects.len();
    let changeset_count = bundle.changesets.len();

    let target = format!("{}/v1/bundle?branch={}", base, branch);
    let (status, body) =
        http::request_with_pin("POST", &target, Some(&wire), token.as_deref(), pin.as_deref())?;
    if status != 200 {
        return Err(bad(format!(
            "push rejected by {} ({}): {}",
            remote_name,
            status,
            String::from_utf8_lossy(&body).trim()
        )));
    }

    conn.execute(
        "INSERT INTO remote_refs(remote, branch, changeset_id, updated_at) VALUES(?1, ?2, ?3, ?4)
         ON CONFLICT(remote, branch) DO UPDATE SET changeset_id = excluded.changeset_id,
                                                   updated_at = excluded.updated_at",
        rusqlite::params![remote_name, branch, bundle.head.clone().unwrap_or_default(), now()],
    )?;

    // The local repository keeps its own working tree in sync with the branch
    // it just shared, so `push` followed by `status` is clean.
    repo::materialise(conn, root, branch)?;

    println!(
        "pushed {} changeset(s), {} object(s) of {} to {} over http",
        changeset_count, object_count, branch, remote_name
    );
    Ok(())
}

/// Pull over HTTP: download the server's bundle for `branch` and merge it.
fn pull_http(
    conn: &rusqlite::Connection,
    root: &Path,
    remote_name: &str,
    url: &str,
    branch: &str,
    author: &str,
) -> ZResult<()> {
    let base = url.trim_end_matches('/');
    let token = remote_token(conn, remote_name)?;
    let have: Vec<String> = ancestors(
        conn,
        &branch_head(conn, branch)?.unwrap_or_else(|| "none".into()),
    )
    .unwrap_or_default();
    let have: Vec<String> = if have == vec!["none".to_string()] { Vec::new() } else { have };

    let target = http::bundle_url(base, branch, &have);
    let pin = remote_pin(conn, remote_name)?;
    let (status, body) =
        http::request_with_pin("GET", &target, None, token.as_deref(), pin.as_deref())?;
    if status != 200 {
        return Err(bad(format!(
            "cannot fetch branch {} from {} ({}): {}",
            branch,
            remote_name,
            status,
            String::from_utf8_lossy(&body).trim()
        )));
    }

    let bundle = remote::Bundle::decode(&body)?;
    let remote_head = bundle
        .head
        .clone()
        .ok_or_else(|| bad(format!("remote {} returned no head for {}", remote_name, branch)))?;
    // Fetch the objects but keep the local branch: the merge below decides the
    // new head. Moving it here would discard local changesets.
    let applied = remote::apply_bundle_opts(conn, &bundle, false)?;

    let current = current_branch(conn)?;
    if current == branch && branch_head(conn, &current)?.as_deref() != Some(remote_head.as_str()) {
        merge_fetched(conn, root, remote_name, branch, &remote_head, author)?;
    }
    repo::materialise(conn, root, &current)?;
    ops::record(conn, "pull", Some(&remote_head), Some(remote_name))?;
    println!("pulled {} changeset(s) from {} ({}) into {}", applied, remote_name, base, branch);
    Ok(())
}

/// Clone over HTTP: ask for the branch list, fetch the first branch.
fn clone_http(url: &str, target: &Path, token: Option<&str>, pin: Option<&str>) -> ZResult<()> {
    let base = url.trim_end_matches('/');

    let (status, body) =
        http::request_with_pin("GET", &format!("{}/v1/branches", base), None, token, pin)?;
    if status != 200 {
        return Err(bad(format!(
            "cannot list branches at {} ({}): {}",
            base,
            status,
            String::from_utf8_lossy(&body).trim()
        )));
    }
    let listing = String::from_utf8_lossy(&body).to_string();
    let branch = listing
        .lines()
        .find_map(|l| l.split('\t').next().filter(|s| !s.is_empty()).map(|s| s.to_string()))
        .ok_or_else(|| bad(format!("{} has no branches", base)))?;

    let (status, body) =
        http::request_with_pin("GET", &http::bundle_url(base, &branch, &[]), None, token, pin)?;
    if status != 200 {
        return Err(bad(format!(
            "cannot fetch {} from {} ({}): {}",
            branch,
            base,
            status,
            String::from_utf8_lossy(&body).trim()
        )));
    }
    let bundle = remote::Bundle::decode(&body)?;

    let zf = repo_dir(target);
    fs::create_dir_all(&zf)?;
    let conn = open_db(target)?;
    init_schema(&conn)?;
    remote::apply_bundle(&conn, &bundle)?;
    set_state(&conn, "current_branch", &branch)?;
    conn.execute(
        "INSERT INTO remotes(name, url, token) VALUES('origin', ?1, ?2)
         ON CONFLICT(name) DO UPDATE SET url = excluded.url, token = excluded.token",
        rusqlite::params![url, token],
    )?;
    let (written, _removed) = repo::restore_worktree(&conn, target, &branch)?;
    println!(
        "cloned {} into {} (branch {}, {} changeset(s), {} file(s))",
        base,
        target.display(),
        branch,
        bundle.changesets.len(),
        written
    );
    Ok(())
}

/// Serve this repository over HTTP.
///
/// Tokens come from `.gesta/auth`, one `token:role:name` per line. Without
/// that file the server only binds to loopback, so an open port with no
/// password is never the accidental default.
pub fn cmd_serve(
    bind: &str,
    port: u16,
    tls: Option<&str>,
    #[allow(unused_variables)] key: Option<&str>,
) -> ZResult<()> {
    let (root, _conn) = open_repo()?;
    let auth = auth::Auth::from_entries(&read_auth_file(&root)?);

    // Without the tls feature the flags still parse, so the error names the
    // cause. A build that silently ignores --tls would be the worst outcome: the
    // operator would believe the port is encrypted.
    #[cfg(not(feature = "tls"))]
    if tls.is_some() {
        return Err(bad(
            "this build has no TLS support: rebuild with --features tls",
        ));
    }

    match tls {
        None => http::serve(root, bind.to_string(), port, auth),
        #[cfg(feature = "tls")]
        Some("self-signed") => {
            // A generated certificate is for a private network. Writing it under
            // The repository directory keeps it with the code and out of a changeset,
            // since that directory is never tracked.
            let dir = repo_dir(&root);
            let (cert, key_path, fp) = crate::tls::generate_self_signed(bind, &dir)?;
            println!("generated {} and {}", cert.display(), key_path.display());
            println!("pin this on clients with --pin {}", fp);
            http::serve_tls(root, bind.to_string(), port, auth, &cert, &key_path)
        }
        #[cfg(feature = "tls")]
        Some(cert_path) => {
            let key_path = key
                .map(std::path::PathBuf::from)
                .ok_or_else(|| bad("--tls <cert> needs --key <key.pem>"))?;
            http::serve_tls(
                root,
                bind.to_string(),
                port,
                auth,
                &std::path::PathBuf::from(cert_path),
                &key_path,
            )
        }
        // The early return above covers every Some(_) in a build without the
        // feature. Restating it as an error keeps the invariant visible: if that
        // early return is ever moved, this is a message instead of a crash.
        #[cfg(not(feature = "tls"))]
        Some(_) => Err(bad(
            "this build has no TLS support: rebuild with --features tls",
        )),
    }
}

fn read_auth_file(root: &Path) -> ZResult<Vec<String>> {
    let path = repo_dir(root).join("auth");
    match fs::read_to_string(&path) {
        Ok(text) => Ok(text.lines().map(|l| l.to_string()).collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(bad(format!("cannot read {}: {}", path.display(), e))),
    }
}

/// Add a token to `.gesta/auth`. The token is generated, never typed by the
/// user, and printed once.
pub fn cmd_token_add(role: &str, name: &str, hashed: bool) -> ZResult<()> {
    let (root, _conn) = open_repo()?;
    let role = match role.to_ascii_lowercase().as_str() {
        "write" => "write",
        "read" => "read",
        other => return Err(bad(format!("unknown role {:?}: use read or write", other))),
    };
    if name.trim().is_empty() || name.contains(':') {
        return Err(bad("token name must be non-empty and must not contain ':'"));
    }
    let token = auth::generate_token(32)?;
    let path = repo_dir(&root).join("auth");
    let mut existing = read_auth_file(&root)?;

    // `--hashed` writes only the digest: the file then cannot be replayed as a
    // credential if it leaks, at the cost of not being able to read the token
    // back from disk.
    let entry = if hashed {
        format!("sha256:{}:{}:{}", auth::digest(&token), role, name.trim())
    } else {
        format!("{}:{}:{}", token, role, name.trim())
    };
    existing.push(entry);
    fs::write(&path, existing.join("\n") + "\n")?;

    println!("created {} token for {:?}", role, name.trim());
    println!("{}", token);
    if hashed {
        println!("\nStored as a SHA-256 digest: this token cannot be recovered from the file.");
    } else {
        println!("\nStored in clear in .gesta/auth, which is never tracked.");
    }
    println!("Give it to the client once. `token list` shows only the last four characters.");
    Ok(())
}

/// Remove a token by name or by its last four characters.
pub fn cmd_token_remove(selector: &str) -> ZResult<()> {
    let (root, _conn) = open_repo()?;
    let path = repo_dir(&root).join("auth");
    let entries = read_auth_file(&root)?;
    if entries.is_empty() {
        return Err(bad("no tokens configured"));
    }

    let mut kept: Vec<String> = Vec::new();
    let mut removed = 0usize;
    for line in entries {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            kept.push(line);
            continue;
        }
        // Both forms end with `<role>:<name>`, so the name is the last field
        // either way; only the identifier before it differs.
        let (name, id) = if let Some(rest) = l.strip_prefix("sha256:") {
            let mut parts = rest.splitn(3, ':');
            let digest = parts.next().unwrap_or("").to_string();
            let _role = parts.next();
            (parts.next().unwrap_or("").to_string(), digest)
        } else {
            let mut parts = l.splitn(3, ':');
            let token = parts.next().unwrap_or("").to_string();
            let _role = parts.next();
            (parts.next().unwrap_or("").to_string(), token)
        };
        let tail: String = id.chars().rev().take(4).collect::<String>().chars().rev().collect();
        if name == selector || tail == selector {
            removed += 1;
            continue;
        }
        kept.push(line);
    }

    if removed == 0 {
        return Err(bad(format!("no token matches {:?}", selector)));
    }
    fs::write(&path, kept.join("\n") + if kept.is_empty() { "" } else { "\n" })?;
    println!("removed {} token(s) matching {:?}", removed, selector);
    println!("Restart `gesta serve` for the change to take effect.");
    Ok(())
}

/// List configured tokens without revealing them.
pub fn cmd_token_list() -> ZResult<()> {
    let (root, _conn) = open_repo()?;
    let entries = read_auth_file(&root)?;
    let mut shown = 0;
    for line in entries {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let (role, name, id) = if let Some(rest) = l.strip_prefix("sha256:") {
            let mut parts = rest.splitn(3, ':');
            let digest = parts.next().unwrap_or("");
            (
                parts.next().unwrap_or("read").to_string(),
                parts.next().unwrap_or("unnamed").to_string(),
                digest.to_string(),
            )
        } else {
            let mut parts = l.splitn(3, ':');
            let token = parts.next().unwrap_or("").to_string();
            (
                parts.next().unwrap_or("read").to_string(),
                parts.next().unwrap_or("unnamed").to_string(),
                token,
            )
        };
        let tail: String = id.chars().rev().take(4).collect::<String>().chars().rev().collect();
        let kind = if l.starts_with("sha256:") { "hashed" } else { "clear" };
        println!("{:<10} {:<18} {:<7} ...{}", role, name, kind, tail);
        shown += 1;
    }
    if shown == 0 {
        println!("no tokens configured; the server only binds to loopback");
    }
    Ok(())
}
