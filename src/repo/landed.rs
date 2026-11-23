//! `gesta landed`: which merge brought a changeset into a branch, and when.
//!
//! This is the question Wren answers with binary lifting over the first-parent chain. The shape of
//! the answer is: given a changeset and a branch, find the first changeset reachable from the branch
//! head that contains the given one. That changeset is where the work landed.
//!
//! The cheap direction first. A changeset whose generation is at or below the branch head's cannot
//! have landed after it, and one that is not an ancestor of the head never landed at all. Both are
//! answered from the generation numbers recorded at commit time, which is why they cost one row read
//! rather than a walk of the whole branch.
//!
//! What is left is the search itself: walk from the head towards the root, following the first parent
//! because that is the line the branch moved along, and stop at the first commit that has the target
//! as an ancestor. That commit is the landing. When the changeset arrived through a merge, the merge
//! is the answer; when the branch is linear, the changeset is its own landing.

use crate::store;
use crate::{bad, ZResult};

/// Where a changeset landed on a branch.
pub struct Landed {
    /// The changeset that first made the target reachable from this branch. A merge commit when the
    /// work arrived through one, the changeset itself when the branch was linear.
    pub landing: String,
    /// The changeset's own message, so a caller can confirm it found what it asked for.
    pub target_message: String,
    /// The landing changeset's message.
    pub landing_message: String,
    /// Creation time of the landing, in seconds.
    pub landed_at: i64,
    /// How many first-parent steps were taken from the branch head. Zero means the target is the head.
    pub steps: usize,
}

/// Find where `target` first became reachable from `branch`.
///
/// Returns None when the target is not on the branch at all, which is a real answer: work that was
/// never landed, or landed somewhere else. It is not an error, and a caller that treats it as one
/// cannot distinguish "not yet" from "wrong repository".
pub fn find(conn: &rusqlite::Connection, target: &str, branch: &str) -> ZResult<Option<Landed>> {
    let Some(head) = store::branch_head(conn, branch)? else {
        return Ok(None); // an empty branch has landed nothing
    };

    let target_message: String = match conn.query_row(
        "SELECT message FROM changesets WHERE id = ?1",
        rusqlite::params![target],
        |r| r.get(0),
    ) {
        Ok(m) => m,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            return Err(bad(format!("no such changeset: {}", target)))
        }
        Err(e) => return Err(e.into()),
    };

    // Not an ancestor of the head at all: never landed here, whatever else is true.
    if !store::is_ancestor(conn, target, &head)? {
        return Ok(None);
    }

    // Walk the first-parent chain from the head towards the root and keep the *oldest* commit that
    // still contains the target. That is where it became reachable, which is what "landed" means.
    //
    // Taking the first one that contains it is the mistake this code made on the first run: on a
    // linear history every commit from the target onwards contains it, so the walk answered "landed
    // at the head, 0 steps from the head" for a change made three commits ago. The number that says
    // where a change arrived is the earliest commit that has it, not the latest.
    //
    // The chain is read once, here, rather than asking `is_ancestor` at every step. That call builds
    // the whole parent map, so a loop over n steps read every row of `changesets` n times: answering
    // one question cost O(n^2) rows, which is the shape the systems this borrows from were built to
    // avoid. One read of the chain plus one read of the parent map answers the same question once.
    let chain = store::first_parent_chain(conn, &head)?;
    let parents = store::parent_map_for(conn)?;

    // The chain runs newest to oldest, and the answer is a prefix of it: every commit from the head
    // down to the landing contains the target, and none below the landing does. That makes the answer
    // a boundary, so a binary search finds it in log(n) ancestry checks rather than one per step.
    //
    // When the target is itself on the chain the boundary is exact and no ancestry check is needed:
    // the target landed at its own position, and the commit below it is a parent that predates it.
    let on_chain = chain.iter().position(|id| id == target);

    let landing_index = match on_chain {
        Some(i) => Some(i),
        None => {
            // Index 0 is known to contain the target (checked above), so a boundary always exists in
            // [0, len). `lo` is the highest index proven to contain it, `hi` the lowest proven not to.
            let mut lo = 0usize;
            let mut hi = chain.len();
            while hi - lo > 1 {
                let mid = lo + (hi - lo) / 2;
                if store::walk_reaches(&parents, &chain[mid], target) {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            Some(lo)
        }
    };

    let landing: Option<(String, usize)> =
        landing_index.and_then(|i| chain.get(i).map(|id| (id.clone(), i)));

    let Some((id, at_steps)) = landing else {
        return Ok(None);
    };
    let (msg, at): (String, i64) = conn.query_row(
        "SELECT message, created_at FROM changesets WHERE id = ?1",
        rusqlite::params![id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(Some(Landed {
        landing: id,
        target_message,
        landing_message: msg,
        landed_at: at,
        steps: at_steps,
    }))
}
