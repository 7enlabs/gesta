//! ZenForge — source control for AI agents.
//!
//! Server-side changesets, content-addressed compressed objects, derived blame
//! and history, three-way merge, remote sync, and an operation log for undo.

mod cli;
mod cli_remote;
mod http;
mod merge;
mod ops;
mod remote;
mod repo;
mod store;
mod worktree;

use clap::{Parser, Subcommand};
use std::error::Error;
use std::path::PathBuf;

use store::{init_schema, migrate, open_db};

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

pub fn normalise(file: &str) -> String {
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

/// Open the repository at the current directory, running any pending migration.
fn open_repo() -> ZResult<(PathBuf, rusqlite::Connection)> {
    let root = repo_root()?;
    let conn = open_db(&root)?;
    if migrate(&conn)? {
        init_schema(&conn)?;
    }
    Ok((root, conn))
}

// CLI.

#[derive(Parser)]
#[command(name = "zenforge", version, about = "Source control for AI agents")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a repository in the current directory
    Init {
        /// Create a bare repository with no working tree
        #[arg(long)]
        bare: bool,
    },
    /// Show repository state and working-tree changes
    Status,
    /// Show changes not yet staged in a changeset (like git diff)
    Diff {
        /// Compare against this changeset instead of the branch head
        #[arg(long)]
        changeset: Option<String>,
        /// Only report names
        #[arg(long)]
        name_only: bool,
    },
    /// List changesets, newest first
    Log {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// One line per changeset
        #[arg(long)]
        oneline: bool,
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
    /// Show a file as of a changeset or branch
    Show {
        /// Path, optionally as `rev:path`
        spec: String,
    },
    /// Write files back to disk from a changeset
    Checkout {
        /// File or `.` for everything on the branch
        file: String,
        #[arg(long)]
        changeset: Option<String>,
    },
    /// Discard uncommitted changes (like git restore)
    Restore {
        /// File or `.` to restore the whole working tree
        file: String,
    },
    /// Undo the last operation (like git reset but reversible both ways)
    Undo,
    /// Show the operation log
    Ops {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Tag a changeset
    Tag {
        name: String,
        #[arg(long)]
        changeset: Option<String>,
        #[arg(long)]
        list: bool,
    },
    /// Branch operations
    Branch {
        #[command(subcommand)]
        sub: BranchCmd,
    },
    /// Remote operations (push, pull, clone)
    Remote {
        #[command(subcommand)]
        sub: RemoteCmd,
    },
    /// Serve this repository over HTTP
    Serve {
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        #[arg(long, default_value_t = 8787)]
        port: u16,
    },
    /// Verify repository integrity
    Fsck {
        /// Delete objects nothing references
        #[arg(long)]
        prune: bool,
    },
    /// Report storage statistics
    Stats,
    /// Ignore patterns
    Ignore {
        #[command(subcommand)]
        sub: IgnoreCmd,
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
    /// Commit every pending changeset in creation order
    CommitAll,
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
    Delete { name: String },
    Merge {
        source: String,
        #[arg(long, default_value = "main")]
        target: String,
        #[arg(long, default_value = "agent")]
        author: String,
    },
    Info { name: String },
}

#[derive(Subcommand)]
enum RemoteCmd {
    Add { name: String, url: String },
    List,
    /// Send this repository's branch to a remote
    Push {
        #[arg(default_value = "origin")]
        remote: String,
        #[arg(long, default_value = "main")]
        branch: String,
    },
    /// Fetch a remote branch and merge it into the current branch
    Pull {
        #[arg(default_value = "origin")]
        remote: String,
        #[arg(long, default_value = "main")]
        branch: String,
        #[arg(long, default_value = "agent")]
        author: String,
    },
    /// Copy a remote repository into a new directory
    Clone { url: String, dir: String },
}

#[derive(Subcommand)]
enum IgnoreCmd {
    /// Add a pattern
    Add { pattern: String },
    /// List patterns
    List,
    /// Remove a pattern
    Remove { pattern: String },
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
        Cmd::Init { bare } => cli::cmd_init(bare),
        Cmd::Status => cli::cmd_status(),
        Cmd::Diff { changeset, name_only } => cli::cmd_diff(changeset.as_deref(), name_only),
        Cmd::Log { limit, oneline } => cli::cmd_log(limit, oneline),
        Cmd::Changeset { sub } => match sub {
            ChangesetCmd::Create { message, author } => cli::cmd_changeset_create(&message, &author),
            ChangesetCmd::Commit { id } => cli::cmd_changeset_commit(&id),
            ChangesetCmd::Apply { id } => cli::cmd_changeset_apply(&id),
            ChangesetCmd::CommitAll => cli::cmd_changeset_commit_all(),
            ChangesetCmd::Diff { id } => cli::cmd_changeset_diff(&id),
            ChangesetCmd::List => cli::cmd_changeset_list(),
        },
        Cmd::Blame { file } => cli::cmd_blame(&file),
        Cmd::History { file } => cli::cmd_history(&file),
        Cmd::Show { spec } => cli::cmd_show(&spec),
        Cmd::Checkout { file, changeset } => cli::cmd_checkout(&file, changeset.as_deref()),
        Cmd::Restore { file } => cli::cmd_restore(&file),
        Cmd::Undo => cli::cmd_undo(),
        Cmd::Ops { limit } => cli::cmd_ops(limit),
        Cmd::Tag { name, changeset, list } => cli::cmd_tag(&name, changeset.as_deref(), list),
        Cmd::Branch { sub } => match sub {
            BranchCmd::Create { name, from } => cli::cmd_branch_create(&name, &from),
            BranchCmd::List => cli::cmd_branch_list(),
            BranchCmd::Switch { name } => cli::cmd_branch_switch(&name),
            BranchCmd::Delete { name } => cli::cmd_branch_delete(&name),
            BranchCmd::Merge { source, target, author } => cli::cmd_branch_merge(&source, &target, &author),
            BranchCmd::Info { name } => cli::cmd_branch_info(&name),
        },
        Cmd::Remote { sub } => match sub {
            RemoteCmd::Add { name, url } => cli_remote::cmd_remote_add(&name, &url),
            RemoteCmd::List => cli_remote::cmd_remote_list(),
            RemoteCmd::Push { remote, branch } => cli_remote::cmd_push(&remote, &branch),
            RemoteCmd::Pull { remote, branch, author } => cli_remote::cmd_pull(&remote, &branch, &author),
            RemoteCmd::Clone { url, dir } => cli_remote::cmd_clone(&url, &dir),
        },
        Cmd::Serve { bind, port } => cli_remote::cmd_serve(&bind, port),
        Cmd::Fsck { prune } => cli_remote::cmd_fsck(prune),
        Cmd::Stats => cli_remote::cmd_stats(),
        Cmd::Ignore { sub } => match sub {
            IgnoreCmd::Add { pattern } => cli_remote::cmd_ignore_add(&pattern),
            IgnoreCmd::List => cli_remote::cmd_ignore_list(),
            IgnoreCmd::Remove { pattern } => cli_remote::cmd_ignore_remove(&pattern),
        },
    }
}
