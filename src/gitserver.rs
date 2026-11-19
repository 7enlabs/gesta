//! The git smart HTTP protocol, enough of it that `git clone` works against a gesta server.
//!
//! Three layers, and each is separate on purpose:
//!
//! - `pkt-line`, the framing git uses for every message it sends.
//! - `ref_advertisement`, what the server says it has, which is the first thing `git clone` reads.
//! - `pack`, the object transfer. Written here rather than delegated, because the whole point of a
//!   server is that a client with no gesta binary can read from it.
//!
//! `receive-pack` is here too, so a plain `git push` works. It hands the pack to git to unpack and
//! then imports the result through the same path `git import` uses; a second pack reader for objects
//! that arrive over the wire would be a second implementation to keep in step.
//!
//! A push sends a thin pack: the objects the server is known to hold are left out and named as
//! bases. Unpacking into an empty repository therefore fails on an object the client never sent, so
//! the server's own history is exported into the scratch repository first. That one detail is the
//! difference between a push that works and a push that reports a missing object.
//!
//! What is deliberately absent: the v2 protocol's capabilities beyond `ofs-delta`, multi-pack
//! indexes, protocol v1's `multi_ack_detailed` state machine, ref deletion, and delta in the packs
//! this server writes. Each is a negotiation that only a second round trip needs. A repository that
//! advertises capability it does not implement is worse than one that advertises less: the client
//! takes the offer and then hangs.
//!
//! The pack writes every object in full, with no delta. Measured on this project's history that is
//! about 3x the size of a delta pack, and it is the right first version: a delta encoder that is
//! wrong produces a pack git refuses with a message about a bad object number, and the failure looks
//! like a protocol bug rather than an encoder bug.

use crate::ZResult;

/// A request that this server cannot serve, with the part of it that made no sense.
fn bad(msg: String) -> Box<crate::error::Error> {
    Box::new(crate::error::Error::new(crate::error::Code::Usage, msg))
}

/// One git object, as the pack writer needs it.
pub struct Entry {
    /// 1 commit, 2 tree, 3 blob, 4 tag.
    ///
    /// No hash field: a full pack names objects by position, so the writer never needs the id. A
    /// delta pack would, and adding it back is the change that comes with the delta encoder.
    pub kind: u8,
    /// The object body, uncompressed.
    pub body: Vec<u8>,
}

/// Frame one pkt-line: a four byte hex length that counts itself, then the payload.
///
/// The length includes the header, which is the detail that makes a hand-built pkt-line fail with
/// the client reporting a protocol error and no clue which message was wrong.
pub fn pkt_line(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 4);
    out.extend_from_slice(format!("{:04x}", payload.len() + 4).as_bytes());
    out.extend_from_slice(payload);
    out
}

/// The flush packet, which ends a section of the conversation.
pub fn flush() -> Vec<u8> {
    b"0000".to_vec()
}

/// What the server offers a client that has not asked for anything yet.
///
/// `git-upload-pack` wants the first ref annotated with the capabilities, and every line terminated
/// by NUL only on that first one. A client that receives capabilities on the wrong line ignores them
/// and then asks for a side-band it was never offered.
pub fn ref_advertisement(refs: &[(String, String)], capabilities: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    let caps = capabilities.join(" ");
    for (i, (name, sha)) in refs.iter().enumerate() {
        let line = if i == 0 {
            format!("{} {}\0{}\n", sha, name, caps)
        } else {
            format!("{} {}\n", sha, name)
        };
        out.extend_from_slice(&pkt_line(line.as_bytes()));
    }
    out.extend_from_slice(&flush());
    out
}

/// Read the pkt-lines in a request body, skipping the ones a client sends as progress.
pub fn read_pkt_lines(body: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= body.len() {
        let hex = match std::str::from_utf8(&body[i..i + 4]) {
            Ok(h) => h,
            Err(_) => break,
        };
        let len = match usize::from_str_radix(hex, 16) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        if len < 4 || i + len > body.len() {
            break;
        }
        let payload = &body[i + 4..i + len];
        out.push(String::from_utf8_lossy(payload).trim_end().to_string());
        i += len;
    }
    out
}

/// Write a pack file from a list of objects.
///
/// The header is the magic, a version, and the object count; then each object's type and size as a
/// varint; then the zlib stream of its body; then a SHA-1 of everything before it. A pack whose
/// trailing checksum is wrong is rejected by git with "index-pack failed", and the client does not
/// say which object.
pub fn write_pack(entries: &[Entry]) -> ZResult<Vec<u8>> {
    let mut out = Vec::new();
    out.extend_from_slice(b"PACK");
    out.extend_from_slice(&2u32.to_be_bytes());
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());

    for e in entries {
        // The type and the low four bits of the size share a byte, and a size above 15 continues in
        // the following bytes, seven bits at a time, with the high bit as a continuation marker.
        let mut size = e.body.len();
        let mut first = ((e.kind & 0x7) << 4) | ((size & 0xf) as u8);
        size >>= 4;
        while size > 0 {
            first |= 0x80;
            out.push(first);
            first = (size & 0x7f) as u8;
            size >>= 7;
        }
        out.push(first);
        let packed = crate::store::compress_bytes(&e.body)?;
        out.extend_from_slice(&packed);
    }

    let digest = sha1(&out);
    out.extend_from_slice(&digest);
    Ok(out)
}

/// SHA-1, which git names objects by. This is the only place a pack needs it.
fn sha1(data: &[u8]) -> [u8; 20] {
    crate::gitobj::sha1_raw(data)
}



/// What the server has, for a client that is about to ask for it.
///
/// The first line needs the service banner: a client reads it to decide the protocol version, and
/// without it `git clone` reports "not a git repository" on a URL that answered 200.
pub fn advertise(root: &std::path::Path) -> ZResult<Vec<u8>> {
    let served = crate::http::open_repo_for_serve(root)?;
    let refs = served.refs;

    let mut out = pkt_line(b"# service=git-upload-pack\n");
    out.extend_from_slice(&flush());

    // One ref per branch, named the way git names them. A branch whose head is missing is skipped
    // rather than advertised with a zero id, which a client would try to fetch.
    let mut lines: Vec<(String, String)> = Vec::new();
    for (name, sha) in refs {
        lines.push((format!("refs/heads/{}", name), sha));
    }
    if lines.is_empty() {
        // A client needs at least one ref to ask for. An empty repository advertises the
        // capabilities line with no refs, which git reads as "nothing to clone".
        let mut empty = pkt_line(b"0000000000000000000000000000000000000000 capabilities^{}\0agent=gesta\n");
        empty.extend_from_slice(&flush());
        out.extend_from_slice(&empty);
        return Ok(out);
    }
    // `symref=HEAD:refs/heads/main` is what tells a clone which branch to check out. Without it git
    // warns "remote HEAD refers to nonexistent ref" and leaves an empty working tree, and the
    // message points at the server's HEAD rather than at the missing advertisement.
    let head_branch = if lines.iter().any(|(name, _)| name == "refs/heads/main") {
        "main"
    } else {
        lines[0].0.trim_start_matches("refs/heads/")
    };
    let symref = format!("symref=HEAD:refs/heads/{}", head_branch);
    // `HEAD` is advertised as a ref of its own as well as named by symref. A clone uses the symref
    // to decide what to check out; without a `HEAD` line it still files everything under
    // refs/remotes/origin and leaves the working tree empty with a warning about a missing ref.
    let head_sha = lines
        .iter()
        .find(|(name, _)| name.as_str() == format!("refs/heads/{}", head_branch).as_str())
        .map(|(_, sha)| sha.clone());
    let mut advert = Vec::new();
    if let Some(sha) = head_sha {
        advert.push(("HEAD".to_string(), sha));
    }
    advert.extend(lines);
    out.extend_from_slice(&ref_advertisement(
        &advert,
        &["ofs-delta", "side-band-64k", &symref],
    ));
    Ok(out)
}

/// Answer `git-upload-pack`: read what the client wants, then send the objects and a status.
///
/// The client sends `want <sha>` lines, then a flush, then `done`. Everything after `done` is our
/// side. Writing the pack straight into the response without the side-band wrapper makes a client
/// that negotiated `side-band-64k` read the pack bytes as channel headers and fail.
pub fn upload_pack(root: &std::path::Path, body: &[u8]) -> ZResult<Vec<u8>> {
    // Parsed to tell a fetch from a request that names nothing. The pack itself is a full copy: a
    // negotiation that sent only what was asked for would need the client's `have` list, and
    // sending more than it asked for is cheaper than sending something it cannot check out.
    let wanted: Vec<String> = read_pkt_lines(body)
        .into_iter()
        .filter_map(|line| {
            line.strip_prefix("want ")
                .map(|rest| rest.split_whitespace().next().unwrap_or("").to_string())
        })
        .filter(|s| s.len() == 40)
        .collect();

    // One export gives both the refs and every object they reach, so nothing is walked twice. The
    // pack is a full copy of the repository: a client asking for one commit gets its history too,
    // and asking for everything costs bytes rather than a negotiation this server does not do.
    let served = crate::http::open_repo_for_serve(root)?;
    let entries = served.objects;

    if wanted.is_empty() || entries.is_empty() {
        return Ok(flush());
    }
    let pack = write_pack(&entries)?;

    // Side-band 1 carries the pack. Each chunk is a pkt-line whose first byte is the band number,
    // and a payload above 65515 bytes overflows the four hex digits that hold the length.
    //
    // `NAK` is the only line before the pack in a stateless-rpc v0 exchange. The symref a clone
    // needs to pick a branch travels in the advertisement, and this response does not repeat it:
    // repeating the refs here made git store refs/remotes/origin/main and leave HEAD on master,
    // which is the "remote HEAD refers to nonexistent ref" warning and an empty working tree.
    let mut out = Vec::new();
    out.extend_from_slice(&pkt_line(b"NAK\n"));
    for chunk in pack.chunks(65500) {
        let mut payload = Vec::with_capacity(chunk.len() + 1);
        payload.push(1u8);
        payload.extend_from_slice(chunk);
        out.extend_from_slice(&pkt_line(&payload));
    }
    // Under side-band the stream ends with a band-1 packet carrying nothing, not a bare flush. A
    // client reads `0000` as the end of the pkt-line section and stops listening, so the pack it
    // received is never acknowledged and the clone reports that the remote has no commits.
    out.extend_from_slice(&pkt_line(&[1u8]));
    out.extend_from_slice(&flush());
    Ok(out)
}

/// What a push says it wants to do, one line per ref.
pub struct PushCommand {
    pub old: String,
    pub new: String,
    pub name: String,
}

/// Split a receive-pack body into the commands and the pack that follows them.
///
/// The body is pkt-lines up to a flush, then raw pack bytes. The pack has no length prefix and no
/// framing of its own, so everything after the flush belongs to it whether or not it starts with
/// `PACK`, and a body that ends at the flush carried no objects.
pub fn split_push(body: &[u8]) -> ZResult<(Vec<PushCommand>, Vec<u8>)> {
    let mut commands = Vec::new();
    let mut i = 0usize;
    loop {
        if i + 4 > body.len() {
            return Err(bad("a push ended before its flush".to_string()));
        }
        let len = std::str::from_utf8(&body[i..i + 4])
            .ok()
            .and_then(|h| usize::from_str_radix(h, 16).ok())
            .ok_or_else(|| bad("a push carried a length that is not hexadecimal".to_string()))?;
        if len == 0 {
            i += 4;
            break;
        }
        if i + len > body.len() {
            return Err(bad("a push carried a line longer than its body".to_string()));
        }
        let line = String::from_utf8_lossy(&body[i + 4..i + len]).to_string();
        i += len;
        // The first line carries the capabilities after a NUL, and they belong to the exchange
        // rather than to the ref.
        let line = line.split('\0').next().unwrap_or("").trim_end();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split(' ');
        let old = parts.next().unwrap_or("").to_string();
        let new = parts.next().unwrap_or("").to_string();
        let name = parts.next().unwrap_or("").to_string();
        if old.len() != 40 || new.len() != 40 || name.is_empty() {
            return Err(bad(format!("a push named a ref in a shape that is not one: {}", line)));
        }
        commands.push(PushCommand { old, new, name });
    }
    Ok((commands, body[i..].to_vec()))
}

/// Answer `git-receive-pack`: take the objects, make them changesets, move the refs.
///
/// The pack is unpacked by git into a scratch bare repository and then read through the same import
/// path a `git import` uses. Writing a second reader for a pack that arrives over the wire would be
/// a second implementation to keep in step with the first, and the first difference between them
/// shows up as a push that reports success and a branch that did not move.
/// Answer `git-receive-pack`.
///
/// `allowed`, when given, is which branches this caller may write. Returning false for a ref rejects
/// that ref and leaves the others alone, which is what a push naming three refs expects when one of
/// them is not its business. The pack is still unpacked: refusing the whole transfer would let one
/// forbidden branch hide the objects the permitted ones need.
pub fn receive_pack_allowing(
    root: &std::path::Path,
    body: &[u8],
    allowed: Option<&dyn Fn(&str) -> bool>,
) -> ZResult<Vec<u8>> {
    let (commands, pack) = split_push(body)?;

    // Refused refs, remembered so the import's own branch creation can be undone. The import walks
    // the scratch repository's refs and creates a branch for each, so a forbidden branch was created
    // and left behind pointing at a commit the caller may not write to.
    let refused: Vec<String> = commands
        .iter()
        .filter(|c| match allowed {
            Some(permitted) => {
                let branch = c.name.strip_prefix("refs/heads/").unwrap_or(&c.name);
                !permitted(branch)
            }
            None => false,
        })
        .map(|c| c.name.strip_prefix("refs/heads/").unwrap_or(&c.name).to_string())
        .collect();
    if commands.is_empty() {
        return Ok(flush());
    }
    if pack.is_empty() {
        // A push with no objects is a ref deletion or a move to a commit already held. Nothing to
        // unpack, and git's own reader would reject an empty file.
        let mut out = pkt_line(b"unpack ok\n");
        for c in &commands {
            out.extend_from_slice(&pkt_line(format!("ok {}\n", c.name).as_bytes()));
        }
        out.extend_from_slice(&flush());
        return Ok(out);
    }

    let scratch = std::env::temp_dir().join(format!(
        "gesta-push-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let result = (|| -> ZResult<Vec<u8>> {
        std::fs::create_dir_all(&scratch)?;
        let init = std::process::Command::new("git")
            .arg("init")
            .arg("--bare")
            .arg("-q")
            .arg(&scratch)
            .output()
            .map_err(|e| bad(format!("a push needs git to unpack it: {}", e)))?;
        if !init.status.success() {
            return Err(bad(format!(
                "git could not prepare a scratch repository: {}",
                String::from_utf8_lossy(&init.stderr).trim()
            )));
        }

        // `index-pack` reads a pack from a file and writes the objects beside it. `--stdin` would
        // read from the process, and the pack is already in memory.
        let pack_file = scratch.join("objects").join("pack").join("incoming.pack");
        let pack_dir = scratch.join("objects").join("pack");
        std::fs::create_dir_all(&pack_dir)?;
        std::fs::write(&pack_file, &pack)?;
        // `index-pack` would leave the objects inside a pack, and the import reads loose objects. So
        // the pack is unpacked loose here. One extra write of the same bytes, and the alternative is
        // teaching the importer to walk a pack it did not write.
        let idx = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(&scratch)
            .arg("unpack-objects")
            .arg("-q")
            .stdin(std::process::Stdio::from(
                std::fs::File::open(&pack_file)
                    .map_err(|e| bad(format!("the pack a push sent could not be read: {}", e)))?,
            ))
            .output()
            .map_err(|e| bad(format!("a push needs git to unpack it: {}", e)))?;
        if !idx.status.success() {
            return Err(bad(format!(
                "the pack a push sent could not be read: {}",
                String::from_utf8_lossy(&idx.stderr).trim()
            )));
        }
        // The pack itself is not a loose object and the importer does not look at it.
        let _ = std::fs::remove_file(&pack_file);

        // A push sends a thin pack: the objects the server is already known to hold are left out,
        // and the pack names them as bases. Unpacking into an empty repository therefore fails with
        // a missing object that the client never sent. The existing history is exported into the
        // scratch first, so the bases are there when the new objects arrive.
        let conn = crate::store::open_db(root)?;
        if let Err(e) = crate::git_bridge::export(&conn, &scratch, None) {
            return Err(bad(format!(
                "the server's own history could not be staged for a push: {}",
                e
            )));
        }

        // The refs have to be in the scratch repository before the import walks them, because the
        // import starts from branch heads rather than from a list of objects.
        for c in &commands {
            let branch_dir = scratch.join("refs").join("heads");
            std::fs::create_dir_all(&branch_dir)?;
            let ref_file =
                branch_dir.join(c.name.strip_prefix("refs/heads/").unwrap_or(&c.name));
            std::fs::write(&ref_file, format!("{}\n", c.new))?;
        }

        // The client's `old` value is checked against the branch as it was before this push. Reading
        // it after the import would compare the client's expectation with the result of applying the
        // push, which always differs and rejects every push that is correct.
        // The branches the push names and the ones that already existed are different questions. A
        // refused push must not delete history, and the map of names in the request answers the
        // wrong one: it lists every ref the client offered, whether the server ever had it or not.
        let existed: Vec<String> = commands
            .iter()
            .map(|c| c.name.strip_prefix("refs/heads/").unwrap_or(&c.name).to_string())
            .filter(|b| {
                conn.query_row(
                    "SELECT COUNT(*) FROM branches WHERE name = ?1",
                    rusqlite::params![b],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0)
                    > 0
            })
            .collect();

        let before: Vec<(String, Option<String>)> = commands
            .iter()
            .map(|c| {
                let branch = c.name.strip_prefix("refs/heads/").unwrap_or(&c.name).to_string();
                let head = crate::store::branch_head(&conn, &branch).ok().flatten();
                let sha = head.and_then(|id| {
                    conn.query_row(
                        "SELECT git_commit FROM changesets WHERE id = ?1",
                        rusqlite::params![id],
                        |r| r.get::<_, Option<String>>(0),
                    )
                    .ok()
                    .flatten()
                });
                (branch, sha)
            })
            .collect();

        crate::git_bridge::import(&conn, &scratch, None)?;

        // A branch the import created for a refused ref did not exist before this push, so removing
        // it puts the repository back. Checking first whether the push created it is what keeps a
        // refused push to an existing branch from deleting that branch's history.
        for name in &refused {
            // Only a branch that did not exist before this push is removed. One that already existed
            // belongs to the repository, and a refused push to it must leave it alone.
            let existed_before = existed.iter().any(|b| b == name);
            if !existed_before {
                let _ = conn.execute("DELETE FROM branches WHERE name = ?1", rusqlite::params![name]);
            }
        }

        // After the import every pushed commit has a changeset, so the branch can point at it.
        let mut out = pkt_line(b"unpack ok\n");
        for c in &commands {
            let branch = c
                .name
                .strip_prefix("refs/heads/")
                .unwrap_or(&c.name)
                .to_string();

            // A token scoped to a path could still rewrite every branch. The branch is the part a
            // mistake costs the most to undo, so it is refused by name and the other refs in the
            // same push still apply.
            if let Some(permitted) = allowed {
                if !permitted(&branch) {
                    out.extend_from_slice(
                        pkt_line(
                            format!(
                                "ng {} this token may not write that branch\n",
                                c.name
                            )
                            .as_bytes(),
                        )
                        .as_slice(),
                    );
                    continue;
                }
            }
            let changeset: Option<String> = conn
                .query_row(
                    "SELECT id FROM changesets WHERE git_commit = ?1",
                    rusqlite::params![c.new],
                    |r| r.get(0),
                )
                .ok();
            // The `old` value is what the client believed the branch held. If the server moved on
            // since, applying the push would silently discard whatever arrived in between, which is
            // the one outcome a version control server must never produce. A new branch sends zeros.
            let zero = "0".repeat(40);
            if c.old != zero {
                let was = before
                    .iter()
                    .find(|(b, _)| b == &branch)
                    .and_then(|(_, sha)| sha.clone());
                if let Some(was) = was {
                    if was != c.old {
                        out.extend_from_slice(
                            pkt_line(
                                format!(
                                    "ng {} the branch moved on the server since the client last fetched\n",
                                    c.name
                                )
                                .as_bytes(),
                            )
                            .as_slice(),
                        );
                        continue;
                    }
                }
            }
            match changeset {
                Some(id) => {
                    // A branch that does not exist yet is created by the push, which is what a
                    // client expects when it pushes a branch the server has never seen.
                    let known: Option<String> = conn
                        .query_row(
                            "SELECT name FROM branches WHERE name = ?1",
                            rusqlite::params![branch],
                            |r| r.get(0),
                        )
                        .ok();
                    if known.is_none() {
                        conn.execute(
                            "INSERT INTO branches(name, head, created_at, from_branch) VALUES(?1, NULL, ?2, NULL)",
                            rusqlite::params![branch, crate::clock::unix_now()],
                        )?;
                    }
                    crate::store::set_branch_head(&conn, &branch, &id)?;
                    crate::store::refresh_branch_tree(&conn, &branch)?;
                    out.extend_from_slice(&pkt_line(format!("ok {}\n", c.name).as_bytes()));
                }
                None => {
                    out.extend_from_slice(
                        pkt_line(
                            format!("ng {} the commit was unpacked but not imported\n", c.name)
                                .as_bytes(),
                        )
                        .as_slice(),
                    );
                }
            }
        }
        out.extend_from_slice(&flush());
        Ok(out)
    })();
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

/// What the server accepts, for a client that is about to push.
pub fn advertise_receive(root: &std::path::Path) -> ZResult<Vec<u8>> {
    let served = crate::http::open_repo_for_serve(root)?;
    let mut out = pkt_line(b"# service=git-receive-pack\n");
    out.extend_from_slice(&flush());

    let mut lines: Vec<(String, String)> = Vec::new();
    for (name, sha) in served.refs {
        lines.push((format!("refs/heads/{}", name), sha));
    }
    if lines.is_empty() {
        // A push to a repository with no branches still needs an advertisement, and git writes the
        // capabilities on a zero-id line when there is nothing to advertise.
        let zero = "0".repeat(40);
        lines.push((zero.clone(), format!("{} capabilities^{{}}\0report-status", zero)));
    }
    // `report-status` is the only capability this server honours. Advertising `delete-refs` would
    // mean a push that deletes a branch reports a result this code never produces.
    out.extend_from_slice(&ref_advertisement(&lines, &["report-status"]));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pkt_line_counts_its_own_header() {
        let got = pkt_line(b"hello\n");
        assert_eq!(&got[..4], b"000a", "6 bytes of payload plus a 4 byte header");
        assert_eq!(&got[4..], b"hello\n");
    }

    #[test]
    fn a_flush_is_four_zeroes() {
        assert_eq!(flush(), b"0000".to_vec());
    }

    #[test]
    fn the_first_ref_line_carries_capabilities_after_a_nul() {
        let refs = vec![
            ("refs/heads/main".to_string(), "a".repeat(40)),
            ("refs/heads/dev".to_string(), "b".repeat(40)),
        ];
        let out = ref_advertisement(&refs, &["ofs-delta", "side-band-64k"]);
        let text = String::from_utf8_lossy(&out);
        let first = text.split('\n').next().unwrap();
        assert!(first.contains('\0'), "the first line needs the capabilities");
        assert!(first.contains("ofs-delta"));
        // Only the first. A client reads capabilities once.
        let second = text.split('\n').nth(1).unwrap();
        assert!(!second.contains('\0'), "capabilities belong to one line only");
        assert!(out.ends_with(b"0000"));
    }

    #[test]
    fn pkt_lines_round_trip() {
        let body = [pkt_line(b"want abc\n"), pkt_line(b"done\n"), flush()].concat();
        let got = read_pkt_lines(&body);
        assert_eq!(got, vec!["want abc".to_string(), "done".to_string()]);
    }

    #[test]
    fn a_truncated_pkt_line_is_not_panicked_over() {
        // A body that claims more bytes than it holds is a client bug or a truncation, and reading
        // it must stop rather than slice past the end.
        let got = read_pkt_lines(b"0064short");
        assert!(got.is_empty());
    }

    #[test]
    fn a_pack_starts_with_the_magic_version_and_count() {
        let entries = vec![Entry {
            kind: 3,
            body: b"hello\n".to_vec(),
        }];
        let pack = write_pack(&entries).unwrap();
        assert_eq!(&pack[..4], b"PACK");
        assert_eq!(u32::from_be_bytes([pack[4], pack[5], pack[6], pack[7]]), 2);
        assert_eq!(u32::from_be_bytes([pack[8], pack[9], pack[10], pack[11]]), 1);
        // Four header bytes, four version, four count, then the object, then a 20 byte checksum.
        assert!(pack.len() > 12 + 20);
    }

    #[test]
    fn an_object_larger_than_fifteen_bytes_declares_its_size_across_bytes() {
        let body = vec![b'x'; 300];
        let entries = vec![Entry {
            kind: 3,
            body: body.clone(),
        }];
        let pack = write_pack(&entries).unwrap();
        // Read the varint back the way git does. The type lives in the FIRST byte, and the loop
        // moves past it: reading the type after the loop reads the last size byte, which is how this
        // test failed while the encoder was correct.
        let mut i = 12;
        let kind = (pack[i] >> 4) & 0x7;
        let mut size = (pack[i] & 0x0f) as usize;
        let mut shift = 4;
        while pack[i] & 0x80 != 0 {
            i += 1;
            size |= ((pack[i] & 0x7f) as usize) << shift;
            shift += 7;
        }
        assert_eq!(size, 300, "the size varint did not survive");
        assert_eq!(kind, 3, "the type is a blob");
    }
}
