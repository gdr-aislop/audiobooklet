#!/usr/bin/env python3
"""A tiny stand-in for an Audiobookshelf server that can sit behind the protections self-hosters
put in front of one, for the connection-option tests (`connection_options.rs`, abs-player's
`streaming_options.rs`). Not a model of the real server: it answers only what those tests ask.

  fake_abs.py certs DIR
      Writes ca.pem, server.pem/server.key (self-signed, valid for 127.0.0.1 and localhost),
      client.p12 (signed by the CA, password "pw") and other.p12 (signed by another CA).
  fake_abs.py serve --log FILE [--port N] [--tls DIR] [--mtls] [--header 'Name: value']
      Serves on 127.0.0.1 and prints "READY <port>" once listening. `--mtls` demands a client
      certificate signed by DIR/ca.pem; `--header` makes every request without that header fail
      with 403, the way a reverse proxy with a shared-secret header does. Every request that gets
      through the TLS handshake is appended to FILE as a JSON line.
"""
import argparse, base64, http.server, json, os, ssl, struct, subprocess, sys, threading, time, wave, io

USER, PASSWORD = "test", "test"


def jwt(exp_in=3600, kind="access"):
    def b64(obj):
        return base64.urlsafe_b64encode(json.dumps(obj).encode()).rstrip(b"=").decode()
    return f"{b64({'alg': 'HS256', 'typ': 'JWT'})}.{b64({'sub': 'u1', 'exp': int(time.time()) + exp_in, 'kind': kind})}.sig"


def make_certs(directory):
    os.makedirs(directory, exist_ok=True)
    def run(*args, **kw):
        subprocess.run(args, check=True, cwd=directory, capture_output=True, **kw)
    ext = "subjectAltName=IP:127.0.0.1,DNS:localhost"
    run("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", "server.key", "-out", "server.pem",
        "-days", "30", "-subj", "/CN=localhost", "-addext", ext)
    for ca in ("ca", "other-ca"):
        run("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", f"{ca}.key", "-out", f"{ca}.pem",
            "-days", "30", "-subj", f"/CN=test {ca}")
    for name, ca in (("client", "ca"), ("other", "other-ca")):
        run("openssl", "req", "-newkey", "rsa:2048", "-nodes", "-keyout", f"{name}.key", "-out", f"{name}.csr", "-subj", f"/CN=test {name}")
        run("openssl", "x509", "-req", "-in", f"{name}.csr", "-CA", f"{ca}.pem", "-CAkey", f"{ca}.key", "-CAcreateserial",
            "-out", f"{name}.pem", "-days", "30")
        run("openssl", "pkcs12", "-export", "-inkey", f"{name}.key", "-in", f"{name}.pem", "-out", f"{name}.p12", "-passout", "pass:pw")


def wav_bytes(seconds=2, rate=8000):
    buf = io.BytesIO()
    with wave.open(buf, "wb") as w:
        w.setnchannels(1); w.setsampwidth(2); w.setframerate(rate)
        w.writeframes(b"".join(struct.pack("<h", int(8000 * ((i // 20) % 2 * 2 - 1))) for i in range(seconds * rate)))
    return buf.getvalue()


AUDIO = wav_bytes()


def make_handler(log_path, header):
    want_name, want_value = (header.split(":", 1)[0].strip().lower(), header.split(":", 1)[1].strip()) if header else (None, None)
    lock = threading.Lock()

    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *args):  # silence stderr
            pass

        def record(self, status):
            peer = None
            if isinstance(self.connection, ssl.SSLSocket):
                cert = self.connection.getpeercert()
                if cert:
                    peer = dict(x[0] for x in cert["subject"]).get("commonName")
            entry = {"method": self.command, "path": self.path.split("?")[0], "status": status,
                     "user_agent": self.headers.get("User-Agent"), "gate_header": self.headers.get(want_name) if want_name else None,
                     "authorization": bool(self.headers.get("Authorization")), "client_cert_cn": peer,
                     "tls": isinstance(self.connection, ssl.SSLSocket), "port": self.server.server_port}
            with lock, open(log_path, "a") as f:
                f.write(json.dumps(entry) + "\n")

        def reply(self, status, body=b"", ctype="application/json", extra=None):
            if isinstance(body, (dict, list)):
                body = json.dumps(body).encode()
            self.send_response(status)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(body)))
            for k, v in (extra or {}).items():
                self.send_header(k, v)
            self.end_headers()
            if self.command != "HEAD":
                self.wfile.write(body)
            self.record(status)

        def gated(self):
            if want_name and self.headers.get(want_name) != want_value:
                self.reply(403, {"error": "blocked by the proxy"})
                return True
            return False

        def authed(self):
            token = (self.headers.get("Authorization") or "").removeprefix("Bearer ").strip()
            if not token:
                token = (self.path.split("token=", 1) + [""])[1].split("&")[0]
            return token.count(".") == 2 or token == "api-key-token"

        def do_HEAD(self):
            self.do_GET()

        def do_GET(self):
            if self.gated():
                return
            path = self.path.split("?")[0]
            if path in ("/ping",):
                return self.reply(200, {"success": True})
            if path == "/status":
                return self.reply(200, {"isInit": True, "app": "audiobookshelf", "serverVersion": "2.36.0"})
            if not self.authed():
                return self.reply(401, {"error": "Unauthorized"})
            if path == "/api/libraries":
                return self.reply(200, {"libraries": [{"id": "11111111-1111-4111-8111-111111111111", "name": "Books", "mediaType": "book", "displayOrder": 1, "icon": "database"}]})
            if path.startswith("/api/items/") and "/file/" in path:
                data, start, status, extra = AUDIO, 0, 200, {"Accept-Ranges": "bytes"}
                rng = self.headers.get("Range")
                if rng and rng.startswith("bytes="):
                    start = int(rng[6:].split("-")[0] or 0)
                    data, status = AUDIO[start:], 206
                    extra["Content-Range"] = f"bytes {start}-{len(AUDIO) - 1}/{len(AUDIO)}"
                return self.reply(status, data, "audio/wav", extra)
            self.reply(404, {"error": "not found"})

        def do_POST(self):
            body = self.rfile.read(int(self.headers.get("Content-Length") or 0))
            if self.gated():
                return
            path = self.path.split("?")[0]
            if path == "/login":
                try:
                    creds = json.loads(body)
                except ValueError:
                    creds = {}
                if creds.get("username") == USER and creds.get("password") == PASSWORD:
                    return self.reply(200, {"user": {"id": "u1", "username": USER, "accessToken": jwt(), "refreshToken": jwt(86400, "refresh")}})
                return self.reply(401, "Unauthorized".encode(), "text/plain")
            if path == "/auth/refresh":
                return self.reply(200, {"user": {"id": "u1", "username": USER, "accessToken": jwt(), "refreshToken": jwt(86400, "refresh")}})
            self.reply(404, {"error": "not found"})

    return Handler


class Server(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def handle_error(self, request, client_address):  # handshake failures are expected in these tests
        pass

    def get_request(self):
        sock, addr = self.socket.accept()
        return sock, addr


def serve(args):
    server = Server(("127.0.0.1", args.port), make_handler(args.log, args.header))
    if args.tls:
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        ctx.load_cert_chain(os.path.join(args.tls, "server.pem"), os.path.join(args.tls, "server.key"))
        if args.mtls:
            ctx.verify_mode = ssl.CERT_REQUIRED
            ctx.load_verify_locations(os.path.join(args.tls, "ca.pem"))
        server.socket = ctx.wrap_socket(server.socket, server_side=True, do_handshake_on_connect=False)
        # Handshake in the request thread, so one bad client can't stall accept().
        orig_process = server.process_request_thread
        def process(request, client_address):
            try:
                request.do_handshake()
            except (ssl.SSLError, OSError):
                try: request.close()
                except OSError: pass
                return
            orig_process(request, client_address)
        server.process_request_thread = process
    print(f"READY {server.server_port}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    p = argparse.ArgumentParser()
    sub = p.add_subparsers(dest="cmd", required=True)
    c = sub.add_parser("certs"); c.add_argument("dir")
    s = sub.add_parser("serve")
    s.add_argument("--log", required=True); s.add_argument("--port", type=int, default=0)
    s.add_argument("--tls"); s.add_argument("--mtls", action="store_true"); s.add_argument("--header")
    a = p.parse_args()
    make_certs(a.dir) if a.cmd == "certs" else serve(a)
