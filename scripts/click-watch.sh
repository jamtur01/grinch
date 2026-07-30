#!/bin/bash
# Watch the *running* Grinch instance's request log and record the Chrome
# window-count delta around each routed click. When the count increases, a NEW
# window was created — the line shows which launch strategy caused it, so the
# intermittent "profile link opened a new window instead of the existing one"
# behaviour can be correlated with what Grinch actually did.
#
# Only the hostname is printed (URL path, query and fragment omitted) so auth
# tokens / magic-links in routed URLs never reach the terminal.
#
# The log file is resolved from the running Grinch's open file descriptor via
# `lsof`, NOT by newest-mtime guessing — a previous run's log can be newer by
# mtime and would send you chasing a stale file.
#
# CAVEAT: the window count is sampled shortly after each log line via
# AppleScript, which Chrome can briefly refuse right after a launch. Such a
# read yields "?" (count unavailable) — NEVER a fabricated 0 — and no delta is
# computed against it. Deltas remain a heuristic: concurrent clicks or manual
# window changes also move the count, so trust a "+N" most when you made a
# single click and weren't otherwise touching Chrome.
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

# Echo the Chrome window count, or nothing (and return non-zero) on failure.
# Retries because Chrome is briefly unresponsive to Apple Events after a
# launch; the old `|| echo 0` fallback turned those failures into bogus 0s
# that fabricated +N "new window" spikes.
chrome_windows() {
	local n i
	for i in 1 2 3; do
		n="$(osascript -e 'tell application "Google Chrome" to count windows' 2>/dev/null || true)"
		case "${n}" in
		'' | *[!0-9]*) sleep 0.3 ;;
		*)
			printf '%s' "${n}"
			return 0
			;;
		esac
	done
	return 1
}

prev="$(chrome_windows || true)"
tail -n0 -F "${log}" | while read -r line; do
	sleep 1
	info="$(printf '%s' "${line}" | python3 -c '
import sys, json, urllib.parse as u
d = json.loads(sys.stdin.read())
host = u.urlparse(d.get("final", "")).hostname or "?"
rule = (d.get("matchedRule") or {}).get("name", "none") if d.get("matchedRule") else "none"
print(d["opener"].get("name", "?"), "->", d.get("strategy", "?"), "| host:", host, "| rule:", rule)
' 2>/dev/null || echo '(unparseable log line)')"
	if now="$(chrome_windows)"; then
		flag=""
		if [ -n "${prev}" ] && [ "${now}" -gt "${prev}" ]; then
			flag="   <<< NEW WINDOW (+$((now - prev)))"
		fi
		echo "[win ${prev:-?}->${now}]${flag}  ${info}"
		prev="${now}"
	else
		# Count unavailable: report it, don't fabricate a delta, keep prev.
		echo "[win ${prev:-?}->? (count unavailable)]  ${info}"
	fi
done
