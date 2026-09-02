#!/bin/bash
# Watch the running Grinch instance's diagnostic log and correlate each routed
# click with continuously sampled Chrome window state. The Python helper keeps
# a pre-event ring buffer and compares stable Chrome window IDs before and after
# launch, which distinguishes real creation from reordering and preserves
# unavailable AppleScript reads instead of fabricating zero windows.
#
# Window/tab titles and URLs are never sampled. Routed URLs are reduced to their
# hostname. Requires options.logRequests: true in the Grinch config. Ctrl-C to
# stop. Optional helper arguments such as `--raw-jsonl PATH` are passed through.
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
	echo "Grinch (pid ${pid}) has no diagnostic log open." >&2
	echo "Set options.logRequests: true in your config, route one link, re-run." >&2
	exit 1
fi

current_space="$(plutil -extract \
	"SpacesDisplayConfiguration.Management Data.Monitors.0.Current Space.ManagedSpaceID" \
	raw -o - "${HOME}/Library/Preferences/com.apple.spaces.plist" 2>/dev/null || echo "?")"
script_dir="$(cd "$(dirname "$0")" && pwd)"

echo "watching: ${log}  (Grinch pid ${pid}, current Space ${current_space})"
exec python3 "${script_dir}/click-watch-monitor.py" --log "${log}" "$@"
