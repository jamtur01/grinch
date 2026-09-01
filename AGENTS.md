# Grinch agent orientation

Grinch is a native macOS browser router: a single Rust binary evaluates a
JavaScript config, resolves each URL to a `BrowserSpec`, and executes a native
`LaunchPlan` through AppKit and LaunchServices.

This file is an index, not a second contributor guide. Global development
standards still apply. Follow the references below before changing behavior,
and keep durable rationale in committed files rather than private scratch
notes.

## Read before changing

### Config syntax or routing semantics

Read these in order:

1. `README.md` sections **Configuration**, **Performance**, and
   **Differences from Finicky**.
2. `examples/grinch.example.js`, the user-facing contract.
3. `CONTRIBUTING.md` section **A few things specific to the engine**.

Config is compiled at load time. Marker helpers such as `domain()`, `from()`,
`running()`, and `strip()` must remain native fast paths; user-authored
JavaScript functions are the explicit slow path.

### Resolver or JavaScript bridge

Read `src/engine.rs`, the relevant module under `src/engine/`, and the matching
tests in `src/engine/integration_tests.rs`. Preserve the runtime-needs analysis,
first-match-wins behavior, ordered rewrites, and the explicit function-arity
contract for passing `ctx`.

Measure performance-sensitive changes with `bench/run.sh` and an existing or
new fixture under `bench/configs/`. Do not infer hot-path cost from wall-clock
browser launches.

### Browser discovery or launch behavior

Read `src/engine/spec.rs`, `src/workspace.rs`, `src/chromium.rs`, and
`src/firefox.rs`. Keep browser-family metadata centralized in the latter two
modules. Launches must continue through `BrowserSpec` and the pure `LaunchPlan`
boundary; do not add a separate direct-executable path.

### URL delivery, menu bar, or app lifecycle

Read `src/app_delegate.rs` and `src/main.rs`. The app delegate owns config
reloads, GURL and `application:openURLs:` delivery, menu state, opener capture,
and dispatch into the workspace layer. Failed reloads leave the previous engine
active.

### SSO, OAuth, or bundle registration

Read `README.md` section **SSO / OAuth popups**, `src/session_handler.rs`, and
the explanatory comments in `Info.plist`. AuthenticationServices callbacks
share the normal routing engine and have separate delivery timing from ordinary
URL events.

### App identity or packaging

Read `brand/`, the icon targets in `Makefile`, and `Info.plist`. The source
iconset uses macOS `@2x` filenames; changing them can make `iconutil` silently
omit Retina representations. The menu-bar image is a template asset so AppKit
can tint it for the current appearance.

### Release process

Read `CONTRIBUTING.md` section **Releasing**, `Makefile`,
`.github/workflows/release.yml`, and `scripts/release-notes.sh`. `Cargo.toml` is
the version source of truth. The release workflow builds, signs, notarizes,
packages, verifies, and publishes the universal app.

## Source map

- `src/main.rs`: process entry, version handling, AppKit startup.
- `src/app_delegate.rs`: runtime orchestration, URL ingress, menu bar, reloads.
- `src/loader.rs`: config path selection and JavaScript evaluation.
- `src/helpers.rs`: embedded JavaScript prelude and marker helpers.
- `src/engine.rs`: compiled routing state and resolve loop.
- `src/engine/compile.rs`: JavaScript config to native matchers and targets.
- `src/engine/rewrite.rs`: native URL transformations and link unwrapping.
- `src/engine/spec.rs`: browser-spec parsing and launch-plan construction.
- `src/engine/jsbridge.rs`: JavaScriptCore and Objective-C bridge functions.
- `src/engine/logging.rs`: optional per-resolve JSONL logging and rotation.
- `src/workspace.rs`: LaunchServices, app discovery, opener data, launches.
- `src/chromium.rs`, `src/firefox.rs`: browser-family profiles and metadata.
- `src/session_handler.rs`: AuthenticationServices requests and callbacks.
- `bench/`: reproducible routing benchmarks.
- `scripts/`: release and browser-window diagnostic tooling.
- `docs/`: static project site.
- `brand/`: app, menu-bar, README, and social identity sources.

## External state

These inputs are not stored in the repository:

- Live user configs under `~/.grinch.js` and `~/.config/`, plus the system-wide
  config under `/Library/Application Support/Grinch/`.
- The installed `/Applications/Grinch.app` and LaunchServices handler cache.
- Accessibility approval, keyed by the app's signature identity.
- Developer ID and notarization credentials stored as GitHub secrets.

`make build` registers the local `Grinch.app` with LaunchServices. For bundle
verification that must not change registration precedence, override
`LSREGISTER=/usr/bin/true`.

## Verification

Run the checks relevant to the change, with this as the standard Rust path:

```sh
cargo fmt --all -- --check
cargo clippy --release --all-targets -- -D warnings
cargo test --release --bin Grinch
```

For brand changes, run `make icon`, round-trip the result through `iconutil`,
and verify the assembled app contains both `grinchTemplate.png` and
`grinchTemplate@2x.png`. For shell changes, run `shellcheck` and `shfmt -d`.

## Maintaining this index

Add a pointer when a new durable subsystem or operational document lands.
Remove stale pointers when implementations are replaced. Point only at files
committed to the repository; temporary notes are not shared project knowledge.
