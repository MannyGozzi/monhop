#!/usr/bin/env python3
"""Agent-to-agent relay for the MonHop lane: Mac (Claude) and Windows (Astra) exchange text over the LAN.

One listener on the Mac's LAN address only, one shared token in every path, and only the two
computers' addresses may connect. Messages are appended to log.jsonl; /wait long-polls for new ones.
"""
import json, os, secrets, sys, threading, time
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit

HERE = os.path.dirname(os.path.abspath(__file__))
HOST_FILE = os.path.join(HERE, "host.txt")  # this machine's LAN address, git-ignored
PEERS_FILE = os.path.join(HERE, "peers.txt")  # one allowed peer address per line, git-ignored
BIND = (open(HOST_FILE).read().strip(), 24880)
ALLOWED = {"127.0.0.1", BIND[0], *(line.strip() for line in open(PEERS_FILE) if line.strip())}
LOG = os.path.join(HERE, "log.jsonl")
TOKEN_FILE = os.path.join(HERE, "token.txt")
MAX_BODY = 256 * 1024
MAX_WAIT = 1500

if not os.path.exists(TOKEN_FILE):
    with open(TOKEN_FILE, "w") as f:
        f.write(secrets.token_hex(16))
TOKEN = open(TOKEN_FILE).read().strip()

lock = threading.Condition()
messages = []
if os.path.exists(LOG):
    with open(LOG) as f:
        messages = [json.loads(line) for line in f if line.strip()]


def append(sender, text):
    with lock:
        item = {"seq": len(messages) + 1, "at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"), "from": sender, "text": text}
        messages.append(item)
        with open(LOG, "a") as f:
            f.write(json.dumps(item, ensure_ascii=False) + "\n")
        lock.notify_all()
        return item


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        sys.stderr.write("%s %s %s\n" % (datetime.now(timezone.utc).strftime("%H:%M:%SZ"), self.client_address[0], fmt % args))

    def reply(self, code, payload):
        body = json.dumps(payload, ensure_ascii=False).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def route(self):
        if self.client_address[0] not in ALLOWED:
            self.reply(403, {"error": "address not allowed"}); return None, None
        url = urlsplit(self.path)
        parts = url.path.strip("/").split("/")
        if len(parts) != 2 or parts[0] != TOKEN:
            self.reply(404, {"error": "not found"}); return None, None
        return parts[1], {k: v[-1] for k, v in parse_qs(url.query).items()}

    def do_GET(self):
        action, q = self.route()
        if action is None: return
        since = int(q.get("since", "0") or 0)
        if action == "health":
            self.reply(200, {"seq": len(messages), "now": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")})
        elif action == "log":
            self.reply(200, [m for m in messages if m["seq"] > since])
        elif action == "wait":
            deadline = time.monotonic() + min(MAX_WAIT, int(q.get("timeout", "600") or 600))
            with lock:
                while len(messages) <= since and time.monotonic() < deadline:
                    lock.wait(timeout=max(0.0, deadline - time.monotonic()))
                fresh = [m for m in messages if m["seq"] > since]
            self.reply(200, fresh)
        else:
            self.reply(404, {"error": "unknown action"})

    def do_POST(self):
        action, q = self.route()
        if action is None: return
        length = int(self.headers.get("Content-Length", "0") or 0)
        if action != "send" or length <= 0 or length > MAX_BODY:
            self.reply(400, {"error": "POST /<token>/send with a body up to 256 KB"}); return
        raw = self.rfile.read(length).decode("utf-8", "replace")
        sender, text = q.get("from"), raw
        if self.headers.get("Content-Type", "").startswith("application/json"):
            try:
                data = json.loads(raw); sender, text = data.get("from", sender), data.get("text", "")
            except ValueError:
                self.reply(400, {"error": "bad json"}); return
        if sender not in ("mac", "win") or not text.strip():
            self.reply(400, {"error": "from must be mac or win and text must not be empty"}); return
        self.reply(200, {"seq": append(sender, text.strip())["seq"]})


if __name__ == "__main__":
    server = ThreadingHTTPServer(BIND, Handler)
    server.daemon_threads = True
    sys.stderr.write("relay listening on %s:%d, %d messages so far\n" % (BIND[0], BIND[1], len(messages)))
    server.serve_forever()
