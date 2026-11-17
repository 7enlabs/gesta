//! Handlers for remotes, integrity checks, statistics and ignore rules.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use rusqlite::OptionalExtension;

use crate::ops;
use crate::remote;
use crate::repo;
use crate::store::{
    ancestors, branch_head, current_branch, get_object, init_schema, object_store_bytes, open_db,
    set_state,
};
use crate::worktree::IgnoreRules;
use crate::{bad, now, open_repo, ZResult, ZF_DIR};

// remotes.

pub fn cmd_remote_add(name: &str, url: &str) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    conn.execute(
        "INSERT INTO remotes(name, url) VALUES(?1, ?2)
         ON CONFLICT(name) DO UPDATE SET url = excluded.url",
        rusqlite::params![name, url],
    )?;
    println!("remote {} -> {}", name, url);
    Ok(())
}

pub fn cmd_remote_list() -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let all: Vec<(String, String)> = {
        let mut stmt = conn.prepare("SELECT name, url FROM remotes ORDER BY name")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    if all.is_empty() {
        println!("no remotes configured");
    }
    for (n, u) in all {
        println!("{:<10} {}", n, u);
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

pub fn cmd_push(remote_name: &str, branch: &str) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let url = remote_url(&conn, remote_name)?;
    let path = remote::resolve_remote(&url);

    // The remote must already exist as a bare repository.
    if !path.join(ZF_DIR).is_dir() {
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
    let path = remote::resolve_remote(&url);

    let remote_conn = open_db(&path)?;
    let remote_head = branch_head(&remote_conn, branch)?
        .ok_or_else(|| bad(format!("remote {} has no branch {}", remote_name, branch)))?;

    let bundle = remote::build_bundle(&remote_conn, branch, &HashSet::new())?;
    let received = remote::Bundle::decode(&bundle.encode()?)?;
    let applied = remote::apply_bundle(&conn, &received)?;

    let current = current_branch(&conn)?;
    if current == branch {
        // Merge the fetched history into the current branch properly.
        let local_head = branch_head(&conn, &current)?;
        if local_head.as_deref() != Some(remote_head.as_str()) {
            let out = repo::merge_branch(&conn, &root, branch, &current, author)?;
            if !out.conflicts.is_empty() {
                eprintln!("pull fetched {} changeset(s) but merging them conflicted:", applied);
                for c in &out.conflicts {
                    eprintln!("  {}", c);
                }
                return Err(bad("pull needs a manual merge"));
            }
        }
    }
    ops::record(&conn, "pull", Some(&remote_head), Some(remote_name))?;
    println!("pulled {} changeset(s) from {} into {}", applied, remote_name, branch);
    Ok(())
}

pub fn cmd_clone(url: &str, dir: &str) -> ZResult<()> {
    let target = Path::new(dir);
    if target.join(ZF_DIR).is_dir() {
        return Err(bad(format!("{} already contains a zenforge repository", dir)));
    }
    fs::create_dir_all(target)?;

    let remote_path = remote::resolve_remote(url);
    if !remote_path.join(ZF_DIR).is_dir() {
        return Err(bad(format!("no zenforge repository at {}", remote_path.display())));
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

    let zf = target.join(ZF_DIR);
    fs::create_dir_all(&zf)?;
    let conn = open_db(target)?;
    init_schema(&conn)?;
    remote::apply_bundle(&conn, &received)?;
    set_state(&conn, "current_branch", &branch)?;
    conn.execute(
        "INSERT INTO remotes(name, url) VALUES('origin', ?1)
         ON CONFLICT(name) DO UPDATE SET url = excluded.url",
        rusqlite::params![url],
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

    let ratio = if raw > 0 { raw as f64 / stored.max(1) as f64 } else { 0.0 };
    println!("objects:     {} ({} bytes raw, {} bytes stored, {:.1}x compression)", objects, raw, stored, ratio);
    println!("changesets:  {}", changesets);
    println!("versions:    {}", versions);
    println!("blame rows:  {}", blame);
    println!("branches:    {}", branches);
    println!("tags:        {}", tags);
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

