"""Stand-ins for the services NOVA's feature tests talk to.

  :9001, :9002  application upstreams "a" and "b" (load balancing)
  :9003         forward-auth service (SSO): cookie session=ok -> 200 + Remote-User
  :9004         plain 200 responder (backend of the ModSecurity image)
  :4318         OTLP/HTTP collector: stores spans from POST /v1/traces
  :9010         control: GET /spans, POST /down/b, POST /up/b
"""
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

SPANS = []
LOCK = threading.Lock()
SERVERS = {}


def make_upstream(name):
    class Upstream(BaseHTTPRequestHandler):
        def do_GET(self):
            body = json.dumps({
                "backend": name,
                "path": self.path,
                "host": self.headers.get("Host"),
                "xff": self.headers.get("X-Forwarded-For"),
                "proto": self.headers.get("X-Forwarded-Proto"),
                "traceparent": self.headers.get("traceparent"),
            }).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        do_HEAD = do_GET

        def log_message(self, *a):
            pass

    return Upstream


class Auth(BaseHTTPRequestHandler):
    def do_GET(self):
        if "session=ok" in (self.headers.get("Cookie") or ""):
            self.send_response(200)
            self.send_header("Remote-User", "alice")
            self.send_header("Content-Length", "0")
        else:
            self.send_response(302)
            self.send_header("Location", "https://login.example/?rd=" + (self.headers.get("X-Forwarded-Uri") or "/"))
            self.send_header("Content-Length", "0")
        self.end_headers()

    do_POST = do_GET

    def log_message(self, *a):
        pass


class Ok(BaseHTTPRequestHandler):
    def _ok(self):
        n = int(self.headers.get("Content-Length") or 0)
        if n:
            self.rfile.read(n)
        self.send_response(200)
        self.send_header("Content-Length", "2")
        self.end_headers()
        self.wfile.write(b"ok")

    do_GET = do_POST = do_PUT = do_DELETE = do_HEAD = _ok

    def log_message(self, *a):
        pass


class Collector(BaseHTTPRequestHandler):
    def do_POST(self):
        n = int(self.headers.get("Content-Length") or 0)
        data = json.loads(self.rfile.read(n) or b"{}")
        found = []
        for rs in data.get("resourceSpans", []):
            for ss in rs.get("scopeSpans", []):
                for s in ss.get("spans", []):
                    attrs = {}
                    for a in s.get("attributes", []):
                        v = a.get("value", {})
                        attrs[a.get("key")] = next(iter(v.values()), None) if v else None
                    found.append({
                        "name": s.get("name"),
                        "trace": s.get("traceId"),
                        "span": s.get("spanId"),
                        "parent": s.get("parentSpanId", ""),
                        "attrs": attrs,
                    })
        with LOCK:
            SPANS.extend(found)
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", "2")
        self.end_headers()
        self.wfile.write(b"{}")

    def log_message(self, *a):
        pass


def serve(port, handler):
    srv = ThreadingHTTPServer(("0.0.0.0", port), handler)
    SERVERS[port] = srv
    threading.Thread(target=srv.serve_forever, daemon=True).start()


class Control(BaseHTTPRequestHandler):
    def do_GET(self):
        with LOCK:
            body = json.dumps(SPANS).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        if self.path == "/down/b" and 9002 in SERVERS:
            srv = SERVERS.pop(9002)
            srv.shutdown()
            srv.server_close()  # connections are now refused
        elif self.path == "/up/b" and 9002 not in SERVERS:
            serve(9002, make_upstream("b"))
        self.send_response(204)
        self.end_headers()

    def log_message(self, *a):
        pass


if __name__ == "__main__":
    serve(9001, make_upstream("a"))
    serve(9002, make_upstream("b"))
    serve(9003, Auth)
    serve(9004, Ok)
    serve(4318, Collector)
    ThreadingHTTPServer(("0.0.0.0", 9010), Control).serve_forever()
