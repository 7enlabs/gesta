//! Access control for the HTTP remote.
//!
//! Two roles: a read token may clone and pull, a write token may also push. When
//! the server has no tokens configured it refuses to bind to anything except a
//! loopback address, so an open port is never the accidental default.

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
    tokens: HashMap<String, (String, Role)>,
    pub required: bool,
}

impl Auth {
    /// Build from `.zenforge/auth` entries.
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
                let mut parts = rest.splitn(3, ':');
                let (Some(digest_hex), Some(role), name) =
                    (parts.next(), parts.next(), parts.next().unwrap_or("unnamed"))
                else {
                    continue;
                };
                let digest_hex = digest_hex.trim().to_ascii_lowercase();
                if digest_hex.len() != 64 || !digest_hex.chars().all(|c| c.is_ascii_hexdigit()) {
                    continue; // a malformed digest must never match anything
                }
                let role = parse_role(role);
                tokens.insert(digest_hex, (name.trim().to_string(), role));
                continue;
            }

            let mut parts = line.splitn(3, ':');
            let token = match parts.next() {
                Some(t) if !t.trim().is_empty() => t.trim().to_string(),
                _ => continue,
            };
            let role = parse_role(parts.next().unwrap_or("read"));
            let name = parts.next().unwrap_or("unnamed").trim().to_string();
            tokens.insert(digest(&token), (name, role));
        }
        Auth { required: !tokens.is_empty(), tokens }
    }

    pub fn token_count(&self) -> usize {
        self.tokens.len()
    }

    /// Resolve a presented token to a role, or `None` when it is unknown.
    pub fn authenticate(&self, presented: &str) -> Option<(String, Role)> {
        self.tokens.get(&digest(presented)).cloned()
    }

    /// Does `role` satisfy `needed`?
    pub fn allows(role: Role, needed: Role) -> bool {
        role >= needed
    }
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

        let (name, role) = a.authenticate("abc123").unwrap();
        assert_eq!(name, "alice");
        assert_eq!(role, Role::Write);

        let (name, role) = a.authenticate("def456").unwrap();
        assert_eq!(name, "bob");
        assert_eq!(role, Role::Read);
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
        let (_, role) = a.authenticate("tokenonly").unwrap();
        assert_eq!(role, Role::Read, "a malformed entry must not grant write");
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
        let (name, role) = a.authenticate(token).unwrap();
        assert_eq!(name, "carol");
        assert_eq!(role, Role::Write);
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
        assert_eq!(a.authenticate("plain").unwrap().1, Role::Read);
        assert_eq!(a.authenticate(secret).unwrap().1, Role::Write);
    }

    #[test]
    fn loopback_detection() {
        assert!(is_loopback("127.0.0.1"));
        assert!(is_loopback("localhost"));
        assert!(is_loopback("::1"));
        assert!(!is_loopback("0.0.0.0"));
        assert!(!is_loopback("192.168.1.10"));
    }
}
