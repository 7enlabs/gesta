//! Command handlers. Kept out of `main.rs` so the entry point stays a thin
//! dispatcher and no single file grows past the point of review.

use std::fs;

use rusqlite::OptionalExtension;

use crate::merge;
use crate::ops;
use crate::history;
use crate::json::Json;
use crate::mcp;
use crate::repo;
use crate::store::{self, branch_head, current_branch, get_object, hash_at_point, init_schema,
    open_db, reachable_from, set_state};
use crate::worktree;
use crate::{bad, fmt_ts, normalise, now, open_repo, repo_root, ZResult, ZF_DIR};

// init / status / diff / log.


/// `sparse set|list|clear`.
pub fn cmd_sparse(sub: crate::SparseCmd) -> ZResult<()> {
    match sub {
        crate::SparseCmd::List => {
            let (_root, conn) = open_repo()?;
            let scope = crate::sparse::SparseScope::load(&conn)?;
            if scope.is_empty() {
                println!("working set: everything (no sparse scope)");
            } else {
                println!("working set: {} pattern(s)", scope.patterns.len());
                for p in &scope.patterns {
                    println!("  {}", p);
                }
            }
            Ok(())
        }
        crate::SparseCmd::Set { patterns } => {
            let (root, conn) = open_repo()?;
            let scope = crate::sparse::SparseScope { patterns: patterns.clone() };
            if scope.is_empty() {
                return Err(bad("give at least one pattern, or use 'sparse clear'"));
            }
            let branch = current_branch(&conn)?;
            let (written, kept) = store::with_retry(|| {
                scope.checkout_sparse(&conn, &root, &branch)
            })?;
            scope.save(&conn)?;
            println!(
                "working set: {} pattern(s). {} file(s) written, {} left in the branch but not checked out.",
                scope.patterns.len(),
                written,
                kept
            );
            println!("Files outside the scope are not treated as deleted.");
            Ok(())
        }
        crate::SparseCmd::Clear => {
            let (root, conn) = open_repo()?;
            crate::sparse::SparseScope::clear(&conn)?;
            let branch = current_branch(&conn)?;
            let n = repo::materialise(&conn, &root, &branch)?;
            println!("working set: everything. {} file(s) restored.", n);
            Ok(())
        }
    }
}

/// Run the MCP server. stdout belongs to the protocol, so nothing else may
/// write to it while this runs.
pub fn cmd_mcp() -> ZResult<()> {
    let root = repo_root()?;
    mcp::serve_stdio(&root)
}

pub fn cmd_init(bare: bool) -> ZResult<()> {
    let root = std::env::current_dir()?;
    let zf = root.join(ZF_DIR);
    if zf.is_dir() {
        println!("already a zenforge repository: {}", zf.display());
        return Ok(());
    }
    fs::create_dir_all(&zf)?;
    let conn = open_db(&root)?;
    init_schema(&conn)?;
    conn.execute(
        "INSERT OR IGNORE INTO branches(name, head, created_at, from_branch) VALUES('main', NULL, ?1, NULL)",
        rusqlite::params![now()],
    )?;
    set_state(&conn, "current_branch", "main")?;
    set_state(&conn, "created_at", &now().to_string())?;
    if bare {
        // A bare repository keeps the database in the current directory and no
        // working tree: the scanner refuses to track anything.
        set_state(&conn, "bare", "1")?;
    }
    println!("initialized zenforge repository at {}", zf.display());
    println!("branch: main (no changesets yet)");
    Ok(())
}

/// Show renames recorded by a changeset, so `changeset diff` can label them.
pub fn print_renames(conn: &rusqlite::Connection, id: &str) -> ZResult<()> {
    for r in history::detect_renames(conn, id, 0.5)? {
        println!(
            "  renamed {} -> {} ({:.0}% similar)",
            r.from,
            r.to,
            r.similarity * 100.0
        );
    }
    Ok(())
}

pub fn cmd_status() -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let branch = current_branch(&conn)?;

    println!("On branch {}", branch);
    match branch_head(&conn, &branch)? {
        Some(h) => println!("HEAD: {}", h),
        None => println!("HEAD: (empty branch)"),
    }

    let total: i64 = conn.query_row("SELECT COUNT(*) FROM changesets", [], |r| r.get(0))?;
    let pending: i64 = conn.query_row(
        "SELECT COUNT(*) FROM changesets WHERE status = 'pending'",
        [],
        |r| r.get(0),
    )?;
    println!("changesets: {} total, {} pending", total, pending);

    let d = repo::diff_worktree(&conn, &root, &branch)?;

    if d.added.is_empty() && d.modified.is_empty() && d.deleted.is_empty() {
        println!("working tree clean ({} files tracked)", d.unchanged);
    } else {
        if !d.added.is_empty() {
            println!("\nnew files ({}):", d.added.len());
            for f in &d.added {
                println!("  A {}", f);
            }
        }
        if !d.modified.is_empty() {
            println!("\nmodified ({}):", d.modified.len());
            for f in &d.modified {
                println!("  M {}", f);
            }
        }
        if !d.deleted.is_empty() {
            println!("\ndeleted ({}):", d.deleted.len());
            for f in &d.deleted {
                println!("  D {}", f);
            }
        }
    }

    let scope = crate::sparse::SparseScope::load(&conn)?;
    if !scope.is_empty() {
        println!(
            "\nsparse: {} pattern(s), files outside it are not treated as deleted",
            scope.patterns.len()
        );
        for p in &scope.patterns {
            println!("  {}", p);
        }
    }

    if d.cached + d.rehashed > 0 {
        println!(
            "\nscan: {} from the stat cache, {} re-hashed",
            d.cached, d.rehashed
        );
    }

    if !d.skipped_binary.is_empty() {
        println!("\nskipped (binary, {} file(s)):", d.skipped_binary.len());
        for f in d.skipped_binary.iter().take(10) {
            println!("  ? {}", f);
        }
        if d.skipped_binary.len() > 10 {
            println!("  ... and {} more", d.skipped_binary.len() - 10);
        }
    }
    Ok(())
}

pub fn cmd_diff(changeset: Option<&str>, name_only: bool) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let branch = current_branch(&conn)?;

    match changeset {
        Some(cs) => {
            let diffs = repo::changeset_diff(&conn, cs)?;
            if diffs.is_empty() {
                println!("changeset {} changes no files", cs);
            }
            for (path, removed, added) in diffs {
                if name_only {
                    println!("{}", path);
                    continue;
                }
                println!("\n--- {}", path);
                for l in &removed {
                    println!("-{}", l);
                }
                for l in &added {
                    println!("+{}", l);
                }
            }
        }
        None => {
            // Only the files whose stat entry changed get read and hashed.
            // Scanning and reading every file made `diff` 16x slower than git on
            // a 2000 file repository, because one edited file still meant 2000
            // reads. diff_worktree is the same path `status` uses, so the two
            // commands cannot disagree about what changed either.
            let d = repo::diff_worktree(&conn, &root, &branch)?;
            let mut changed: Vec<String> = Vec::new();
            changed.extend(d.added.iter().cloned());
            changed.extend(d.modified.iter().cloned());
            changed.sort();

            let mut any = false;
            for rel in changed {
                let full = root.join(&rel);
                let Some(content) = worktree::read_text(&full) else {
                    continue;
                };
                let old_hash = store::latest_hash(&conn, &rel, &branch)?;
                any = true;
                if name_only {
                    println!("{}", rel);
                    continue;
                }
                let old_text = match &old_hash {
                    Some(h) => get_object(&conn, h)?,
                    None => String::new(),
                };
                println!("\n--- {}", rel);
                let (removed, added) = merge::line_diff(&old_text, &content);
                for l in &removed {
                    println!("-{}", l);
                }
                for l in &added {
                    println!("+{}", l);
                }
            }
            if !any {
                println!("no changes");
            }
        }
    }
    Ok(())
}

pub fn cmd_log(limit: usize, oneline: bool) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let all: Vec<(String, String, String, i64, String, Option<String>)> = {
        let mut stmt = conn.prepare(
            "SELECT id, message, author, created_at, status, parent FROM changesets
             ORDER BY created_at DESC, rowid DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut n = 0;
    for (id, msg, author, ts, status, parent) in all {
        if oneline {
            println!("{:<14} {} {}", &id[..id.len().min(14)], fmt_ts(ts), msg);
        } else {
            println!("changeset {:<14}", id);
            println!("Author:  {}", author);
            println!("Date:    {}", fmt_ts(ts));
            println!("Status:  {}", status);
            if let Some(p) = parent {
                println!("Parent:  {}", p);
            }
            for line in msg.lines() {
                println!("    {}", line);
            }
            println!();
        }
        n += 1;
    }
    if n == 0 {
        println!("no changesets yet");
    }
    Ok(())
}

// changesets.

pub fn cmd_changeset_create(message: &str, author: &str) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let branch = current_branch(&conn)?;

    match store::with_retry(|| repo::create_changeset(&conn, &root, message, author))? {
        None => {
            println!("nothing to snapshot: working tree matches branch {}", branch);
            Ok(())
        }
        Some(cs) => {
            println!("created changeset {} ({} file(s), branch {})", cs.id, cs.files.len(), branch);
            for (rel, old, _) in &cs.files {
                println!("  {} {}", if old.is_none() { "A" } else { "M" }, rel);
            }
            for rel in &cs.deleted {
                println!("  D {}", rel);
            }
            if !cs.skipped.is_empty() {
                println!("\nskipped {} binary file(s):", cs.skipped.len());
                for f in cs.skipped.iter().take(10) {
                    println!("  ? {}", f);
                }
            }
            println!("\nnext: zenforge changeset commit {}", cs.id);
            Ok(())
        }
    }
}

pub fn cmd_changeset_commit(id: &str) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let n = store::with_retry(|| repo::commit_one(&conn, id))?;
    if n == 0 {
        println!("changeset {} already committed", id);
    } else {
        let branch = current_branch(&conn)?;
        println!("committed {} on branch {} ({} file(s))", id, branch, n);
        ops::record(&conn, "commit", Some(id), None)?;
    }
    Ok(())
}

pub fn cmd_changeset_apply(id: &str) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let cutoff: Option<i64> = conn
        .query_row(
            "SELECT created_at FROM changesets WHERE id = ?1",
            rusqlite::params![id],
            |r| r.get::<_, i64>(0),
        )
        .optional()?;
    let cutoff = cutoff.ok_or_else(|| bad(format!("no such changeset: {}", id)))?;

    let ids: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT id FROM changesets WHERE status = 'pending' AND created_at <= ?1
             ORDER BY created_at, rowid",
        )?;
        let rows = stmt.query_map(rusqlite::params![cutoff], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    if ids.is_empty() {
        println!("no pending changesets to apply at or before {}", id);
        return Ok(());
    }
    let mut total = 0usize;
    for cs in &ids {
        total += store::with_retry(|| repo::commit_one(&conn, cs))?;
    }
    ops::record(&conn, "changeset apply", Some(id), None)?;
    println!("applied {} changeset(s) up to {} ({} file(s))", ids.len(), id, total);
    Ok(())
}

pub fn cmd_changeset_commit_all() -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let ids: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT id FROM changesets WHERE status = 'pending' ORDER BY created_at, rowid",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    if ids.is_empty() {
        println!("no pending changesets");
        return Ok(());
    }
    let mut total = 0usize;
    for cs in &ids {
        total += store::with_retry(|| repo::commit_one(&conn, cs))?;
    }
    ops::record(&conn, "commit all", None, Some(&format!("{} changesets", ids.len())))?;
    println!("committed {} changeset(s) ({} file(s))", ids.len(), total);
    Ok(())
}

pub fn cmd_changeset_diff(id: &str) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let msg: Option<String> = conn
        .query_row(
            "SELECT message FROM changesets WHERE id = ?1",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .optional()?;
    println!(
        "changeset {}: {}",
        id,
        msg.ok_or_else(|| bad(format!("no such changeset: {}", id)))?
    );
    print_renames(&conn, id)?;

    for (path, removed, added) in repo::changeset_diff(&conn, id)? {
        println!("\n--- {}", path);
        for l in &removed {
            println!("-{}", l);
        }
        for l in &added {
            println!("+{}", l);
        }
    }
    Ok(())
}

pub fn cmd_changeset_list() -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let all: Vec<(String, String, String, i64)> = {
        let mut stmt = conn.prepare(
            "SELECT id, message, author, created_at FROM changesets
             WHERE status = 'pending' ORDER BY created_at, rowid",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    if all.is_empty() {
        println!("no pending changesets");
        return Ok(());
    }
    for (id, msg, author, ts) in all {
        println!("{:<14} {} {}  {}", id, fmt_ts(ts), author, msg);
    }
    Ok(())
}

// blame / history / show / checkout / restore.

pub fn cmd_revert(id: &str, reason: Option<&str>, author: &str) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let revert_id = store::with_retry(|| history::revert_changeset(&conn, &root, id, author, reason))?;
    ops::record(&conn, "revert", Some(&revert_id), Some(id))?;
    println!("reverted {} as {}", id, revert_id);
    println!("\nThe revert is a normal changeset: `zenforge log` shows both, and");
    println!("reverting the revert restores the change.");
    Ok(())
}

pub fn cmd_blame(file: &str) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let rel = normalise(file);

    let content = match worktree::read_text(&root.join(&rel)) {
        Some(c) => c,
        None => {
            let branch = current_branch(&conn)?;
            let h = store::latest_hash(&conn, &rel, &branch)?
                .ok_or_else(|| bad(format!("file not found: {}", rel)))?;
            get_object(&conn, &h)?
        }
    };

    let rows: Vec<(i64, String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT line_no, changeset_id, author FROM blame WHERE path = ?1 ORDER BY line_no",
        )?;
        let rows = stmt.query_map(rusqlite::params![rel], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    if rows.is_empty() {
        println!("no blame data for {} (never committed)", rel);
        return Ok(());
    }
    let map: std::collections::HashMap<i64, (String, String)> =
        rows.into_iter().map(|(n, c, a)| (n, (c, a))).collect();

    let mut unattributed = 0usize;
    for (i, line) in content.lines().enumerate() {
        let n = (i + 1) as i64;
        match map.get(&n) {
            Some((cs, author)) => println!("{:<14} {:<10} {:>4} | {}", cs, author, n, line),
            None => {
                unattributed += 1;
                println!("{:<14} {:<10} {:>4} | {}", "?", "?", n, line)
            }
        }
    }
    if unattributed > 0 {
        eprintln!("warning: {} line(s) have no attributed changeset", unattributed);
    }
    Ok(())
}

pub fn cmd_history(file: &str) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let rel = normalise(file);

    // Follow renames backwards so a file that was moved still shows the history
    // it earned under its old name.
    let chain = history::rename_history(&conn, &rel, 10)?;

    let mut all: Vec<(String, i64, String, i64, String, String)> = Vec::new();
    for name in &chain {
        let mut stmt = conn.prepare(
            "SELECT fv.path, fv.version, fv.hash, fv.created_at, cs.message, cs.author
             FROM file_versions fv JOIN changesets cs ON cs.id = fv.changeset_id
             WHERE fv.path = ?1 ORDER BY fv.version DESC",
        )?;
        let rows = stmt.query_map(rusqlite::params![name], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
            ))
        })?;
        for r in rows {
            all.push(r?);
        }
    }

    if all.is_empty() {
        println!("no history for {}", rel);
        return Ok(());
    }
    if chain.len() > 1 {
        println!("following renames: {}", chain.join(" <- "));
    }
    for (name, ver, hash, ts, msg, author) in all {
        println!(
            "v{:<3} {} {} {:<10} {:<20} {}",
            ver,
            &hash[..8.min(hash.len())],
            fmt_ts(ts),
            author,
            name,
            msg
        );
    }
    Ok(())
}

pub fn cmd_show(spec: &str) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let (rev, path) = match spec.split_once(':') {
        Some((r, p)) => (r.to_string(), normalise(p)),
        None => (current_branch(&conn)?, normalise(spec)),
    };
    let cs = store::resolve_rev(&conn, &rev)?
        .ok_or_else(|| bad(format!("no such branch, tag or changeset: {}", rev)))?;
    let hash = hash_at_point(&conn, &path, &cs)?
        .ok_or_else(|| bad(format!("{} does not exist at {}", path, rev)))?;
    print!("{}", get_object(&conn, &hash)?);
    Ok(())
}

pub fn cmd_checkout(file: &str, changeset: Option<&str>) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    if file == "." {
        let branch = current_branch(&conn)?;
        let (written, removed) = repo::restore_worktree(&conn, &root, &branch)?;
        println!("restored {} file(s), removed {} stale file(s)", written, removed);
        return Ok(());
    }
    let rel = normalise(file);
    let bytes = repo::checkout_file(&conn, &root, &rel, changeset)?;
    println!("checked out {} ({} bytes)", rel, bytes);
    Ok(())
}

pub fn cmd_restore(file: &str) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let branch = current_branch(&conn)?;
    if file == "." {
        let (written, removed) = repo::restore_worktree(&conn, &root, &branch)?;
        println!("restored {} file(s), removed {} stale file(s)", written, removed);
        return Ok(());
    }
    let rel = normalise(file);
    let bytes = repo::checkout_file(&conn, &root, &rel, None)?;
    println!("restored {} from branch {} ({} bytes)", rel, branch, bytes);
    Ok(())
}

pub fn cmd_undo() -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let msg = ops::undo(&conn)?;
    println!("{}", msg);
    Ok(())
}

pub fn cmd_ops(limit: usize) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let ops = ops::list(&conn, limit)?;
    if ops.is_empty() {
        println!("no operations recorded");
        return Ok(());
    }
    for o in ops {
        println!(
            "{:>5}  {}  {:<16} {}{}",
            o.id,
            fmt_ts(o.created_at),
            o.kind,
            o.target.unwrap_or_default(),
            o.detail.map(|d| format!(" ({})", d)).unwrap_or_default()
        );
    }
    Ok(())
}

pub fn cmd_tag(name: &str, changeset: Option<&str>, list: bool) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    if list || name == "--list" {
        let tags: Vec<(String, String, i64)> = {
            let mut stmt = conn.prepare("SELECT name, changeset_id, created_at FROM tags ORDER BY name")?;
            let rows = stmt.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        if tags.is_empty() {
            println!("no tags");
        }
        for (n, cs, ts) in tags {
            println!("{:<20} {} {}", n, cs, fmt_ts(ts));
        }
        return Ok(());
    }
    let target = match changeset {
        Some(c) => c.to_string(),
        None => {
            let branch = current_branch(&conn)?;
            branch_head(&conn, &branch)?.ok_or_else(|| bad("branch has no changesets to tag"))?
        }
    };
    conn.execute(
        "INSERT INTO tags(name, changeset_id, message, created_at) VALUES(?1, ?2, NULL, ?3)
         ON CONFLICT(name) DO UPDATE SET changeset_id = excluded.changeset_id",
        rusqlite::params![name, target, now()],
    )?;
    ops::record(&conn, "tag", Some(name), None)?;
    println!("tagged {} at {}", name, target);
    Ok(())
}

// branches.

pub fn cmd_branch_create(name: &str, from: &str) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let exists: Option<String> = conn
        .query_row("SELECT name FROM branches WHERE name = ?1", rusqlite::params![name], |r| r.get(0))
        .optional()?;
    if exists.is_some() {
        println!("branch {} already exists", name);
        return Ok(());
    }
    let head = branch_head(&conn, from)?;
    conn.execute(
        "INSERT INTO branches(name, head, created_at, from_branch) VALUES(?1, ?2, ?3, ?4)",
        rusqlite::params![name, head, now(), from],
    )?;
    ops::record(&conn, "branch create", Some(name), Some(from))?;
    println!("created branch {} from {} at {}", name, from, head.unwrap_or_else(|| "(empty)".into()));
    Ok(())
}

pub fn cmd_branch_list() -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let cur = current_branch(&conn)?;
    let all: Vec<(String, Option<String>, i64)> = {
        let mut stmt = conn.prepare("SELECT name, head, created_at FROM branches ORDER BY created_at")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (name, head, ts) in all {
        let mark = if name == cur { "*" } else { " " };
        match head {
            Some(h) => println!("{} {:<16} head={} created={}", mark, name, h, fmt_ts(ts)),
            None => println!("{} {:<16} (empty)      created={}", mark, name, fmt_ts(ts)),
        }
    }
    Ok(())
}

pub fn cmd_branch_switch(name: &str) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let target = branch_head(&conn, name)?;
    set_state(&conn, "current_branch", name)?;
    // Switching branches must make the working tree match the new head, or the
    // next status would report every file as modified.
    let (written, removed) = repo::restore_worktree(&conn, &root, name)?;
    ops::record(&conn, "branch switch", Some(name), None)?;
    println!(
        "switched to branch {} ({} file(s) materialised, {} removed, head {})",
        name,
        written,
        removed,
        target.unwrap_or_else(|| "(empty)".into())
    );
    Ok(())
}

pub fn cmd_branch_delete(name: &str) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let cur = current_branch(&conn)?;
    if name == cur {
        return Err(bad(format!("cannot delete the branch you are on ({})", name)));
    }
    let exists = branch_head(&conn, name).is_ok();
    if !exists {
        return Err(bad(format!("no such branch: {}", name)));
    }
    conn.execute("DELETE FROM branches WHERE name = ?1", rusqlite::params![name])?;
    ops::record(&conn, "branch delete", Some(name), None)?;
    println!("deleted branch {}", name);
    Ok(())
}

/// Print the conflicts of a merge attempt as JSON.
pub fn cmd_branch_merge_json(source: &str, target: &str, author: &str) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let out = store::with_retry(|| repo::merge_branch(&conn, &root, source, target, author))?;
    emit(crate::json::Json::obj(vec![
        (
            "merged",
            crate::json::Json::Bool(out.merge_changeset.is_some()),
        ),
        (
            "changeset",
            out.merge_changeset
                .clone()
                .map(crate::json::Json::str)
                .unwrap_or(crate::json::Json::Null),
        ),
        (
            "applied",
            crate::json::Json::arr(out.applied.iter().cloned().map(crate::json::Json::str).collect()),
        ),
        ("conflicts", crate::conflict::reports_to_json(&out.reports)),
    ]));
    // A conflicted merge is still a failure for a script, so keep the exit code.
    if out.merge_changeset.is_none() && !out.reports.is_empty() {
        return Err(bad("merge produced conflicts"));
    }
    Ok(())
}

/// Report the conflicts of a merge without performing it (dry run).
///
/// An agent asks this first, decides how to resolve, then writes its answer.
pub fn cmd_merge_preview(source: &str, target: &str) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let out = repo::merge_preview(&conn, &root, source, target)?;
    emit(crate::conflict::reports_to_json(&out));
    Ok(())
}

pub fn cmd_branch_merge(source: &str, target: &str, author: &str) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let out = store::with_retry(|| repo::merge_branch(&conn, &root, source, target, author))?;

    if !out.conflicts.is_empty() {
        eprintln!(
            "merge stopped: {} conflicted file(s). Nothing was committed and the head did not move.",
            out.conflicts.len()
        );
        for c in &out.conflicts {
            eprintln!("  conflict: {} (their version saved as {}.theirs)", c, c);
        }
        eprintln!("\nResolve the files, then commit the result with 'changeset create'.");
        return Err(bad("merge produced conflicts"));
    }

    if out.applied.is_empty() {
        println!("nothing to merge: {} has no changesets {} lacks", source, target);
        return Ok(());
    }
    ops::record(&conn, "merge", out.merge_changeset.as_deref(), Some(source))?;
    println!(
        "merged {} into {}: {} changeset(s)",
        source,
        target,
        out.applied.len()
    );
    for id in &out.applied {
        println!("  {}", id);
    }
    Ok(())
}

pub fn cmd_branch_info(name: &str) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let (head, ts, from): (Option<String>, i64, Option<String>) = conn
        .query_row(
            "SELECT head, created_at, from_branch FROM branches WHERE name = ?1",
            rusqlite::params![name],
            |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .map_err(|_| bad(format!("no such branch: {}", name)))?;

    println!("branch:      {}", name);
    println!("created:     {}", fmt_ts(ts));
    println!("from:        {}", from.unwrap_or_else(|| "-".into()));
    println!("head:        {}", head.clone().unwrap_or_else(|| "(empty)".into()));
    println!("changesets:  {}", reachable_from(&conn, name)?.len());
    if let (Some(h), Some(other)) = (&head, None::<String>) {
        let _ = (h, other);
    }
    Ok(())
}

/// Shorthand for the JSON command return type.
type YResultAlias = ZResult<()>;

// JSON output. Each function mirrors the text command's data exactly; the text
// version stays the default because a terminal is still the common case.

fn emit(v: Json) {
    println!("{}", v.render_pretty());
}

pub fn cmd_status_json() -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let branch = current_branch(&conn)?;
    let head = branch_head(&conn, &branch)?;
    let d = repo::diff_worktree(&conn, &root, &branch)?;

    let pending: Vec<Json> = {
        let mut stmt = conn.prepare(
            "SELECT id, message, author, created_at FROM changesets
             WHERE status='pending' ORDER BY created_at, rowid",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Json::obj(vec![
                ("id", Json::str(r.get::<_, String>(0)?)),
                ("message", Json::str(r.get::<_, String>(1)?)),
                ("author", Json::str(r.get::<_, String>(2)?)),
                ("created_at", Json::int(r.get::<_, i64>(3)?)),
            ]))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    emit(Json::obj(vec![
        ("branch", Json::str(branch)),
        ("head", head.map(Json::str).unwrap_or(Json::Null)),
        ("pending_changesets", Json::arr(pending)),
        ("added", Json::arr(d.added.into_iter().map(Json::str).collect())),
        ("modified", Json::arr(d.modified.into_iter().map(Json::str).collect())),
        ("deleted", Json::arr(d.deleted.into_iter().map(Json::str).collect())),
        (
            "skipped_binary",
            Json::arr(d.skipped_binary.into_iter().map(Json::str).collect()),
        ),
        ("unchanged", Json::int(d.unchanged as i64)),
    ]));
    Ok(())
}

pub fn cmd_log_json(limit: usize) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let mut stmt = conn.prepare(
        "SELECT id, message, author, created_at, status, parent FROM changesets
         ORDER BY created_at DESC, rowid DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map(rusqlite::params![limit as i64], |r| {
        Ok(Json::obj(vec![
            ("id", Json::str(r.get::<_, String>(0)?)),
            ("message", Json::str(r.get::<_, String>(1)?)),
            ("author", Json::str(r.get::<_, String>(2)?)),
            ("created_at", Json::int(r.get::<_, i64>(3)?)),
            ("status", Json::str(r.get::<_, String>(4)?)),
            (
                "parent",
                r.get::<_, Option<String>>(5)?.map(Json::str).unwrap_or(Json::Null),
            ),
        ]))
    })?;
    emit(Json::arr(rows.collect::<rusqlite::Result<Vec<_>>>()?));
    Ok(())
}

pub fn cmd_blame_json(file: &str) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let rel = normalise(file);
    let mut stmt = conn.prepare(
        "SELECT line_no, changeset_id, author FROM blame WHERE path = ?1 ORDER BY line_no",
    )?;
    let rows = stmt.query_map(rusqlite::params![rel], |r| {
        Ok(Json::obj(vec![
            ("line", Json::int(r.get::<_, i64>(0)?)),
            ("changeset", Json::str(r.get::<_, String>(1)?)),
            ("author", Json::str(r.get::<_, String>(2)?)),
        ]))
    })?;
    let lines = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    let count = lines.len();
    emit(Json::obj(vec![
        ("path", Json::str(rel)),
        ("line_count", Json::int(count as i64)),
        ("lines", Json::arr(lines)),
    ]));
    Ok(())
}

pub fn cmd_history_json(file: &str) -> YResultAlias {
    let (_root, conn) = open_repo()?;
    let rel = normalise(file);
    let chain = history::rename_history(&conn, &rel, 10)?;

    let mut versions = Vec::new();
    for name in &chain {
        let mut stmt = conn.prepare(
            "SELECT fv.path, fv.version, fv.hash, fv.created_at, cs.id, cs.message, cs.author
             FROM file_versions fv JOIN changesets cs ON cs.id = fv.changeset_id
             WHERE fv.path = ?1 ORDER BY fv.version DESC",
        )?;
        let rows = stmt.query_map(rusqlite::params![name], |r| {
            Ok(Json::obj(vec![
                ("path", Json::str(r.get::<_, String>(0)?)),
                ("version", Json::int(r.get::<_, i64>(1)?)),
                ("hash", Json::str(r.get::<_, String>(2)?)),
                ("created_at", Json::int(r.get::<_, i64>(3)?)),
                ("changeset", Json::str(r.get::<_, String>(4)?)),
                ("message", Json::str(r.get::<_, String>(5)?)),
                ("author", Json::str(r.get::<_, String>(6)?)),
            ]))
        })?;
        for r in rows {
            versions.push(r?);
        }
    }

    emit(Json::obj(vec![
        ("path", Json::str(rel)),
        ("rename_chain", Json::arr(chain.into_iter().map(Json::str).collect())),
        ("versions", Json::arr(versions)),
    ]));
    Ok(())
}

pub fn cmd_diff_json(changeset: Option<&str>) -> YResultAlias {
    let (root, conn) = open_repo()?;
    match changeset {
        Some(id) => {
            let files = repo::changeset_diff(&conn, id)?;
            let renames = history::detect_renames(&conn, id, 0.5)?;
            emit(Json::obj(vec![
                ("changeset", Json::str(id)),
                (
                    "files",
                    Json::arr(
                        files
                            .into_iter()
                            .map(|(path, removed, added)| {
                                Json::obj(vec![
                                    ("path", Json::str(path)),
                                    ("removed", Json::arr(removed.into_iter().map(Json::str).collect())),
                                    ("added", Json::arr(added.into_iter().map(Json::str).collect())),
                                ])
                            })
                            .collect(),
                    ),
                ),
                (
                    "renames",
                    Json::arr(
                        renames
                            .into_iter()
                            .map(|r| {
                                Json::obj(vec![
                                    ("from", Json::str(r.from)),
                                    ("to", Json::str(r.to)),
                                    ("similarity", Json::Float(r.similarity)),
                                ])
                            })
                            .collect(),
                    ),
                ),
            ]));
        }
        None => {
            let branch = current_branch(&conn)?;
            let d = repo::diff_worktree(&conn, &root, &branch)?;
            emit(Json::obj(vec![
                ("branch", Json::str(branch)),
                ("added", Json::arr(d.added.into_iter().map(Json::str).collect())),
                ("modified", Json::arr(d.modified.into_iter().map(Json::str).collect())),
                ("deleted", Json::arr(d.deleted.into_iter().map(Json::str).collect())),
                (
                    "skipped_binary",
                    Json::arr(d.skipped_binary.into_iter().map(Json::str).collect()),
                ),
                ("unchanged", Json::int(d.unchanged as i64)),
            ]));
        }
    }
    Ok(())
}

pub fn cmd_tag_json() -> YResultAlias {
    let (_root, conn) = open_repo()?;
    let mut stmt = conn.prepare("SELECT name, changeset_id, created_at FROM tags ORDER BY name")?;
    let rows = stmt.query_map([], |r| {
        Ok(Json::obj(vec![
            ("name", Json::str(r.get::<_, String>(0)?)),
            ("changeset", Json::str(r.get::<_, String>(1)?)),
            ("created_at", Json::int(r.get::<_, i64>(2)?)),
        ]))
    })?;
    emit(Json::arr(rows.collect::<rusqlite::Result<Vec<_>>>()?));
    Ok(())
}

pub fn cmd_ops_json(limit: usize) -> YResultAlias {
    let (_root, conn) = open_repo()?;
    let ops = ops::list(&conn, limit)?;
    emit(Json::arr(
        ops.into_iter()
            .map(|o| {
                Json::obj(vec![
                    ("id", Json::int(o.id)),
                    ("kind", Json::str(o.kind)),
                    ("target", o.target.map(Json::str).unwrap_or(Json::Null)),
                    ("detail", o.detail.map(Json::str).unwrap_or(Json::Null)),
                    ("created_at", Json::int(o.created_at)),
                ])
            })
            .collect(),
    ));
    Ok(())
}

pub fn cmd_branch_json(sub: crate::BranchCmd) -> YResultAlias {
    let (_root, conn) = open_repo()?;
    match sub {
        crate::BranchCmd::List => {
            let cur = current_branch(&conn)?;
            let mut stmt = conn.prepare("SELECT name, head, created_at FROM branches ORDER BY name")?;
            let rows = stmt.query_map([], |r| {
                Ok(Json::obj(vec![
                    ("name", Json::str(r.get::<_, String>(0)?)),
                    (
                        "head",
                        r.get::<_, Option<String>>(1)?.map(Json::str).unwrap_or(Json::Null),
                    ),
                    ("created_at", Json::int(r.get::<_, i64>(2)?)),
                ]))
            })?;
            let list = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            emit(Json::obj(vec![
                ("current", Json::str(cur)),
                ("branches", Json::arr(list)),
            ]));
            Ok(())
        }
        other => {
            // JSON mode is for reading state. A write still runs and prints its
            // normal text, because scripting a mutation was never the point.
            crate::cli::dispatch_branch_text(other)
        }
    }
}

pub fn cmd_changeset_json(sub: crate::ChangesetCmd) -> YResultAlias {
    match sub {
        crate::ChangesetCmd::List => {
            let (_root, conn) = open_repo()?;
            let mut stmt = conn.prepare(
                "SELECT id, message, author, created_at FROM changesets
                 WHERE status='pending' ORDER BY created_at, rowid",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(Json::obj(vec![
                    ("id", Json::str(r.get::<_, String>(0)?)),
                    ("message", Json::str(r.get::<_, String>(1)?)),
                    ("author", Json::str(r.get::<_, String>(2)?)),
                    ("created_at", Json::int(r.get::<_, i64>(3)?)),
                ]))
            })?;
            emit(Json::arr(rows.collect::<rusqlite::Result<Vec<_>>>()?));
            Ok(())
        }
        other => crate::cli::dispatch_changeset_text(other),
    }
}

// Text-mode dispatch for the subcommands JSON mode does not cover. Keeping it
// here means `main.rs` does not need to know which subcommands have a JSON form.
pub fn dispatch_branch_text(sub: crate::BranchCmd) -> ZResult<()> {
    match sub {
        crate::BranchCmd::Create { name, from } => cmd_branch_create(&name, &from),
        crate::BranchCmd::List => cmd_branch_list(),
        crate::BranchCmd::Switch { name } => cmd_branch_switch(&name),
        crate::BranchCmd::Delete { name } => cmd_branch_delete(&name),
        crate::BranchCmd::Merge { source, target, author } => cmd_branch_merge(&source, &target, &author),
        crate::BranchCmd::Info { name } => cmd_branch_info(&name),
        crate::BranchCmd::Conflicts { source, target } => cmd_merge_preview(&source, &target),
    }
}

pub fn dispatch_changeset_text(sub: crate::ChangesetCmd) -> ZResult<()> {
    match sub {
        crate::ChangesetCmd::Create { message, author } => cmd_changeset_create(&message, &author),
        crate::ChangesetCmd::Commit { id } => cmd_changeset_commit(&id),
        crate::ChangesetCmd::Apply { id } => cmd_changeset_apply(&id),
        crate::ChangesetCmd::CommitAll => cmd_changeset_commit_all(),
        crate::ChangesetCmd::Diff { id } => cmd_changeset_diff(&id),
        crate::ChangesetCmd::List => cmd_changeset_list(),
    }
}
