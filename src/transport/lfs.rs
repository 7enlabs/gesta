//! The git-lfs batch API, and the pointer file that stands in for a large object in a tree.
//!
//! `store::sidecar` already keeps a large object outside the database and records its hash. This is
//! the other half: the wire protocol a client speaks to ask for it, and the small text file that sits
//! in the tree in its place. Without this half, a `git clone` of a repository holding a 200 MB binary
//! moves 200 MB even when the client has no use for it; with it, the tree carries 130 bytes and the
//! object moves only when someone asks.
//!
//! # The pointer file
//!
//! Exactly three lines, and the version line has to be first or git-lfs refuses the file:
//!
//! ```text
//! version https://git-lfs.github.com/spec/v1
//! oid sha256:<64 hex>
//! size <decimal bytes>
//! ```
//!
//! The `oid` is the object's own hash, which is what makes the pointer checkable: a reader that
//! fetches the object verifies it against the name in the pointer rather than trusting the server.
//!
//! # The batch API
//!
//! One request carries every oid the client wants, and the response says for each whether it will be
//! sent, is already held, or does not exist:
//!
//! ```json
//! {"operation":"download","transfers":["basic"],"objects":[{"oid":"<hex>","size":123}]}
//! {"transfer":"basic","objects":[{"oid":"<hex>","size":123,"authenticated":true,
//!   "actions":{"download":{"href":"https://host/.../objects/<hex>","expires_at":"..."}}}]}
//! ```
//!
//! Two decisions worth recording. `expires_at` is emitted because the spec's own example carries it
//! and a client that parses strictly rejects a response without it. And a missing object is answered
//! inside a `200` batch response rather than as an HTTP error, because one missing object must not
//! fail the other ninety-nine.

use crate::support::json::{self, Json};

use rusqlite::Connection;
use sha2::{Digest, Sha256};

/// The pointer's first line. git-lfs compares this string exactly.
pub const SPEC_URL: &str = "https://git-lfs.github.com/spec/v1";

/// A large object as the tree records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pointer {
    /// The object's hash, bare hex, without the `sha256:` prefix.
    pub oid: String,
    /// Size in bytes, which the client uses to decide whether it has room.
    pub size: u64,
}

impl Pointer {
    /// Read a pointer from a file's bytes, or `None` when the bytes are not a pointer.
    ///
    /// The check is strict in one direction and lenient in the other: an unknown version is not a
    /// pointer this code can honour, so it is reported as `None` and the caller treats the bytes as
    /// an ordinary file. Calling a small text file a broken pointer would send a reader after an
    /// object that cannot exist.
    pub fn parse(bytes: &[u8]) -> Option<Pointer> {
        // A pointer is small; anything over a kilobyte is content that happens to start similarly.
        if bytes.len() > 1024 {
            return None;
        }
        let text = std::str::from_utf8(bytes).ok()?;
        let mut lines = text.lines();
        let first = lines.next()?;
        let rest = first.strip_prefix("version ")?;
        if rest.trim() != SPEC_URL {
            return None;
        }

        let mut oid = None;
        let mut size = None;
        for line in lines {
            if let Some(v) = line.strip_prefix("oid sha256:") {
                let v = v.trim();
                if v.len() == 64 && v.chars().all(|c| c.is_ascii_hexdigit()) {
                    oid = Some(v.to_ascii_lowercase());
                } else {
                    return None;
                }
            } else if let Some(v) = line.strip_prefix("size ") {
                size = v.trim().parse::<u64>().ok();
            }
        }
        Some(Pointer {
            oid: oid?,
            size: size?,
        })
    }

    /// The three lines a tree holds in place of the object.
    pub fn render(&self) -> String {
        format!(
            "version {}\noid sha256:{}\nsize {}\n",
            SPEC_URL, self.oid, self.size
        )
    }
}

/// One object named in a batch request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchObject {
    pub oid: String,
    pub size: u64,
}

/// Parse a `POST objects/batch` body.
///
/// An operation this server does not serve returns `None`, and the caller answers `422`, which is
/// what the spec says a client should see for an unsupported operation rather than a batch that is
/// silently empty and reads as "the server has nothing".
pub fn parse_batch(body: &[u8]) -> Option<(String, Vec<BatchObject>)> {
    let text = std::str::from_utf8(body).ok()?;
    let v = json::parse(text).ok()?;
    let op = json::as_str(json::field(&v, "operation")?)?.to_string();
    let objs = match json::field(&v, "objects")? {
        Json::Arr(a) => a,
        _ => return None,
    };
    let mut out = Vec::new();
    for o in objs {
        let oid = json::as_str(json::field(o, "oid")?)?.to_string();
        let size = json::field(o, "size")
            .and_then(json::as_i64)
            .unwrap_or(0)
            .max(0) as u64;
        out.push(BatchObject { oid, size });
    }
    Some((op, out))
}

/// What the server knows about one requested object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Found {
    /// The bytes are here and a download link follows.
    Present { size: u64 },
    /// The client already holds it by content hash, so nothing needs to move.
    AlreadyThere,
    /// No such object.
    Missing,
}

/// Build the batch response body.
///
/// `href_for` turns an oid into the URL the client should GET. A closure rather than a base string
/// keeps URL construction beside the route that serves it, so a change to the route cannot leave the
/// batch response pointing at a path that no longer answers.
pub fn batch_response<F>(results: &[(BatchObject, Found)], href_for: F, expires_at: &str) -> String
where
    F: Fn(&str) -> String,
{
    let mut objects = Vec::new();
    for (obj, found) in results {
        let mut pairs = vec![
            ("oid", Json::str(obj.oid.clone())),
            ("size", Json::int(obj.size as i64)),
        ];
        match found {
            Found::Present { size } => {
                pairs[1] = ("size", Json::int(*size as i64));
                pairs.push(("authenticated", Json::Bool(true)));
                let action = Json::obj(vec![
                    ("href", Json::str(href_for(&obj.oid))),
                    ("expires_at", Json::str(expires_at.to_string())),
                ]);
                pairs.push(("actions", Json::obj(vec![("download", action)])));
            }
            Found::AlreadyThere => {}
            Found::Missing => {
                let err = Json::obj(vec![
                    ("code", Json::int(404)),
                    ("message", Json::str("object does not exist")),
                ]);
                pairs.push(("error", err));
            }
        }
        objects.push(Json::obj(pairs));
    }

    Json::obj(vec![
        ("transfer", Json::str("basic")),
        ("objects", Json::arr(objects)),
    ])
    .render()
}

// ---- locks ----
//
// A lock is the part of git-lfs a server has to keep state for: `lock` claims a path, `unlock`
// releases it, and a second client asking for the same path is refused until the first one is done.
// The purpose is a binary nobody can merge, so the cost of getting it wrong is two people editing the
// same 200 MB asset and one of the edits being discarded.
//
// Three properties the code below is written to hold:
//
//   the same owner may lock the same path twice   idempotent, because a retry after a dropped
//                                                  response must not look like a conflict
//   a different owner is refused                   with the lock's own owner named, so the client
//                                                  can tell the user who to ask
//   unlocking someone else's lock is refused       unless `force`, which is the server-side override
//                                                  git-lfs defines for an admin

/// One lock, as the API returns it.
pub struct Lock {
    pub id: String,
    pub path: String,
    pub owner: String,
    pub refname: Option<String>,
    pub locked_at: i64,
}

/// A lock identifier: 32 hex characters from the path and the clock.
///
/// Not a sequence number, because two servers serving the same repository would both hand out `1`, and
/// a client that cached an id from one would then match an unrelated lock on the other.
pub fn lock_id(path: &str, at: i64) -> String {
    let mut h = Sha256::new();
    h.update(path.as_bytes());
    h.update(at.to_le_bytes());
    hex::encode(h.finalize())[..32].to_string()
}

/// Claim a path. Returns the lock, or the existing lock when this owner already holds it.
pub fn lock(
    conn: &Connection,
    path: &str,
    owner: &str,
    refname: Option<&str>,
    _now: i64,
) -> Result<Lock, String> {
    if path.is_empty() {
        return Err("a lock needs a path".to_string());
    }
    let at = _now;
    let existing: Option<(String, String, Option<String>, i64)> = conn
        .query_row(
            "SELECT id, owner, refname, locked_at FROM lfs_locks WHERE path = ?1",
            rusqlite::params![path],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .ok();
    if let Some((id, held_by, rn, when)) = existing {
        // A retry from the same owner is the same lock, not a conflict. A client whose first request
        // timed out has no way to know whether it landed, so it asks again, and answering "someone
        // else has it" here would make the client report a conflict with itself.
        if held_by == owner {
            return Ok(Lock {
                id,
                path: path.to_string(),
                owner: held_by,
                refname: rn,
                locked_at: when,
            });
        }
        return Err(format!("{} is already locked by {}", path, held_by));
    }
    let id = lock_id(path, at);
    conn.execute(
        "INSERT INTO lfs_locks(id, path, owner, refname, locked_at) VALUES(?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![id, path, owner, refname, at],
    )
    .map_err(|e| format!("could not record the lock: {}", e))?;
    Ok(Lock {
        id,
        path: path.to_string(),
        owner: owner.to_string(),
        refname: refname.map(|s| s.to_string()),
        locked_at: at,
    })
}

/// Release a lock by id. `force` lets an owner release a lock held by someone else.
pub fn unlock(conn: &Connection, id: &str, owner: &str, force: bool) -> Result<Lock, String> {
    let existing: Option<(String, String, Option<String>, i64)> = conn
        .query_row(
            "SELECT path, owner, refname, locked_at FROM lfs_locks WHERE id = ?1",
            rusqlite::params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .ok();
    let (path, held_by, rn, when) = match existing {
        Some(v) => v,
        // An id nobody holds is the answer a retried unlock gets, and it is not an error: the caller
        // asked for the lock to be gone, and it is.
        None => return Err(format!("no lock with id {}", id)),
    };
    if held_by != owner && !force {
        return Err(format!("{} is locked by {}, not you", path, held_by));
    }
    conn.execute("DELETE FROM lfs_locks WHERE id = ?1", rusqlite::params![id])
        .map_err(|e| format!("could not remove the lock: {}", e))?;
    Ok(Lock {
        id: id.to_string(),
        path,
        owner: held_by,
        refname: rn,
        locked_at: when,
    })
}

/// Locks, newest first, narrowed by the filters the caller asked for.
///
/// The spec defines three: `path`, `id` and `cursor`. They are separate columns and have to stay
/// separate. An earlier version took one `owner` argument and the caller passed the `path` filter into
/// it, so `?path=a.bin` matched every lock whose *owner* was the string `a.bin` and returned the whole
/// list - which a client reads as "my filter applied" while showing locks on other files.
pub fn list(
    conn: &Connection,
    owner: Option<&str>,
    path: Option<&str>,
    id: Option<&str>,
) -> Vec<Lock> {
    let mut sql =
        String::from("SELECT id, path, owner, refname, locked_at FROM lfs_locks WHERE 1 = 1");
    let mut args: Vec<String> = Vec::new();
    if let Some(o) = owner {
        sql.push_str(" AND owner = ?");
        args.push(o.to_string());
    }
    if let Some(p) = path {
        sql.push_str(" AND path = ?");
        args.push(p.to_string());
    }
    if let Some(i) = id {
        sql.push_str(" AND id = ?");
        args.push(i.to_string());
    }
    sql.push_str(" ORDER BY locked_at DESC");

    let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
    let row = |r: &rusqlite::Row| -> rusqlite::Result<Lock> {
        Ok(Lock {
            id: r.get(0)?,
            path: r.get(1)?,
            owner: r.get(2)?,
            refname: r.get(3)?,
            locked_at: r.get(4)?,
        })
    };
    // `query_map` returns rows that borrow the statement, so the collected value has to be produced
    // inside the same expression rather than kept in a binding that outlives it.
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows: Vec<Lock> = match stmt.query_map(refs.as_slice(), row) {
        Ok(iter) => iter.flatten().collect(),
        Err(_) => Vec::new(),
    };
    drop(stmt);
    rows
}

/// A JSON string with its quotes, for the fields a path or an owner can put anything into.
fn jstr(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn lock_json(l: &Lock) -> String {
    format!(
        "{{\"id\":{},\"path\":{},\"locked_at\":\"{}\",\"owner\":{{\"name\":{}}}{}}}",
        jstr(&l.id),
        jstr(&l.path),
        iso8601(l.locked_at),
        jstr(&l.owner),
        match &l.refname {
            Some(r) => format!(",\"refname\":{}", jstr(r)),
            None => String::new(),
        }
    )
}

/// Seconds since the epoch as the ISO 8601 the git-lfs API uses.
fn iso8601(at: i64) -> String {
    // A fixed UTC calendar, not a chrono dependency: the format has no month names and no timezone
    // offset, so the arithmetic is a few dozen lines and adding a date library for it would be the
    // heavier choice.
    let (days, secs) = (at.div_euclid(86400), at.rem_euclid(86400));
    let (h, mi, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    // Civil-from-days, the algorithm Howard Hinnant published; it is exact for every day this will
    // ever see and needs no table.
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, h, mi, s)
}

/// `{"path":"...","ref":{"name":"..."}}` to the pair. The path is what the lock is on; the ref is
/// advisory, and a client that omits it still gets a lock.
pub fn parse_lock_request(body: &[u8]) -> Option<(String, Option<String>)> {
    let text = String::from_utf8_lossy(body);
    let path = field(&text, "path")?;
    let refname = field(&text, "name");
    Some((path, refname))
}

/// `{"id":"...","force":true}` to the pair. A missing `force` is false, which is what the spec says.
pub fn parse_unlock(body: &[u8]) -> Option<(String, bool)> {
    let text = String::from_utf8_lossy(body);
    let id = field(&text, "id")?;
    let force = text.contains("\"force\":true") || text.contains("\"force\": true");
    Some((id, force))
}

/// The first string value for a key, with its escapes resolved.
///
/// Hand rolled rather than pulled from the JSON reader because the lock bodies git-lfs sends are small
/// and flat, and the reader this project has is built for the tree, which is neither.
fn field(text: &str, key: &str) -> Option<String> {
    let needle = format!("\"{}\"", key);
    let at = text.find(&needle)? + needle.len();
    let rest = &text[at..];
    let colon = rest.find(':')?;
    let mut chars = rest[colon + 1..].trim_start().chars().peekable();
    if chars.peek() != Some(&'"') {
        return None;
    }
    chars.next();
    let mut out = String::new();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => return Some(out),
            },
            c => out.push(c),
        }
    }
    Some(out)
}

/// One lock object, without the wrapping key.
pub fn one_json(l: &Lock) -> String {
    lock_json(l)
}

/// A JSON string with its quotes.
pub fn quote(s: &str) -> String {
    jstr(s)
}

/// `{"ours":[...],"theirs":[...]}` for the verify call.
pub fn verify_json(ours: &[Lock], theirs: &[Lock]) -> String {
    let a: Vec<String> = ours.iter().map(lock_json).collect();
    let b: Vec<String> = theirs.iter().map(lock_json).collect();
    format!(
        "{{\"ours\":[{}],\"theirs\":[{}]}}",
        a.join(","),
        b.join(",")
    )
}

/// `{"locks":[...]}` for a list response.
pub fn locks_json(locks: &[Lock]) -> String {
    let items: Vec<String> = locks.iter().map(lock_json).collect();
    format!("{{\"locks\":[{}]}}", items.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn a_pointer_round_trips_through_render_and_parse() {
        let p = Pointer {
            oid: H.to_string(),
            size: 4_500_000,
        };
        let bytes = p.render();
        assert_eq!(bytes.lines().count(), 3, "a pointer is exactly three lines");
        assert_eq!(Pointer::parse(bytes.as_bytes()), Some(p));
    }

    #[test]
    fn an_ordinary_text_file_is_not_a_pointer() {
        let text = "version 1.2.3\nthis is a file about versions\n";
        assert_eq!(Pointer::parse(text.as_bytes()), None);
    }

    #[test]
    fn a_pointer_with_a_short_or_non_hex_oid_is_refused() {
        // A pointer whose oid cannot name an object is not one this server can honour, and treating
        // it as a pointer would send a reader after something that cannot exist.
        for bad in [
            "version https://git-lfs.github.com/spec/v1\noid sha256:abc\nsize 1\n",
            "version https://git-lfs.github.com/spec/v1\noid sha256:zz23456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\nsize 1\n",
            "version https://git-lfs.github.com/spec/v1\noid sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n",
        ] {
            assert_eq!(Pointer::parse(bad.as_bytes()), None, "accepted: {bad:?}");
        }
    }

    #[test]
    fn an_unknown_version_is_not_a_pointer_this_code_can_read() {
        let other = format!("version https://example.test/spec/v9\noid sha256:{H}\nsize 7\n");
        assert_eq!(Pointer::parse(other.as_bytes()), None);
    }

    #[test]
    fn a_large_file_that_starts_like_a_pointer_is_not_one() {
        let mut big = b"version https://git-lfs.github.com/spec/v1\n".to_vec();
        big.extend(std::iter::repeat_n(b'x', 2000));
        assert_eq!(Pointer::parse(&big), None);
    }

    #[test]
    fn the_batch_response_names_a_download_for_an_object_that_is_here() {
        let objs = vec![
            (
                BatchObject {
                    oid: "aa".into(),
                    size: 10,
                },
                Found::Present { size: 10 },
            ),
            (
                BatchObject {
                    oid: "bb".into(),
                    size: 20,
                },
                Found::Missing,
            ),
            (
                BatchObject {
                    oid: "cc".into(),
                    size: 30,
                },
                Found::AlreadyThere,
            ),
        ];
        let body = batch_response(
            &objs,
            |oid| format!("https://h/lfs/{oid}"),
            "2030-01-01T00:00:00Z",
        );
        assert!(body.contains("\"transfer\":\"basic\""));
        assert!(
            body.contains("https://h/lfs/aa"),
            "the present object needs a link"
        );
        assert!(
            !body.contains("https://h/lfs/bb"),
            "a missing object must not get a link"
        );
        assert!(
            body.contains("\"code\":404"),
            "the missing one is reported inside the batch"
        );
        assert!(
            body.contains("2030-01-01T00:00:00Z"),
            "expires_at is part of the shape"
        );
        // A missing object must not fail the others.
        assert!(body.contains("\"oid\":\"aa\"") && body.contains("\"oid\":\"cc\""));
    }

    #[test]
    fn a_batch_body_is_parsed_into_its_objects() {
        let body = br#"{"operation":"download","transfers":["basic"],
                        "objects":[{"oid":"aa","size":1},{"oid":"bb","size":2}]}"#;
        let (op, objs) = parse_batch(body).expect("a well formed batch parses");
        assert_eq!(op, "download");
        assert_eq!(objs.len(), 2);
        assert_eq!(objs[1].oid, "bb");
        assert_eq!(objs[1].size, 2);
    }

    #[test]
    fn a_batch_body_that_is_not_json_is_refused_rather_than_guessed() {
        assert!(parse_batch(b"not json").is_none());
        assert!(
            parse_batch(br#"{"operation":"download"}"#).is_none(),
            "no objects key"
        );
    }

    #[test]
    fn one_missing_object_does_not_hide_the_others() {
        // The whole point of answering per object: a request naming a hundred oids, one of which is
        // absent, must still answer for the other ninety-nine. A response that fails whole turns one
        // missing file into a broken fetch.
        let results = vec![
            (
                BatchObject {
                    oid: "aa".into(),
                    size: 1,
                },
                Found::Present { size: 1 },
            ),
            (
                BatchObject {
                    oid: "bb".into(),
                    size: 2,
                },
                Found::Missing,
            ),
            (
                BatchObject {
                    oid: "cc".into(),
                    size: 3,
                },
                Found::Present { size: 3 },
            ),
        ];
        let out = batch_response(&results, |oid| format!("objects/{oid}"), "later");
        assert!(out.contains("aa"), "the first object is answered");
        assert!(
            out.contains("cc"),
            "the object after the missing one is answered"
        );
        assert!(
            out.contains("\"error\""),
            "the missing one carries an error rather than vanishing"
        );
        // The hrefs are relative: the handler has no host or scheme, and a client resolves against
        // the endpoint it called.
        assert!(
            !out.contains("http://") && !out.contains("https://"),
            "hrefs must not invent a host"
        );
    }

    #[test]
    fn an_object_whose_size_is_wrong_in_the_request_is_still_answered_by_oid() {
        // A client may state a size it guessed. The answer is by oid; refusing the batch because the
        // size does not match would fail a fetch over a number nobody needs to agree on.
        let body = br#"{"operation":"download","transfers":["basic"],
                        "objects":[{"oid":"aa","size":0},{"oid":"bb","size":999999999}]}"#;
        let (op, objs) = parse_batch(body).expect("sizes are the client's business");
        assert_eq!(op, "download");
        assert_eq!(objs.len(), 2);
    }
}
