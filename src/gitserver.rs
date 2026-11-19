//! The git smart HTTP protocol, enough of it that `git clone` works against a gesta server.
//!
//! Three layers, and each is separate on purpose:
//!
//! - `pkt-line`, the framing git uses for every message it sends.
//! - `ref_advertisement`, what the server says it has, which is the first thing `git clone` reads.
//! - `pack`, the object transfer. Written here rather than delegated, because the whole point of a
//!   server is that a client with no gesta binary can read from it.
//!
//! What is deliberately absent: the v2 protocol's capabilities beyond `ofs-delta`, multi-pack
//! indexes, protocol v1's `multi_ack_detailed` state machine, and push. Each is a negotiation that
//! only a second round trip needs, and a clone is one round trip. A repository that advertises
//! capability it does not implement is worse than one that advertises less: the client takes the
//! offer and then hangs.
//!
//! The pack writes every object in full, with no delta. Measured on this project's history that is
//! about 3x the size of a delta pack, and it is the right first version: a delta encoder that is
//! wrong produces a pack git refuses with a message about a bad object number, and the failure looks
//! like a protocol bug rather than an encoder bug.

use crate::ZResult;

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
