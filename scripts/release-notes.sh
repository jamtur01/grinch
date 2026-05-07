#!/bin/bash
# Generate markdown release notes for the commits between two tags.
#
# Usage:
#   scripts/release-notes.sh <previous-tag> <current-tag>
#
# If <previous-tag> is "auto", picks the tag immediately before <current-tag>
# from `git tag --sort=v:refname`. The current tag must already exist.
#
# Output goes to stdout; the release workflow captures it into a file and
# hands it to action-gh-release as the release body.

set -euo pipefail

prev="${1:?usage: release-notes.sh <previous-tag>|auto <current-tag>}"
curr="${2:?usage: release-notes.sh <previous-tag>|auto <current-tag>}"

if [ "$prev" = "auto" ]; then
    # Pick the version tag immediately before $curr. Restrict to `v*`-prefix
    # tags so transient/test tags can't poison the lookup, and exclude $curr
    # itself by walking the sorted list and stopping when we hit it.
    prev=$(git tag --list 'v*' --sort=v:refname \
        | awk -v c="$curr" '$0 == c { exit } { last = $0 } END { print last }')
    if [ -z "$prev" ]; then
        echo "::error::no previous v* tag found before $curr" >&2
        exit 1
    fi
fi

# Resolve the repo URL once for the compare link. Falls back to a placeholder
# if the remote is missing (e.g. local dry-run).
remote_url=$(git config --get remote.origin.url 2>/dev/null || echo "")
case "$remote_url" in
    git@github.com:*)
        repo="${remote_url#git@github.com:}"
        repo="${repo%.git}"
        compare="https://github.com/${repo}/compare/${prev}...${curr}"
        ;;
    https://github.com/*)
        repo="${remote_url#https://github.com/}"
        repo="${repo%.git}"
        compare="https://github.com/${repo}/compare/${prev}...${curr}"
        ;;
    *)
        compare=""
        ;;
esac

# Categorise each commit subject into one of these buckets via a simple prefix
# match. Anything that doesn't match a bucket falls through to "Other".
fixes=""
perf=""
tests=""
docs=""
other=""

# `git log --reverse` walks oldest-first so the output reads in the same
# order things landed. `tformat` (vs `format`) terminates the LAST line
# with a newline — `format` doesn't, and `while read` would silently drop
# the final commit.
while IFS= read -r line; do
    sha="${line%% *}"
    subj="${line#* }"
    item="- ${subj} (\`${sha}\`)"
    case "$subj" in
        Fix*|fix:*)
            fixes="${fixes}${item}"$'\n'
            ;;
        "Slow path"*|"P1"*|"P2"*|"P3"*|"P4"*|"P5"*|"P6"*|Perf*|perf*)
            perf="${perf}${item}"$'\n'
            ;;
        "Add tests"*|test:*)
            tests="${tests}${item}"$'\n'
            ;;
        Document*|Refresh*|docs:*)
            docs="${docs}${item}"$'\n'
            ;;
        *)
            other="${other}${item}"$'\n'
            ;;
    esac
done < <(git log --reverse --pretty=tformat:'%h %s' "${prev}..${curr}")

emit_section() {
    local title="$1"
    local body="$2"
    if [ -n "$body" ]; then
        printf '### %s\n\n%s\n' "$title" "$body"
    fi
}

# Header + sections. Each section is omitted when empty so the body stays tight.
printf '## %s\n\n' "$curr"

emit_section "Performance" "$perf"
emit_section "Bug fixes" "$fixes"
emit_section "Tests" "$tests"
emit_section "Docs" "$docs"
emit_section "Other" "$other"

if [ -n "$compare" ]; then
    printf '\n---\n\n**Full changelog**: %s\n' "$compare"
fi
