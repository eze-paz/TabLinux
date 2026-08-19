#!/usr/bin/env python3
"""Static server that never lets the browser cache anything.

python -m http.server sends Last-Modified and no Cache-Control, so Chrome
heuristically caches the ES modules and the Worker script. Editing
web/vm-worker.js and reloading then runs the OLD code, silently — which cost
an hour of debugging a "bug" that was simply never loaded.

    python3 serve.py [port]
"""

import functools
import http.server
import os
import sys


class NoCache(http.server.SimpleHTTPRequestHandler):
    def end_headers(self):
        self.send_header("Cache-Control", "no-store, must-revalidate")
        self.send_header("Pragma", "no-cache")
        self.send_header("Expires", "0")
        # Cross-origin isolation, matching production (the relay server sets
        # these globally): SharedArrayBuffer for the worker's futex naps.
        self.send_header("Cross-Origin-Opener-Policy", "same-origin")
        self.send_header("Cross-Origin-Embedder-Policy", "credentialless")
        super().end_headers()

    def log_message(self, fmt, *args):  # keep the console readable
        if "404" in (fmt % args):
            super().log_message(fmt, *args)


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8139
    os.chdir(os.path.dirname(os.path.abspath(__file__)))
    handler = functools.partial(NoCache, directory=os.getcwd())
    http.server.ThreadingHTTPServer(("127.0.0.1", port), handler).serve_forever()
