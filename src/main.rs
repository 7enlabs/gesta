//! gesta: source control for AI agents.
//!
//! Server-side changesets, content-addressed compressed objects, derived blame
//! and history, three-way merge, remote sync, and an operation log for undo.

mod auth;
mod bisect;
mod brief;
mod cache;
mod clock;
mod cli;
mod cli_remote;
mod conflict;
mod content;
mod delta;
mod grep;
mod history;
mod hooks;
mod http;
mod json;
mod mcp;
mod merge;
mod ops;
mod remote;
mod rebase;
mod repo;
mod s3;
mod secrets;
mod sigv4;
mod sparse;
mod stash;
mod subrepo;
mod store;
#[cfg(feature = "tls")]
mod tls;
mod worktree;

use clap::{Parser, Subcommand};
use std::error::Error;
use std::path::PathBuf;

use store::{init_schema, migrate, open_db};

/// Directory that holds the repository. Named after the tool, and the old name is
/// still recognised so repositories created before the rename keep working.
pub const ZF_DIR: &str = ".gesta";
/// The directory this project used before it was renamed. Reading it is
/// supported; nothing ever writes to it.
pub const LEGACY_DIR: &str = ".zenforge";

/// The repository directory in `root`, whatever name it was created under.
///
/// Every path built against `ZF_DIR` would look in `.gesta/` and miss a
/// repository made before the rename, so callers ask here instead. When neither
/// exists the new name is returned, which is what `init` then creates.
/// The database file for the repository in `root`, under whichever name the
/// repository was created with.
///
/// The directory name and the file name changed together, so a repository made
/// before the rename has `.zenforge/zenforge.db`. Looking only for `.gesta` and
/// `gesta.db` finds nothing inside an existing repository, and the schema init
/// then creates an empty database next to the real one, which looks like every
/// file went missing.
pub fn repo_db(root: &std::path::Path) -> PathBuf {
    let new = repo_dir(root).join(ZF_DB);
    let legacy = root.join(LEGACY_DIR).join(LEGACY_DB);

    // Size, not existence. An empty file of the new name is what a failed or
    // interrupted start leaves behind, and preferring it over the real database
    // next to it makes every repository look empty.
    let size = |p: &std::path::Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    if size(&new) > 0 {
        return new;
    }
    if size(&legacy) > 0 {
        return legacy;
    }
    new
}

pub fn repo_dir(root: &std::path::Path) -> PathBuf {
    let new = root.join(ZF_DIR);
    if new.is_dir() {
        return new;
    }
    let old = root.join(LEGACY_DIR);
    if old.is_dir() {
        return old;
    }
    new
}
pub const ZF_DB: &str = "gesta.db";
/// Database file name used before the rename, inside `LEGACY_DIR`.
pub const LEGACY_DB: &str = "zenforge.db";

pub type ZResult<T> = std::result::Result<T, Box<dyn Error>>;

pub fn bad(msg: impl Into<String>) -> Box<dyn Error> {
    msg.into().into()
}

pub fn now() -> i64 {
    clock::unix_now()
}

fn fmt_ts(ts: i64) -> String {
    clock::format_minute(ts)
}

pub fn normalise(file: &str) -> String {
    file.replace('\\', "/")
}

pub fn repo_root() -> ZResult<PathBuf> {
    let mut cur = std::env::current_dir()?;
    loop {
        // A repository made before the rename has the old directory name. Reading
        // it keeps every existing repository usable; `gesta init` and every write
        // path use the new name, so nothing new lands in the old directory.
        if cur.join(ZF_DIR).is_dir() || cur.join(LEGACY_DIR).is_dir() {
            return Ok(cur);
        }
        if !cur.pop() {
            return Err(bad("not a repository (run 'gesta init')"));
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
#[command(name = "gesta", version, about = "Source control for AI agents")]
struct Cli {
    /// Print machine-readable JSON instead of text
    #[arg(long, global = true)]
    json: bool,
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
        /// Compare from this revision (a branch, tag or changeset) to --to
        #[arg(long)]
        from: Option<String>,
        /// Compare to this revision. Defaults to the working tree
        #[arg(long)]
        to: Option<String>,
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
    /// Everything a session needs before its first edit, in one call
    Brief {
        /// Print the object instead of a short summary
        #[arg(long)]
        json: bool,
    },
    /// Search the working tree, or every version in the history
    Grep {
        /// Text to find
        pattern: String,
        /// Search every recorded version instead of the files on disk
        #[arg(long)]
        history: bool,
        #[arg(long)]
        ignore_case: bool,
        /// Match whole words only
        #[arg(long)]
        word: bool,
        /// Only paths starting with this prefix
        #[arg(long)]
        path: Option<String>,
        /// How many versions to read when searching history
        #[arg(long, default_value_t = 5000)]
        limit: i64,
        /// Print `path:line` only, for piping into another tool
        #[arg(long)]
        files_with_matches: bool,
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
    /// Replay this branch's changesets onto another point
    Rebase {
        /// Branch or changeset to replay onto. Not needed with --continue or
        /// --abort, which work from the saved state.
        onto: Option<String>,
        /// Branch to rebase. Defaults to the current branch.
        #[arg(long)]
        branch: Option<String>,
        #[arg(long, default_value = "agent")]
        author: String,
        /// Finish a stopped rebase using what is on disk
        #[arg(long)]
        continue_: bool,
        /// Put the branch back where the rebase started
        #[arg(long = "abort")]
        abort: bool,
    },
    /// Apply the change one changeset made onto the current branch
    CherryPick {
        /// Changeset id, tag, or branch name
        source: String,
        /// Branch to apply onto. Defaults to the current branch.
        #[arg(long)]
        onto: Option<String>,
        #[arg(long, default_value = "agent")]
        author: String,
    },
    /// Search the history for the changeset that introduced a fault
    Bisect {
        #[command(subcommand)]
        sub: BisectCmd,
    },
    /// Manage paths that point at other repositories
    Sub {
        #[command(subcommand)]
        sub: SubCmd,
    },
    /// Save the working tree aside and reset it to the head
    Stash {
        #[command(subcommand)]
        sub: StashCmd,
    },
    /// Manage hooks that run before and after a commit
    Hook {
        #[command(subcommand)]
        sub: HookCmd,
    },
    /// Undo the last operation (like git reset but reversible both ways)
    Undo,
    /// Put back the most recently undone operation
    Redo,
    /// Undo the effect of one changeset by recording a new one
    Revert {
        id: String,
        #[arg(long)]
        reason: Option<String>,
        #[arg(long, default_value = "agent")]
        author: String,
    },
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
    /// Manage stored credentials for remotes
    Secret {
        #[command(subcommand)]
        sub: SecretCmd,
    },
    /// Land a branch onto a target, refusing if the target moved since it was read
    Land {
        /// Branch whose changes are being landed
        source: String,
        /// Branch being landed onto
        #[arg(long, default_value = "main")]
        onto: String,
        /// The head read before the merge, which the write must still find
        #[arg(long)]
        expect: Option<String>,
        #[arg(long, default_value = "agent")]
        author: String,
    },
    /// Show a stack of changesets step by step, with a risk read on each
    Review {
        /// Start after this revision; everything after it is shown
        #[arg(long)]
        from: Option<String>,
        /// End at this revision. Defaults to the branch head
        #[arg(long)]
        to: Option<String>,
        /// Show only steps at this band or above: low, medium, high
        #[arg(long)]
        min_risk: Option<String>,
    },
    /// Run an MCP server on stdio, exposing the repository as tools
    Mcp,
    /// Serve this repository over HTTP
    Serve {
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        #[arg(long, default_value_t = 8787)]
        port: u16,
        /// Serve over TLS. Give `self-signed` to generate a certificate, or the
        /// path to a PEM certificate.
        #[arg(long)]
        tls: Option<String>,
        /// Private key for `--tls`, required when the certificate is given as a
        /// path. Ignored for `self-signed`.
        #[arg(long)]
        key: Option<String>,
    },
    /// Limit the working tree to a set of paths
    Sparse {
        #[command(subcommand)]
        sub: SparseCmd,
    },
    /// Verify repository integrity
    Fsck {
        /// Delete objects nothing references
        #[arg(long)]
        prune: bool,
        /// Also compare the working tree with the head. Off by default because it reads
        /// every tracked file.
        #[arg(long)]
        worktree: bool,
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
        /// Which session produced this change, so the reasoning stays attached.
        /// Defaults to GESTA_SESSION when that is set.
        #[arg(long)]
        session: Option<String>,
        /// Why the change was made, in one line
        #[arg(long)]
        intent: Option<String>,
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
enum BisectCmd {
    /// Begin a search between a known good and a known bad changeset
    Start { good: String, bad: String },
    /// Report the changeset under test as good, or as bad
    Good,
    Bad,
    /// Show the state of the search
    Status,
    /// Abandon the search and put the working tree back
    Cancel,
}

#[derive(Subcommand)]
enum SubCmd {
    /// Record a path as a subrepository
    Add {
        path: String,
        /// Where the child repository lives
        remote: String,
    },
    /// Show the subrepositories and what each is pinned to
    List,
    /// Pin a subrepository to the child's current head
    Pin { path: String },
    /// Stop tracking a path as a subrepository
    Remove { path: String },
}

#[derive(Subcommand)]
enum StashCmd {
    /// Save the working tree aside and reset it to the head
    Save {
        #[arg(long, default_value = "wip")]
        message: String,
        #[arg(long, default_value = "agent")]
        author: String,
    },
    /// List saved stashes, newest first
    List,
    /// Write the newest stash back, or a chosen one by id
    Pop {
        #[arg(long)]
        id: Option<i64>,
    },
    /// Delete a stash without applying it
    Drop {
        #[arg(long)]
        id: Option<i64>,
    },
}

#[derive(Subcommand)]
enum HookCmd {
    /// List the hooks and whether each one is installed
    List,
    /// Write a hook from a file or from standard input
    Install {
        name: String,
        /// Path to a script. Without it, the body is read from standard input.
        #[arg(long)]
        file: Option<String>,
    },
    /// Remove a hook
    Remove { name: String },
}

#[derive(Subcommand)]
enum SparseCmd {
    /// Set the working set and check those paths out
    Set { patterns: Vec<String> },
    /// Show the current working set
    List,
    /// Return to a full checkout
    Clear,
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
    /// Report what a merge would conflict on, without touching anything
    Conflicts {
        source: String,
        #[arg(long, default_value = "main")]
        target: String,
    },
}

#[derive(Subcommand)]
enum SecretCmd {
    /// Ask for a passphrase and create the encrypted store
    Init,
    /// Set one entry; the value is read from the terminal, never from an argument
    Set {
        /// Entry name, for example s3.e2.access_key
        name: String,
    },
    /// List entry names. Values are never printed.
    List,
    /// Remove one entry
    Remove { name: String },
}

#[derive(Subcommand)]
enum RemoteCmd {
    Add {
        name: String,
        url: String,
        /// Token the server requires, stored with the remote.
        #[arg(long)]
        token: Option<String>,
        /// SHA-256 fingerprint of the server certificate, for a self-signed one.
        #[arg(long)]
        pin: Option<String>,
    },
    List,
    /// Manage access tokens for serving over HTTP
    Token {
        #[command(subcommand)]
        sub: TokenCmd,
    },
    /// List the branches a remote holds, for remotes that can be asked
    Ls {
        #[arg(default_value = "origin")]
        remote: String,
    },
    /// Extend the history of a shallow clone by taking older changesets
    Deepen {
        #[arg(long)]
        depth: Option<usize>,
        /// Take everything, removing the shallow boundary
        #[arg(long)]
        unshallow: bool,
        #[arg(long, default_value = "origin")]
        remote: String,
        #[arg(long, default_value = "main")]
        branch: String,
        /// Only report what would be fetched
        #[arg(long)]
        dry_run: bool,
    },
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
    Clone {
        url: String,
        dir: String,
        #[arg(long)]
        token: Option<String>,
        /// SHA-256 fingerprint of the server certificate, for a self-signed one.
        #[arg(long)]
        pin: Option<String>,
        /// Take only the newest N changesets, for a history too long to download whole
        #[arg(long)]
        depth: Option<usize>,
    },
}

#[derive(Subcommand)]
enum TokenCmd {
    /// Generate a token and write it to .gesta/auth
    Add {
        /// read or write
        #[arg(default_value = "write")]
        role: String,
        /// Label shown in `token list`
        name: String,
        /// Store only the digest, so a leaked auth file cannot be replayed
        #[arg(long)]
        hashed: bool,
    },
    /// List configured tokens without revealing them
    List,
    /// Remove a token by name or by its last four characters
    Remove { selector: String },
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

/// Exit quietly when the reader closed the pipe.
///
/// `println!` panics on a broken pipe, so `gesta status | head -1` printed a
/// panic message and a backtrace from inside the standard library. Every tool
/// that writes to stdout is piped into something eventually, and git exits
/// silently in the same situation, so a closed pipe is normal use rather than an
/// error worth a stack trace.
///
/// Only a broken pipe is swallowed. Any other panic keeps the default message,
/// because hiding a real bug would be worse than the noise.
fn quiet_broken_pipe() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info.payload();
        let text = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        if text.contains("Broken pipe") {
            std::process::exit(0);
        }
        default(info);
    }));
}

fn main() {
    quiet_broken_pipe();
    if let Err(e) = run() {
        eprintln!("gesta: {}", e);
        std::process::exit(1);
    }
}

fn run() -> ZResult<()> {
    let cli = Cli::parse();
    let json = cli.json;
    match cli.cmd {
        Cmd::Status if json => cli::cmd_status_json(),
        Cmd::Log { limit, .. } if json => cli::cmd_log_json(limit),
        Cmd::Blame { file } if json => cli::cmd_blame_json(&file),
        Cmd::History { file } if json => cli::cmd_history_json(&file),
        Cmd::Diff { changeset, from, to, .. } if json => {
            if from.is_some() || to.is_some() {
                cli::cmd_diff_range_json(from.as_deref(), to.as_deref())
            } else {
                cli::cmd_diff_json(changeset.as_deref())
            }
        }
        Cmd::Tag { .. } if json => cli::cmd_tag_json(),
        Cmd::Ops { limit } if json => cli::cmd_ops_json(limit),
        // The specific arms come before the general one: a catch-all
        // `Cmd::Branch { sub } if json` would swallow them, and a match arm
        // that never runs is worse than a compile error.
        Cmd::Branch {
            sub: BranchCmd::Conflicts { source, target },
        } => cli::cmd_merge_preview(&source, &target),
        Cmd::Branch {
            sub: BranchCmd::Merge { source, target, author },
        } if json => cli::cmd_branch_merge_json(&source, &target, &author),
        Cmd::Branch { sub } if json => cli::cmd_branch_json(sub),
        Cmd::Changeset { sub } if json => cli::cmd_changeset_json(sub),
        Cmd::Sparse { sub } => cli::cmd_sparse(sub),
        Cmd::Rebase { onto, branch, author, continue_, abort } => {
            cli::cmd_rebase(onto.as_deref(), branch.as_deref(), &author, continue_, abort)
        }
        Cmd::CherryPick { source, onto, author } => cli::cmd_cherry_pick(&source, onto.as_deref(), &author),
        Cmd::Bisect { sub } => cli::cmd_bisect(sub),
        Cmd::Sub { sub } => cli::cmd_sub(sub, json),
        Cmd::Stash { sub } => cli::cmd_stash(sub, json),
        Cmd::Hook { sub } => cli::cmd_hook(sub),
        Cmd::Init { bare } => cli::cmd_init(bare),
        Cmd::Status => cli::cmd_status(),
        Cmd::Diff { changeset, from, to, name_only } => {
            if from.is_some() || to.is_some() {
                cli::cmd_diff_range(from.as_deref(), to.as_deref(), name_only)
            } else {
                cli::cmd_diff(changeset.as_deref(), name_only)
            }
        }
        Cmd::Log { limit, oneline } => cli::cmd_log(limit, oneline),
        Cmd::Changeset { sub } => match sub {
            ChangesetCmd::Create { message, author, session, intent } => {
                let session = session.or_else(|| std::env::var("GESTA_SESSION").ok());
                cli::cmd_changeset_create_meta(
                    &message,
                    &author,
                    session.as_deref(),
                    intent.as_deref(),
                )
            }
            ChangesetCmd::Commit { id } => cli::cmd_changeset_commit(&id),
            ChangesetCmd::Apply { id } => cli::cmd_changeset_apply(&id),
            ChangesetCmd::CommitAll => cli::cmd_changeset_commit_all(),
            ChangesetCmd::Diff { id } => cli::cmd_changeset_diff(&id),
            ChangesetCmd::List => cli::cmd_changeset_list(),
        },
        Cmd::Blame { file } => cli::cmd_blame(&file),
        Cmd::History { file } => cli::cmd_history(&file),
        Cmd::Show { spec } => cli::cmd_show(&spec),
        Cmd::Brief { json: brief_json } => {
            if brief_json {
                cli::cmd_brief_json()
            } else {
                cli::cmd_brief()
            }
        }
        Cmd::Grep {
            pattern,
            history,
            ignore_case,
            word,
            path,
            limit,
            files_with_matches,
        } => {
            if json {
                cli::cmd_grep_json(
                    &pattern,
                    history,
                    ignore_case,
                    word,
                    path.as_deref(),
                    limit,
                )
            } else {
                cli::cmd_grep(
                    &pattern,
                    history,
                    ignore_case,
                    word,
                    path.as_deref(),
                    limit,
                    files_with_matches,
                )
            }
        }
        Cmd::Checkout { file, changeset } => cli::cmd_checkout(&file, changeset.as_deref()),
        Cmd::Restore { file } => cli::cmd_restore(&file),
        Cmd::Undo => cli::cmd_undo(),
        Cmd::Redo => cli::cmd_redo(),
        Cmd::Revert { id, reason, author } => {
            cli::cmd_revert(&id, reason.as_deref(), &author)
        }
        Cmd::Ops { limit } => cli::cmd_ops(limit),
        Cmd::Tag { name, changeset, list } => cli::cmd_tag(&name, changeset.as_deref(), list),
        Cmd::Branch { sub } => match sub {
            BranchCmd::Create { name, from } => cli::cmd_branch_create(&name, &from),
            BranchCmd::List => cli::cmd_branch_list(),
            BranchCmd::Switch { name } => cli::cmd_branch_switch(&name),
            BranchCmd::Delete { name } => cli::cmd_branch_delete(&name),
            BranchCmd::Merge { source, target, author } => cli::cmd_branch_merge(&source, &target, &author),
            BranchCmd::Info { name } => cli::cmd_branch_info(&name),
            BranchCmd::Conflicts { source, target } => cli::cmd_merge_preview(&source, &target),
        },
        Cmd::Remote { sub } => match sub {
            RemoteCmd::Add { name, url, token, pin } => {
                cli_remote::cmd_remote_add(&name, &url, token.as_deref(), pin.as_deref())
            }
            RemoteCmd::List => cli_remote::cmd_remote_list(),
            RemoteCmd::Token { sub } => match sub {
                TokenCmd::Add { role, name, hashed } => cli_remote::cmd_token_add(&role, &name, hashed),
                TokenCmd::List => cli_remote::cmd_token_list(),
                TokenCmd::Remove { selector } => cli_remote::cmd_token_remove(&selector),
            },
            RemoteCmd::Ls { remote } => cli_remote::cmd_remote_ls(&remote),
            RemoteCmd::Deepen { depth, unshallow, remote, branch, dry_run } => {
                cli_remote::cmd_deepen(depth, unshallow, &remote, &branch, dry_run)
            }
            RemoteCmd::Push { remote, branch } => cli_remote::cmd_push(&remote, &branch),
            RemoteCmd::Pull { remote, branch, author } => cli_remote::cmd_pull(&remote, &branch, &author),
            RemoteCmd::Clone { url, dir, token, pin, depth } => {
                cli_remote::cmd_clone(&url, &dir, token.as_deref(), pin.as_deref(), depth)
            }
        },
        Cmd::Secret { sub } => cli::cmd_secret(sub),
        Cmd::Land { source, onto, expect, author } => {
            cli::cmd_land(&source, &onto, expect.as_deref(), &author)
        }
        Cmd::Review { from, to, min_risk } => cli::cmd_review(from.as_deref(), to.as_deref(), min_risk.as_deref()),
        Cmd::Mcp => cli::cmd_mcp(),
        Cmd::Serve { bind, port, tls, key } => {
            cli_remote::cmd_serve(&bind, port, tls.as_deref(), key.as_deref())
        }
        Cmd::Fsck { prune, worktree } => cli_remote::cmd_fsck(prune, worktree),
        Cmd::Stats => cli_remote::cmd_stats(),
        Cmd::Ignore { sub } => match sub {
            IgnoreCmd::Add { pattern } => cli_remote::cmd_ignore_add(&pattern),
            IgnoreCmd::List => cli_remote::cmd_ignore_list(),
            IgnoreCmd::Remove { pattern } => cli_remote::cmd_ignore_remove(&pattern),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "gesta-root-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_new_repository_uses_the_current_directory_name() {
        let d = tmp("new");
        std::fs::create_dir_all(d.join(ZF_DIR)).unwrap();
        assert_eq!(repo_dir(&d), d.join(ZF_DIR));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_repository_made_before_the_rename_is_still_found() {
        // The rename touched the directory name and the database file name at
        // once. Finding the directory but not the database created an empty one
        // beside the real one, and the repository then looked empty.
        let d = tmp("old");
        std::fs::create_dir_all(d.join(LEGACY_DIR)).unwrap();
        std::fs::write(d.join(LEGACY_DIR).join(LEGACY_DB), b"not empty").unwrap();

        assert_eq!(repo_dir(&d), d.join(LEGACY_DIR));
        assert_eq!(repo_db(&d), d.join(LEGACY_DIR).join(LEGACY_DB));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_empty_database_of_the_new_name_does_not_shadow_the_real_one() {
        // An interrupted start can leave a zero byte file behind. Choosing it
        // because it exists is how a full repository reads as empty.
        let d = tmp("shadow");
        std::fs::create_dir_all(d.join(LEGACY_DIR)).unwrap();
        std::fs::write(d.join(LEGACY_DIR).join(LEGACY_DB), b"schema and data").unwrap();
        std::fs::write(d.join(LEGACY_DIR).join(ZF_DB), b"").unwrap();

        assert_eq!(repo_db(&d), d.join(LEGACY_DIR).join(LEGACY_DB));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_new_name_wins_when_both_exist_with_content() {
        let d = tmp("both");
        std::fs::create_dir_all(d.join(ZF_DIR)).unwrap();
        std::fs::create_dir_all(d.join(LEGACY_DIR)).unwrap();
        std::fs::write(d.join(ZF_DIR).join(ZF_DB), b"new data").unwrap();
        std::fs::write(d.join(LEGACY_DIR).join(LEGACY_DB), b"old data").unwrap();

        assert_eq!(repo_dir(&d), d.join(ZF_DIR));
        assert_eq!(repo_db(&d), d.join(ZF_DIR).join(ZF_DB));
        let _ = std::fs::remove_dir_all(&d);
    }
}
