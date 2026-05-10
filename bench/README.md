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
