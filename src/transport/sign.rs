//! Signed changesets.
//!
//! A commit message says who wrote a change. Without a signature that claim is whatever the
//! author typed, which is fine on one machine and worthless the moment a recorded history is
//! shared. Ed25519 over the changeset's own identity gives a verifier something to check that
//! does not depend on trusting the writer.
//!
//! What is signed is the changeset id plus its message, author and parent. The id already
//! commits to the tree, so signing it covers the content without walking the tree again.
//!
//! ```text
//! $ gesta key generate
//! created a signing key: /home/me/.gesta/signing.key
//! public key: 3f2a...c1   add this to the people you want to verify
//!
//! $ gesta changeset create "fix the parser" --author me --sign
//! signed with 3f2a...c1
//! ```
//!
//! The private key is one file, mode 0600, and it never leaves the machine. This is not a
//! replacement for a hardware token: a compromised account defeats it exactly as it defeats an
//! ssh key, and the docs say so.

use crate::{bad, ZResult};
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use std::path::{Path, PathBuf};

/// Where the signing key lives when no path is given.
pub fn default_key_path() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    Path::new(&home).join(".gesta").join("signing.key")
}

/// The exact bytes a signature covers.
///
/// Every field that identifies the change is here, and the separator makes a field that shifts
/// from one position to another produce a different payload rather than the same one.
pub fn signed_payload(id: &str, message: &str, author: &str, parent: Option<&str>) -> Vec<u8> {
    format!(
        "gesta-changeset-v1\nid:{}\nparent:{}\nauthor:{}\nmessage:{}",
        id,
        parent.unwrap_or("-"),
        author,
        message
    )
    .into_bytes()
}

/// A key pair on disk: the private seed, hex encoded, three lines of context.
pub struct SigningKey {
    pair: Ed25519KeyPair,
    pub path: PathBuf,
}

impl SigningKey {
    /// Generate a new key and write it, refusing to overwrite an existing one.
    pub fn generate(path: &Path) -> ZResult<SigningKey> {
        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng)
            .map_err(|_| bad("the system random source refused to generate a key"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // One call, not `exists()` then `write()` (#161). The check and the write were two
        // operations, so two concurrent generates both passed the check and the second overwrote the
        // first - a key lost with no error to either caller. And `fs::write` creates 0644 under the
        // usual umask, with the mode fixed up afterwards, so the key was readable by any local
        // process for the width of the write. `create_new` with mode 0600 at creation answers
        // "does it exist" and "make it mine" in the same syscall.
        match crate::support::private_file::write_new_private(path, pkcs8.as_ref()) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(bad(format!(
                    "{} already exists: move it aside rather than replacing it, or every signature it made becomes unverifiable",
                    path.display()
                )));
            }
            Err(e) => return Err(e.into()),
        }
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
            .map_err(|_| bad("the key just generated cannot be loaded"))?;
        Ok(SigningKey {
            pair,
            path: path.to_path_buf(),
        })
    }

    /// Load an existing key.
    pub fn load(path: &Path) -> ZResult<SigningKey> {
        let bytes = std::fs::read(path).map_err(|e| {
            bad(format!(
                "cannot read the signing key {}: {}. Run `gesta key generate` first",
                path.display(),
                e
            ))
        })?;
        let pair = Ed25519KeyPair::from_pkcs8(&bytes).map_err(|_| {
            bad(format!(
                "{} is not an Ed25519 signing key this build can read",
                path.display()
            ))
        })?;
        Ok(SigningKey {
            pair,
            path: path.to_path_buf(),
        })
    }

    /// The public half, hex, which is what a verifier needs.
    pub fn public_hex(&self) -> String {
        hex::encode(self.pair.public_key().as_ref())
    }

    /// Sign a changeset's identity.
    pub fn sign_changeset(
        &self,
        id: &str,
        message: &str,
        author: &str,
        parent: Option<&str>,
    ) -> String {
        let payload = signed_payload(id, message, author, parent);
        hex::encode(self.pair.sign(&payload).as_ref())
    }
}

/// Check a signature against a public key.
pub fn verify(
    public_hex: &str,
    id: &str,
    message: &str,
    author: &str,
    parent: Option<&str>,
    signature_hex: &str,
) -> ZResult<()> {
    let public =
        hex::decode(public_hex.trim()).map_err(|_| bad("the public key is not valid hex"))?;
    let signature =
        hex::decode(signature_hex.trim()).map_err(|_| bad("the signature is not valid hex"))?;
    let payload = signed_payload(id, message, author, parent);
    UnparsedPublicKey::new(&ring::signature::ED25519, &public)
        .verify(&payload, &signature)
        .map_err(|_| {
            bad(format!(
                "signature does not match changeset {} as recorded: the message, the author or the parent was changed after signing",
                id
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_key(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("gesta-key-{}-{}", std::process::id(), name))
    }

    #[test]
    fn a_signature_verifies_against_the_key_that_made_it() {
        let path = temp_key("round");
        let _ = std::fs::remove_file(&path);
        let key = SigningKey::generate(&path).unwrap();
        let sig = key.sign_changeset("csabc", "a message", "alice", Some("csparent"));
        verify(
            &key.public_hex(),
            "csabc",
            "a message",
            "alice",
            Some("csparent"),
            &sig,
        )
        .expect("a signature made by this key must verify");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_changed_message_breaks_the_signature() {
        let path = temp_key("tamper-msg");
        let _ = std::fs::remove_file(&path);
        let key = SigningKey::generate(&path).unwrap();
        let sig = key.sign_changeset("csabc", "the real message", "alice", None);
        let err = verify(
            &key.public_hex(),
            "csabc",
            "a different message",
            "alice",
            None,
            &sig,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("does not match"), "unhelpful message: {}", err);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_changed_author_breaks_the_signature() {
        let path = temp_key("tamper-author");
        let _ = std::fs::remove_file(&path);
        let key = SigningKey::generate(&path).unwrap();
        let sig = key.sign_changeset("csabc", "m", "alice", None);
        assert!(verify(&key.public_hex(), "csabc", "m", "mallory", None, &sig).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_changed_parent_breaks_the_signature() {
        let path = temp_key("tamper-parent");
        let _ = std::fs::remove_file(&path);
        let key = SigningKey::generate(&path).unwrap();
        let sig = key.sign_changeset("csabc", "m", "alice", Some("csp1"));
        assert!(verify(&key.public_hex(), "csabc", "m", "alice", Some("csp2"), &sig).is_err());
        // Also a parent that was removed.
        assert!(verify(&key.public_hex(), "csabc", "m", "alice", None, &sig).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn another_key_does_not_verify_it() {
        let a = temp_key("other-a");
        let b = temp_key("other-b");
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
        let ka = SigningKey::generate(&a).unwrap();
        let kb = SigningKey::generate(&b).unwrap();
        let sig = ka.sign_changeset("csabc", "m", "alice", None);
        assert!(
            verify(&kb.public_hex(), "csabc", "m", "alice", None, &sig).is_err(),
            "a signature verified against the wrong key"
        );
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn generating_over_an_existing_key_is_refused() {
        let path = temp_key("exists");
        let _ = std::fs::remove_file(&path);
        SigningKey::generate(&path).unwrap();
        // `match` rather than `unwrap_err`: a SigningKey holds a private key, so it must not
        // implement Debug, and `expect` would need one to print the value it was handed.
        match SigningKey::generate(&path) {
            Ok(_) => panic!("replacing a key silently would strand every signature it made"),
            Err(e) => {
                let text = e.to_string();
                assert!(
                    text.contains("already exists"),
                    "unhelpful message: {}",
                    text
                );
            }
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_payload_separates_its_fields() {
        // A message that shifts into the author position must not produce the same bytes.
        let a = signed_payload("cs1", "x\nauthor:bob", "alice", None);
        let b = signed_payload("cs1", "x", "bob", None);
        assert_ne!(a, b);
    }
    #[test]
    fn a_generated_key_is_created_unreadable_not_made_so_afterwards() {
        // #161, and the first version of this test was a lesson in what does not work.
        //
        // It asserted on the mode of the finished file. That test passes against the old code too -
        // `fs::write` then `set_permissions(0600)` ends at 0600, so the only thing it proved was
        // that the fix-up ran. The window between the two calls is real and invisible from the end
        // state, which is the whole point: you cannot observe a gap by looking at what comes after
        // it (A60).
        //
        // What can be observed is the property that removes the gap. `create_new` with mode 0600
        // makes the file exist and private in the same syscall, so a second create cannot succeed
        // and no reader sees a wider file. The mode is still asserted - it is the outcome a person
        // cares about - but the race test above is what makes the claim, and this one would pass
        // either way, so it is stated as the weaker check it is.
        let d = std::env::temp_dir().join(format!("zf-key-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let path = d.join("signing.key");

        SigningKey::generate(&path).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the key ended at {mode:o}, not 600");
        }
        let key = SigningKey::load(&path).unwrap();
        assert_eq!(key.path, path);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn concurrent_generates_against_one_path_produce_exactly_one_key() {
        // The half of #161 a sequential test cannot reach. `if path.exists() { refuse }` followed by
        // `write` is a check-then-act: two threads can both pass the check, and then both write, so
        // the loser's key vanishes with no error to either caller - and any signature the loser made
        // stops verifying. Racing them is the only way to see it.
        //
        // Sixteen threads on a barrier, so they start together rather than one after another. With
        // `create_new` exactly one wins by construction; it is not a matter of timing.
        let d = std::env::temp_dir().join(format!("zf-key-race2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let path = d.join("contended.key");

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
        let mut handles = Vec::new();
        for _ in 0..16 {
            let path = path.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                SigningKey::generate(&path).is_ok()
            }));
        }
        let wins = handles
            .into_iter()
            .filter(|_| true)
            .map(|h| h.join().unwrap())
            .filter(|ok| *ok)
            .count();
        assert_eq!(
            wins, 1,
            "{wins} of 16 racing generates reported success on one path; the loser's key was \
             overwritten and neither caller was told"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn two_generates_cannot_both_win() {
        // The other half of #161: `if path.exists() { refuse }` then `write` let two callers both
        // pass the check, and the second overwrote the first's key with no error to either. With
        // `create_new` exactly one call succeeds and the other gets AlreadyExists.
        let d = std::env::temp_dir().join(format!("zf-key-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let path = d.join("contended.key");

        let first = SigningKey::generate(&path);
        assert!(first.is_ok(), "the first generate must succeed");
        let winner = std::fs::read(&path).unwrap();

        let second = SigningKey::generate(&path);
        assert!(
            second.is_err(),
            "a second generate on the same path reported success and replaced a key that signatures \
             already depend on"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            winner,
            "the losing call rewrote the file"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_refusal_for_an_existing_key_still_says_why() {
        // The message is the whole value of the refusal: "already exists" without the reason reads
        // like a bug, and somebody moves the file aside without knowing what it costs.
        let d = std::env::temp_dir().join(format!("zf-key-msg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let path = d.join("k");
        SigningKey::generate(&path).unwrap();
        let err = match SigningKey::generate(&path) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a second generate on the same path succeeded"),
        };
        assert!(
            err.contains("unverifiable"),
            "the refusal does not explain why replacing the key is worse than failing: {err}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
