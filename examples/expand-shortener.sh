#!/bin/bash
# Expand known URL shorteners and hand the first destination outside them
# to the system's default browser handler — typically
# Grinch, which then routes it via the rules in ~/.config/grinch.js.
#
# Why this isn't built into Grinch: resolve() is synchronous on purpose so
# every click stays in the microsecond range. Following a redirect is a
# network round-trip (50–500 ms) and would turn click latency into network
# latency. See the "Performance" section of README.md for the rationale.
#
# Usage:
#   expand-shortener.sh "https://bit.ly/xxx"
#
# Hook into your workflow however you already trigger scripts:
#   - Raycast / Alfred: bind to a hotkey, paste URL from clipboard.
#   - Hammerspoon: hs.urlevent.bind("expand", function(_, params) ... end).
#   - Shortcuts.app: wrap as a Quick Action that takes URLs from the share
#     sheet, runs `/path/to/expand-shortener.sh "$1"`.
#   - Plain terminal: `expand-shortener.sh "$(pbpaste)"` after copying.

set -euo pipefail

url="${1:?usage: expand-shortener.sh <url>}"

is_shortener() {
	local authority host
	case "$1" in
	[Hh][Tt][Tt][Pp]://* | [Hh][Tt][Tt][Pp][Ss]://*) ;;
	*) return 1 ;;
	esac
	authority=${1#*://}
	authority=${authority%%[/?#]*}
	host=${authority##*@}
	host=${host%%:*}
	host=$(printf '%s' "$host" | tr '[:upper:]' '[:lower:]')
	host=${host%.}
	case "${host#www.}" in
	bit.ly | t.co | goo.gl | lnkd.in | ow.ly | buff.ly | tinyurl.com) return 0 ;;
	*) return 1 ;;
	esac
}

expand_shortener() {
	local current="$1" next remaining hop deadline=$((SECONDS + 5))
	for ((hop = 0; hop < 10; hop++)); do
		if ! is_shortener "$current"; then
			printf '%s\n' "$current"
			return
		fi
		remaining=$((deadline - SECONDS))
		((remaining > 0)) || return 1
		# Leave destination redirects (and their login cookies) to the browser.
		# curl resolves relative Location values without fetching the next hop.
		next=$(curl --disable --silent --fail --head --globoff --proto '=http,https' \
			--output /dev/null --max-time "$remaining" \
			--user-agent 'Mozilla/5.0 (compatible; grinch-expander)' \
			--write-out '%{redirect_url}' -- "$current" 2>/dev/null) || return 1
		if [[ -z $next ]]; then
			printf '%s\n' "$current"
			return
		fi
		case "$next" in
		[Hh][Tt][Tt][Pp]://* | [Hh][Tt][Tt][Pp][Ss]://*) current=$next ;;
		*) return 1 ;;
		esac
	done
	is_shortener "$current" && return 1
	printf '%s\n' "$current"
}

final=$(expand_shortener "$url") || final="$url"

# Always opens via the system default browser, which is presumably Grinch.
# Grinch sees the destination, or the original URL if expansion fails or
# exceeds ten requests / five seconds. Unknown hosts go straight to the browser.
open -- "$final"
