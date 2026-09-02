#!/usr/bin/env python3
"""Correlate Grinch route events with privacy-safe Chrome window snapshots."""

from __future__ import annotations

import argparse
import collections
import dataclasses
import json
import pathlib
import signal
import subprocess
import sys
import threading
import time
import urllib.parse
from typing import Iterable, Optional, Sequence


@dataclasses.dataclass(frozen=True)
class WindowState:
    id: int
    index: int
    tabs: int
    minimized: bool
    mode: str
    bounds: tuple[int, int, int, int]


@dataclasses.dataclass(frozen=True)
class Snapshot:
    ts: float
    available: bool
    windows: Optional[tuple[WindowState, ...]]
    chrome_frontmost: Optional[bool] = None
    error: Optional[str] = None


@dataclasses.dataclass(frozen=True)
class WindowDelta:
    created: tuple[int, ...]
    removed: tuple[int, ...]
    reordered: tuple[int, ...]


APPLE_SCRIPT = r"""
set delim to ASCII character 9
tell application "System Events"
    set chromeRunning to exists process "Google Chrome"
    if chromeRunning then
        set chromeFrontmost to frontmost of process "Google Chrome"
    else
        return "OK" & delim & "false"
    end if
end tell

tell application "Google Chrome"
    set out to "OK" & delim & chromeFrontmost & linefeed
    repeat with i from 1 to (count of windows)
        set w to window i
        set b to bounds of w
        set out to out & (id of w) & delim & i & delim & (count of tabs of w) & delim & ¬
            (minimized of w) & delim & (mode of w) & delim & ¬
            (item 1 of b) & delim & (item 2 of b) & delim & ¬
            (item 3 of b) & delim & (item 4 of b) & linefeed
    end repeat
    return out
end tell
"""


def _bool(value: str) -> bool:
    if value == "true":
        return True
    if value == "false":
        return False
    raise ValueError(f"invalid boolean: {value}")


def parse_snapshot_output(raw: str, ts: float) -> Snapshot:
    lines = raw.splitlines()
    if not lines:
        return Snapshot(
            ts=ts, available=False, windows=None, error="empty AppleScript output"
        )
    header = lines[0].split("\t", 1)
    if header[0] != "OK":
        error = header[1] if len(header) > 1 else "AppleScript unavailable"
        return Snapshot(ts=ts, available=False, windows=None, error=error)
    try:
        frontmost = _bool(header[1])
        windows = []
        for line in lines[1:]:
            if not line:
                continue
            fields = line.split("\t")
            if len(fields) != 9:
                raise ValueError(f"expected 9 fields, got {len(fields)}")
            windows.append(
                WindowState(
                    id=int(fields[0]),
                    index=int(fields[1]),
                    tabs=int(fields[2]),
                    minimized=_bool(fields[3]),
                    mode=fields[4],
                    bounds=(
                        int(fields[5]),
                        int(fields[6]),
                        int(fields[7]),
                        int(fields[8]),
                    ),
                )
            )
        return Snapshot(
            ts=ts, available=True, chrome_frontmost=frontmost, windows=tuple(windows)
        )
    except (IndexError, TypeError, ValueError) as error:
        return Snapshot(
            ts=ts,
            available=False,
            windows=None,
            error=f"invalid AppleScript output: {error}",
        )


def snapshot_to_record(snapshot: Snapshot) -> dict:
    record = {
        "kind": "snapshot",
        "ts": snapshot.ts,
        "available": snapshot.available,
        "chrome_frontmost": snapshot.chrome_frontmost,
        "error": snapshot.error,
        "windows": None,
    }
    if snapshot.windows is not None:
        record["windows"] = [dataclasses.asdict(window) for window in snapshot.windows]
    return record


def snapshot_from_record(record: dict) -> Snapshot:
    raw_windows = record.get("windows")
    windows = None
    if raw_windows is not None:
        windows = tuple(
            WindowState(
                id=int(window["id"]),
                index=int(window["index"]),
                tabs=int(window["tabs"]),
                minimized=bool(window["minimized"]),
                mode=str(window["mode"]),
                bounds=(
                    int(window["bounds"][0]),
                    int(window["bounds"][1]),
                    int(window["bounds"][2]),
                    int(window["bounds"][3]),
                ),
            )
            for window in raw_windows
        )
    return Snapshot(
        ts=float(record["ts"]),
        available=bool(record["available"]),
        chrome_frontmost=record.get("chrome_frontmost"),
        windows=windows,
        error=record.get("error"),
    )


def compare_snapshots(before: Snapshot, after: Snapshot) -> Optional[WindowDelta]:
    if (
        not before.available
        or not after.available
        or before.windows is None
        or after.windows is None
    ):
        return None
    before_by_id = {window.id: window for window in before.windows}
    after_by_id = {window.id: window for window in after.windows}
    return WindowDelta(
        created=tuple(sorted(after_by_id.keys() - before_by_id.keys())),
        removed=tuple(sorted(before_by_id.keys() - after_by_id.keys())),
        reordered=tuple(
            sorted(
                window_id
                for window_id in before_by_id.keys() & after_by_id.keys()
                if before_by_id[window_id].index != after_by_id[window_id].index
            )
        ),
    )


def nearest_before(snapshots: Iterable[Snapshot], ts: float) -> Optional[Snapshot]:
    candidates = [
        snapshot for snapshot in snapshots if snapshot.available and snapshot.ts <= ts
    ]
    return max(candidates, key=lambda snapshot: snapshot.ts, default=None)


def sanitize_event(event: dict) -> dict:
    parsed = urllib.parse.urlparse(str(event.get("final", "")))
    profile_arg = None
    for arg in event.get("args") or []:
        if isinstance(arg, str) and arg.startswith("--profile-directory="):
            profile_arg = arg.split("=", 1)[1]
            break
    matched_rule = event.get("matchedRule")
    rule = (
        matched_rule.get("name", "none") if isinstance(matched_rule, dict) else "none"
    )
    opener = event.get("opener")
    opener_name = opener.get("name", "?") if isinstance(opener, dict) else "?"
    return {
        "kind": "event",
        "ts": float(event.get("ts", 0.0)),
        "host": parsed.hostname or "?",
        "strategy": str(event.get("strategy", "?")),
        "profile_arg": profile_arg,
        "rule": str(rule),
        "opener": str(opener_name),
    }


def parse_resolve_event(line: str) -> Optional[dict]:
    """Parse a diagnostic-log line, returning only resolve events."""
    record = json.loads(line)
    if not isinstance(record, dict):
        raise TypeError("diagnostic event must be an object")
    if record.get("event") != "resolve":
        return None
    return sanitize_event(record)


def _format_window(window: WindowState) -> str:
    bounds = ",".join(str(value) for value in window.bounds)
    return f"id={window.id} index={window.index} tabs={window.tabs} min={str(window.minimized).lower()} mode={window.mode} bounds={bounds}"


def format_diagnostic(
    event: dict,
    before: Optional[Snapshot],
    after: Sequence[Snapshot],
    last_index_one_age: dict[int, float],
    chrome_background_age: Optional[float] = None,
) -> str:
    host = event.get("host", "?")
    rule = event.get("rule", "none")
    strategy = event.get("strategy", "?")
    profile = event.get("profile_arg") or "?"
    if before is None:
        return f"[window state unavailable] {event.get('opener', '?')} -> {strategy} | host: {host} | rule: {rule}"

    event_ts = float(event.get("ts") or before.ts + 0.1)
    deltas = [compare_snapshots(before, sample) for sample in after]
    created = sorted(
        {window_id for delta in deltas if delta for window_id in delta.created}
    )
    final = next((sample for sample in reversed(after) if sample.available), None)
    before_count = len(before.windows or ())
    final_count = len(final.windows or ()) if final else "?"
    if not created:
        return f"[reuse {before_count}->{final_count}] {event.get('opener', '?')} -> {strategy} | host: {host} | rule: {rule}"

    lines = [
        f"[win {before_count}->{final_count}] <<< NEW WINDOW "
        f"added={created} profile={profile} | host: {host} | rule: {rule} "
        f"| strategy: {strategy}"
    ]
    background = (
        f" chrome_background_for={chrome_background_age:.1f}s"
        if chrome_background_age is not None
        else ""
    )
    lines.append(
        f"before {round((before.ts - event_ts) * 1000):+d}ms "
        f"chrome_frontmost={before.chrome_frontmost}{background}"
    )
    for window in before.windows or ():
        age = last_index_one_age.get(window.id)
        suffix = f" last_index_1={age:.1f}s ago" if age is not None else ""
        lines.append(f"  {_format_window(window)}{suffix}")
    for sample, delta in zip(after, deltas):
        offset = round((sample.ts - event_ts) * 1000)
        if not sample.available:
            lines.append(f"after {offset:+d}ms unavailable={sample.error or '?'}")
        elif delta is not None:
            lines.append(
                f"after {offset:+d}ms added={list(delta.created)} "
                f"removed={list(delta.removed)} "
                f"reordered={list(delta.reordered)}"
            )
    return "\n".join(lines)


def take_snapshot(timeout: float = 1.0) -> Snapshot:
    try:
        result = subprocess.run(
            ["osascript", "-e", APPLE_SCRIPT],
            capture_output=True,
            text=True,
            timeout=timeout,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        return Snapshot(ts=time.time(), available=False, windows=None, error=str(error))
    completed = time.time()
    if result.returncode != 0:
        error = result.stderr.strip() or f"osascript exited {result.returncode}"
        return Snapshot(ts=completed, available=False, windows=None, error=error)
    return parse_snapshot_output(result.stdout, completed)


class SnapshotBuffer:
    def __init__(self, retention_seconds: float = 10.0):
        self.retention_seconds = retention_seconds
        self.snapshots: collections.deque[Snapshot] = collections.deque()
        self.condition = threading.Condition()
        self.last_index_one_at: dict[int, float] = {}
        self.index_one_id: Optional[int] = None
        self.chrome_background_since: Optional[float] = None

    def append(self, snapshot: Snapshot) -> None:
        with self.condition:
            self.snapshots.append(snapshot)
            cutoff = snapshot.ts - self.retention_seconds
            while self.snapshots and self.snapshots[0].ts < cutoff:
                self.snapshots.popleft()
            if snapshot.available:
                if snapshot.chrome_frontmost:
                    self.chrome_background_since = None
                elif self.chrome_background_since is None:
                    self.chrome_background_since = snapshot.ts
                index_one = next(
                    (
                        window.id
                        for window in snapshot.windows or ()
                        if window.index == 1
                    ),
                    None,
                )
                if index_one != self.index_one_id:
                    self.index_one_id = index_one
                    if index_one is not None:
                        self.last_index_one_at[index_one] = snapshot.ts
            self.condition.notify_all()

    def before(self, ts: float) -> Optional[Snapshot]:
        with self.condition:
            return nearest_before(tuple(self.snapshots), ts)

    def at_or_after(self, ts: float, timeout: float) -> Optional[Snapshot]:
        deadline = time.monotonic() + timeout
        with self.condition:
            while True:
                candidate = next(
                    (snapshot for snapshot in self.snapshots if snapshot.ts >= ts), None
                )
                if candidate is not None:
                    return candidate
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    return None
                self.condition.wait(remaining)

    def index_one_ages(self, at: float) -> dict[int, float]:
        with self.condition:
            return {
                window_id: max(0.0, at - seen)
                for window_id, seen in self.last_index_one_at.items()
            }

    def background_age(self, at: float) -> Optional[float]:
        with self.condition:
            if self.chrome_background_since is None:
                return None
            return max(0.0, at - self.chrome_background_since)


class RawWriter:
    def __init__(self, path: Optional[pathlib.Path]):
        self.file = path.open("a", encoding="utf-8") if path else None
        self.lock = threading.Lock()

    def write(self, record: dict) -> None:
        if self.file is None:
            return
        with self.lock:
            self.file.write(json.dumps(record, separators=(",", ":")) + "\n")
            self.file.flush()

    def close(self) -> None:
        if self.file:
            self.file.close()


def sampler(
    buffer: SnapshotBuffer, writer: RawWriter, stop: threading.Event, interval: float
) -> None:
    while not stop.is_set():
        started = time.monotonic()
        snapshot = take_snapshot()
        buffer.append(snapshot)
        writer.write(snapshot_to_record(snapshot))
        stop.wait(max(0.0, interval - (time.monotonic() - started)))


def follow_lines(path: pathlib.Path, stop: threading.Event):
    with path.open("r", encoding="utf-8", errors="replace") as stream:
        stream.seek(0, 2)
        while not stop.is_set():
            line = stream.readline()
            if line:
                yield line
            else:
                stop.wait(0.05)


def run(args: argparse.Namespace) -> int:
    stop = threading.Event()
    for signum in (signal.SIGINT, signal.SIGTERM):
        signal.signal(signum, lambda _signum, _frame: stop.set())
    buffer = SnapshotBuffer(retention_seconds=args.retention)
    writer = RawWriter(args.raw_jsonl)
    thread = threading.Thread(
        target=sampler, args=(buffer, writer, stop, args.interval), daemon=True
    )
    thread.start()
    try:
        for line in follow_lines(args.log, stop):
            try:
                event = parse_resolve_event(line)
            except (TypeError, ValueError, json.JSONDecodeError):
                print("[unparseable Grinch log line]", flush=True)
                continue
            if event is None:
                continue
            writer.write(event)
            event_ts = event["ts"] or time.time()
            before = buffer.before(event_ts)
            post = []
            for offset in (0.25, 0.75, 1.5):
                sample = buffer.at_or_after(event_ts + offset, timeout=offset + 1.5)
                if sample is not None and (not post or sample is not post[-1]):
                    post.append(sample)
            ages = buffer.index_one_ages(event_ts)
            diagnostic = format_diagnostic(
                event,
                before,
                post,
                ages,
                chrome_background_age=buffer.background_age(event_ts),
            )
            print(diagnostic, flush=True)
            writer.write(
                {
                    "kind": "diagnostic",
                    "ts": time.time(),
                    "event": event,
                    "text": diagnostic,
                }
            )
    finally:
        stop.set()
        thread.join(timeout=2.0)
        writer.close()
    return 0


def parse_args(argv: Optional[Sequence[str]] = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--log",
        required=True,
        type=pathlib.Path,
        help="running Grinch JSONL diagnostic log",
    )
    parser.add_argument(
        "--raw-jsonl",
        type=pathlib.Path,
        help="append privacy-safe samples and analyses",
    )
    parser.add_argument(
        "--interval", type=float, default=0.25, help="Chrome sample interval in seconds"
    )
    parser.add_argument(
        "--retention", type=float, default=10.0, help="pre-event ring-buffer duration"
    )
    return parser.parse_args(argv)


def main() -> int:
    return run(parse_args())


if __name__ == "__main__":
    sys.exit(main())
