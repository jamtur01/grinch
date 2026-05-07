#!/bin/bash
# Run the standard Grinch perf workloads and emit a markdown table.
#
# Each fixture is `bench/configs/NN-name.grinch.js` with a header comment
# specifying the URL to drive resolve() with and the iteration count.
# Median of 10 runs is reported per workload.
#
# Usage:
#   bench/run.sh              # all workloads
#   bench/run.sh hot          # only declarative-only configs (01–07)
#   bench/run.sh slow         # only fn-based configs (08–13)
#
# Requires a release binary at target/release/Grinch — script will rebuild
# if missing or older than any source file.

set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
binary="$repo_root/target/release/Grinch"
configs_dir="$repo_root/bench/configs"

# Rebuild if needed.
if [[ ! -x "$binary" ]] || [[ -n "$(find "$repo_root/src" -newer "$binary" -name '*.rs' 2>/dev/null)" ]]; then
    echo "# building release binary..." >&2
    (cd "$repo_root" && cargo build --release 2>&1 | tail -3 >&2)
fi

# Extract a metadata field from a config's header comment.
#   read_meta <file> URL          → first "// URL: ..." line
#   read_meta <file> Iterations   → first "// Iterations: ..." line
read_meta() {
    local file="$1"
    local key="$2"
    awk -v k="$key" '$0 ~ "^// " k ":" { sub("^// " k ": *", ""); print; exit }' "$file"
}

# Pretty workload label = filename without prefix and extension.
#   01-floor.grinch.js → "floor"
label_of() {
    local name="$(basename "$1" .grinch.js)"
    echo "${name#[0-9][0-9]-}"
}

# Bench one workload, print median ns/op.
bench_one() {
    local cfg_file="$1"
    local iters="$2"
    local url="$3"
    # Stage the config under a private HOME so the loader picks it up.
    local stage; stage="$(mktemp -d)"
    cp "$cfg_file" "$stage/.grinch.js"
    local results=()
    for _ in $(seq 1 10); do
        results+=("$(HOME="$stage" "$binary" --bench "$iters" "$url" \
            | awk '/Per-op/ { gsub(/ns/, ""); print $2 }')")
    done
    rm -rf "$stage"
    printf "%s\n" "${results[@]}" | sort -n | sed -n '5p'
}

filter="${1:-all}"

# Markdown table header.
case "$filter" in
    hot|all)
        printf '\n## Hot path (declarative-only configs)\n\n'
        printf '| Workload | ns/op |\n|---|---:|\n'
        for cfg in "$configs_dir"/0[1-7]-*.grinch.js; do
            iters="$(read_meta "$cfg" Iterations)"
            url="$(read_meta "$cfg" URL)"
            label="$(label_of "$cfg")"
            ns="$(bench_one "$cfg" "$iters" "$url")"
            printf '| %s | %s |\n' "$label" "$ns"
        done
        ;;
esac

case "$filter" in
    slow|all)
        printf '\n## Slow path (configs with `(url, ctx) => …` fn matchers)\n\n'
        printf '| Workload | ns/op |\n|---|---:|\n'
        for cfg in "$configs_dir"/0[89]-*.grinch.js "$configs_dir"/1[0-3]-*.grinch.js; do
            iters="$(read_meta "$cfg" Iterations)"
            url="$(read_meta "$cfg" URL)"
            label="$(label_of "$cfg")"
            ns="$(bench_one "$cfg" "$iters" "$url")"
            printf '| %s | %s |\n' "$label" "$ns"
        done
        ;;
esac
