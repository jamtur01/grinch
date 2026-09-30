"""Run with: uv run --no-project python scripts/tests/test_expand_shortener.py."""

import http.server
import os
import pathlib
import shlex
import shutil
import subprocess
import tempfile
import threading
import time
import unittest

SCRIPT = pathlib.Path(__file__).resolve().parents[2] / "examples/expand-shortener.sh"
TARGET = "http://destination.test/private?document=42#section"


class RedirectServer(http.server.BaseHTTPRequestHandler):
    requests: list[str] = []

    def do_HEAD(self) -> None:
        """Serve shortener and authentication hops without external requests."""
        self.requests.append(self.path)
        redirects = {
            "/short": TARGET,
            "/chain": "http://t.co/relative",
            "/relative": "/short",
            "/private?document=42": "/login",
            "/loop": "/loop",
            "/to-error": "http://t.co/error",
            "/to-disconnect": "http://t.co/disconnect",
            "/delay": "/slow",
            "/unsafe": "file:///private/secret",
        }
        if self.path == "/disconnect":
            self.close_connection = True
            return
        if self.path == "/delay":
            time.sleep(2)
        if self.path == "/slow":
            time.sleep(6)
        location = redirects.get(self.path)
        self.send_response(503 if self.path == "/error" else 302 if location else 200)
        if location:
            self.send_header("Location", location)
        if self.path == "/private?document=42":
            self.send_header("Set-Cookie", "return_to=/private?document=42")
        self.end_headers()

    def log_message(self, format: str, *args: object) -> None:
        """Keep expected HTTP requests out of test output."""


class ShortenerTests(unittest.TestCase):
    def test_expansion_preserves_destination_and_falls_back_on_failure(self) -> None:
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), RedirectServer)
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        try:
            with tempfile.TemporaryDirectory() as directory:
                binary_dir = pathlib.Path(directory)
                curl = shutil.which("curl")
                self.assertIsNotNone(curl)
                (binary_dir / "curl").write_text(
                    "#!/bin/bash\nset -euo pipefail\n"
                    f"exec {shlex.quote(str(curl))} --disable --noproxy '*' "
                    f'--connect-to ::127.0.0.1:{server.server_port} "$@"\n'
                )
                (binary_dir / "open").write_text(
                    '#!/bin/bash\nset -euo pipefail\nprintf "%s\\n" "$@"\n'
                )
                for executable in binary_dir.iterdir():
                    executable.chmod(0o700)
                environment = os.environ | {
                    "PATH": f"{binary_dir}:{os.environ['PATH']}"
                }
                cases = [
                    ("http://bit.ly/short", TARGET, ["/short"]),
                    (
                        "http://tinyurl.com/chain",
                        TARGET,
                        ["/chain", "/relative", "/short"],
                    ),
                    ("http://BIT.LY/short", TARGET, ["/short"]),
                    (TARGET, TARGET, []),
                    (
                        "http://bit.ly/no-redirect",
                        "http://bit.ly/no-redirect",
                        ["/no-redirect"],
                    ),
                    (
                        "http://bit.ly/to-error",
                        "http://bit.ly/to-error",
                        ["/to-error", "/error"],
                    ),
                    (
                        "http://bit.ly/to-disconnect",
                        "http://bit.ly/to-disconnect",
                        ["/to-disconnect", "/disconnect"],
                    ),
                    ("http://bit.ly/unsafe", "http://bit.ly/unsafe", ["/unsafe"]),
                    ("http://bit.ly/loop", "http://bit.ly/loop", ["/loop"] * 10),
                    ("http://bit.ly/delay", "http://bit.ly/delay", ["/delay", "/slow"]),
                    ("http://bit.ly:invalid/short", "http://bit.ly:invalid/short", []),
                    (
                        "http://bit.ly.evil.test/short",
                        "http://bit.ly.evil.test/short",
                        [],
                    ),
                    (
                        "http://bit.ly@evil.test/short",
                        "http://bit.ly@evil.test/short",
                        [],
                    ),
                    ("--args", "--args", []),
                ]
                for source, destination, requests in cases:
                    with self.subTest(source=source):
                        RedirectServer.requests = []
                        started = time.monotonic()
                        result = subprocess.run(
                            ["/bin/bash", str(SCRIPT), source],
                            env=environment,
                            capture_output=True,
                            text=True,
                            check=True,
                            timeout=7,
                        )
                        self.assertLess(time.monotonic() - started, 6)
                        self.assertEqual(
                            result.stdout.splitlines(), ["--", destination]
                        )
                        self.assertEqual(RedirectServer.requests, requests)
        finally:
            server.shutdown()
            server.server_close()
            worker.join()


if __name__ == "__main__":
    unittest.main()
