#!/usr/bin/env python3
"""Sign a request against an S3 endpoint and print the outcome.

Used to check credentials and list buckets before a remote is configured. Reads
everything from the environment so no secret is written to a file or a shell
history entry:

  S3_ENDPOINT, S3_REGION, S3_ACCESS_KEY, S3_SECRET_KEY, and optionally S3_PATH.
"""

import hashlib
import hmac
import os
import ssl
import sys
import urllib.parse
import urllib.request

EP = os.environ["S3_ENDPOINT"].rstrip("/")
REGION = os.environ.get("S3_REGION", "us-east-1")
AK = os.environ["S3_ACCESS_KEY"]
SK = os.environ["S3_SECRET_KEY"]
PATH = os.environ.get("S3_PATH", "/")
METHOD = os.environ.get("S3_METHOD", "GET")
BODY = os.environ.get("S3_BODY", "").encode()


def sha(b):
    return hashlib.sha256(b).hexdigest()


def enc(s, keep_slash):
    out = []
    for b in s.encode():
        c = chr(b)
        if (c.isalnum() and b < 128) or c in "-_.~" or (keep_slash and c == "/"):
            out.append(c)
        else:
            out.append("%%%02X" % b)
    return "".join(out)


def sign_key(secret, date, region, service):
    k = ("AWS4" + secret).encode()
    for p in (date, region, service, "aws4_request"):
        k = hmac.new(k, p.encode(), hashlib.sha256).digest()
    return k


# The endpoint may or may not carry a scheme.
base = EP if EP.startswith(("http://", "https://")) else "https://" + EP
u = urllib.parse.urlparse(base)
host = u.netloc
host_header = host
scheme = u.scheme

amz_date = os.environ.get("S3_DATE")
if not amz_date:
    import datetime

    amz_date = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
date = amz_date[:8]

payload_hash = sha(BODY)
headers = {
    "host": host_header,
    "x-amz-content-sha256": payload_hash,
    "x-amz-date": amz_date,
}
if METHOD == "PUT":
    headers["content-length"] = str(len(BODY))

names = sorted(headers)
canon_headers = "".join("%s:%s\n" % (n, headers[n]) for n in names)
signed = ";".join(names)
canonical = "\n".join(
    [METHOD, enc(PATH, True), "", canon_headers, signed, payload_hash]
)
scope = "%s/%s/s3/aws4_request" % (date, REGION)
sts = "\n".join(["AWS4-HMAC-SHA256", amz_date, scope, sha(canonical.encode())])
sig = hmac.new(sign_key(SK, date, REGION, "s3"), sts.encode(), hashlib.sha256).hexdigest()
auth = "AWS4-HMAC-SHA256 Credential=%s/%s, SignedHeaders=%s, Signature=%s" % (
    AK, scope, signed, sig
)

req = urllib.request.Request(base + PATH, data=BODY if METHOD == "PUT" else None, method=METHOD)
for n in names:
    if n == "host":
        continue
    req.add_header(n, headers[n])
req.add_header("Authorization", auth)

ctx = ssl.create_default_context()
try:
    with urllib.request.urlopen(req, timeout=30, context=ctx) as r:
        data = r.read()
        print("STATUS", r.status)
        print(data[:2000].decode("utf-8", "replace"))
except urllib.error.HTTPError as e:
    print("STATUS", e.code)
    print(e.read()[:2000].decode("utf-8", "replace"))
    sys.exit(1)
