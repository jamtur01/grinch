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
sample, the lower median. Routing benchmarks drain an autorelease pool every 1,000 operations.
Routing times use nanoseconds per operation (`ns/op`);
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
| `19-dynamic-object` | A dynamic browser target returned as an object |
| `20-from` | Matching a synthetic opener's bundle ID |
| `21-logged` | Routing with JSONL serialization and file writes |

Declarative workloads never cross the JavaScript bridge. When no rewrite fires,
the resolve borrows the input URL string (`Cow::Borrowed`) throughout;
`quick_host` is skipped when the config has no host-using matcher and borrows
the host slice when it is already lowercase ASCII.

The config-loading benchmark uses `01-floor` and `14-many-rules`, with 300
loads per sample. It includes JavaScriptCore setup, config evaluation, and rule
compilation.

To measure selectable Firefox profile lookup without touching a real profile,
create a temporary profile group and run the config-loading benchmark:

```sh
stage=$(mktemp -d)
firefox="$stage/Library/Application Support/Firefox"
mkdir -p "$firefox/Profile Groups" "$firefox/Profiles/work"
printf '[Profile0]\nName=Legacy\nStoreID=Group1\n' > "$firefox/profiles.ini"
sqlite3 "$firefox/Profile Groups/Group1.sqlite" \
  "CREATE TABLE Profiles(id INTEGER, name TEXT, path TEXT);
   INSERT INTO Profiles VALUES (1, 'Work', 'Profiles/work');"
printf '%s\n' 'module.exports = { default: { name: "org.mozilla.firefox", profile: "Work" } };' \
  > "$stage/.grinch.js"
for i in $(seq 1 10); do
  HOME="$stage" target/release/Grinch --bench-init 30
done
```

This measures cached data being cleared and the group database being reread on
each reload. The temporary directory can be removed after the test.

## Results

Representative release-build measurements on Apple Silicon:

| Routing workload | ns/op |
|---|---:|
| floor | 6.3 |
| default | 62.8 |
| strip | 182.2 |
| bare | 38.4 |
| domain | 45.5 |
| regex | 23.2 |
| wildcard | 31.4 |
| many-rules | 263.3 |
| rewrite-then-domain | 242.9 |
| slow-native | 4717.1 |
| slow-dynopen | 3855.3 |
| slow-early | 39.2 |
| slow-drop | 2143.9 |
| slow-proto | 3395.6 |
| slow-slack | 4194.8 |
| slow-urlonly | 2363.0 |
| dynamic-default | 1959.4 |
| dynamic-object | 6480.2 |
| rewrite-chain | 10095.9 |
| from | 8.4 |
| logged | 6304.1 |

| Config loading | µs/op |
|---|---:|
| floor | 448.65 |
| many-rules | 714.60 |
| Firefox selectable profile | 560.11 |

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
