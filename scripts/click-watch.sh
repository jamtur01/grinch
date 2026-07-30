#!/bin/bash
# Watch the *running* Grinch instance's request log and record the Chrome
# window-count delta around each routed click. When the count increases, a NEW
# window was created — the line shows which launch strategy caused it, so the
# intermittent "profile link opened a new window instead of the existing one"
# behaviour can be correlated with what Grinch actually did.
#
# Only the hostname is printed (URL path, query and fragment are omitted) so
# auth tokens / magic-links in routed URLs never reach the terminal.
#
# The log file is resolved from the running Grinch's open file descriptor via
# `lsof`, NOT by newest-mtime guessing — a previous run's log can be newer by
# mtime and would send you chasing a stale file.
#
# Requires options.logRequests: true in your Grinch config. Ctrl-C to stop.
set -euo pipefail

pid="$(pgrep -f 'Grinch.app/Contents/MacOS/Grinch' | head -1 || true)"
if [ -z "${pid}" ]; then
	echo "Grinch is not running." >&2
	exit 1
fi

log="$(lsof -p "${pid}" 2>/dev/null |
	grep -oE '/Users/[^[:space:]]+/Library/Logs/Grinch/[^[:space:]]+\.log' |
	head -1 || true)"
if [ -z "${log}" ]; then
	echo "Grinch (pid ${pid}) has no request log open." >&2
	echo "Set options.logRequests: true in your config, route one link, re-run." >&2
	exit 1
fi
echo "watching: ${log}  (Grinch pid ${pid})"

chrome_windows() {
	osascript -e 'tell application "Google Chrome" to count windows' 2>/dev/null || echo 0
}

prev="$(chrome_windows)"
tail -n0 -F "${log}" | while read -r line; do
	sleep 1
	now="$(chrome_windows)"
	delta=$((now - prev))
	info="$(printf '%s' "${line}" | python3 -c '
import sys, json, urllib.parse as u
d = json.loads(sys.stdin.read())
host = u.urlparse(d.get("final", "")).hostname or "?"
rule = (d.get("matchedRule") or {}).get("name", "none") if d.get("matchedRule") else "none"
print(d["opener"].get("name", "?"), "->", d.get("strategy", "?"), "| host:", host, "| rule:", rule)
' 2>/dev/null || echo '(unparseable log line)')"
	flag=""
	if [ "${delta}" -gt 0 ]; then
		flag="   <<< NEW WINDOW (+${delta})"
	fi
	echo "[win ${prev}->${now}]${flag}  ${info}"
	prev="${now}"
done
