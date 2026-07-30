#!/bin/bash
# Watch the *running* Grinch instance's request log and record the Chrome
# window-count delta around each routed click. When the count increases, a NEW
# window was created — the line shows which launch strategy caused it, so the
# intermittent "profile link opened a new window instead of the existing one"
# behaviour can be correlated with what Grinch actually did.
#
# On a new-window event it also dumps a snapshot of every Chrome window's state
# (index, tab count, minimized, mode, bounds) plus the current Space id. This
# reveals whether an existing target window was minimized / fullscreen / an odd
# mode when Chrome chose to spawn a new one instead of reusing it. AppleScript
# does NOT expose a window's profile or Space, and window/tab TITLES/URLs are
# deliberately omitted (they leak email subjects, tokens, etc.) — identify your
# Personal window by its tab count and bounds.
#
# The routed URL is reduced to its hostname (path/query/fragment stripped) so
# auth tokens / magic-links never reach the terminal.
#
# The log file is resolved from the running Grinch's open file descriptor via
# `lsof`, NOT by newest-mtime guessing (a previous run's log can be newer).
#
# CAVEAT: window counts are sampled via AppleScript, which Chrome can briefly
# refuse right after a launch; such a read yields "?" (unavailable), NEVER a
# fabricated 0, and no delta is computed against it. Deltas are a heuristic —
# concurrent clicks / manual window changes also move the count.
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

# Current macOS Space id (single value; AppleScript can't map windows to
# Spaces, so per-window Space is deliberately not attempted).
current_space() {
	plutil -extract \
		"SpacesDisplayConfiguration.Management Data.Monitors.0.Current Space.ManagedSpaceID" \
		raw -o - "${HOME}/Library/Preferences/com.apple.spaces.plist" 2>/dev/null || echo "?"
}

echo "watching: ${log}  (Grinch pid ${pid}, current Space $(current_space))"

# Echo the Chrome window count, or nothing (return non-zero) on failure.
# Retries because Chrome is briefly unresponsive to Apple Events after a
# launch; the naive `|| echo 0` fallback fabricated +N "new window" spikes.
chrome_windows() {
	local n
	for _ in 1 2 3; do
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

# Per-window state at a new-window event. profile/Space are NOT available via
# AppleScript, and titles/URLs are omitted (they leak content). minimized /
# mode / tab-count / bounds are what reveal why an existing window was skipped.
snapshot_windows() {
	osascript <<'OSA' 2>/dev/null || echo "  (window snapshot unavailable)"
tell application "Google Chrome"
	set out to ""
	repeat with i from 1 to (count of windows)
		set w to window i
		set b to bounds of w
		set out to out & "  win#" & i & " tabs=" & (count of tabs of w) & ¬
			" min=" & (minimized of w) & " mode=" & (mode of w) & ¬
			" bounds=" & (item 1 of b) & "," & (item 2 of b) & "," & ¬
			(item 3 of b) & "," & (item 4 of b) & linefeed
	end repeat
	return out
end tell
OSA
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
		if [ -n "${prev}" ] && [ "${now}" -gt "${prev}" ]; then
			echo "[win ${prev}->${now}]   <<< NEW WINDOW (+$((now - prev)))  ${info}"
			echo "  --- window state at this event (current Space $(current_space)) ---"
			snapshot_windows
		else
			echo "[win ${prev:-?}->${now}]  ${info}"
		fi
		prev="${now}"
	else
		echo "[win ${prev:-?}->? (count unavailable)]  ${info}"
	fi
done
