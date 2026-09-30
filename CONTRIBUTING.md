# Contributing to Grinch

Thanks for the interest. Grinch is a small, single-maintainer project, so
response times can be variable — issues and PRs are welcome but please
read the rest of this file first so we don't waste each other's time.

By participating you agree to the [Code of Conduct](CODE_OF_CONDUCT.md).

## Reporting bugs and asking for features

Only the [latest published release](https://github.com/jamtur01/grinch/releases/latest)
is supported for issues and security fixes. Upgrade and reproduce the problem
before filing a bug. Older releases do not receive backported fixes.

Report suspected vulnerabilities privately using [SECURITY.md](SECURITY.md),
not a public issue or pull request. Redact credentials, sign-in links, personal
data, and local paths from configs and diagnostic logs before sharing them.

Use the issue templates under [`.github/ISSUE_TEMPLATE/`](.github/ISSUE_TEMPLATE):

- [Bug report](.github/ISSUE_TEMPLATE/bug_report.md) — for things that
  don't work the way they're documented. Include your Grinch and macOS
  versions, browser/version, a minimal config, a sanitized example URL,
  and the rule you expected to fire. Attach relevant events from **Open
  Diagnostic Log**; enable `options.logRequests` to include routing decisions.
  `launch_error` records launch failures even when request logging is off.
  `Grinch --test "<example-url>"` helps check routing without launching a browser;
  include the originating app and modifier keys when they affect the result.
- [Feature request](.github/ISSUE_TEMPLATE/feature_request.md) — for
  new behaviours. Please describe the routing problem you're trying to
  solve before sketching the API; sometimes there's already a way.

For Finicky-compatibility questions, check `examples/grinch.example.js`
first — every supported syntax form has at least one example there.

## Local development

### Prerequisites

- macOS (Apple Silicon or Intel; CI builds a universal binary)
- Xcode command-line tools (`xcode-select --install`)
- Rust stable via `rustup`, with the `clippy` and `rustfmt` components

Use rustup's tools consistently, including Cargo subcommands:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
rustup update stable
rustup component add clippy rustfmt --toolchain stable
rustc +stable --version
cargo +stable clippy --version
```

Keep that PATH order for builds and checks. A Homebrew `rustc` or
`cargo-clippy` earlier on PATH can run a different version even when Cargo
was selected with `rustup run`. CI uses stable Rust with rustup first on PATH.

The release toolchain (signing, notarisation, DMG packaging) is only
needed if you're cutting an actual release — day-to-day development
just needs `cargo`.

### Pre-commit hook

The repo ships a `.githooks/pre-commit` that runs `cargo fmt --all --
--check` on commits that touch Rust files. Activate it once per clone:

```sh
git config core.hooksPath .githooks
```

If the hook rejects a commit, run `cargo fmt --all`, re-stage, and try
again. CI runs the same check, so this keeps push-and-CI-fail loops
out of the workflow.

### Building and running

The Makefile drives the per-arch build, app-bundle assembly, and the
DMG packaging. For a quick dev cycle you usually want:

```sh
cargo build --release           # builds target/release/Grinch (current arch)
make build                      # same plus assembles Grinch.app
make build UNIVERSAL=1          # universal arm64+x86_64, for releases
```

CLI modes bypass the menu-bar app and are useful while iterating:

```sh
./target/release/Grinch --test "https://github.com/jamtur01/grinch"
./target/release/Grinch --bench 100000 "https://example.com/?utm_source=x"
```

Both load whichever config exists at `~/.grinch.js`,
`~/.config/grinch.js`, `~/.config/grinch/grinch.js`, or
`/Library/Application Support/Grinch/grinch.js` (the last is system-wide /
MDM, checked last). To exercise a different config without touching your
real one, stage it under a temp `HOME`:

```sh
mkdir -p /tmp/scratch && cp examples/grinch.example.js /tmp/scratch/.grinch.js
HOME=/tmp/scratch ./target/release/Grinch --test "https://x.example/"
```

### Tests

```sh
cargo test --release --bin Grinch
```

CI runs the same command on every push and PR (see
[`.github/workflows/ci.yml`](.github/workflows/ci.yml)). The full suite
should pass on a clean main; if a change of yours breaks one, please
either fix it or include a paragraph in the PR explaining why the test
was wrong.

New tests should target real bugs or real behaviour, not implementation
details. The bar is "would removing this test let a real bug ship?".
Look at `src/engine.rs::tests` and `src/chromium.rs::tests` for the
existing style.

Use `isolated_diagnostics()` for test diagnostics. Tests of default log paths
must use a private temporary HOME; no test should write to a real user's
`~/Library/Logs/Grinch`. Background launch-error tests must share the same
writer and rotation state as the main-thread diagnostics.

### Supply-chain policy

Install the version used by CI, then check the dependency graph against
[`deny.toml`](deny.toml):

```sh
cargo install --locked cargo-deny --version 0.20.2
cargo deny check
```

The policy rejects known advisories, duplicate crate versions, wildcard
dependency requirements, and dependencies outside crates.io. It permits only
the license expressions needed by the current dependency graph.

### Benchmarks

```sh
bench/run.sh                    # full workload set (~90 seconds)
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
2. **Runtime needs are computed at config load**: `needs_opener`,
   `needs_opener_full`, `needs_modifiers`, and `needs_host`. URL-only callbacks,
   including dynamic defaults, skip unused native context. Declarative `from()`
   matchers need only the sender's bundle ID; callbacks with `ctx` need full
   opener data. If you add a matcher that reads context, update
   `analyse_runtime_needs` and the ingress tests so required lookups aren't skipped.

## Releasing (maintainer notes)

These are the steps to cut a release; they don't affect contributors
but they live here for the maintainer's reference.

1. Bump `version` in `Cargo.toml`. Run `cargo build --release` so the
   lockfile picks it up. Update `README.md` and `docs/index.html` to describe
   shipped behavior. Keep change summaries in release notes and support wording
   linked to the latest release.
2. Commit + push to `main`. Wait for CI and CodeQL to go green for that commit.
3. `git tag -s -a vX.Y.Z -m "vX.Y.Z"` and `git push origin vX.Y.Z`.
4. The release workflow ([`.github/workflows/release.yml`](.github/workflows/release.yml))
   builds a universal `Grinch.app`, verifies its bundle version matches the
   tag, signs and notarises it, packages the DMG, generates release notes from
   `git log` between this tag and the previous `v*` tag, and uploads everything
   to the GitHub release.
5. Confirm the workflow succeeded, download the published DMG and `.sha256`,
   and check the checksum and notarization ticket:

   ```sh
   shasum -a 256 -c Grinch-vX.Y.Z.dmg.sha256
   xcrun stapler validate Grinch-vX.Y.Z.dmg
   spctl --assess --type install --verbose Grinch-vX.Y.Z.dmg
   ```
