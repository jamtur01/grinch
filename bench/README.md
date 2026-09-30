# Grinch perf workloads

Standardised configs for measuring `engine::resolve()` cost on the same
URL set every time, so the README's perf numbers can be reproduced and
regressions are visible.

```
bench/
├── README.md         (this file)
├── run.sh            (driver — emits a markdown table)
└── configs/
    ├── 01-floor.grinch.js
    ├── 02-default.grinch.js
    ├── 03-strip.grinch.js
    ├── 04-bare.grinch.js
    ├── 05-domain.grinch.js
    ├── 06-regex.grinch.js
    ├── 07-wildcard.grinch.js          # 01–07: declarative-only (hot path)
    ├── 08-slow-native.grinch.js
    ├── 09-slow-dynopen.grinch.js
    ├── 10-slow-early.grinch.js
    ├── 11-slow-drop.grinch.js
    ├── 12-slow-proto.grinch.js
    └── 13-slow-slack.grinch.js        # 08–13: fn-based (slow path)
```

Each config carries the URL to drive `--bench` with, and an iteration
count, in its header comment:

```js
// URL: https://example.com/no/match
// Iterations: 200000
```

`run.sh` reads those, runs `Grinch --bench` ten times per workload, and
reports the median `ns/op`.

## Running

```sh
bench/run.sh           # full workload set (about 90 seconds)
bench/run.sh hot       # just the declarative-only set (~30s)
bench/run.sh slow      # just the fn-based set (~60s)
```

The script rebuilds `target/release/Grinch` if it's missing or older
than any source file, then stages each config under a private
`HOME=$(mktemp -d)` so it doesn't disturb your real `~/.grinch.js`.

The ignored ingress benchmark includes sender lookup through
`NSRunningApplication`, modifier capture, and resolution. Run it alone on a
logged-in Mac with Finder running; it never launches a browser:

```sh
cargo test --release --bin Grinch benchmark_ingress -- --ignored --nocapture --test-threads=1
```

It reports the median of ten 2,000-operation samples after warmup, using
`17-dynamic-default` and `20-from`. The regular `--bench` workloads use a
synthetic opener, so they do not measure savings from avoiding native lookups.
`18-rewrite-chain` covers mutable URL callbacks followed by a function matcher.

## macOS 27 review measurements (2026-09-30)

Apple M4 Max, macOS 27.0 (26A428), Xcode 27.0 (27A266a), Rust/Cargo 1.96.0,
release builds. The same compiler and machine were used for every comparison.
Measurements exclude browser launches, Apple Event delivery, and cold-start latency.

Native ingress compares `35b589f` (correctness fixes, before the two native
lookup optimizations) with `03e1178`. Three alternating before/after process
pairs each ran ten samples of 2,000 operations after warmup. These are the
medians of those three process medians:

| Native ingress workload | Before, µs/op | After, µs/op | Reduction |
|---|---:|---:|---:|
| URL-only dynamic default | 91.60 | 1.62 | 98.2% |
| Declarative `from()` with a Finder sender PID | 85.14 | 60.07 | 29.4% |

The dynamic default avoids the opener and modifier lookup entirely. `from()`
still identifies the sender, but does not fetch its unused name and path.
These gains depend on native lookup latency; the synthetic-opener benchmarks
below cannot measure them.

The complete existing engine suite compares the original `37cbe81` with
`03e1178`, using the fixture iteration counts and ten runs per workload.
As in `run.sh`, each ten-run median is the fifth ordered sample.

| Engine workload | Original, ns/op | After fixes, ns/op |
|---|---:|---:|
| floor | 6.7 | 6.6 |
| default | 67.7 | 64.2 |
| strip | 185.1 | 179.5 |
| bare | 43.9 | 39.6 |
| domain | 50.5 | 46.1 |
| regex | 27.5 | 23.1 |
| wildcard | 32.1 | 31.6 |
| many-rules | 279.3 | 268.1 |
| rewrite-then-domain | 253.6 | 239.2 |
| slow-native | 5174.2 | 5176.8 |
| slow-dynopen | 4475.6 | 4539.8 |
| slow-early | 42.9 | 40.1 |
| slow-drop | 2412.5 | 2422.9 |
| slow-proto | 3858.2 | 3955.6 |
| slow-slack | 5817.6 | 5839.3 |
| slow-urlonly | 2635.5 | 2669.3 |

Reload medians were 437.82 → 466.23 µs for the floor config and
622.05 → 624.19 µs for many-rules. These are separate process runs, so
small differences include system and JIT variation.

An additional ten alternating original/final pairs checked the matcher
batching changes with 100,000 operations per run:

| Four-function batch | Original, µs/op | After fixes, µs/op |
|---|---:|---:|
| URL + context | 5.277 | 5.454 |
| URL only | 2.601 | 2.649 |

The measured increases were 0.177 µs (3.4%) and 0.048 µs (1.8%). The
dispatcher binds receivers once at config load and retains one bridge
crossing per batch while preserving each callback's argument count.

New engine-only fixtures measured 2.251 µs for dynamic-default,
14.615 µs for rewrite-chain, and 8.5 ns for from.
