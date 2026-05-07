# Contributing to Grinch

Thanks for the interest. Grinch is a small, single-maintainer project, so
response times can be variable — issues and PRs are welcome but please
read the rest of this file first so we don't waste each other's time.

By participating you agree to the [Code of Conduct](CODE_OF_CONDUCT.md).

## Reporting bugs and asking for features

Use the issue templates under [`.github/ISSUE_TEMPLATE/`](.github/ISSUE_TEMPLATE):

- [Bug report](.github/ISSUE_TEMPLATE/bug_report.md) — for things that
  don't work the way they're documented. Please include the URL Grinch
  was given, the rule you expected to fire, and the output of `Grinch
  --test "<that-url>"` so we can see how the engine actually resolved
  it. If routing depends on opener / modifiers, also run with
  `GRINCH_DEBUG=1` and paste the resolve trace.
- [Feature request](.github/ISSUE_TEMPLATE/feature_request.md) — for
  new behaviours. Please describe the routing problem you're trying to
  solve before sketching the API; sometimes there's already a way.

For Finicky-compatibility questions, check `examples/grinch.example.js`
first — every supported syntax form has at least one example there.

## Local development

### Prerequisites

- macOS (Apple Silicon or Intel; CI builds a universal binary)
- Xcode command-line tools (`xcode-select --install`)
- Rust stable via `rustup` (Cargo's `rust-toolchain` defaults will pick
  this up)

The release toolchain (signing, notarisation, DMG packaging) is only
needed if you're cutting an actual release — day-to-day development
just needs `cargo`.

### Building and running

The Makefile drives the per-arch build, app-bundle assembly, and the
DMG packaging. For a quick dev cycle you usually want:

```sh
cargo build --release           # builds target/release/Grinch (current arch)
make build                      # same plus assembles Grinch.app
make build UNIVERSAL=1          # universal arm64+x86_64, for releases
```

There are two CLI modes that bypass the menu-bar app, useful while
iterating:

```sh
./target/release/Grinch --test "https://github.com/jamtur01/grinch"
./target/release/Grinch --bench 100000 "https://example.com/?utm_source=x"
```

Both load whichever config exists at `~/.grinch.js` or
`~/.config/grinch.js`. To exercise a different config without touching
your real one, stage it under a temp `HOME`:

```sh
mkdir -p /tmp/scratch && cp examples/grinch.example.js /tmp/scratch/.grinch.js
HOME=/tmp/scratch ./target/release/Grinch --test "https://x.example/"
```

### Tests

```sh
cargo test --release --bin Grinch
```

CI runs the same command on every push and PR (see
[`.github/workflows/ci.yml`](.github/workflows/ci.yml)). All 39 tests
should pass on a clean main; if a change of yours breaks one, please
either fix it or include a paragraph in the PR explaining why the test
was wrong.

New tests should target real bugs or real behaviour, not implementation
details. The bar is "would removing this test let a real bug ship?".
Look at `src/engine.rs::tests` and `src/chromium.rs::tests` for the
existing style.

### Benchmarks

```sh
bench/run.sh                    # all 13 workloads (~90 seconds)
bench/run.sh hot                # declarative-only (fast)
bench/run.sh slow               # fn-based (slower)
```

The harness lives in [`bench/`](bench/) — see [`bench/README.md`](bench/README.md)
for the full layout. If you're touching `engine::resolve` or anything
in the prelude (`src/helpers.rs`), please include before/after numbers
in your PR. Real fix to point at: the existing `bench/configs/*` cover
both hot and slow paths; add a new fixture if your change exercises a
workload none of them hit.

### Style

```sh
cargo fmt --all                         # rustfmt before committing
cargo clippy --release --all-targets -- -D warnings
```

CI enforces both. Code style notes that aren't auto-enforced:

- Comments should explain **why**, not what. The code already shows the
  what; doc comments describe non-obvious invariants and trade-offs.
- Rust naming is `snake_case` for functions, `PascalCase` for types.
- Avoid panics outside `expect("…")` for conditions that genuinely
  can't happen given upstream invariants. If you find yourself reaching
  for `unwrap()` on user-controlled input, return a `Result` instead.
- The engine is intentionally not `Send`/`Sync` — keep it that way. The
  resolve loop is single-threaded by design (see the comment on
  `Engine`); cross-thread requests would deadlock against the main run
  loop anyway.

### Commit messages

- Subject ≤ 72 chars, imperative mood.
- Body explains the why, including bench deltas if you touched the hot
  or slow path. The release-notes generator
  ([`scripts/release-notes.sh`](scripts/release-notes.sh)) categorises
  on subject prefix:
  - `Fix …` — bug fixes
  - `Slow path …`, `P1+P2: …`, `Perf: …` — performance work
  - `Add tests …` — test-only changes
  - `Document …`, `Refresh …` — docs
  - everything else falls through to "Other"

  Picking a known prefix when one fits keeps the auto-generated release
  notes tidy.
- One logical change per commit. Mass formatting passes go in their own
  commit so review diffs aren't drowned out.

### Pull requests

Small PRs are easier to review and ship. If you're sending a non-trivial
change, please open an issue first — most rejected PRs are work that
duplicated something the maintainer was already doing or didn't fit the
project's scope.

The PR description should describe what's in the diff *now*, not the
journey to get there. If you're fixing a bug, link the issue; if you're
adding a feature, point at the user need.

## A few things specific to the engine

If your change touches `src/engine.rs`, two non-obvious invariants are
worth knowing:

1. **The fn-arity ctx-passing contract.** User fns receive `ctx` only
   when they declare two-or-more formal parameters (`f.length >= 2`).
   This lets the engine skip building `ctx` *and* skip the
   LaunchServices opener IPC for url-only configs, but it means
   patterns that reach ctx via `arguments[1]`, rest params, or the JS
   default-param fallback (`(url, ctx = {}) => …`) silently see no
   ctx. The grimmest of these is detected at config load and warned
   about — see `warn_if_fn_might_read_ctx` in `src/engine.rs`. If you
   change this contract, update the docstring on `UserFn` and the
   warning message together.
2. **Three runtime-needs flags compute at config load**: `needs_opener`,
   `needs_modifiers`, `needs_host`. They drive whether AppDelegate runs
   `frontmost_opener()` / `current_modifier_flags()` per click and
   whether `resolve()` calls `quick_host`. If you add a matcher kind
   that reads any of these, update `analyse_runtime_needs` accordingly
   — getting it wrong is the kind of bug that's silent (a `from()`
   matcher that always fails because the opener is `Opener::default()`).

## Releasing (maintainer notes)

These are the steps to cut a release; they don't affect contributors
but they live here for the maintainer's reference.

1. Bump `version` in `Cargo.toml`. Run `cargo build --release` so the
   lockfile picks it up.
2. Commit + push to `main`. Wait for CI to go green.
3. `git tag -s -a vX.Y.Z -m "vX.Y.Z"` and `git push origin vX.Y.Z`.
4. The release workflow ([`.github/workflows/release.yml`](.github/workflows/release.yml))
   builds a universal `Grinch.app`, signs and notarises it, packages
   the DMG, generates release notes from `git log` between this tag and
   the previous `v*` tag, and uploads everything to the GitHub release.
