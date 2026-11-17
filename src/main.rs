//! ZenForge — source control for AI agents.
//!
//! Server-side changesets, content-addressed objects, derived blame/history.
//! State lives in `.zenforge/zenforge.db` (SQLite).

mod repo;
mod store;
mod worktree;

use clap::{Parser, Subcommand};
use std::error::Error;
use std::fs;
use std::path::PathBuf;

use rusqlite::OptionalExtension;
use store::{branch_head, current_branch, get_object, init_schema, open_db, reachable_from, set_state};

pub const ZF_DIR: &str = ".zenforge";
pub const ZF_DB: &str = "zenforge.db";

pub type ZResult<T> = std::result::Result<T, Box<dyn Error>>;

pub fn bad(msg: impl Into<String>) -> Box<dyn Error> {
    msg.into().into()
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn fmt_ts(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| ts.to_string())
}

fn normalise(file: &str) -> String {
    file.replace('\\', "/")
}

fn repo_root() -> ZResult<PathBuf> {
    let mut cur = std::env::current_dir()?;
    loop {
        if cur.join(ZF_DIR).is_dir() {
            return Ok(cur);
        }
        if !cur.pop() {
            return Err(bad("not a zenforge repository (run 'zenforge init')"));
        }
    }
}

// ---------- CLI ----------

#[derive(Parser)]
#[command(name = "zenforge", version, about = "Source control for AI agents")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a repository in the current directory
    Init,
    /// Show repository state and pending changes
    Status,
    /// List changesets, newest first
    Log {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Changeset operations
    Changeset {
        #[command(subcommand)]
        sub: ChangesetCmd,
    },
    /// Show which changeset introduced each line of a file
    Blame { file: String },
    /// Show version history of a file
    History { file: String },
    /// Write a file back to disk from a changeset
    Checkout {
        file: String,
        #[arg(long)]
        changeset: Option<String>,
    },
    /// Branch operations
    Branch {
        #[command(subcommand)]
        sub: BranchCmd,
    },
}

#[derive(Subcommand)]
enum ChangesetCmd {
    /// Snapshot the working tree as a pending changeset
    Create {
        message: String,
        #[arg(long, default_value = "agent")]
        author: String,
    },
    /// Commit a pending changeset onto the current branch
    Commit { id: String },
    /// Commit all pending changesets up to and including this one
    Apply { id: String },
    /// Show what a changeset changed
    Diff { id: String },
    /// List pending changesets
    List,
}

#[derive(Subcommand)]
enum BranchCmd {
    Create {
        name: String,
        #[arg(long, default_value = "main")]
        from: String,
    },
    List,
    Switch { name: String },
    Merge {
        source: String,
        #[arg(long, default_value = "main")]
        target: String,
    },
    Info { name: String },
}

fn main() {
    if let Err(e) = run() {
        eprintln!("zenforge: {}", e);
        std::process::exit(1);
    }
}

fn run() -> ZResult<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Init => cmd_init(),
        Cmd::Status => cmd_status(),
        Cmd::Log { limit } => cmd_log(limit),
        Cmd::Changeset { sub } => match sub {
            ChangesetCmd::Create { message, author } => cmd_changeset_create(&message, &author),
            ChangesetCmd::Commit { id } => cmd_changeset_commit(&id),
            ChangesetCmd::Apply { id } => cmd_changeset_apply(&id),
            ChangesetCmd::Diff { id } => cmd_changeset_diff(&id),
            ChangesetCmd::List => cmd_changeset_list(),
        },
        Cmd::Blame { file } => cmd_blame(&file),
        Cmd::History { file } => cmd_history(&file),
        Cmd::Checkout { file, changeset } => cmd_checkout(&file, changeset.as_deref()),
        Cmd::Branch { sub } => match sub {
            BranchCmd::Create { name, from } => cmd_branch_create(&name, &from),
            BranchCmd::List => cmd_branch_list(),
            BranchCmd::Switch { name } => cmd_branch_switch(&name),
            BranchCmd::Merge { source, target } => cmd_branch_merge(&source, &target),
            BranchCmd::Info { name } => cmd_branch_info(&name),
        },
    }
}

// ---------- init / status / log ----------

fn cmd_init() -> ZResult<()> {
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
    println!("initialized zenforge repository at {}", zf.display());
    println!("branch: main (no changesets yet)");
    Ok(())
}

fn cmd_status() -> ZResult<()> {
    let root = repo_root()?;
    let conn = open_db(&root)?;
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

    if d.added.is_empty() && d.modified.is_empty() {
        println!("working tree clean ({} files tracked)", d.unchanged);
    } else {
        if !d.added.is_empty() {
            println!("\nnew files ({}):", d.added.len());
            for f in &d.added {
                println!("  + {}", f);
            }
        }
        if !d.modified.is_empty() {
            println!("\nmodified ({}):", d.modified.len());
            for f in &d.modified {
                println!("  M {}", f);
            }
        }
    }

    if !d.skipped_binary.is_empty() {
        println!("\nskipped (not UTF-8, {} file(s)):", d.skipped_binary.len());
        for f in d.skipped_binary.iter().take(10) {
            println!("  ? {}", f);
        }
        if d.skipped_binary.len() > 10 {
            println!("  ... and {} more", d.skipped_binary.len() - 10);
        }
    }
    Ok(())
}

fn cmd_log(limit: usize) -> ZResult<()> {
    let root = repo_root()?;
    let conn = open_db(&root)?;
    let all: Vec<(String, String, String, i64, String)> = {
        let mut stmt = conn.prepare(
            "SELECT id, message, author, created_at, status FROM changesets
             ORDER BY created_at DESC, rowid DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    let mut n = 0;
    for (id, msg, author, ts, status) in all {
        println!("{:<14} {} {} [{}] {}", id, fmt_ts(ts), author, status, msg);
        n += 1;
    }
    if n == 0 {
        println!("no changesets yet");
    }
    Ok(())
}

// ---------- changesets ----------

fn cmd_changeset_create(message: &str, author: &str) -> ZResult<()> {
    let root = repo_root()?;
    let conn = open_db(&root)?;
    let branch = current_branch(&conn)?;

    match repo::create_changeset(&conn, &root, message, author)? {
        None => {
            println!("nothing to snapshot: working tree matches branch {}", branch);
            Ok(())
        }
        Some(cs) => {
            println!(
                "created changeset {} ({} file(s), branch {})",
                cs.id,
                cs.files.len(),
                branch
            );
            for (rel, old, _) in &cs.files {
                println!("  {} {}", if old.is_none() { "A" } else { "M" }, rel);
            }
            if !cs.skipped.is_empty() {
                println!(
                    "\nskipped {} file(s) that are not UTF-8 (not tracked by zenforge):",
                    cs.skipped.len()
                );
                for f in cs.skipped.iter().take(10) {
                    println!("  ? {}", f);
                }
                if cs.skipped.len() > 10 {
                    println!("  ... and {} more", cs.skipped.len() - 10);
                }
            }
            println!("\nnext: zenforge changeset commit {}", cs.id);
            Ok(())
        }
    }
}

fn cmd_changeset_commit(id: &str) -> ZResult<()> {
    let root = repo_root()?;
    let conn = open_db(&root)?;
    let n = repo::commit_one(&conn, id)?;
    if n == 0 {
        println!("changeset {} already committed", id);
    } else {
        println!(
            "committed {} on branch {} ({} file(s))",
            id,
            current_branch(&conn)?,
            n
        );
    }
    Ok(())
}

fn cmd_changeset_apply(id: &str) -> ZResult<()> {
    let root = repo_root()?;
    let conn = open_db(&root)?;

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
        let rows = stmt.query_map(rusqlite::params![cutoff], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };

    if ids.is_empty() {
        println!("no pending changesets to apply at or before {}", id);
        return Ok(());
    }
    let mut total = 0usize;
    for cs in &ids {
        total += repo::commit_one(&conn, cs)?;
    }
    println!("applied {} changeset(s) up to {} ({} file(s))", ids.len(), id, total);
    Ok(())
}

fn cmd_changeset_diff(id: &str) -> ZResult<()> {
    let root = repo_root()?;
    let conn = open_db(&root)?;

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

    let files: Vec<(String, Option<String>, String)> = {
        let mut stmt = conn.prepare(
            "SELECT path, old_hash, new_hash FROM changeset_files
             WHERE changeset_id = ?1 ORDER BY path",
        )?;
        let rows = stmt.query_map(rusqlite::params![id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };

    for (path, old, new) in files {
        println!("\n--- {}", path);
        let old_text = match old {
            Some(h) => get_object(&conn, &h)?,
            None => String::new(),
        };
        let new_text = get_object(&conn, &new)?;
        let (removed, added) = repo::line_diff(&old_text, &new_text);
        for l in &removed {
            println!("-{}", l);
        }
        for l in &added {
            println!("+{}", l);
        }
    }
    Ok(())
}

fn cmd_changeset_list() -> ZResult<()> {
    let root = repo_root()?;
    let conn = open_db(&root)?;
    let all: Vec<(String, String, String, i64)> = {
        let mut stmt = conn.prepare(
            "SELECT id, message, author, created_at FROM changesets
             WHERE status = 'pending' ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    let mut n = 0;
    for (id, msg, author, ts) in all {
        println!("{:<14} {} {}  {}", id, fmt_ts(ts), author, msg);
        n += 1;
    }
    if n == 0 {
        println!("no pending changesets");
    }
    Ok(())
}

// ---------- blame / history / checkout ----------

fn cmd_blame(file: &str) -> ZResult<()> {
    let root = repo_root()?;
    let conn = open_db(&root)?;
    let rel = normalise(file);

    let content = match worktree::read_text(&root.join(&rel)) {
        Some(c) => c,
        None => {
            let h = store::latest_hash(&conn, &rel, &current_branch(&conn)?)?
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
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
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
            Some((cs, author)) => println!("{:<14} {:<8} {:>4} | {}", cs, author, n, line),
            None => {
                unattributed += 1;
                println!("{:<14} {:<8} {:>4} | {}", "?", "?", n, line)
            }
        }
    }
    if unattributed > 0 {
        eprintln!(
            "warning: {} line(s) have no attributed changeset (uncommitted or duplicate content)",
            unattributed
        );
    }
    Ok(())
}

fn cmd_history(file: &str) -> ZResult<()> {
    let root = repo_root()?;
    let conn = open_db(&root)?;
    let rel = normalise(file);

    let all: Vec<(i64, String, i64, String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT fv.version, fv.hash, fv.created_at, cs.message, cs.author
             FROM file_versions fv JOIN changesets cs ON cs.id = fv.changeset_id
             WHERE fv.path = ?1 ORDER BY fv.version DESC",
        )?;
        let rows = stmt.query_map(rusqlite::params![rel], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    let mut n = 0;
    for (ver, hash, ts, msg, author) in all {
        let short = &hash[..8.min(hash.len())];
        println!("v{:<3} {} {} {} {}", ver, short, fmt_ts(ts), author, msg);
        n += 1;
    }
    if n == 0 {
        println!("no history for {}", rel);
    }
    Ok(())
}

fn cmd_checkout(file: &str, changeset: Option<&str>) -> ZResult<()> {
    let root = repo_root()?;
    let conn = open_db(&root)?;
    let rel = normalise(file);
    let bytes = repo::checkout_file(&conn, &root, &rel, changeset)?;
    println!("checked out {} ({} bytes)", rel, bytes);
    Ok(())
}

// ---------- branches ----------

fn cmd_branch_create(name: &str, from: &str) -> ZResult<()> {
    let root = repo_root()?;
    let conn = open_db(&root)?;

    let exists: Option<String> = conn
        .query_row(
            "SELECT name FROM branches WHERE name = ?1",
            rusqlite::params![name],
            |r| r.get(0),
        )
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
    println!(
        "created branch {} from {} at {}",
        name,
        from,
        head.unwrap_or_else(|| "(empty)".into())
    );
    Ok(())
}

fn cmd_branch_list() -> ZResult<()> {
    let root = repo_root()?;
    let conn = open_db(&root)?;
    let cur = current_branch(&conn)?;
    let all: Vec<(String, Option<String>, i64)> = {
        let mut stmt =
            conn.prepare("SELECT name, head, created_at FROM branches ORDER BY created_at")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
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

fn cmd_branch_switch(name: &str) -> ZResult<()> {
    let root = repo_root()?;
    let conn = open_db(&root)?;
    branch_head(&conn, name)?;
    set_state(&conn, "current_branch", name)?;
    println!("switched to branch {}", name);
    Ok(())
}

fn cmd_branch_merge(source: &str, target: &str) -> ZResult<()> {
    let root = repo_root()?;
    let conn = open_db(&root)?;
    let applied = repo::merge_branch(&conn, source, target)?;
    if applied.is_empty() {
        println!("nothing to merge: {} has no changesets {} lacks", source, target);
    } else {
        println!(
            "merged {} into {}: {} changeset(s) applied",
            source,
            target,
            applied.len()
        );
        for id in &applied {
            println!("  {}", id);
        }
    }
    Ok(())
}

fn cmd_branch_info(name: &str) -> ZResult<()> {
    let root = repo_root()?;
    let conn = open_db(&root)?;

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
    Ok(())
}
