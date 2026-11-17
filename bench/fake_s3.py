#!/usr/bin/env python3
"""A tiny S3 endpoint that verifies SigV4 independently of the client.

The point is not to reimplement S3. It is to check the client's signature the way
a real server does: rebuild the canonical request from the request that arrived,
derive the signing key from the secret, and compare. A client that signs a
different string than it sends is caught here. A client that agrees with itself
but not with the specification is caught here too, because this code shares
nothing with the Rust one.
"""

import hashlib
import hmac
import os
import re
import sys
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ACCESS_KEY = os.environ.get("FAKE_S3_KEY", "testkey")
SECRET_KEY = os.environ.get("FAKE_S3_SECRET", "testsecret")
REGION = os.environ.get("FAKE_S3_REGION", "us-east-1")
STORE = {}

failures = []


def sha256_hex(b):
    return hashlib.sha256(b).hexdigest()


def signing_key(secret, date, region, service):
    k = ("AWS4" + secret).encode()
    for part in (date, region, service, "aws4_request"):
        k = hmac.new(k, part.encode(), hashlib.sha256).digest()
    return k


def uri_encode(s, keep_slash):
    out = []
    for b in s.encode():
        c = chr(b)
        if c.isalnum() and b < 128 or c in "-_.~" or (keep_slash and c == "/"):
            out.append(c)
        else:
            out.append("%%%02X" % b)
    return "".join(out)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _object_key(self):
        # The request path is /<bucket>/<key>; the store holds <key> only, so the
        # bucket segment is dropped. Keeping it made every listing prefix miss.
        path = urllib.parse.urlparse(self.path).path.lstrip("/")
        bucket, _, key = path.partition("/")
        return key if key else bucket

    def _verify(self, body):
        auth = self.headers.get("Authorization", "")
        if not auth.startswith("AWS4-HMAC-SHA256 "):
            print("RAW AUTH: %r" % auth, file=sys.stderr, flush=True)
            return "no AWS4 authorization header"
        # The prefix was stripped above, so the match starts at Credential.
        m = re.match(
            r"Credential=([^/]+)/(\d{8})/([^/]+)/([^/]+)/aws4_request,"
            r" SignedHeaders=([^,]+), Signature=([0-9a-f]{64})",
            auth[len("AWS4-HMAC-SHA256 "):],
        )
        if not m:
            print("UNPARSEABLE AUTH: %r" % auth, file=sys.stderr, flush=True)
            return "authorization header does not parse"
        key_id, date, region, service, signed_headers, given = m.groups()
        if key_id != ACCESS_KEY:
            return "unknown access key %s" % key_id
        if region != REGION:
            return "wrong region %s" % region

        parsed = urllib.parse.urlparse(self.path)
        path = parsed.path or "/"
        query = parsed.query
        canon_query = ""
        if query:
            pairs = urllib.parse.parse_qsl(query, keep_blank_values=True)
            canon_query = "&".join(
                sorted("%s=%s" % (uri_encode(k, False), uri_encode(v, False)) for k, v in pairs)
            )

        names = [h.strip().lower() for h in signed_headers.split(";")]
        canon_header_lines = []
        for n in names:
            v = self.headers.get(n)
            if v is None:
                return "signed header %s is missing from the request" % n
            canon_header_lines.append("%s:%s\n" % (n, " ".join(v.split())))

        payload_hash = self.headers.get("x-amz-content-sha256")
        if payload_hash is None:
            return "x-amz-content-sha256 is missing"
        actual_hash = sha256_hex(body)
        if payload_hash != actual_hash:
            return "payload hash in the header does not match the body"

        canonical_request = "\n".join(
            [
                self.command,
                uri_encode(urllib.parse.unquote(path), True),
                canon_query,
                "".join(canon_header_lines),
                ";".join(names),
                payload_hash,
            ]
        )
        amz_date = self.headers.get("x-amz-date", "")
        scope = "%s/%s/%s/aws4_request" % (date, region, service)
        string_to_sign = "\n".join(
            ["AWS4-HMAC-SHA256", amz_date, scope, sha256_hex(canonical_request.encode())]
        )
        expected = hmac.new(
            signing_key(SECRET_KEY, date, region, service),
            string_to_sign.encode(),
            hashlib.sha256,
        ).hexdigest()
        if not hmac.compare_digest(expected, given):
            print("SIGNATURE MISMATCH", file=sys.stderr)
            print("expected:", expected, file=sys.stderr)
            print("given   :", given, file=sys.stderr)
            print("canonical request was:\n" + canonical_request, file=sys.stderr)
            return "signature mismatch"
        return None

    def do_PUT(self):
        n = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(n) if n else b""
        err = self._verify(body)
        if err:
            print("PUT rejected: %s" % err, file=sys.stderr, flush=True)
            failures.append(err)
            self.send_response(403)
            payload = ("<Error><Code>SignatureDoesNotMatch</Code><Message>%s</Message></Error>" % err).encode()
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return
        STORE[self._object_key()] = body
        self.send_response(200)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_GET(self):
        err = self._verify(b"")
        if err:
            print("GET rejected: %s" % err, file=sys.stderr, flush=True)
            failures.append(err)
            self.send_response(403)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        # A bucket level listing: /bucket?list-type=2&prefix=... The object path
        # has no trailing key, so this is asked before the object lookup.
        parsed = urllib.parse.urlparse(self.path)
        q = urllib.parse.parse_qs(parsed.query)
        if q.get("list-type") == ["2"]:
            prefix = q.get("prefix", [""])[0]
            keys = sorted(k for k in STORE if k.startswith(prefix))
            parts = [
                '<?xml version="1.0" encoding="UTF-8"?>',
                '<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">',
                "<Name>%s</Name>" % parsed.path.strip("/"),
                "<Prefix>%s</Prefix>" % prefix,
                "<KeyCount>%d</KeyCount>" % len(keys),
            ]
            for k in keys:
                parts.append("<Contents><Key>%s</Key><Size>%d</Size></Contents>" % (k, len(STORE[k])))
            parts.append("</ListBucketResult>")
            body = "\n".join(parts).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/xml")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        body = STORE.get(self._object_key())
        if body is None:
            self.send_response(404)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 9000
    srv = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    print("fake s3 listening on %d with %d objects" % (port, len(STORE)), flush=True)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        if failures:
            print("FAILURES: %s" % failures, flush=True)
            sys.exit(1)
