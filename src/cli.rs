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
use crate::{bad, fmt_ts, normalise, now, open_repo, repo_dir, repo_root, ZResult};

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
    let zf = repo_dir(&root);
    if zf.is_dir() {
        println!("already a repository: {}", zf.display());
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
    println!("initialized repository at {}", zf.display());
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

/// What the repository does have, for an error that just said what it does not.
///
/// A message that names the missing value and stops leaves the reader guessing whether they
/// typed it wrong or never made it. Listing what exists answers that in the same line.
fn branch_names(conn: &rusqlite::Connection) -> String {
    let mut stmt = match conn.prepare("SELECT name FROM branches ORDER BY name LIMIT 8") {
        Ok(s) => s,
        Err(_) => return String::new(),
    };
    let names: Vec<String> = match stmt.query_map([], |r| r.get::<_, String>(0)) {
        Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
        Err(_) => return String::new(),
    };
    if names.is_empty() {
        return "; no branches yet, create one with `gesta branch create <name>`".to_string();
    }
    format!("; known branches: {}", names.join(", "))
}

/// Pending changesets whose recorded content no longer matches the working tree.
///
/// This is what a lost update looks like from the outside: two writers snapshot the same
/// base, one commits, and the other's snapshot now describes a file that has moved on. The
/// commit itself is not wrong at the database level, which is why nothing complained.
fn pending_out_of_date(
    conn: &rusqlite::Connection,
    root: &std::path::Path,
    _total: i64,
) -> ZResult<Vec<(String, Vec<String>)>> {
    let pending: Vec<(String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT cf.changeset_id, cf.path
             FROM changesets c JOIN changeset_files cf ON cf.changeset_id = c.id
             WHERE c.status = 'pending' ORDER BY cf.changeset_id, cf.path",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut stale: Vec<(String, Vec<String>)> = Vec::new();
    for (id, path) in pending {
        let recorded: Option<String> = conn
            .query_row(
                "SELECT new_hash FROM changeset_files WHERE changeset_id = ?1 AND path = ?2",
                rusqlite::params![id, path],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        let on_disk = match std::fs::read(root.join(&path)) {
            Ok(bytes) => Some(crate::store::hash_content(&String::from_utf8_lossy(&bytes))),
            Err(_) => None,
        };
        if recorded != on_disk {
            match stale.iter_mut().find(|(sid, _)| sid == &id) {
                Some((_, paths)) => paths.push(path),
                None => stale.push((id, vec![path])),
            }
        }
    }
    Ok(stale)
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

    // A pending changeset is a snapshot of an earlier moment. If the file it recorded has
    // changed since, or another writer produced a changeset against the same base, committing
    // it stores content that no longer describes what is on disk. Saying so is the difference
    // between a confusing history and an explainable one.
    if pending > 0 {
        let stale = pending_out_of_date(&conn, &root, total)?;
        if !stale.is_empty() {
            println!(
                "\nwarning: {} pending changeset(s) no longer match the working tree:",
                stale.len()
            );
            for (id, paths) in stale.iter().take(5) {
                println!("  {} ({})", &id[..id.len().min(14)], paths.join(", "));
            }
            println!("Commit or discard them before snapshotting again.");
        }
    }

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
    // One row of the log: id, message, author, time, status, parent, session, intent.
    type LogRow = (
        String,
        String,
        String,
        i64,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let all: Vec<LogRow> = {
        let mut stmt = conn.prepare(
            "SELECT id, message, author, created_at, status, parent, session, intent
             FROM changesets ORDER BY created_at DESC, rowid DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut n = 0;
    for (id, msg, author, ts, status, parent, session, intent) in all {
        if oneline {
            // The intent rides along on one line, because a reason that needs a second
            // command to see is a reason nobody reads.
            match &intent {
                Some(i) => println!(
                    "{:<14} {} {}  ({})",
                    &id[..id.len().min(14)],
                    fmt_ts(ts),
                    msg,
                    i
                ),
                None => println!("{:<14} {} {}", &id[..id.len().min(14)], fmt_ts(ts), msg),
            }
        } else {
            println!("changeset {:<14}", id);
            println!("Author:  {}", author);
            println!("Date:    {}", fmt_ts(ts));
            println!("Status:  {}", status);
            if let Some(p) = parent {
                println!("Parent:  {}", p);
            }
            if let Some(s) = session {
                println!("Session: {}", s);
            }
            if let Some(i) = intent {
                println!("Intent:  {}", i);
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

/// Create a changeset, recording which session produced it and why.
///
/// When an agent writes most of the code, the reason behind a change is part of the
/// change. A message says what happened; the session and intent keep it attached to the
/// conversation that produced it, so a reviewer can ask why instead of guessing from the
/// shape of the diff.
pub fn cmd_changeset_create_meta(
    message: &str,
    author: &str,
    session: Option<&str>,
    intent: Option<&str>,
) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let branch = current_branch(&conn)?;

    match store::with_retry(|| repo::create_changeset(&conn, &root, message, author))? {
        None => {
            println!("nothing to snapshot: working tree matches branch {}", branch);
            Ok(())
        }
        Some(cs) => {
            // Recorded after the row exists, because the id comes from the snapshot.
            if session.is_some() || intent.is_some() {
                conn.execute(
                    "UPDATE changesets SET session = ?1, intent = ?2 WHERE id = ?3",
                    rusqlite::params![session, intent, cs.id],
                )?;
            }
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
            println!("\nnext: gesta changeset commit {}", cs.id);
            Ok(())
        }
    }
}

pub fn cmd_changeset_commit(id: &str) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    // The hook runs before anything is written, so a refusal leaves the
    // repository exactly as it was. A hook that ran afterwards could only report
    // a commit that already happened.
    crate::hooks::run(&root, "pre-commit", &["commit", id])?;
    let n = store::with_retry(|| repo::commit_one(&conn, id))?;
    if n > 0 {
        // A post-commit failure is reported but does not undo the commit: the
        // commit is durable, and rolling it back here would leave the store and
        // the message disagreeing. The error still propagates so a caller knows.
        crate::hooks::run(&root, "post-commit", &["commit", id])?;
    }
    if n == 0 {
        println!("changeset {} already committed", id);
    } else {
        let branch = current_branch(&conn)?;
        println!("committed {} on branch {} ({} file(s))", id, branch, n);
        // Recorded after the fact with the state it produced, because undo restores an
        // operation's own snapshot and a commit has no earlier hook here. The important
        // part is that `undo` and this agree on the meaning; see `ops::record`.
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
    println!("\nThe revert is a normal changeset: `gesta log` shows both, and");
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

    // A blame row answers who wrote a line. With provenance it can also answer which
    // session wrote it and why, which is the question that actually comes up when an
    // agent produced most of the file.
    let rows: Vec<(i64, String, String, Option<String>)> = {
        let mut stmt = conn.prepare(
            "SELECT b.line_no, b.changeset_id, b.author, c.intent
             FROM blame b LEFT JOIN changesets c ON c.id = b.changeset_id
             WHERE b.path = ?1 ORDER BY b.line_no",
        )?;
        let rows = stmt.query_map(rusqlite::params![rel], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    if rows.is_empty() {
        println!("no blame data for {} (never committed)", rel);
        return Ok(());
    }
    let map: std::collections::HashMap<i64, (String, String, Option<String>)> = rows
        .into_iter()
        .map(|(n, c, a, i)| (n, (c, a, i)))
        .collect();

    let mut unattributed = 0usize;
    let mut intents: Vec<String> = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let n = (i + 1) as i64;
        match map.get(&n) {
            Some((cs, author, intent)) => {
                println!("{:<14} {:<10} {:>4} | {}", cs, author, n, line);
                // Collected rather than printed per line, because a reason repeated on
                // every line of a block is noise. It is listed once, at the end.
                if let Some(it) = intent {
                    let entry = format!("{}: {}", &cs[..cs.len().min(14)], it);
                    if !intents.contains(&entry) {
                        intents.push(entry);
                    }
                }
            }
            None => {
                unattributed += 1;
                println!("{:<14} {:<10} {:>4} | {}", "?", "?", n, line)
            }
        }
    }
    if !intents.is_empty() {
        println!("\nwhy these lines exist:");
        for i in &intents {
            println!("  {}", i);
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
    let (root, conn) = open_repo()?;
    let msg = ops::undo(&conn, &root)?;
    println!("{}", msg);
    Ok(())
}

/// `gesta redo`: put back the most recently undone operation.
pub fn cmd_redo() -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let msg = ops::redo(&conn, &root)?;
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
    // A new branch points at the same changeset, so it starts with the same
    // committed tree. Without this the branch reads as an empty tree until its
    // first commit, and `status` on it reports every file as new.
    crate::store::refresh_branch_tree(&conn, name)?;
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
    if branch_head(&conn, name).is_err() {
        return Err(bad(format!("no such branch: {}{}", name, branch_names(&conn))));
    }
    conn.execute("DELETE FROM branches WHERE name = ?1", rusqlite::params![name])?;
    // The derived tree goes with the branch. Leaving it behind would make a
    // branch created later under the same name start with a stale tree.
    conn.execute(
        "DELETE FROM branch_tree WHERE branch = ?1",
        rusqlite::params![name],
    )?;
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
        .map_err(|_| bad(format!("no such branch: {}{}", name, branch_names(&conn))))?;

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
        crate::ChangesetCmd::Create { message, author, session, intent } => {
            cmd_changeset_create_meta(&message, &author, session.as_deref(), intent.as_deref())
        }
        crate::ChangesetCmd::Commit { id } => cmd_changeset_commit(&id),
        crate::ChangesetCmd::Apply { id } => cmd_changeset_apply(&id),
        crate::ChangesetCmd::CommitAll => cmd_changeset_commit_all(),
        crate::ChangesetCmd::Diff { id } => cmd_changeset_diff(&id),
        crate::ChangesetCmd::List => cmd_changeset_list(),
    }
}

pub fn cmd_stash(sub: crate::StashCmd, json: bool) -> ZResult<()> {
    use crate::StashCmd;
    match sub {
        StashCmd::Save { message, author } => {
            let (root, conn) = open_repo()?;
            let (id, n) = crate::stash::save(&conn, &root, &message, &author)?;
            if n == 0 {
                println!("nothing to stash: the working tree matches the head");
                return Ok(());
            }
            if json {
                println!(
                    "{{\"id\":{},\"files\":{},\"message\":\"{}\"}}",
                    id,
                    n,
                    message.replace('"', "\\\"")
                );
            } else {
                println!("saved stash {} ({} file(s)): {}", id, n, message);
                println!("the working tree is back at the head");
            }
            Ok(())
        }
        StashCmd::List => {
            let (_root, conn) = open_repo()?;
            let all = crate::stash::list(&conn)?;
            if json {
                let mut parts = Vec::new();
                for s in &all {
                    parts.push(format!(
                        "{{\"id\":{},\"message\":\"{}\",\"files\":{},\"created_at\":{}}}",
                        s.id,
                        s.message.replace('"', "\\\""),
                        s.files.len(),
                        s.created_at
                    ));
                }
                println!("[{}]", parts.join(","));
                return Ok(());
            }
            if all.is_empty() {
                println!("no stashes");
                return Ok(());
            }
            for s in &all {
                println!(
                    "stash {}  {}  {} file(s)  {}",
                    s.id,
                    fmt_ts(s.created_at),
                    s.files.len(),
                    s.message
                );
            }
            Ok(())
        }
        StashCmd::Pop { id } => {
            let (root, conn) = open_repo()?;
            let (s, written, blocked) = crate::stash::pop(&conn, &root, id)?;
            if !blocked.is_empty() {
                // Refusing is the safe answer: writing over a path the working
                // tree has changed would destroy work that was never recorded.
                println!("stash {} not applied: these paths have local changes", s.id);
                for p in &blocked {
                    println!("  {}", p);
                }
                println!("commit or restore them first, then pop again");
                return Ok(());
            }
            println!("applied stash {} ({} file(s)): {}", s.id, written, s.message);
            Ok(())
        }
        StashCmd::Drop { id } => {
            let (_root, conn) = open_repo()?;
            let target = match id {
                Some(i) => i,
                None => {
                    let all = crate::stash::list(&conn)?;
                    all.first()
                        .map(|s| s.id)
                        .ok_or_else(|| bad("no stash to drop".to_string()))?
                }
            };
            crate::stash::drop_stash(&conn, target)?;
            println!("dropped stash {}", target);
            Ok(())
        }
    }
}

pub fn cmd_hook(sub: crate::HookCmd) -> ZResult<()> {
    use crate::HookCmd;
    match sub {
        HookCmd::List => {
            let (root, _conn) = open_repo()?;
            for (name, installed) in crate::hooks::list(&root) {
                println!("{}  {}", if installed { "installed" } else { "absent   " }, name);
            }
            Ok(())
        }
        HookCmd::Install { name, file } => {
            let (root, _conn) = open_repo()?;
            let body = match file {
                Some(path) => std::fs::read_to_string(&path)
                    .map_err(|e| bad(format!("cannot read {}: {}", path, e)))?,
                None => {
                    use std::io::Read;
                    let mut buf = String::new();
                    std::io::stdin().read_to_string(&mut buf)?;
                    buf
                }
            };
            if body.trim().is_empty() {
                return Err(bad("a hook needs a body: pass --file or write it on stdin"));
            }
            let path = crate::hooks::install(&root, &name, &body)?;
            println!("installed {} at {}", name, path.display());
            Ok(())
        }
        HookCmd::Remove { name } => {
            let (root, _conn) = open_repo()?;
            if crate::hooks::remove(&root, &name)? {
                println!("removed {}", name);
            } else {
                println!("{} was not installed", name);
            }
            Ok(())
        }
    }
}

pub fn cmd_bisect(sub: crate::BisectCmd) -> ZResult<()> {
    use crate::BisectCmd;
    match sub {
        BisectCmd::Start { good, bad: bad_point } => {
            let (root, conn) = open_repo()?;
            if crate::bisect::load(&conn)?.is_some() {
                return Err(bad(
                    "a bisect is already running: answer it or run `gesta bisect cancel`"
                        .to_string(),
                ));
            }
            let s = crate::bisect::start(&conn, &good, &bad_point)?;
            println!(
                "bisecting {} candidate(s) between {} and {}",
                s.candidates.len(),
                &good[..good.len().min(14)],
                &bad_point[..bad_point.len().min(14)]
            );
            // The first question comes right away, so the caller never has to
            // make a separate "what next" call before testing anything.
            match crate::bisect::step(&conn, &root, true)? {
                crate::bisect::Step::Test(id) => {
                    println!("testing {}", id);
                    println!("the working tree now holds that changeset: run your test, then");
                    println!("  gesta bisect good    if it passes");
                    println!("  gesta bisect bad     if it fails");
                }
                crate::bisect::Step::Found(id) => {
                    crate::bisect::finish(&conn, &root)?;
                    println!("first bad changeset: {}", id);
                }
            }
            Ok(())
        }
        BisectCmd::Good | BisectCmd::Bad => {
            let is_good = matches!(sub, BisectCmd::Good);
            let (root, conn) = open_repo()?;
            match crate::bisect::step(&conn, &root, is_good)? {
                crate::bisect::Step::Test(id) => {
                    println!("testing {}", id);
                    Ok(())
                }
                crate::bisect::Step::Found(id) => {
                    // Restoring the tree is part of finishing, not a separate
                    // thing to remember.
                    crate::bisect::finish(&conn, &root)?;
                    println!("first bad changeset: {}", id);
                    println!("the working tree is back at the branch head");
                    if let Ok(Some((msg, author))) = conn
                        .query_row(
                            "SELECT message, author FROM changesets WHERE id = ?1",
                            rusqlite::params![id],
                            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                        )
                        .map(Some)
                    {
                        println!("  {} by {}", msg, author);
                    }
                    Ok(())
                }
            }
        }
        BisectCmd::Status => {
            let (_root, conn) = open_repo()?;
            match crate::bisect::load(&conn)? {
                None => {
                    println!("no bisect running");
                    Ok(())
                }
                Some(s) => {
                    println!("good:       {}", s.good);
                    println!("bad:        {}", s.bad);
                    println!("remaining:  {} candidate(s)", s.candidates.len());
                    println!("questions:  {}", s.steps);
                    if let Some(cur) = s.current {
                        println!("testing:    {}", cur);
                    }
                    Ok(())
                }
            }
        }
        BisectCmd::Cancel => {
            let (root, conn) = open_repo()?;
            if crate::bisect::load(&conn)?.is_none() {
                println!("no bisect running");
                return Ok(());
            }
            let n = crate::bisect::finish(&conn, &root)?;
            println!("bisect cancelled, {} file(s) restored to the head", n);
            Ok(())
        }
    }
}

pub fn cmd_cherry_pick(source: &str, onto: Option<&str>, author: &str) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let target = match onto {
        Some(b) => b.to_string(),
        None => current_branch(&conn)?,
    };
    let out = repo::cherry_pick(&conn, &root, source, &target, author)?;

    if !out.conflicts.is_empty() {
        println!("cherry-pick stopped: {} file(s) conflict", out.conflicts.len());
        for p in &out.conflicts {
            println!("  {}", p);
        }
        println!("the head is unchanged; their side was written beside each file as .theirs");
        return Err(bad("cherry-pick refused the conflicted paths".to_string()));
    }
    match out.merge_changeset {
        Some(id) => {
            println!("picked {} onto {} as {}", source, target, id);
            Ok(())
        }
        None => {
            println!("nothing to pick: the change is already present on {}", target);
            Ok(())
        }
    }
}

pub fn cmd_rebase(
    onto: Option<&str>,
    branch: Option<&str>,
    author: &str,
    do_continue: bool,
    do_abort: bool,
) -> ZResult<()> {
    let (root, conn) = open_repo()?;

    if do_abort {
        let name = crate::rebase::abort(&conn, &root)?;
        println!("rebase aborted, {} is back where it started", name);
        return Ok(());
    }

    if do_continue {
        report_rebase(crate::rebase::continue_after_resolution(&conn, &root, author)?);
        return Ok(());
    }

    let onto = onto.ok_or_else(|| {
        bad("rebase needs a point to move onto, or --continue / --abort".to_string())
    })?;
    let target = match branch {
        Some(b) => b.to_string(),
        None => current_branch(&conn)?,
    };
    let progress = crate::rebase::start(&conn, &root, &target, onto, author)?;
    report_rebase(progress);
    Ok(())
}

fn report_rebase(p: crate::rebase::Progress) {
    use crate::rebase::Progress;
    match p {
        Progress::Nothing => {
            println!("nothing to rebase: the base already contains this branch");
        }
        Progress::Done(ids) if ids.is_empty() => {
            println!("rebase finished: nothing needed recording");
        }
        Progress::Done(ids) => {
            println!("rebased {} changeset(s) onto the new base", ids.len());
        }
        Progress::Stopped { conflicts } => {
            println!("rebase stopped: {} file(s) conflict", conflicts.len());
            for c in &conflicts {
                println!("  {}", c);
            }
            println!();
            println!("resolve them, then:");
            println!("  gesta rebase --continue   keeps the resolution as a changeset");
            println!("  gesta rebase --abort      puts the branch back");
        }
    }
}

pub fn cmd_sub(sub: crate::SubCmd, json: bool) -> ZResult<()> {
    use crate::SubCmd;
    match sub {
        SubCmd::Add { path, remote } => {
            let (_root, conn) = open_repo()?;
            let s = crate::subrepo::add(&conn, &path, &remote)?;
            println!("added subrepository {} -> {}", s.path, s.remote);
            println!("next: place a checkout there, then `gesta sub pin {}`", s.path);
            Ok(())
        }
        SubCmd::List => {
            let (root, conn) = open_repo()?;
            let all = crate::subrepo::list(&conn)?;
            if json {
                let parts: Vec<String> = all
                    .iter()
                    .map(|s| {
                        format!(
                            "{{\"path\":\"{}\",\"remote\":\"{}\",\"pinned\":\"{}\",\"present\":{}}}",
                            s.path,
                            s.remote,
                            s.pinned,
                            crate::subrepo::is_present(&root, s)
                        )
                    })
                    .collect();
                println!("[{}]", parts.join(","));
                return Ok(());
            }
            if all.is_empty() {
                println!("no subrepositories");
                return Ok(());
            }
            for s in &all {
                // A missing checkout is stated, not hidden: a caller that acts on
                // the pin needs to know the directory is not there.
                let state = if crate::subrepo::is_present(&root, s) {
                    "present"
                } else {
                    "missing"
                };
                let pin = if s.pinned.is_empty() {
                    "unpinned".to_string()
                } else {
                    s.pinned.clone()
                };
                println!("{}  {}  {}  {}", s.path, pin, state, s.remote);
            }
            Ok(())
        }
        SubCmd::Pin { path } => {
            let (root, conn) = open_repo()?;
            let s = crate::subrepo::get(&conn, &path)?
                .ok_or_else(|| bad(format!("{} is not a subrepository", path)))?;
            let head = crate::subrepo::child_head(&root, &s).ok_or_else(|| {
                bad(format!(
                    "{} has no repository checkout, so there is nothing to pin",
                    s.path
                ))
            })?;
            let s = crate::subrepo::pin(&conn, &path, &head)?;
            println!("pinned {} at {}", s.path, s.pinned);
            Ok(())
        }
        SubCmd::Remove { path } => {
            let (_root, conn) = open_repo()?;
            if crate::subrepo::remove(&conn, &path)? {
                println!("removed subrepository {}", path);
            } else {
                println!("{} was not a subrepository", path);
            }
            Ok(())
        }
    }
}

/// Read a line from the terminal without echoing it.
///
/// The value is typed, not passed as an argument: an argument is visible in the
/// process list and lands in shell history, and a secret in either is a secret that
/// has to be rotated.
fn read_hidden(prompt: &str) -> ZResult<String> {
    use std::io::{BufRead, Write};

    print!("{}", prompt);
    std::io::stdout().flush()?;

    #[cfg(unix)]
    {
        // stty is present wherever a terminal is, and turning echo off for one read
        // is the one thing this needs from it.
        let _ = std::process::Command::new("stty")
            .arg("-echo")
            .stdin(std::process::Stdio::inherit())
            .status();
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        let _ = std::process::Command::new("stty")
            .arg("echo")
            .stdin(std::process::Stdio::inherit())
            .status();
        println!();
        #[allow(clippy::needless_return)] // the other cfg branch returns below
        return Ok(line.trim_end().to_string());
    }

    #[cfg(not(unix))]
    {
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        Ok(line.trim_end().to_string())
    }
}

/// Handle `gesta secret`.
pub fn cmd_secret(sub: crate::SecretCmd) -> ZResult<()> {
    use crate::SecretCmd;

    match sub {
        SecretCmd::Init => {
            let path = match std::env::var("GESTA_SECRETS_FILE") {
                Ok(p) if !p.is_empty() => std::path::PathBuf::from(p),
                _ => crate::secrets::default_path()?,
            };
            println!("store: {}", path.display());
            let first = read_hidden("passphrase: ")?;
            if first.len() < 8 {
                return Err(bad("a passphrase shorter than 8 characters is not worth the file it protects".to_string()));
            }
            let again = read_hidden("repeat: ")?;
            if first != again {
                return Err(bad("the two passphrases differ".to_string()));
            }
            let store = crate::secrets::SecretStore {
                path,
                passphrase: first,
            };
            let existing = store.load()?;
            // Saving an empty map is what creates the file, and keeps what is there.
            store.save(&existing)?;
            println!("created {} ({} entries kept)", store.path.display(), existing.len());
            Ok(())
        }
        SecretCmd::Set { name } => {
            let store = crate::secrets::SecretStore::from_env()?;
            let value = read_hidden(&format!("value for {}: ", name))?;
            if value.is_empty() {
                return Err(bad("an empty value is not stored".to_string()));
            }
            store.put(&name, &value)?;
            println!("stored {} in {}", name, store.path.display());
            Ok(())
        }
        SecretCmd::List => {
            let store = crate::secrets::SecretStore::from_env()?;
            let entries = store.load()?;
            if entries.is_empty() {
                println!("no entries in {}", store.path.display());
            } else {
                println!("{} ({} entries)", store.path.display(), entries.len());
                for name in entries.keys() {
                    println!("  {}", name);
                }
            }
            Ok(())
        }
        SecretCmd::Remove { name } => {
            let store = crate::secrets::SecretStore::from_env()?;
            if store.remove(&name)? {
                println!("removed {}", name);
            } else {
                println!("no entry named {}", name);
            }
            Ok(())
        }
    }
}

/// `gesta diff --from A --to B`: compare two revisions.
pub fn cmd_diff_range(from: Option<&str>, to: Option<&str>, name_only: bool) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let diffs = crate::repo::diff_revs(&conn, from, to)?;
    if diffs.is_empty() {
        println!(
            "no difference between {} and {}",
            from.unwrap_or("an empty tree"),
            to.unwrap_or("an empty tree")
        );
        return Ok(());
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
    Ok(())
}

/// The same as `cmd_diff_range` with `--json`.
pub fn cmd_diff_range_json(from: Option<&str>, to: Option<&str>) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let diffs = crate::repo::diff_revs(&conn, from, to)?;
    use crate::json::Json;
    let arr = Json::arr(
        diffs
            .into_iter()
            .map(|(path, removed, added)| {
                Json::obj(vec![
                    ("path", Json::str(path)),
                    (
                        "removed",
                        Json::arr(removed.into_iter().map(Json::str).collect()),
                    ),
                    ("added", Json::arr(added.into_iter().map(Json::str).collect())),
                ])
            })
            .collect(),
    );
    println!("{}", arr.render());
    Ok(())
}

/// `gesta review`: a stack of changesets, one step at a time, each with a risk read.
///
/// This is the answer to reviewing what an agent produced. A net diff between two
/// points hides the order things happened in, and the order is most of the reasoning.
pub fn cmd_review(from: Option<&str>, to: Option<&str>, min_risk: Option<&str>) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let branch = current_branch(&conn)?;
    let target = to.unwrap_or(&branch);

    let steps = crate::repo::review_stack(&conn, from, target)?;
    if steps.is_empty() {
        println!("nothing between {} and {}", from.unwrap_or("the start"), target);
        return Ok(());
    }

    let floor = match min_risk {
        Some("low") => 0,
        Some("medium") => 4,
        Some("high") => 9,
        Some(other) => {
            return Err(bad(format!(
                "unknown risk band: {} (use low, medium or high)",
                other
            )))
        }
        None => 0,
    };

    let mut shown = 0usize;
    let mut hidden = 0usize;
    for (n, step) in steps.iter().enumerate() {
        if step.risk.score < floor {
            hidden += 1;
            continue;
        }
        shown += 1;
        println!(
            "\n[{}] {}  {}  ({})",
            n + 1,
            &step.id[..step.id.len().min(14)],
            step.message,
            step.author
        );
        println!(
            "    risk {} ({})  {} file(s), {}",
            step.risk.band(),
            step.risk.score,
            step.files.len(),
            step.risk.reasons.join("; ")
        );
        for (path, removed, added) in &step.files {
            println!("    --- {}", path);
            for l in removed {
                println!("    -{}", l);
            }
            for l in added {
                println!("    +{}", l);
            }
        }
    }
    println!(
        "\n{} step(s) shown, {} below the {} threshold",
        shown,
        hidden,
        min_risk.unwrap_or("low")
    );
    Ok(())
}

/// `gesta land`: merge one branch into another, but only if the target has not moved.
///
/// Without the check, two people landing at the same time both read the same head, both
/// merge against it, and the second overwrites the first with no error anywhere. A
/// compare-and-swap turns that into a refusal, which is the whole point of a queue.
pub fn cmd_land(source: &str, onto: &str, expect: Option<&str>, author: &str) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let before = store::branch_head(&conn, onto)?;
    if let Some(e) = expect {
        if before.as_deref() != Some(e) {
            return Err(bad(format!(
                "{} moved: expected {}, found {}. Re-read the head and prepare again.",
                onto,
                e,
                before.as_deref().unwrap_or("(none)")
            )));
        }
    }

    let out = crate::repo::land_branch(&conn, &root, source, onto, before.as_deref(), author)?;
    if !out.conflicts.is_empty() {
        eprintln!(
            "{} change(s) conflict when landing {} onto {}:",
            out.conflicts.len(),
            source,
            onto
        );
        for c in &out.conflicts {
            eprintln!("  {} (their version saved as {}.theirs)", c, c);
        }
        return Err(bad(
            "landing needs a manual merge; the target was not moved".to_string(),
        ));
    }
    if out.applied.is_empty() {
        println!("{} has nothing that {} does not already have", source, onto);
        return Ok(());
    }
    let after = store::branch_head(&conn, onto)?;

    // Landing moved the target head, so the working tree has to follow when the target is
    // the branch in front of the user. Without this the file the merge added exists in
    // history and is reported as deleted on disk, which looks like data loss.
    let current = current_branch(&conn)?;
    let refreshed = if current == onto {
        Some(repo::materialise(&conn, &root, onto)?)
    } else {
        None
    };

    ops::record(&conn, "land", after.as_deref(), Some(source))?;
    println!(
        "landed {} onto {} ({} change(s), head now {}{})",
        source,
        onto,
        out.applied.len(),
        after.as_deref().unwrap_or("(none)"),
        match refreshed {
            Some(n) => format!(", {} file(s) written", n),
            None => String::new(),
        }
    );
    Ok(())
}

#[cfg(test)]
mod pending_tests {
    use super::*;

    /// A pending changeset is a snapshot of an earlier moment. If the file moved on before it
    /// was committed, committing it stores content that does not describe the disk, and
    /// nothing else would say so.
    #[test]
    fn a_pending_changeset_that_no_longer_matches_the_disk_is_reported() {
        let conn = crate::store::tests::mem_db();
        let dir = std::env::temp_dir().join(format!("gesta-pending-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        std::fs::write(dir.join("f.txt"), "v1\n").unwrap();
        crate::repo::tests::commit_files(&conn, "p1", "base", &[("f.txt", "v1\n")]);
        crate::store::set_branch_head(&conn, "main", "p1").unwrap();
        crate::store::set_state(&conn, "current_branch", "main").unwrap();

        // Snapshot a change without committing it.
        std::fs::write(dir.join("f.txt"), "v2\n").unwrap();
        let cs = crate::repo::create_changeset(&conn, &dir, "snapshot", "a")
            .unwrap()
            .expect("there is a change to snapshot");

        // Matching the disk, so nothing is stale yet.
        let clean = pending_out_of_date(&conn, &dir, 0).unwrap();
        assert!(
            clean.is_empty(),
            "a fresh snapshot must not be reported: {:?}",
            clean
        );

        // The file moves on before the commit.
        std::fs::write(dir.join("f.txt"), "v3\n").unwrap();
        let stale = pending_out_of_date(&conn, &dir, 0).unwrap();
        assert_eq!(stale.len(), 1, "the stale snapshot was not found");
        assert_eq!(stale[0].0, cs.id);
        assert_eq!(stale[0].1, vec!["f.txt".to_string()]);

        // Deleting the file is also a mismatch.
        std::fs::remove_file(dir.join("f.txt")).unwrap();
        let gone = pending_out_of_date(&conn, &dir, 0).unwrap();
        assert_eq!(gone.len(), 1, "a file removed after the snapshot was not reported");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
