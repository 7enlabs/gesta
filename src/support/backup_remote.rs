//! Encrypted off-site backups.
//!
//! `backup::backup` takes a consistent SQLite snapshot onto the local disk. That is a copy, not a
//! recovery plan: it sits on the same machine as the repository, so one disk failure loses both. This
//! seals that snapshot and puts it somewhere else.
//!
//! Three decisions:
//!
//! 1. **Encrypted before it leaves the process.** A backup carries every object, every branch and
//!    every blame range the repository has. Uploading that in the clear to an object store means the
//!    store's access control is the only thing between a stranger and the whole history. The seal is
//!    ChaCha20-Poly1305 with a key derived by HMAC-SHA256 from the passphrase, the same primitives the
//!    secret store already uses, so there is one implementation of "seal bytes" in the tree.
//!
//! 2. **The key never travels with the file.** The sealed format carries salt and nonce, never the
//!    passphrase. A backup and its passphrase stored in the same place is not encryption.
//!
//! 3. **Pruned by count, not by age.** A backup schedule that a person forgets to run leaves an old
//!    file; a backup schedule that runs every hour and never prunes fills a bucket. Keeping the last N
//!    is the rule that survives both.

use crate::transport::s3::S3Config;
use crate::{bad, ZResult};

const MAGIC: &str = "gesta-backup-v1";
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;

/// What a push did.
pub struct Pushed {
    pub key: String,
    pub plain_bytes: u64,
    pub sealed_bytes: u64,
    pub changesets: i64,
    pub objects: i64,
}

/// Seal arbitrary bytes with a passphrase.
///
/// The layout is `magic salt nonce ciphertext`, all hex after the magic, which is what makes the file
/// a text file: it can be pasted, diffed and stored in a place that only takes strings. A binary
/// format would be smaller and would be a second thing to parse on restore.
pub fn seal(plain: &[u8], passphrase: &str) -> ZResult<Vec<u8>> {
    use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305};
    use ring::rand::{SecureRandom, SystemRandom};

    if passphrase.is_empty() {
        return Err(bad(
            "a backup cannot be sealed without a passphrase: an unencrypted copy of the whole \
             history in an object store is not a backup, it is a disclosure"
                .to_string(),
        ));
    }

    let rng = SystemRandom::new();
    let mut salt = [0u8; SALT_LEN];
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rng.fill(&mut salt)
        .map_err(|_| bad("the system random source failed".to_string()))?;
    rng.fill(&mut nonce_bytes)
        .map_err(|_| bad("the system random source failed".to_string()))?;

    let key_bytes = crate::support::secrets::derive_key(passphrase, &salt);
    let unbound = UnboundKey::new(&CHACHA20_POLY1305, &key_bytes)
        .map_err(|_| bad("cannot build the cipher key".to_string()))?;
    let key = LessSafeKey::new(unbound);
    let nonce = Nonce::assume_unique_for_key(nonce_bytes);

    let mut buf = plain.to_vec();
    key.seal_in_place_append_tag(nonce, Aad::empty(), &mut buf)
        .map_err(|_| bad("sealing the backup failed".to_string()))?;

    let mut out = Vec::new();
    out.extend_from_slice(MAGIC.as_bytes());
    out.push(b'\n');
    out.extend_from_slice(hex::encode(salt).as_bytes());
    out.push(b'\n');
    out.extend_from_slice(hex::encode(nonce_bytes).as_bytes());
    out.push(b'\n');
    out.extend_from_slice(hex::encode(&buf).as_bytes());
    out.push(b'\n');
    Ok(out)
}

/// Open a sealed backup.
///
/// The tag is checked before the plaintext is returned, so a file that was truncated by a failed
/// upload fails here rather than during a restore into a live repository.
pub fn unseal(sealed: &[u8], passphrase: &str) -> ZResult<Vec<u8>> {
    use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305};

    let text = std::str::from_utf8(sealed).map_err(|_| {
        bad("the sealed backup is not text, so it is not a gesta backup".to_string())
    })?;
    let mut lines = text.lines();
    let magic = lines.next().unwrap_or("");
    if magic != MAGIC {
        return Err(bad(format!(
            "the file does not start with {MAGIC}, so it is not a sealed gesta backup"
        )));
    }
    let salt = hex::decode(lines.next().unwrap_or(""))
        .map_err(|_| bad("the salt is not valid hex".to_string()))?;
    let nonce_bytes = hex::decode(lines.next().unwrap_or(""))
        .map_err(|_| bad("the nonce is not valid hex".to_string()))?;
    let mut ct = hex::decode(lines.next().unwrap_or(""))
        .map_err(|_| bad("the ciphertext is not valid hex".to_string()))?;

    // Nothing may follow the ciphertext (#69). Four lines are the whole format, so a fifth is
    // either corruption or a file that had something appended, and a reader that stops after the
    // fields it wants treats both as a valid backup. Appended data is evidence, not padding.
    if lines.next().is_some() {
        return Err(bad(
            "the sealed backup has data after the ciphertext; it may have been appended to"
                .to_string(),
        ));
    }

    if nonce_bytes.len() != NONCE_LEN {
        return Err(bad("the nonce has the wrong length".to_string()));
    }
    let mut nonce_arr = [0u8; NONCE_LEN];
    nonce_arr.copy_from_slice(&nonce_bytes);

    let key_bytes = crate::support::secrets::derive_key(passphrase, &salt);
    let unbound = UnboundKey::new(&CHACHA20_POLY1305, &key_bytes)
        .map_err(|_| bad("cannot build the cipher key".to_string()))?;
    let key = LessSafeKey::new(unbound);
    let nonce = Nonce::assume_unique_for_key(nonce_arr);

    let plain = key
        .open_in_place(nonce, Aad::empty(), &mut ct)
        .map_err(|_| {
            bad(
                "cannot open the backup: wrong passphrase, or the file was truncated or edited"
                    .to_string(),
            )
        })?;
    Ok(plain.to_vec())
}

/// Take a snapshot, seal it, and upload it under `prefix/gesta-<timestamp>.bak`.
///
/// The timestamp is UTC and zero padded, so listing the prefix sorts chronologically without reading
/// the files. That is what makes pruning by count possible with one listing.
/// A directory for the plaintext backup that only this process can enter, mode 0700 on Unix.
///
/// The database written there is the whole repository in clear: every object, every unhashed token,
/// the entire history. `std::env::temp_dir()` is shared and `/tmp` is 1777, and `Connection::open`
/// creates its file with the default umask, so the plaintext store was world-readable for the length
/// of the upload - with SQLite's `-wal` and `-shm` sidecars beside it under the same permissions. A
/// directory nobody else can enter covers all three, and its mode is a property of this call, which
/// is why it is its own function and can be asserted without a network upload.
fn private_temp_dir() -> ZResult<std::path::PathBuf> {
    let d = std::env::temp_dir().join(format!(
        "gesta-backup-{}-{}",
        std::process::id(),
        crate::support::clock::unix_now()
    ));
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(&d)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(&d)?;
    }
    Ok(d)
}

pub fn push(
    conn: &rusqlite::Connection,
    s3: &S3Config,
    prefix: &str,
    passphrase: &str,
    keep: usize,
) -> ZResult<Pushed> {
    let tmp_dir = private_temp_dir()?;
    let tmp = tmp_dir.join("store.db");

    let report = match crate::support::backup::backup(conn, &tmp) {
        Ok(r) => r,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            return Err(e);
        }
    };

    let plain = match std::fs::read(&tmp) {
        Ok(b) => b,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            return Err(e.into());
        }
    };
    // Gone before the sealed bytes go anywhere near the network.
    let _ = std::fs::remove_dir_all(&tmp_dir);

    let sealed = seal(&plain, passphrase)?;
    let stamp = utc_stamp(std::time::SystemTime::now());
    let key = format!("{}/gesta-{}.bak", prefix.trim_end_matches('/'), stamp);

    s3.put(&key, &sealed)?;

    // Pruned after the upload, not before: a failed upload must not remove the last good backup. A
    // prune that fails is not a reason to report the backup as failed, because the backup succeeded
    // and the next run prunes again.
    let _ = prune(s3, prefix, keep);

    Ok(Pushed {
        key,
        plain_bytes: plain.len() as u64,
        sealed_bytes: sealed.len() as u64,
        changesets: report.changesets,
        objects: report.objects,
    })
}

/// List the prefix and delete everything past the newest `keep`.
///
/// Returns how many were removed. Not an error to have fewer than `keep`: a fresh schedule has one.
/// Is this key one of the backups `push` writes?
///
/// `gesta-YYYYMMDDTHHMMSSmmmZ.bak`, named by `utc_stamp`. The check is on the shape, not on a
/// prefix match, because a prefix is a shared namespace: `prefix` is whatever the user configured, an
/// empty prefix or `backups` is entirely normal, and the bucket may hold anything else under it.
fn is_our_backup(key: &str) -> bool {
    let Some(name) = key.rsplit('/').next() else {
        return false;
    };
    let Some(stamp) = name
        .strip_prefix("gesta-")
        .and_then(|n| n.strip_suffix(".bak"))
    else {
        return false;
    };
    // 20260101T120000000Z, which is 19 characters: 8 date, 'T', 9 time with milliseconds, 'Z'.
    stamp.len() == 19
        && stamp.ends_with('Z')
        && stamp.as_bytes()[8] == b'T'
        && stamp[..8].bytes().all(|b| b.is_ascii_digit())
        && stamp[9..17].bytes().all(|b| b.is_ascii_digit())
}

/// Delete old backups under `prefix`, keeping the newest `keep`.
///
/// Only keys this program wrote are candidates. The first version sorted whatever `list_keys(prefix)`
/// returned and deleted the oldest `len - keep` of them, so a bucket shared with anything else - a
/// log archive, another tool's snapshots, the user's own files under a prefix like `backups` - lost
/// them, and the count reported it as backups pruned. Deleting data that was never yours is not a
/// retention policy.
///
/// A `keep` of zero is refused rather than obeyed: it is the one value that would delete every
/// backup, and no reading of "keep the newest N" makes that safe. The upload has already succeeded by
/// the time prune runs, so returning here loses nothing.
pub fn prune(s3: &S3Config, prefix: &str, keep: usize) -> ZResult<usize> {
    let doomed = what_to_prune(&s3.list_keys(prefix)?, keep);
    let mut removed = 0;
    for k in &doomed {
        s3.delete(k)?;
        removed += 1;
    }
    Ok(removed)
}

/// Which of these keys should be deleted to keep the newest `keep` backups.
///
/// The decision is its own function, without the bucket around it, because the first version of the
/// test for this asked `is_our_backup` directly and passed with the filter removed from `prune` - the
/// helper was right and `prune` was not using it. A question that has to hold under a policy belongs
/// where it can be asked of the actual candidates.
fn what_to_prune(all_keys: &[String], keep: usize) -> Vec<String> {
    // Keeping zero would delete every backup, including the one just uploaded. No reading of "keep
    // the newest N" makes that safe, and prune runs after a successful upload, so doing nothing
    // loses nothing.
    if keep == 0 {
        return Vec::new();
    }
    let mut keys: Vec<String> = all_keys
        .iter()
        .filter(|k| is_our_backup(k))
        .cloned()
        .collect();
    if keys.len() <= keep {
        return Vec::new();
    }
    // Oldest first: the stamp sorts as text in time order, so the name decides.
    keys.sort();
    keys[..keys.len() - keep].to_vec()
}

/// `YYYYMMDDTHHMMSSmmmZ`, which sorts as text in the same order as time.
///
/// Milliseconds, because a second is not fine enough to name a backup uniquely: an agent loop can
/// push twice inside one, and two uploads to one key leave the bucket holding whichever finished
/// last. A counter would be simpler and would not sort.
fn utc_stamp(now: std::time::SystemTime) -> String {
    let unix = now
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    // Days since the epoch to a civil date, the standard algorithm. A dependency on chrono for this
    // one conversion would be a second date implementation in a tree that already has one in
    // `support::clock`.
    let millis = unix.rem_euclid(1000);
    let secs_total = unix.div_euclid(1000);
    let days = secs_total.div_euclid(86_400);
    let secs = secs_total.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}{:03}Z",
        y,
        m,
        d,
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60,
        millis
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plaintext_backup_lives_in_a_directory_only_its_owner_can_enter() {
        // #84: the database was written straight into `std::env::temp_dir()` - `/tmp`, mode 1777 -
        // with the default umask, so the whole repository in clear was readable by every user on the
        // machine for the length of the upload, with SQLite's `-wal` and `-shm` sidecars beside it
        // under the same permissions.
        let d = private_temp_dir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&d).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, 0o700,
                "the directory must be owner-only, was {:o}",
                mode
            );
        }
        assert!(
            d.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("gesta-backup-"),
            "the directory name must be recognisable: {}",
            d.display()
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_sealed_backup_does_not_leave_its_plaintext_behind() {
        // The directory is removed after the read and before the upload, so there is a window where
        // the store exists on disk and none where it outlives the push. A test that just looked for
        // `remove_dir_all` found the error-path removal first and proved nothing.
        let src = include_str!("backup_remote.rs");
        let body = &src[src.find("pub fn push(").unwrap()..];
        let body = &body[..body.find("pub fn prune(").unwrap_or(body.len())];
        let read_at = body
            .find("std::fs::read(&tmp)")
            .expect("push reads the database");
        let put_at = body.find("s3.put(&key").expect("push uploads");
        let last_remove = body
            .match_indices("remove_dir_all(&tmp_dir)")
            .map(|(i, _)| i)
            .max()
            .unwrap();
        assert!(
            read_at < last_remove,
            "the plaintext must be removed after it is read; got read at {read_at}, remove at {last_remove}"
        );
        assert!(
            last_remove < put_at,
            "the plaintext must be gone before the upload starts; remove at {last_remove}, put at {put_at}"
        );
    }
    #[test]
    fn pruning_leaves_keys_that_are_not_backups_alone() {
        // #87: `prune` sorted whatever `list_keys(prefix)` returned and deleted the oldest
        // `len - keep`. A prefix is a shared namespace, so a bucket holding a log archive or another
        // tool's snapshots under the same prefix lost them, and the count called them backups.
        //
        // This asks `what_to_prune`, the decision `prune` actually makes. The first version called
        // `is_our_backup` directly and passed with the filter removed from `prune`: the helper was
        // right and its caller was not.
        let keys: Vec<String> = vec![
            "backups/gesta-20260101T000000000Z.bak".into(),
            "backups/gesta-20260102T000000000Z.bak".into(),
            "backups/gesta-20260103T000000000Z.bak".into(),
            // Nothing to do with this program, under the same prefix.
            "backups/access.log".into(),
            "backups/other-tool-20260101.tar".into(),
            "backups/gesta-notes.txt".into(),
        ];
        let doomed = what_to_prune(&keys, 2);
        assert_eq!(
            doomed,
            vec!["backups/gesta-20260101T000000000Z.bak".to_string()],
            "only the oldest backup may go, and only one of them"
        );
        // The keys that are not ours are absent at every keep.
        for keep in [1usize, 2, 3, 4] {
            for k in what_to_prune(&keys, keep) {
                assert!(is_our_backup(&k), "keep {keep} would delete {k}");
            }
        }
        // And with nothing to prune the answer is empty, not an error.
        assert!(what_to_prune(&keys[3..], 1).is_empty());
    }

    #[test]
    fn a_keep_of_zero_is_not_obeyed() {
        // The old code would have deleted every backup, including the one just uploaded. Prune runs
        // after a successful upload, so doing nothing is the only safe answer - and the decision is
        // asked here rather than `prune`, which would need a bucket to answer.
        let keys: Vec<String> = vec![
            "b/gesta-20260101T000000000Z.bak".into(),
            "b/gesta-20260102T000000000Z.bak".into(),
        ];
        assert!(
            what_to_prune(&keys, 0).is_empty(),
            "keep 0 must delete nothing"
        );
        assert_eq!(
            what_to_prune(&keys, 1).len(),
            1,
            "keep 1 deletes the older one"
        );
    }
    #[test]
    fn a_sealed_backup_with_data_appended_is_refused() {
        // #69, the other half: `unseal` took four lines and ignored the rest, so a backup with
        // garbage appended after the ciphertext still opened.
        let plain = b"not a real store".to_vec();
        let sealed = seal(&plain, "passphrase").unwrap();
        let mut extended = sealed.clone();
        extended.extend_from_slice(b"\nappended garbage\n");
        let err = unseal(&extended, "passphrase").unwrap_err().to_string();
        assert!(
            err.contains("after the ciphertext"),
            "trailing data must be named, got {err}"
        );
        // The untouched seal still opens.
        assert_eq!(unseal(&sealed, "passphrase").unwrap(), plain);
    }
}
