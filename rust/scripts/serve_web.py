#!/usr/bin/env python3
"""Serve the built web app (crates/cb-web/dist) as a single-page application.

`python3 -m http.server` answers 404 for `/trade`, `/admin` and every other
client-side route, because no such file exists. This server hands out the file
when there is one and `index.html` otherwise, which is what the router in the
browser needs. Loopback only; no caching, so a rebuilt bundle is picked up on
the next reload.

    serve_web.py <dist-dir> <port>
"""
import http.server
import os
import sys


class SpaHandler(http.server.SimpleHTTPRequestHandler):
    # Keep-alive, so a page that fetches a stylesheet, a wasm bundle, two
    # fonts and twenty sprites does not open twenty-five connections at once.
    protocol_version = "HTTP/1.1"

    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=DIST, **kwargs)

    def send_response_only(self, code, message=None):
        super().send_response_only(code, message)
        self.send_header("Cache-Control", "no-store")

    def translate_path(self, path):
        # A file when there is one, index.html otherwise — for GET and HEAD
        # alike, so a route like /admin answers 200 to both.
        target = super().translate_path(path)
        if os.path.isfile(target):
            return target
        return os.path.join(DIST, "index.html")

    def log_message(self, fmt, *args):
        pass  # one line per asset per reload is noise, not information


if __name__ == "__main__":
    DIST = os.path.abspath(sys.argv[1])
    port = int(sys.argv[2])
    http.server.ThreadingHTTPServer.allow_reuse_address = True
    # The default listen backlog is five. A browser opening a page here asks
    # for a couple of dozen files in the same instant, and every connection
    # past the fifth was reset by the kernel — which showed up as sprites that
    # randomly failed to load and, once, as a wasm bundle that never arrived
    # and a page that stayed blank.
    http.server.ThreadingHTTPServer.request_queue_size = 256
    http.server.ThreadingHTTPServer.daemon_threads = True
    with http.server.ThreadingHTTPServer(("127.0.0.1", port), SpaHandler) as srv:
        srv.serve_forever()
