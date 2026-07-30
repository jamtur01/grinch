#!/usr/bin/env python3
import dataclasses
import importlib.util
import json
import pathlib
import sys
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("click_watch_monitor", ROOT / "click-watch-monitor.py")
monitor = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = monitor
SPEC.loader.exec_module(monitor)


class SnapshotParsingTests(unittest.TestCase):
    def test_parses_stable_window_ids_and_state(self):
        raw = "OK\tfalse\n121\t1\t9\tfalse\tnormal\t0\t25\t1440\t900\n122\t2\t3\ttrue\tnormal\t10\t30\t900\t700\n"
        snapshot = monitor.parse_snapshot_output(raw, 100.25)
        self.assertTrue(snapshot.available)
        self.assertFalse(snapshot.chrome_frontmost)
        self.assertEqual([window.id for window in snapshot.windows], [121, 122])
        self.assertEqual(snapshot.windows[0].bounds, (0, 25, 1440, 900))
        self.assertTrue(snapshot.windows[1].minimized)

    def test_unavailable_snapshot_is_not_an_empty_success(self):
        snapshot = monitor.parse_snapshot_output("UNAVAILABLE\tApple event timed out\n", 100.0)
        self.assertFalse(snapshot.available)
        self.assertIsNone(snapshot.windows)
        self.assertIn("timed out", snapshot.error)

    def test_empty_success_is_a_real_zero_window_snapshot(self):
        snapshot = monitor.parse_snapshot_output("OK\tfalse\n", 100.0)
        self.assertTrue(snapshot.available)
        self.assertEqual(snapshot.windows, ())


class AnalysisTests(unittest.TestCase):
    def setUp(self):
        self.before = monitor.Snapshot(
            ts=10.0,
            available=True,
            chrome_frontmost=False,
            windows=(
                monitor.WindowState(121, 1, 9, False, "normal", (0, 25, 1440, 900)),
                monitor.WindowState(122, 2, 3, False, "normal", (30, 30, 900, 700)),
            ),
        )

    def test_delta_uses_ids_for_created_removed_and_reordered(self):
        after = monitor.Snapshot(
            ts=10.3,
            available=True,
            chrome_frontmost=True,
            windows=(
                monitor.WindowState(123, 1, 1, False, "normal", (0, 25, 1440, 900)),
                monitor.WindowState(121, 2, 10, False, "normal", (0, 25, 1440, 900)),
            ),
        )
        delta = monitor.compare_snapshots(self.before, after)
        self.assertEqual(delta.created, (123,))
        self.assertEqual(delta.removed, (122,))
        self.assertEqual(delta.reordered, (121,))

    def test_unavailable_sample_has_no_delta(self):
        unavailable = monitor.Snapshot(ts=10.3, available=False, windows=None, error="denied")
        self.assertIsNone(monitor.compare_snapshots(self.before, unavailable))

    def test_nearest_before_never_selects_post_event_sample(self):
        snapshots = [
            self.before,
            monitor.Snapshot(ts=10.2, available=True, chrome_frontmost=False, windows=()),
        ]
        self.assertIs(monitor.nearest_before(snapshots, 10.1), self.before)

    def test_event_is_reduced_to_hostname_and_safe_fields(self):
        event = monitor.sanitize_event(
            {
                "ts": 10.1,
                "final": "https://example.com/private/path?token=secret#fragment",
                "url": "https://source.invalid/even-more-secret",
                "strategy": "launch_new_instance",
                "args": ["--profile-directory=Profile 10"],
                "opener": {"name": "Mail", "bundleId": "com.apple.mail", "pid": 42},
                "matchedRule": {"index": 2, "name": "work"},
            }
        )
        encoded = json.dumps(event)
        self.assertEqual(event["host"], "example.com")
        self.assertEqual(event["profile_arg"], "Profile 10")
        self.assertNotIn("private", encoded)
        self.assertNotIn("secret", encoded)
        self.assertNotIn("source.invalid", encoded)

    def test_fixture_preserves_unavailable_instead_of_fabricating_zero(self):
        fixture = ROOT / "tests" / "fixtures" / "window_samples.jsonl"
        snapshots = [monitor.snapshot_from_record(json.loads(line)) for line in fixture.read_text().splitlines()]
        self.assertTrue(snapshots[0].available)
        self.assertFalse(snapshots[1].available)
        self.assertIsNone(snapshots[1].windows)
        self.assertEqual(monitor.compare_snapshots(snapshots[0], snapshots[1]), None)

    def test_index_one_timestamp_changes_only_when_front_window_changes(self):
        buffer = monitor.SnapshotBuffer()
        buffer.append(self.before)
        buffer.append(monitor.Snapshot(ts=11.0, available=True, chrome_frontmost=False, windows=self.before.windows))
        self.assertEqual(buffer.index_one_ages(12.0)[121], 2.0)

        swapped = tuple(
            dataclasses.replace(window, index=2 if window.id == 121 else 1)
            for window in self.before.windows
        )
        buffer.append(monitor.Snapshot(ts=11.5, available=True, chrome_frontmost=False, windows=swapped))
        self.assertEqual(buffer.index_one_ages(12.0)[122], 0.5)


class FormattingTests(unittest.TestCase):
    def test_reuse_is_one_concise_line(self):
        event = {"host": "example.com", "rule": "work", "strategy": "launch_new_instance", "profile_arg": "Profile 10", "opener": "Mail"}
        after = monitor.Snapshot(ts=10.3, available=True, chrome_frontmost=True, windows=self.self_before_windows())
        text = monitor.format_diagnostic(event, self.before_snapshot(), [after], {})
        self.assertEqual(len(text.splitlines()), 1)
        self.assertIn("reuse", text)
        self.assertIn("example.com", text)

    def test_created_window_gets_pre_and_post_diagnostic(self):
        before = self.before_snapshot()
        created = monitor.WindowState(999, 1, 1, False, "normal", (10, 10, 1000, 800))
        after = monitor.Snapshot(ts=10.3, available=True, chrome_frontmost=True, windows=(created,) + before.windows)
        event = {"host": "example.com", "rule": "work", "strategy": "launch_new_instance", "profile_arg": "Profile 10", "opener": "Mail"}
        text = monitor.format_diagnostic(event, before, [after], {121: 120.0}, chrome_background_age=30.0)
        self.assertIn("NEW WINDOW", text)
        self.assertIn("chrome_background_for=30.0s", text)
        self.assertIn("added=[999]", text)
        self.assertIn("before -100ms", text)
        self.assertIn("id=121", text)
        self.assertNotIn("http", text)

    @staticmethod
    def self_before_windows():
        return (monitor.WindowState(121, 1, 9, False, "normal", (0, 25, 1440, 900)),)

    @classmethod
    def before_snapshot(cls):
        return monitor.Snapshot(ts=10.0, available=True, chrome_frontmost=False, windows=cls.self_before_windows())


if __name__ == "__main__":
    unittest.main()
