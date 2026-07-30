# Chrome Window Monitor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add privacy-safe pre/post Chrome window monitoring without changing Grinch launch behavior.

**Architecture:** Preserve the shell entry point for process and log discovery, then delegate sampling, JSONL following, timestamp correlation, and diagnostics to a standard-library Python module. Keep parsing and analysis pure so fixtures can exercise the failure-prone logic without Chrome.

**Tech Stack:** Bash, Python 3 standard library, AppleScript, `unittest`, ShellCheck, Cargo.

---

### Task 1: Pure snapshot and event analysis

**Files:**
- Create: `scripts/tests/test_click_watch_monitor.py`
- Create: `scripts/tests/fixtures/window_samples.jsonl`
- Create: `scripts/click-watch-monitor.py`

- [ ] Write failing tests for valid snapshots, unavailable snapshots, stable-ID creation/removal/reorder deltas, timestamp selection, hostname redaction, and the unavailable-not-zero regression.
- [ ] Run `python3 -m unittest discover -s scripts/tests -p 'test_*.py' -v` and confirm failure because the monitor module is absent.
- [ ] Implement immutable snapshot/window records plus pure parsing, correlation, redaction, and delta functions.
- [ ] Re-run the unit suite and confirm it passes.

### Task 2: Live sampler and diagnostics

**Files:**
- Modify: `scripts/tests/test_click_watch_monitor.py`
- Modify: `scripts/click-watch-monitor.py`

- [ ] Write failing tests for concise reuse output and structured new-window diagnostics.
- [ ] Run the focused tests and confirm the formatter behavior is missing.
- [ ] Implement non-activating AppleScript sampling, a ten-second ring buffer, frontmost/index history, timed post-event collection, optional raw privacy-safe JSONL, and signal-safe shutdown.
- [ ] Re-run the unit suite and confirm it passes.

### Task 3: Additive shell entry point

**Files:**
- Create: `scripts/tests/test-click-watch.sh`
- Modify: `scripts/click-watch.sh`

- [ ] Write a failing smoke test with fake `pgrep`, `lsof`, and `python3` commands that expects the shell entry point to pass the resolved log to the Python helper.
- [ ] Run `bash scripts/tests/test-click-watch.sh` and confirm failure against the old inline monitor.
- [ ] Reduce the shell script to discovery, current-Space lookup, startup status, and `exec python3 ... --log ...`, preserving existing missing-process and missing-log messages.
- [ ] Re-run the smoke test and confirm it passes.

### Task 4: Full verification and commit

**Files:**
- Review all files above.

- [ ] Run `python3 -m unittest discover -s scripts/tests -p 'test_*.py' -v`.
- [ ] Run `bash scripts/tests/test-click-watch.sh`.
- [ ] Run `shellcheck scripts/click-watch.sh scripts/tests/test-click-watch.sh` when ShellCheck is installed.
- [ ] Run `cargo test`.
- [ ] Inspect `git diff --check`, `git diff`, and privacy-sensitive field names.
- [ ] Commit the cohesive implementation with the repository's click-watch commit style.
