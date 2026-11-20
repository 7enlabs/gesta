"""A collector that records what it receives, so an end-to-end audit test is about bytes on a socket.

Run: python3 bench/audit_collector.py <port> <out-file>
"""
import http.server
import sys


class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(n).decode("utf-8", "replace")
        auth = self.headers.get("Authorization", "")
        with open(sys.argv[2], "a", encoding="utf-8") as fh:
            fh.write(body)
            fh.write("---auth:" + auth + "\n")
        self.send_response(200)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def log_message(self, *a):
        pass


if __name__ == "__main__":
    http.server.HTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
