# Chrome Window Monitor Design

## Goal

Diagnose Chrome's intermittent choice to create a new window for a profile that already has an eligible window. Improve observation only. Do not change Grinch routing or launch behavior.

## Architecture

Keep `scripts/click-watch.sh` as the stable user-facing command and have it locate the running Grinch log before invoking `scripts/click-watch-monitor.py`. The Python monitor continuously samples Chrome window state into a bounded ring buffer while independently following Grinch JSONL. This gives each route event a real pre-launch observation rather than the existing post-launch-only sample.

The monitor uses Chrome window IDs as stable identities. Each successful snapshot contains the sample time, Chrome frontmost state, and each window's ID, index, tab count, minimized state, mode, and bounds. It never requests titles or tab URLs. A failed AppleScript read is represented as unavailable, not as an empty window list.

## Event correlation

Grinch writes `ts` immediately before launch. For each log event, the monitor chooses the newest successful snapshot at or before that timestamp, then gathers post-event snapshots near 250, 750, and 1500 milliseconds. Window ID set and index comparisons classify created, removed, and reordered windows. The monitor also tracks how long Chrome has been backgrounded and when each window was last index 1, providing evidence for minimized/odd-mode and stale-last-active hypotheses.

## Output and privacy

Routine reuse emits one concise line with hostname, rule, strategy, and window count transition. A created-window event emits a diagnostic block containing the correlated pre-state, timed post-states, ID-set changes, and candidate existing windows. URLs are reduced to hostnames. Titles, URL paths, query strings, fragments, and tab contents are never sampled or emitted.

An optional `--raw-jsonl` path records the same privacy-safe observations and analyses for later fixture replay. It contains hostnames but not source URLs.

## Failure handling

Missing Chrome, Apple Event refusal, malformed log lines, and interrupted tailing do not become zero-window samples. Unavailable observations are explicit and excluded from deltas. The shell wrapper retains its existing errors for missing Grinch and missing request logging.

## Testing

Use Python's standard-library `unittest` so the diagnostic adds no dependency. Unit and fixture tests cover AppleScript snapshot parsing, unavailable reads, event correlation, stable-ID deltas, reordering, hostname-only redaction, and the prior fabricated-zero regression. A shell smoke test injects fake discovery commands and verifies that the wrapper delegates to Python with the resolved log. Existing Rust tests and ShellCheck remain final regression gates.
