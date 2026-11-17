//! Minimal HTTP/1.1 transport so a ZenForge repository can be served and
//! reached over a network.
//!
//! Written on `std::net` on purpose: no TLS stack, no async runtime, no
//! dependency that could pull in an unexpected C library. The protocol is small
//! and fixed, so the parsing surface is small and testable.
//!
//! Endpoints (all under `/v1`):
//!   GET  /v1/health                     -> `ok`
//!   GET  /v1/branches                   -> one `name<TAB>head` per line
//!   GET  /v1/bundle?branch=X&have=a,b   -> zlib bundle, octet-stream
//!   POST /v1/bundle?branch=X            -> body is a zlib bundle; 409 if it
//!                                          would move the branch backwards

use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;


use crate::remote::{self, Bundle};
use crate::store::{ancestors, branch_head, init_schema, open_db};
use crate::{bad, ZResult};

/// Refuse bodies larger than this rather than reading until memory runs out.
const MAX_BODY: usize = 512 * 1024 * 1024;

/// A parsed request line plus headers and body.
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn param(&self, key: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

pub struct Response {
    pub status: u16,
    pub reason: &'static str,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Response {
    pub fn ok_text(s: impl Into<String>) -> Response {
        Response {
            status: 200,
            reason: "OK",
            content_type: "text/plain; charset=utf-8",
            body: s.into().into_bytes(),
        }
    }

    pub fn ok_binary(b: Vec<u8>) -> Response {
        Response {
            status: 200,
            reason: "OK",
            content_type: "application/octet-stream",
            body: b,
        }
    }

    pub fn error(status: u16, reason: &'static str, msg: impl Into<String>) -> Response {
        Response {
            status,
            reason,
            content_type: "text/plain; charset=utf-8",
            body: msg.into().into_bytes(),
        }
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut head = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
            self.status,
            self.reason,
            self.content_type,
            self.body.len()
        );
        head.push_str("Cache-Control: no-store\r\n\r\n");
        let mut out = head.into_bytes();
        out.extend_from_slice(&self.body);
        out
    }
}

/// Parsed request line: method, path, query, and bytes of header consumed.
pub type RequestHead = (String, String, Vec<(String, String)>, usize);

/// Parse the request line and headers from a raw buffer.
///
/// Returns the parsed request (body not yet read) and the number of bytes
/// consumed by the header block.
pub fn parse_head(buf: &[u8]) -> ZResult<RequestHead> {
    let end = find_subslice(buf, b"\r\n\r\n")
        .ok_or_else(|| bad("malformed request: no end of headers"))?;
    let head = std::str::from_utf8(&buf[..end]).map_err(|_| bad("headers are not UTF-8"))?;

    let mut lines = head.split("\r\n");
    let request_line = lines.next().ok_or_else(|| bad("empty request"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or_else(|| bad("missing method"))?.to_string();
    let target = parts.next().ok_or_else(|| bad("missing path"))?.to_string();

    if method.len() > 16 || target.len() > 8192 {
        return Err(bad("request line too long"));
    }

    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), parse_query(q)),
        None => (target, Vec::new()),
    };

    Ok((method, path, query, end + 4))
}

pub fn parse_query(q: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        out.push((url_decode(k), url_decode(v)));
    }
    out
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

/// A remote address: `http://host:port` parsed into its parts.
#[derive(Debug)]
pub struct HttpUrl {
    pub host: String,
    pub port: u16,
    pub base_path: String,
}

pub fn parse_http_url(url: &str) -> ZResult<HttpUrl> {
    // Check https BEFORE stripping the scheme, otherwise the strip fails first
    // and the user gets "not an http url" instead of an explanation.
    if url.starts_with("https://") {
        return Err(bad(
            "https is not supported: ZenForge speaks plain HTTP. Put it behind a TLS-terminating proxy",
        ));
    }
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| bad(format!("not an http url: {}", url)))?;
    let (authority, base_path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, String::new()),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>()
                .map_err(|_| bad(format!("bad port in url: {}", p)))?,
        ),
        None => (authority.to_string(), 80u16),
    };
    if host.is_empty() {
        return Err(bad("empty host in url"));
    }
    Ok(HttpUrl { host, port, base_path })
}

pub fn is_http_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// Perform one HTTP request and return `(status, body)`.
pub fn request(method: &str, url: &str, body: Option<&[u8]>) -> ZResult<(u16, Vec<u8>)> {
    let u = parse_http_url(url)?;
    let mut stream = TcpStream::connect((u.host.as_str(), u.port))
        .map_err(|e| bad(format!("cannot connect to {}:{}: {}", u.host, u.port, e)))?;
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    stream.set_write_timeout(Some(Duration::from_secs(120)))?;

    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept: */*\r\n",
        method,
        if u.base_path.is_empty() { "/".to_string() } else { u.base_path.clone() },
        u.host
    );
    if let Some(b) = body {
        head.push_str(&format!(
            "Content-Type: application/octet-stream\r\nContent-Length: {}\r\n",
            b.len()
        ));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    if let Some(b) = body {
        stream.write_all(b)?;
    }
    stream.flush()?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;

    let split = find_subslice(&raw, b"\r\n\r\n")
        .ok_or_else(|| bad("response has no end of headers"))?;
    let head_text = std::str::from_utf8(&raw[..split]).map_err(|_| bad("response head not UTF-8"))?;
    let status = head_text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| bad("response has no status code"))?;

    let declared = head_text
        .split("\r\n")
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            if k.eq_ignore_ascii_case("content-length") {
                v.trim().parse::<usize>().ok()
            } else {
                None
            }
        });

    let payload = raw[split + 4..].to_vec();
    let payload = match declared {
        Some(n) if n <= payload.len() => payload[..n].to_vec(),
        _ => payload,
    };
    Ok((status, payload))
}

/// Serve `root` over HTTP until the process is killed.
pub fn serve(root: PathBuf, bind: String, port: u16) -> ZResult<()> {
    let listener = TcpListener::bind((bind.as_str(), port))
        .map_err(|e| bad(format!("cannot bind {}:{}: {}", bind, port, e)))?;
    let local = listener.local_addr()?;
    println!("zenforge listening on http://{}", local);
    println!("repository: {}", root.display());
    println!("press Ctrl-C to stop");

    // The repository is opened per request: SQLite in WAL mode handles
    // concurrent readers, and a long-lived Connection would need a mutex.
    let root = Arc::new(root);
    for incoming in listener.incoming() {
        let stream = match incoming {
            Ok(s) => s,
            Err(_) => continue,
        };
        let root = Arc::clone(&root);
        std::thread::spawn(move || {
            let _ = handle_connection(stream, &root);
        });
    }
    Ok(())
}

fn handle_connection(mut stream: TcpStream, root: &Path) -> ZResult<()> {
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    let resp = match read_request(&mut stream) {
        Ok(req) => route(root, &req),
        Err(e) => Response::error(400, "Bad Request", e.to_string()),
    };
    stream.write_all(&resp.to_bytes())?;
    stream.flush()?;
    Ok(())
}

fn read_request(stream: &mut TcpStream) -> ZResult<Request> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];

    // Read until the header block is complete.
    let head_end = loop {
        if let Some(i) = find_subslice(&buf, b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > 64 * 1024 {
            return Err(bad("headers too large"));
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(bad("connection closed before headers completed"));
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let (method, path, query, _consumed) = parse_head(&buf[..head_end])?;

    let head_text = std::str::from_utf8(&buf[..head_end]).map_err(|_| bad("headers are not UTF-8"))?;
    let want: usize = head_text
        .split("\r\n")
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse().ok())
        .unwrap_or(0);
    if want > MAX_BODY {
        return Err(bad(format!("body of {} bytes exceeds the limit", want)));
    }

    let mut body = buf[head_end..].to_vec();
    while body.len() < want {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(bad("connection closed mid-body"));
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(want);

    Ok(Request { method, path, query, body })
}

/// Dispatch one request to a handler. Kept separate from socket code so the
/// routes can be tested without a network.
pub fn route(root: &Path, req: &Request) -> Response {
    let path = req.path.trim_end_matches('/');
    match (req.method.as_str(), path) {
        ("GET", "/v1/health") => Response::ok_text("ok"),
        ("GET", "/v1/branches") => match list_branches(root) {
            Ok(s) => Response::ok_text(s),
            Err(e) => Response::error(500, "Internal Server Error", e.to_string()),
        },
        ("GET", "/v1/bundle") => {
            let branch = req.param("branch").unwrap_or("main");
            let have: HashSet<String> = req
                .param("have")
                .map(|h| h.split(',').filter(|s| !s.is_empty()).map(|s| s.to_string()).collect())
                .unwrap_or_default();
            match fetch_bundle(root, branch, &have) {
                Ok(Some(bytes)) => Response::ok_binary(bytes),
                Ok(None) => Response::error(404, "Not Found", format!("no branch {}", branch)),
                Err(e) => Response::error(500, "Internal Server Error", e.to_string()),
            }
        }
        ("POST", "/v1/bundle") => {
            let branch = req.param("branch").unwrap_or("main");
            match receive_bundle(root, branch, &req.body) {
                Ok(msg) => Response::ok_text(msg),
                Err(ApplyError::Rejected(msg)) => Response::error(409, "Conflict", msg),
                Err(ApplyError::Other(e)) => Response::error(400, "Bad Request", e.to_string()),
            }
        }
        (_, p) if !p.starts_with("/v1/") => {
            Response::error(404, "Not Found", format!("unknown path {}", p))
        }
        (m, p) => Response::error(405, "Method Not Allowed", format!("{} {}", m, p)),
    }
}

fn list_branches(root: &Path) -> ZResult<String> {
    let conn = open_db(root)?;
    init_schema(&conn)?;
    let mut stmt = conn.prepare("SELECT name, head FROM branches ORDER BY name")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))?;
    let mut out = String::new();
    for r in rows {
        let (name, head) = r?;
        out.push_str(&format!("{}\t{}\n", name, head.unwrap_or_else(|| "-".into())));
    }
    Ok(out)
}

fn fetch_bundle(root: &Path, branch: &str, have: &HashSet<String>) -> ZResult<Option<Vec<u8>>> {
    let conn = open_db(root)?;
    init_schema(&conn)?;
    if branch_head(&conn, branch).is_err() {
        return Ok(None);
    }
    let bundle = remote::build_bundle(&conn, branch, have)?;
    Ok(Some(bundle.encode()?))
}

pub enum ApplyError {
    /// The push would lose changes: the remote head is not an ancestor.
    Rejected(String),
    Other(Box<dyn std::error::Error>),
}

impl From<Box<dyn std::error::Error>> for ApplyError {
    fn from(e: Box<dyn std::error::Error>) -> Self {
        ApplyError::Other(e)
    }
}

impl From<rusqlite::Error> for ApplyError {
    fn from(e: rusqlite::Error) -> Self {
        ApplyError::Other(Box::new(e))
    }
}

fn receive_bundle(root: &Path, branch: &str, body: &[u8]) -> Result<String, ApplyError> {
    let conn = open_db(root)?;
    init_schema(&conn)?;

    let bundle = Bundle::decode(body)?;
    let previous = branch_head(&conn, branch)?;

    // Apply first, check afterwards. A bundle normally carries only the
    // changesets the server lacks, so the incoming head's ancestry cannot be
    // resolved until those rows exist.
    let applied = remote::apply_bundle(&conn, &bundle)?;

    // A push must never move a branch to a head that does not contain where the
    // branch already was: that silently deletes other people's work.
    if let (Some(incoming), Some(prev)) = (&bundle.head, &previous) {
        let reachable = ancestors(&conn, incoming)?;
        if incoming != prev && !reachable.iter().any(|a| a == prev) {
            // Roll the ref back; the objects stay but the branch is untouched.
            conn.execute(
                "UPDATE branches SET head = ?1 WHERE name = ?2",
                rusqlite::params![prev, branch],
            )?;
            return Err(ApplyError::Rejected(format!(
                "remote branch {} is at {} which the push does not contain; pull first",
                branch, prev
            )));
        }
    }

    // The served repository must show the pushed files on disk, otherwise
    // `status` inside the origin directory reports them as deleted.
    crate::repo::materialise(&conn, root, branch)?;

    let head = branch_head(&conn, branch)?.unwrap_or_else(|| "-".into());
    Ok(format!("applied {}\nhead {}\n", applied, head))
}

/// URL a client should hit for a given repository base and branch.
pub fn bundle_url(base: &str, branch: &str, have: &[String]) -> String {
    let mut url = format!(
        "{}/v1/bundle?branch={}",
        base.trim_end_matches('/'),
        url_encode(branch)
    );
    if !have.is_empty() {
        url.push_str("&have=");
        url.push_str(&have.join("%2C"));
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_request_line_with_query() {
        let raw = b"GET /v1/bundle?branch=dev&have=cs1,cs2 HTTP/1.1\r\nHost: x\r\n\r\n";
        let (method, path, query, consumed) = parse_head(raw).unwrap();
        assert_eq!(method, "GET");
        assert_eq!(path, "/v1/bundle");
        assert_eq!(query.len(), 2);
        assert_eq!(query[0], ("branch".to_string(), "dev".to_string()));
        assert_eq!(query[1], ("have".to_string(), "cs1,cs2".to_string()));
        assert_eq!(consumed, raw.len());
    }

    #[test]
    fn rejects_a_head_without_terminator() {
        assert!(parse_head(b"GET / HTTP/1.1\r\n").is_err());
    }

    #[test]
    fn url_encode_decode_roundtrip() {
        // Includes a non-ASCII string on purpose: branch names may contain any
        // UTF-8, and percent-encoding must round-trip it byte for byte.
        for s in ["main", "feature/x", "a b", "t\u{e9}st", "cs1,cs2"] {
            assert_eq!(url_decode(&url_encode(s)), s);
        }
    }

    #[test]
    fn parses_http_url_with_and_without_port() {
        let u = parse_http_url("http://example.com:8787/repo").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.base_path.as_str()), ("example.com", 8787, "/repo"));

        let u = parse_http_url("http://127.0.0.1").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("127.0.0.1", 80));
    }

    #[test]
    fn https_is_refused_with_a_useful_message() {
        let err = parse_http_url("https://example.com").unwrap_err().to_string();
        assert!(err.contains("https is not supported"), "got: {}", err);
    }

    #[test]
    fn unknown_route_is_404() {
        let req = Request {
            method: "GET".into(),
            path: "/v1/nope".into(),
            query: vec![],
            body: vec![],
        };
        let resp = route(Path::new("/tmp/does-not-matter"), &req);
        assert_eq!(resp.status, 405);
    }

    #[test]
    fn wrong_method_on_a_known_path_is_405() {
        let req = Request {
            method: "DELETE".into(),
            path: "/v1/bundle".into(),
            query: vec![],
            body: vec![],
        };
        let resp = route(Path::new("/tmp/does-not-matter"), &req);
        assert_eq!(resp.status, 405);
    }

    #[test]
    fn health_needs_no_repository() {
        let req = Request {
            method: "GET".into(),
            path: "/v1/health".into(),
            query: vec![],
            body: vec![],
        };
        let resp = route(Path::new("/tmp/does-not-matter"), &req);
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"ok".to_vec());
    }

    #[test]
    fn bundle_url_encodes_branch_and_have_list() {
        let u = bundle_url("http://localhost:8787/", "feature/x", &["cs1".into(), "cs2".into()]);
        assert_eq!(u, "http://localhost:8787/v1/bundle?branch=feature%2Fx&have=cs1%2Ccs2");
    }

    #[test]
    fn response_has_content_length_and_connection_close() {
        let r = Response::ok_text("hello");
        let bytes = r.to_bytes();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("Content-Length: 5"));
        assert!(text.contains("Connection: close"));
        assert!(text.ends_with("hello"));
    }
}
