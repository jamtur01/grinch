#!/bin/bash
# Follow URL shorteners (bit.ly, t.co, goo.gl, lnkd.in, etc.) and hand the
# final destination to the system's default browser handler — typically
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

# `curl --location` follows redirects; `--head` keeps it cheap (no body).
# `--write-out '%{url_effective}'` prints the final URL after all redirects
# resolved (or the original URL if none did). `--max-time 5` bounds the worst
# case so a slow shortener can't wedge the script.
#
# `--` terminates options so a URL that begins with `-` (rare but possible
# from the share-sheet) doesn't get mis-parsed by curl.
final=$(
    curl \
        --silent \
        --location \
        --head \
        --output /dev/null \
        --max-time 5 \
        --user-agent 'Mozilla/5.0 (compatible; grinch-expander)' \
        --write-out '%{url_effective}' \
        -- "$url" 2>/dev/null
) || final="$url"

# Always opens via the system default browser, which is presumably Grinch.
# Grinch then sees the expanded URL and routes it through your normal rules
# — the shortener host never reaches your match logic.
open -- "$final"
