import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "release-notes.sh"


class ReleaseNotesTest(unittest.TestCase):
    def test_beta_and_stable_release_history(self):
        with tempfile.TemporaryDirectory() as directory:
            env = os.environ | {
                "GIT_AUTHOR_NAME": "Test",
                "GIT_AUTHOR_EMAIL": "test@example.invalid",
                "GIT_COMMITTER_NAME": "Test",
                "GIT_COMMITTER_EMAIL": "test@example.invalid",
            }

            def git(*args):
                return subprocess.run(
                    ["git", *args],
                    cwd=directory,
                    env=env,
                    check=True,
                    capture_output=True,
                )

            git("init", "-q")
            git("remote", "add", "origin", "https://github.com/example/grinch.git")
            for tag in ["v0.9.1", "v0.9.2-beta.1", "v0.9.2-beta.2", "v0.9.2"]:
                git("commit", "-q", "--allow-empty", "-m", f"Add {tag}")
                git("tag", tag)

            for current, previous in [
                ("v0.9.2-beta.1", "v0.9.1"),
                ("v0.9.2-beta.2", "v0.9.2-beta.1"),
                ("v0.9.2", "v0.9.1"),
            ]:
                with self.subTest(current=current):
                    notes = subprocess.check_output(
                        [SCRIPT, "auto", current], cwd=directory, text=True
                    )
                    self.assertIn(f"/compare/{previous}...{current}", notes)
                    self.assertIn(f"Add {current}", notes)
                    if current == "v0.9.2":
                        self.assertIn("Add v0.9.2-beta.1", notes)


if __name__ == "__main__":
    unittest.main()
