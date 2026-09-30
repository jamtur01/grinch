# Grinch benchmarks

Measure URL routing and config-loading costs with repeatable workloads.
The benchmarks do not launch browsers or measure page loading.

## Running

Run from the repository root on macOS:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release
bench/run.sh           # all workloads
bench/run.sh hot       # declarative routing, including the logging workload
bench/run.sh slow      # JavaScript callbacks
bench/run.sh init      # config loading
```

The runner stages each config under a temporary HOME, keeping your config and
logs untouched. It runs each workload ten times and reports the fifth sorted
sample, the lower median. Routing times use nanoseconds per operation (`ns/op`);
config-loading times use microseconds per operation (`µs/op`).

## Workloads

Configs live in [`configs/`](configs/). Each supplies its input URL and iteration
count in header comments:

```js
// URL: https://example.com/no/match
// Iterations: 200000
```

| Configs | What they measure |
|---|---|
| `01`–`07`, `14`–`15` | Declarative matching, parameter stripping, and rule count |
| `08`–`13`, `16` | JavaScript matchers and rewrites, with and without context |
| `17-dynamic-default` | A URL-only function used as the default browser target |
| `18-rewrite-chain` | Mutable URL rewrites followed by a function matcher |
| `20-from` | Matching a synthetic opener's bundle ID |
| `21-logged` | Routing with JSONL serialization and file writes |

The config-loading benchmark uses `01-floor` and `14-many-rules`, with 300
loads per sample. It includes JavaScriptCore setup, config evaluation, and rule
compilation.

## Results

Representative release-build measurements on Apple Silicon:

| Routing workload | ns/op |
|---|---:|
| floor | 6.6 |
| default | 64.2 |
| strip | 179.5 |
| bare | 39.6 |
| domain | 46.1 |
| regex | 23.1 |
| wildcard | 31.6 |
| many-rules | 268.1 |
| rewrite-then-domain | 239.2 |
| slow-native | 5454 |
| slow-dynopen | 4539.8 |
| slow-early | 40.1 |
| slow-drop | 2422.9 |
| slow-proto | 3955.6 |
| slow-slack | 5839.3 |
| slow-urlonly | 2669.3 |
| dynamic-default | 2251 |
| rewrite-chain | 14615 |
| from | 8.5 |
| logged | 8275.4 |

| Config loading | µs/op |
|---|---:|
| floor | 466.23 |
| many-rules | 624.19 |

## Including macOS lookups

The regular routing workloads use a synthetic opener. To include the native
sender and modifier lookups required by a config, run this benchmark alone on a
logged-in Mac with Finder running:

```sh
cargo test --release --bin Grinch benchmark_ingress -- --ignored --nocapture --test-threads=1
```

It reports the median of ten 2,000-operation samples after warmup, using
`17-dynamic-default` and `20-from`. It excludes Apple Event delivery and browser
launches.

| Workload | µs/op |
|---|---:|
| URL-only dynamic default | 1.62 |
| Declarative `from()` with a Finder sender PID | 60.07 |

For comparisons, use the same machine, compiler, configs, and iteration counts.
Alternate builds between runs to reduce the effect of changing system load.
Small differences can reflect system load or JavaScriptCore warmup.
