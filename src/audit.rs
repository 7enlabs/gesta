#![allow(dead_code)]
//! The record of what the server decided, not only what it did.
//!
//! `access.log` records the requests that succeeded. A request that was refused left no trace, so a
//! token being guessed, a path outside a scope being tried, or a branch outside a token's range being
//! pushed all looked like nothing happened. That is the wrong way round: the refusals are the events
//! worth reading, and a log that only holds successes answers "was it used" but never "was it tried".
//!
//! The format is one JSON document per line: a person can read it, `jq` can filter it, and a reader
//! that does not understand a field can ignore it. It is appended under a lock because two threads
//! writing one file interleave their lines, and a half line is a document that fails to parse.

use crate::ZResult;
use std::io::Write;

/// What the server decided about a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Decision {
    Allowed,
    /// No credential, or one that matches nothing.
    NoToken,
    /// A token that exists and has expired.
    Expired,
    /// A real token whose role is below what the request needed.
    WrongRole,
    /// A real token that may not write this path.
    PathRefused,
    /// A real token that may not write this branch.
    BranchRefused,
    /// The request was understood and the arguments were wrong.
    BadRequest,
    /// The work was attempted and failed.
    Failed,
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Allowed => "allowed",
            Decision::NoToken => "no-token",
            Decision::Expired => "expired",
            Decision::WrongRole => "wrong-role",
            Decision::PathRefused => "path-refused",
            Decision::BranchRefused => "branch-refused",
            Decision::BadRequest => "bad-request",
            Decision::Failed => "failed",
        }
    }

    /// Whether the request changed anything. A reader filtering for events that matter usually wants
    /// the refusals, so this is stated rather than inferred from the string.
    pub fn allowed(self) -> bool {
        self == Decision::Allowed
    }
}

/// One line of the record.
#[allow(dead_code)]
pub struct Event<'a> {
    pub who: &'a str,
    pub decision: Decision,
    pub action: &'a str,
    /// The path, branch or ref the request named.
    pub target: &'a str,
    /// Why it was refused, or what it did. Short, and never a credential.
    pub reason: &'a str,
    pub remote: Option<&'a str>,
}

/// Append one event.
///
/// Never returns an error the caller is expected to act on: a request that was served must not fail
/// because the log could not be written, and a request that was refused is already being refused. The
/// failure is printed once to stderr so an operator who looks sees it.
#[allow(dead_code)]
pub fn record(root: &std::path::Path, event: &Event) {
    let path = crate::repo_dir(root).join("audit.log");
    let mut fields: Vec<(&str, String)> = vec![
        ("at", crate::clock::format_rfc3339(crate::clock::unix_now())),
        ("who", event.who.to_string()),
        ("decision", event.decision.as_str().to_string()),
        ("action", event.action.to_string()),
        ("target", event.target.to_string()),
        ("reason", event.reason.to_string()),
    ];
    if let Some(remote) = event.remote {
        fields.push(("remote", remote.to_string()));
    }

    // Rendered through the project's own writer so a quote or a newline in a reason cannot produce a
    // line that is not a document.
    let doc = crate::json::Json::obj(
        fields
            .into_iter()
            .map(|(k, v)| (k, crate::json::Json::str(v)))
            .collect(),
    );
    let line = format!("{}\n", doc.render());

    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        if f.write_all(line.as_bytes()).is_err() {
            eprintln!("audit log is not writable: {}", path.display());
        }
    }
}

/// A parsed line, for `gesta remote audit`.
#[allow(dead_code)]
pub struct Entry {
    pub at: String,
    pub who: String,
    pub decision: String,
    pub action: String,
    pub target: String,
    pub reason: String,
    pub raw: String,
}

/// Read the record, newest last.
///
/// `only_refused` answers the question a person actually asks after an incident: what was tried and
/// turned away.
#[allow(dead_code)]
pub fn read(root: &std::path::Path, only_refused: bool, limit: Option<usize>) -> ZResult<Vec<Entry>> {
    let path = crate::repo_dir(root).join("audit.log");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(Box::new(crate::error::Error::new(
                crate::error::Code::NotFound,
                format!("cannot read {}: {}", path.display(), e),
            )))
        }
    };

    let mut out: Vec<Entry> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let doc = match crate::json::parse(line) {
            Ok(d) => d,
            // A line that does not parse is a line written by an older build or cut short by a full
            // disk. Skipping it silently would hide the damage; keeping the raw text shows it.
            Err(_) => {
                out.push(Entry {
                    at: String::new(),
                    who: String::new(),
                    decision: "unreadable".to_string(),
                    action: String::new(),
                    target: String::new(),
                    reason: "this line is not a document".to_string(),
                    raw: line.to_string(),
                });
                continue;
            }
        };
        let get = |k: &str| -> String {
            crate::json::field(&doc, k)
                .and_then(crate::json::as_str)
                .unwrap_or("")
                .to_string()
        };
        let entry = Entry {
            at: get("at"),
            who: get("who"),
            decision: get("decision"),
            action: get("action"),
            target: get("target"),
            reason: get("reason"),
            raw: line.to_string(),
        };
        if only_refused && entry.decision == "allowed" {
            continue;
        }
        out.push(entry);
    }

    if let Some(n) = limit {
        if out.len() > n {
            out = out.split_off(out.len() - n);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "gesta-audit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(d.join(".gesta")).unwrap();
        d
    }

    #[test]
    fn a_refusal_is_recorded_and_is_a_document() {
        let d = dir();
        record(
            &d,
            &Event {
                who: "anonymous",
                decision: Decision::NoToken,
                action: "file",
                target: "a.txt",
                reason: "missing or invalid token",
                remote: Some("10.0.0.7"),
            },
        );
        let rows = read(&d, false, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].decision, "no-token");
        assert_eq!(rows[0].who, "anonymous");
        assert_eq!(rows[0].target, "a.txt");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_reason_with_a_quote_stays_one_line_and_one_document() {
        // The reason is where a path or a branch name lands, and both can hold a quote.
        let d = dir();
        record(
            &d,
            &Event {
                who: "agent",
                decision: Decision::BranchRefused,
                action: "push",
                target: "refs/heads/a\"b\nc",
                reason: "may not write that branch",
                remote: None,
            },
        );
        let rows = read(&d, false, None).unwrap();
        assert_eq!(rows.len(), 1, "a newline in the target made two lines");
        assert_eq!(rows[0].decision, "branch-refused");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn filtering_for_refusals_drops_the_successes() {
        let d = dir();
        for (decision, who) in [
            (Decision::Allowed, "agent"),
            (Decision::NoToken, "anonymous"),
            (Decision::Allowed, "agent"),
        ] {
            record(
                &d,
                &Event {
                    who,
                    decision,
                    action: "file",
                    target: "x",
                    reason: "",
                    remote: None,
                },
            );
        }
        let all = read(&d, false, None).unwrap();
        assert_eq!(all.len(), 3);
        let refused = read(&d, true, None).unwrap();
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].decision, "no-token");
        // The newest N, because the question is always about what just happened.
        let recent = read(&d, false, Some(2)).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[1].who, "agent");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_line_that_does_not_parse_is_shown_rather_than_skipped() {
        let d = dir();
        // Two lines: an invalid JSON line and a plain text line. Both are kept as evidence,
        // because a log that silently drops the line that does not parse hides the damage.
        std::fs::write(
            d.join(".gesta").join("audit.log"),
            "{\"at\":\"x\"\nnot json\n",
        )
        .unwrap();
        let rows = read(&d, false, None).unwrap();
        // Both lines are unreadable; both are kept as evidence.
        assert_eq!(rows.len(), 2);
        // First line is the truncated JSON, second is the plain text.
        assert_eq!(rows[0].decision, "unreadable");
        assert_eq!(rows[0].raw, "{\"at\":\"x\"");
        assert_eq!(rows[1].decision, "unreadable");
        assert_eq!(rows[1].raw, "not json");
        let _ = std::fs::remove_dir_all(&d);
    }
}
