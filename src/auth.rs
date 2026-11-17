//! Access control for the HTTP remote.
//!
//! Two roles: a read token may clone and pull, a write token may also push. A
//! token may also carry a comma-separated list of path globs; a write that
//! touches a path outside that list is refused. When the server has no tokens
//! configured it refuses to bind to anything except a loopback address, so an
//! open port is never the accidental default.

use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// What a token is allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Read,
    Write,
}

/// Token store. Tokens are kept as SHA-256 digests, never in clear text, so a
/// leaked config file does not leak working credentials.
pub struct Auth {
    tokens: HashMap<String, Grant>,
    pub required: bool,
}

/// One authenticated token: who it is, what it may do, and where.
#[derive(Debug, Clone)]
pub struct Grant {
    pub name: String,
    pub role: Role,
    /// Glob patterns for the paths this token may write. Empty means every path,
    /// which is what an entry without a scope field grants.
    pub scope: Vec<String>,
    /// Unix timestamp after which the token is dead. `None` means it does not
    /// expire, which is the old behaviour and still the default.
    pub expires_at: Option<i64>,
}

impl Grant {
    pub fn is_expired(&self, now: i64) -> bool {
        matches!(self.expires_at, Some(t) if now >= t)
    }
}

impl Grant {
    /// Is a write to `path` inside this grant's scope?
    ///
    /// An empty scope means unrestricted. A pattern without a slash matches the
    /// file name at any depth, so `*.md` covers `docs/a.md` and `a.md`, which is
    /// what a person writing the pattern usually means.
    pub fn permits(&self, path: &str) -> bool {
        if self.scope.is_empty() {
            return true;
        }
        let name = path.rsplit('/').next().unwrap_or(path);
        self.scope.iter().any(|pat| {
            glob_match(pat, path) || (!pat.contains('/') && glob_match(pat, name))
        })
    }
}

/// Match one glob pattern. Supports `*` inside a segment and `**` across
/// segments, plus `?` for a single character.
/// Match one glob pattern against a path.
///
/// Shared with sparse checkout: two matchers that disagree about `**` would be
/// a subtle and very annoying bug.
pub fn glob_is_match(pattern: &str, text: &str) -> bool {
    glob_match(pattern, text)
}

fn glob_match(pattern: &str, text: &str) -> bool {
    fn go(p: &[char], t: &[char]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        if p[0] == '*' && p.len() > 1 && p[1] == '*' {
            // `**` crosses directory separators; the leading `/` in `**/x` is
            // optional so `**/a.md` also matches a top level `a.md`.
            let rest = &p[2..];
            let rest = if rest.first() == Some(&'/') { &rest[1..] } else { rest };
            for skip in 0..=t.len() {
                if go(rest, &t[skip..]) {
                    return true;
                }
                if skip < t.len() && t[skip] == '/' {
                    continue;
                }
            }
            // Simple fallback: try every suffix.
            for skip in 0..=t.len() {
                if go(rest, &t[skip..]) {
                    return true;
                }
            }
            false
        } else if p[0] == '*' {
            let rest = &p[1..];
            if go(rest, t) {
                return true;
            }
            for skip in 0..t.len() {
                if t[skip] == '/' {
                    return false; // a single `*` does not cross a separator
                }
                if go(rest, &t[skip + 1..]) {
                    return true;
                }
            }
            false
        } else if p[0] == '?' {
            !t.is_empty() && t[0] != '/' && go(&p[1..], &t[1..])
        } else {
            !t.is_empty() && p[0] == t[0] && go(&p[1..], &t[1..])
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let x: Vec<char> = text.chars().collect();
    go(&p, &x)
}

impl Auth {
    /// Build from the `auth` file inside the repository directory.
    ///
    /// Two forms are accepted, and the file may mix them:
    ///
    /// ```text
    /// <token>:<role>:<name>                 token in clear, hashed at load
    /// sha256:<digest>:<role>:<name>         only the digest is stored
    /// ```
    ///
    /// The second form is the one to use when the file might be readable by
    /// someone else: a stolen digest cannot be presented as a token.
    pub fn from_entries(entries: &[String]) -> Auth {
        let mut tokens = HashMap::new();
        for entry in entries {
            let line = entry.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            // `sha256:<digest>:<role>:<name>` has four fields, the clear form
            // has three, so the digest marker has to be handled first.
            if let Some(rest) = line.strip_prefix("sha256:") {
                let mut parts = rest.splitn(5, ':');
                let (Some(digest_hex), Some(role)) = (parts.next(), parts.next()) else {
                    continue;
                };
                let name = parts.next().unwrap_or("unnamed");
                let scope = parts.next().unwrap_or("");
                let expires = parts.next().unwrap_or("");
                let digest_hex = digest_hex.trim().to_ascii_lowercase();
                if digest_hex.len() != 64 || !digest_hex.chars().all(|c| c.is_ascii_hexdigit()) {
                    continue; // a malformed digest must never match anything
                }
                let role = parse_role(role);
                tokens.insert(
                    digest_hex,
                    Grant {
                        name: name.trim().to_string(),
                        role,
                        scope: parse_scope(scope),
                        expires_at: parse_expiry(expires),
                    },
                );
                continue;
            }

            let mut parts = line.splitn(5, ':');
            let token = match parts.next() {
                Some(t) if !t.trim().is_empty() => t.trim().to_string(),
                _ => continue,
            };
            let role = parse_role(parts.next().unwrap_or("read"));
            let name = parts.next().unwrap_or("unnamed").trim().to_string();
            let scope = parts.next().unwrap_or("");
            let expires = parts.next().unwrap_or("");
            tokens.insert(
                digest(&token),
                Grant {
                    name,
                    role,
                    scope: parse_scope(scope),
                    expires_at: parse_expiry(expires),
                },
            );
        }
        Auth { required: !tokens.is_empty(), tokens }
    }

    pub fn token_count(&self) -> usize {
        self.tokens.len()
    }

    /// Resolve a presented token to a role, or `None` when it is unknown.
    #[cfg(test)]
    pub fn authenticate(&self, presented: &str) -> Option<Grant> {
        self.tokens.get(&digest(presented)).cloned()
    }

    /// Authenticate and reject a grant that has expired.
    ///
    /// Separate from `authenticate` so a caller can tell "wrong token" from
    /// "right token, too late" and answer 401 with a different reason. The
    /// server uses this one.
    pub fn authenticate_now(&self, presented: &str, now: i64) -> AuthOutcome {
        match self.tokens.get(&digest(presented)) {
            None => AuthOutcome::Unknown,
            Some(g) if g.is_expired(now) => AuthOutcome::Expired(g.name.clone()),
            Some(g) => AuthOutcome::Granted(g.clone()),
        }
    }

    /// Does `role` satisfy `needed`?
    pub fn allows(role: Role, needed: Role) -> bool {
        role >= needed
    }
}

/// The result of checking a presented token.
#[derive(Debug, Clone)]
pub enum AuthOutcome {
    Granted(Grant),
    Expired(String),
    Unknown,
}

/// Parse an expiry field.
///
/// Accepts a unix timestamp, an ISO date (`2027-03-01`), or an ISO timestamp.
/// An empty field or `never` means no expiry. Anything unparseable also means no
/// expiry rather than "expires now", because a typo locking every agent out of a
/// repository is worse than a token that lives longer than intended.
pub fn parse_expiry(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("never") {
        return None;
    }
    if let Ok(n) = s.parse::<i64>() {
        return Some(if n > 1_000_000_000_000 { n / 1000 } else { n });
    }
    // A bare date expires at the last second of that day; a full timestamp is
    // taken as written. Both are UTC, which is what the stored value is.
    if s.len() == 10 {
        return crate::clock::end_of_day(s);
    }
    crate::clock::parse_minute(s).or_else(|| {
        // A timestamp with seconds, such as 2027-03-01T12:00:00.
        let base = crate::clock::parse_minute(&s[..16.min(s.len())])?;
        let rest = s.get(16..)?;
        let secs: i64 = rest.trim_start_matches(':').parse().ok()?;
        Some(base + secs)
    })
}

fn parse_scope(s: &str) -> Vec<String> {
    s.split(',')
        .map(|p| p.trim())
        .filter(|p| !p.is_empty() && *p != "*")
        .map(|p| p.trim_start_matches("./").to_string())
        .collect()
}

fn parse_role(s: &str) -> Role {
    if s.trim().eq_ignore_ascii_case("write") {
        Role::Write
    } else {
        Role::Read
    }
}

pub fn digest(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    hex::encode(h.finalize())
}

/// Generate a token of `bytes` random bytes, hex encoded.
///
/// Reads from the OS entropy source. Returns an error rather than falling back
/// to a weak source, because a guessable token is worse than no server.
pub fn generate_token(bytes: usize) -> std::io::Result<String> {
    let mut buf = vec![0u8; bytes];
    let mut f = std::fs::File::open("/dev/urandom")?;
    use std::io::Read;
    f.read_exact(&mut buf)?;
    Ok(hex::encode(buf))
}

/// A bind address is private when it can only be reached from this machine.
pub fn is_loopback(bind: &str) -> bool {
    matches!(bind, "127.0.0.1" | "localhost" | "::1" | "[::1]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_has_no_tokens() {
        let a = Auth::from_entries(&[]);
        assert!(!a.required);
        assert_eq!(a.token_count(), 0);
    }

    #[test]
    fn parses_token_role_and_name() {
        let a = Auth::from_entries(&["abc123:write:alice".to_string(), "def456:read:bob".to_string()]);
        assert_eq!(a.token_count(), 2);

        let g = a.authenticate("abc123").unwrap();
        assert_eq!(g.name, "alice");
        assert_eq!(g.role, Role::Write);

        let g = a.authenticate("def456").unwrap();
        assert_eq!(g.name, "bob");
        assert_eq!(g.role, Role::Read);
    }

    #[test]
    fn unknown_token_is_rejected() {
        let a = Auth::from_entries(&["abc123:write:alice".to_string()]);
        assert!(a.authenticate("wrong").is_none());
        assert!(a.authenticate("").is_none());
    }

    #[test]
    fn a_line_without_a_role_defaults_to_read() {
        let a = Auth::from_entries(&["tokenonly".to_string()]);
        let g = a.authenticate("tokenonly").unwrap();
        assert_eq!(g.role, Role::Read, "a malformed entry must not grant write");
    }

    #[test]
    fn comments_and_blanks_are_ignored() {
        let a = Auth::from_entries(&[
            "# a comment".to_string(),
            "".to_string(),
            "  ".to_string(),
            "real:write:x".to_string(),
        ]);
        assert_eq!(a.token_count(), 1);
    }

    #[test]
    fn write_role_satisfies_read_but_not_the_reverse() {
        assert!(Auth::allows(Role::Write, Role::Write));
        assert!(Auth::allows(Role::Write, Role::Read));
        assert!(Auth::allows(Role::Read, Role::Read));
        assert!(!Auth::allows(Role::Read, Role::Write));
    }

    #[test]
    fn digests_are_stable_and_not_reversible() {
        assert_eq!(digest("abc"), digest("abc"));
        assert_ne!(digest("abc"), digest("abd"));
        assert_eq!(digest("abc").len(), 64);
        assert!(!digest("abc").contains("abc"));
    }

    #[test]
    fn generated_tokens_have_the_requested_entropy_and_differ() {
        let a = generate_token(32).unwrap();
        let b = generate_token(32).unwrap();
        assert_eq!(a.len(), 64, "32 bytes hex encoded");
        assert_ne!(a, b, "two calls must not repeat");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn a_stored_digest_authenticates_the_matching_token() {
        let token = "s3cret-token-value";
        let line = format!("sha256:{}:write:carol", digest(token));
        let a = Auth::from_entries(&[line]);
        let g = a.authenticate(token).unwrap();
        assert_eq!(g.name, "carol");
        assert_eq!(g.role, Role::Write);
        assert!(a.authenticate("wrong").is_none());
    }

    #[test]
    fn a_stored_digest_is_not_itself_a_token() {
        // Presenting the digest must not authenticate: only the preimage does.
        let token = "real-token";
        let d = digest(token);
        let a = Auth::from_entries(&[format!("sha256:{}:write:x", d)]);
        assert!(a.authenticate(&d).is_none(), "a stolen digest must not work as a token");
        assert!(a.authenticate(token).is_some());
    }

    #[test]
    fn a_malformed_digest_never_matches() {
        let a = Auth::from_entries(&["sha256:nothex:write:x".to_string()]);
        assert_eq!(a.token_count(), 0, "a broken line must not create an entry");
    }

    #[test]
    fn clear_and_hashed_entries_can_be_mixed() {
        let secret = "abc";
        let a = Auth::from_entries(&[
            "plain:read:p".to_string(),
            format!("sha256:{}:write:h", digest(secret)),
        ]);
        assert_eq!(a.token_count(), 2);
        assert_eq!(a.authenticate("plain").unwrap().role, Role::Read);
        assert_eq!(a.authenticate(secret).unwrap().role, Role::Write);
    }

    #[test]
    fn loopback_detection() {
        assert!(is_loopback("127.0.0.1"));
        assert!(is_loopback("localhost"));
        assert!(is_loopback("::1"));
        assert!(!is_loopback("0.0.0.0"));
        assert!(!is_loopback("192.168.1.10"));
    }

    #[test]
    fn a_token_without_an_expiry_field_never_expires() {
        let a = Auth::from_entries(&["abc:write:alice".to_string()]);
        let g = a.authenticate("abc").unwrap();
        assert_eq!(g.expires_at, None);
        assert!(!g.is_expired(i64::MAX / 2));
    }

    #[test]
    fn a_unix_timestamp_expiry_is_honoured() {
        let a = Auth::from_entries(&["abc:write:alice::1000".to_string()]);
        let g = a.authenticate("abc").unwrap();
        assert_eq!(g.expires_at, Some(1000));
        assert!(!g.is_expired(999));
        assert!(g.is_expired(1000), "the exact second counts as expired");
        assert!(g.is_expired(1001));
    }

    #[test]
    fn an_iso_date_expiry_covers_the_whole_day() {
        // A person writing 2027-03-01 means "through that day", not "until
        // midnight at the start of it".
        let ts = parse_expiry("2027-03-01").unwrap();
        // Stated as the formatted text, so the assertion reads the same way the
        // value is shown to a person.
        assert_eq!(crate::clock::format_rfc3339(ts), "2027-03-01T23:59:59Z");
    }

    #[test]
    fn a_millisecond_timestamp_is_normalised_to_seconds() {
        assert_eq!(parse_expiry("1700000000000"), Some(1_700_000_000));
    }

    #[test]
    fn an_unparseable_expiry_means_no_expiry_not_lockout() {
        // A typo must not revoke every agent at once.
        assert_eq!(parse_expiry("next tuesday"), None);
        assert_eq!(parse_expiry(""), None);
        assert_eq!(parse_expiry("never"), None);
    }

    #[test]
    fn authenticate_now_separates_expired_from_unknown() {
        let a = Auth::from_entries(&["abc:write:alice::1000".to_string()]);
        assert!(matches!(a.authenticate_now("abc", 500), AuthOutcome::Granted(_)));
        match a.authenticate_now("abc", 2000) {
            AuthOutcome::Expired(name) => assert_eq!(name, "alice"),
            other => panic!("expected Expired, got {:?}", other),
        }
        assert!(matches!(a.authenticate_now("wrong", 500), AuthOutcome::Unknown));
    }

    #[test]
    fn an_expiry_works_on_the_hashed_form_too() {
        let token = "s3cret";
        let line = format!("sha256:{}:read:bob:docs/**:1800000000", digest(token));
        let a = Auth::from_entries(&[line]);
        let g = a.authenticate(token).unwrap();
        assert_eq!(g.expires_at, Some(1_800_000_000));
        assert_eq!(g.scope, vec!["docs/**".to_string()]);
    }

    #[test]
    fn a_scope_pattern_matches_the_way_a_person_expects() {
        let g = Grant {
            name: "x".into(),
            role: Role::Write,
            scope: vec!["docs/**".into(), "*.md".into(), "src/lib/one.rs".into()],
            expires_at: None,
        };
        assert!(g.permits("docs/a/b.md"));
        assert!(g.permits("README.md"));
        assert!(g.permits("nested/readme.md"), "a bare name pattern matches any depth");
        assert!(g.permits("src/lib/one.rs"));
        assert!(!g.permits("src/lib/two.rs"));
        assert!(!g.permits("src/main.rs"));
    }
}
