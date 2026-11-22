#!/usr/bin/env python3
"""Provider-free wire-level check of the Connect consumer's request headers."""

import argparse
from http.server import BaseHTTPRequestHandler, HTTPServer
import importlib.util
import json
from pathlib import Path
import tempfile
import threading
import unittest


spec = importlib.util.spec_from_file_location("fabric_connect", Path(__file__).with_name("fabric-connect.py"))
connect = importlib.util.module_from_spec(spec)
spec.loader.exec_module(connect)


class RequestTest(unittest.TestCase):
    def test_application_headers_on_get_and_post(self):
        observed = []

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
                observed.append((self.command, self.path, self.headers, body))
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.end_headers()
                self.wfile.write(b'{"ok":true}')

            do_POST = do_GET

            def log_message(self, format, *args):
                pass

        with tempfile.TemporaryDirectory(prefix="fabric-connect-request-") as directory:
            with HTTPServer(("127.0.0.1", 0), Handler) as server:
                thread = threading.Thread(target=server.serve_forever, daemon=True)
                thread.start()
                try:
                    token = "fixture-only-not-a-real-credential-23"
                    worker = "fixture-worker-23"
                    connection = Path(directory) / "connection.json"
                    connection.write_text(json.dumps({
                        "schema_version": 1,
                        "endpoint": f"http://127.0.0.1:{server.server_port}",
                        "worker_id": worker, "token": token,
                    }))
                    connection.chmod(0o600)
                    client = connect.Client(argparse.Namespace(
                        connection=str(connection), allow_loopback_http=True))
                    self.assertEqual(client.request("/status"), {"ok": True})
                    self.assertEqual(client.request("/poll", {"issue": 23}), {"ok": True})
                finally:
                    server.shutdown()
                    thread.join()

        self.assertEqual(len(observed), 2)
        for (method, path, headers, body), (expected_method, suffix) in zip(
                observed, [("GET", "/status"), ("POST", "/poll")]):
            with self.subTest(method=expected_method):
                self.assertEqual(method, expected_method)
                self.assertEqual(path, connect.PREFIX + suffix)
                self.assertEqual(headers["User-Agent"],
                                 "Fabric-Connect (+https://github.com/satansdeer/fabric)")
                self.assertEqual(headers["Authorization"], "Bearer " + token)
                self.assertEqual(headers["Content-Type"], "application/json")
                if method == "POST":
                    self.assertEqual(json.loads(body), {"worker_id": worker, "issue": 23})
                else:
                    self.assertEqual(body, b"")


if __name__ == "__main__":
    unittest.main()
