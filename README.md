# Grinch

A tiny, fast macOS browser router. Set it as your default browser; it routes
each URL to the right browser based on rules in `~/.config/grinch.js` (or
`~/.grinch.js`).

Inspired by [Finicky](https://github.com/johnste/finicky) and
[Finch](https://github.com/expelledboy/finch) — most of Finicky's config DSL
works in Grinch unchanged. Differences are summarised at the bottom of this
file.

- **~1500 LOC Rust** + a small embedded JS prelude
- **70–400 ns** hot-path resolve latency (12–59× faster than Finch on the
  same workload; see [Performance](#performance) below)
- **~16 MB** resident memory (vs ~140 MB for Finicky)
- Native `JavaScriptCore`, no bundler, no transpiler, no Electron
- Config is real JavaScript — simple cases look like data, full power available

## Install

Requires macOS 13 or later. The release build is a universal binary
(Apple Silicon + Intel) signed and notarized with a Developer ID, so
Gatekeeper won't warn on first launch.

### From a release (recommended)

Grab the latest `Grinch-vX.Y.Z.zip` from
[Releases](https://github.com/jamtur01/grinch/releases/latest), unzip,
and drag `Grinch.app` into `/Applications`.

Or from the terminal:

```sh
ZIP=$(curl -fsSL https://api.github.com/repos/jamtur01/grinch/releases/latest \
  | grep -oE '"browser_download_url": "[^"]*\.zip"' | cut -d'"' -f4)
curl -fsSL "$ZIP" -o /tmp/grinch.zip
ditto -x -k /tmp/grinch.zip /Applications
open /Applications/Grinch.app
```

`ditto -x -k` is used instead of `unzip` so the stapled notarization
ticket and code-signature metadata are preserved on extraction.

### From source

Requires a recent Rust toolchain.

```sh
git clone https://github.com/jamtur01/grinch
cd grinch
make run
```

Pass `UNIVERSAL=1` to `make build` to produce a fat binary locally
(needs both rustup targets: `rustup target add aarch64-apple-darwin
x86_64-apple-darwin`).

### After installing

Launch Grinch (🎄 in your menu bar), open **System Settings → Desktop &
Dock → Default web browser** and select Grinch. Edit `~/.config/grinch.js` to
define your rules — see [`examples/grinch.example.js`](examples/grinch.example.js)
for the full feature surface.

If your finicky.js works as a starting point, copy it across:

```sh
cp ~/.config/finicky.js ~/.config/grinch.js
# (a few `export default` → `module.exports =` and `await fetch()` adjustments
# may be needed; see "Differences from Finicky" below)
```

## Configuration

Drop a JavaScript file at `~/.config/grinch.js` (or `~/.grinch.js`). It must
export a config object via CommonJS:

```js
module.exports = {
  default: ...,           // required: fallback browser
  browsers: { ... },      // optional: named-browser dictionary
  rewrite: [ ... ],       // optional: URL rewriters, applied in order
  rules: [ ... ],         // optional: routing rules, first match wins
};
```

Finicky-style aliases are accepted everywhere: `defaultBrowser`, `handlers`,
`browser` work identically to `default`, `rules`, `open`.

### Browser specs

A browser is one of:

| Form | Means |
|---|---|
| `"Google Chrome"` | App display name; Grinch resolves to bundle ID at config-load |
| `"com.google.Chrome"` | Bundle ID (any reverse-DNS string is treated as one) |
| `{ name: "..." }` | Same as a bare string |
| `{ name: "Google Chrome", profile: "Work" }` | Chromium profile shorthand — expanded to `--profile-directory=Work` for Chromium-family bundle IDs |
| `{ name: "...", args: ["--incognito"] }` | Bundle ID + extra launch args |
| `{ name: "...", openInBackground: true }` | Don't activate (keep focus where it is) |
| `(url, ctx) => "..."` | Dynamic — return any of the above |
| `null` | Suppress: do nothing |

The `profile` shorthand is auto-expanded for: Chrome, Brave, Edge, Vivaldi,
Arc, Opera, Chromium. For other apps it's silently dropped with a load-time
warning.

You can predefine browsers in a top-level `browsers` map:

```js
const browsers = {
  personal: { name: "Google Chrome", profile: "Personal" },
  work:     { name: "Google Chrome", profile: "Work" },
  zen:      "app.zen-browser.zen",
};
```

then refer to them by key (`open: "personal"`) or by reference
(`open: browsers.personal`).

### Match types

A `match:` field accepts one matcher or an array of them (OR semantics — any
hit triggers).

| Syntax | Matches | Notes |
|---|---|---|
| `"github.com"` | hostname, exactly or as subdomain | Bare strings without `*` or `/` are hostname patterns. Most common form. |
| `"*.slack.com/*"` | wildcard, full URL | Strings containing `*` or `/` compile to a Finicky-style anchored regex |
| `"zoom.us/j/*"` | wildcard with implicit `https?://` prefix | |
| `"slack:*"` | URLs with the slack scheme | |
| `domain("a.com", "b.com")` | any of the listed hostnames or their subdomains | Compiled to a single fast check |
| `from("com.tinyspeck.slackmacgap")` | URL was opened by this app | Caller bundle ID; matches `ctx.opener.bundleId` |
| `running("us.zoom.xos")` | this app is currently running | Lazily computed once per resolve |
| `/regex/` | regex against full URL | Case-insensitive |
| `(url, ctx) => bool` | anything | Slow path (~10 µs extra) — full power |

Helper return values like `domain(...)`/`from(...)`/`running(...)`/`strip(...)`
are *data*, not functions — Grinch recognises the marker shape at config-load
and compiles to native Rust matchers/rewriters. The JS bridge is only crossed
on the hot path for user-written `(url, ctx) => ...` predicates.

### URL rewrites

`rewrite` is an array. Every matching rewriter applies, in order.

| Form | Effect |
|---|---|
| `strip("utm_*", "fbclid")` | Strip these query params (trailing `*` is a prefix wildcard) |
| `{ match: ..., url: "https://..." }` | Replace URL when match hits |
| `{ match: ..., url: (url, ctx) => ... }` | Transform URL via JS |
| `{ match: ..., url: () => null }` | Drop the URL (suppress, open nothing) |

A `url` rewrite function receives a URL instance as its first argument and
the ctx as its second. It can return:

| Return | Effect |
|---|---|
| `string` | Use as the new URL |
| `URL` instance (incl. mutated input) | Use `.href` |
| `{protocol, host, pathname, search, hash, ...}` | Concatenate fields into a URL |
| `null` / `undefined` | Drop the URL |

`new URL(href)` works in user code. The polyfill is mutable: `url.protocol = "https:"`,
`url.hostname = "..."`, and `url.searchParams.set("k", "v")` are all reflected
in subsequent reads of `.href`.

### Rules

`rules` (or `handlers`) is an array. First match wins.

```js
rules: [
  { match: ..., open: ... },                    // route to a browser
  { match: ..., open: null },                   // suppress (open nothing)
  { match: ..., url: ..., open: ... },          // rewrite on match, then route
]
```

`open` (Grinch) and `browser` (Finicky) are aliases.

### The `ctx` object

The second argument to every user fn is `ctx`:

```js
{
  url: "https://...",            // input URL passed to resolve (the "originalUrl")
  originalUrl: "https://...",    // alias of ctx.url
  opener: {
    bundleId: "com.microsoft.Outlook",
    name: "Microsoft Outlook",
    path: "/Applications/Microsoft Outlook.app/Contents/MacOS/Microsoft Outlook",
    windowTitle: "...",          // lazy: requires Accessibility permission
  },
  modifiers: {
    shift: false, option: false, command: false, control: false,
  },
}
```

`ctx.url` is pinned to the URL passed into `resolve()` — it doesn't reflect
intermediate rewrites. The first argument (a URL instance) is the *current*
URL and is rebuilt per fn call.

`opener.windowTitle` is a lazy getter. The first time a rule reads it,
Grinch fetches the focused window title via the Accessibility API (~5 ms
XPC call). Configs that never reference `windowTitle` pay nothing. On first
launch, Grinch will prompt for Accessibility permission; until granted,
`windowTitle` returns `""`.

### Globals

The only globals Grinch installs are the marker helpers — `domain()`,
`from()`, `running()`, `strip()` — and the `URL` polyfill. There is no
`finicky.*` namespace; the equivalent functionality is on the existing
primitives:

| Want | Use |
|---|---|
| Match hostname or subdomain | `domain("github.com", ...)` or just `"github.com"` |
| Match by opener bundle ID | `from("com.microsoft.Outlook")` or `(url, ctx) => ctx.opener.bundleId === "..."` |
| Match if app is running | `running("us.zoom.xos")` |
| Read modifier keys | `(url, ctx) => ctx.modifiers.shift` |
| Read opener metadata | `ctx.opener.{bundleId, name, path, windowTitle}` |

`console.log/warn/error` are no-ops — JavaScriptCore has no console;
output is discarded rather than bridged.

### Reload after editing

```sh
kill -HUP $(pgrep -f Grinch.app/Contents/MacOS/Grinch)
```

Or use the menu bar icon → **Reload Config**.

## Commands

```sh
make build                          # build Grinch.app
make run                            # build + register + launch
make test URL="https://..."         # dry-run a URL through the rules
make clean
```

The binary also has `--bench N <url>` for in-process resolve benchmarking.

## Performance

Measured on Apple Silicon, macOS 25, release build, median of 10 runs at
100 k–200 k iterations each.

### Hot path (declarative-only configs)

These are the workloads that hit the bulk of the rules-array — domain
matchers, regex, wildcards. No JS bridge crossings.

| Workload | ns/op |
|---|---:|
| Floor: empty rules, no rewrite | **68** |
| Default fallback, no query | 170 |
| Default fallback, strip removes a param | 391 |
| Bare-hostname match (`"github.com"`) | 168 |
| `domain()` match | 151 |
| Regex match | 125 |
| Wildcard match (`"zoom.us/j/*"`) | 154 |

### Slow path (configs with `(url, ctx) => …` fn matchers)

User-written predicates and rewrites cross into JavaScriptCore. The first
fn call in a resolve costs ~5 µs (URL polyfill construction + ctx build);
subsequent fn calls within the same resolve reuse the cached URL instance
and ctx object.

| Workload | µs/op |
|---|---:|
| Plain URL through 4 fn matchers | 7.3 |
| `?browser=` dynamic open fn | 7.8 |
| Native rule wins early (no fn fires) | 7.8 |
| Drop URL via `() => null` | 8.2 |
| HTTP→HTTPS via URL mutation | 11.5 |
| Full Slack-web → `slack://` rewrite | 13.3 |

### Memory

| | Resident | Peak |
|---|---:|---:|
| Grinch | **16 MB** | 17 MB |

### Compared to alternatives

Same hardware, same config, same URLs.

| Workload | Finch (Swift) | **Grinch (Rust)** | Speedup |
|---|---:|---:|---:|
| Default fallback, no query | 9,308 ns | **162 ns** | 57× |
| Default fallback, strip removes | 10,898 ns | **447 ns** | 24× |
| Bare-hostname match | 5,242 ns | **161 ns** | 33× |
| Subdomain via `domain()` | 5,784 ns | **155 ns** | 37× |
| Regex match | 1,454 ns | **117 ns** | 12× |
| Wildcard match | 9,060 ns | **153 ns** | 59× |

| | Finch | **Grinch** | Finicky |
|---|---:|---:|---:|
| Resident memory | 14.6 MB | **15.5 MB** | 142.5 MB |
| Peak memory | 15.5 MB | **16.6 MB** | 391.2 MB |
| Source LOC | ~700 | ~1,500 | ~2,900 |

Grinch's wins over Finch come from native, allocation-aware Rust:
`regex` crate vs `NSRegularExpression`, byte-level subdomain matching,
per-resolve `quick_host` caching, `Rc<BrowserSpec>` instead of deep
clone on every match, ASCII-only lowercase, and a strip short-circuit
when nothing changes. Finicky's higher memory footprint is its bundled
WebView config UI eagerly loading WebKit, not engine weight — Finicky
uses goja (Go JS) for resolve, which crosses a JS bridge for every match.

### Click latency in practice

The `--bench` numbers above measure `resolve()` in isolation. Real
clicks add a few more steps:

- **macOS Apple Event dispatch**: 1–5 ms from the originating app.
- **`frontmost_opener()`**: ~100–500 µs of LaunchServices IPC, *only
  when the engine reports it needs the opener* (any rule using
  `from()`, `ctx.opener.*`, or any user fn matcher). Configs with
  pure declarative matchers skip this entirely.
- **`current_modifier_flags()`**: ~100 ns kernel call, same gating —
  skipped unless a fn matcher might read modifiers.
- **`open_url()`**: ~few ms for `NSWorkspace.openApplicationAtURL` to
  hand off to the target browser.

So full click-to-browser latency is dominated by macOS event dispatch
(~ms) and target-browser launch (~ms), not by Grinch. Grinch's
contribution is two-to-four orders of magnitude smaller.

### How it works

`domain()`, `from()`, `strip()`, etc. return marker objects like
`{__type: "domain", hosts: [...]}` that Rust recognises at config load
and compiles to native `regex::Regex` / `HashSet<String>` / etc. The
Rust↔JS bridge is only crossed for user-written `(url, ctx) => ...`
predicates and rewrites. Within a single resolve, the URL instance,
ctx object, parsed hostname, and `fn_args` NSArray are cached and
reused across callbacks.

LaunchServices lookups (`URLForApplicationWithBundleIdentifier`,
`fullPathForApplication`) and Chromium `Local State` parsing are also
cached — first call hits the system, subsequent calls are HashMap
probes. `BrowserSpec`s are held as `Rc<…>` internally so a successful
match is a refcount bump, not a `String + Vec<String>` deep clone.

## Differences from Finicky

If you're porting a Finicky config, these are the places you'll need to
adjust:

1. **`module.exports = { ... }` instead of `export default { ... }`.**
   JavaScriptCore in Grinch evaluates scripts, not modules — `import`/`export`
   syntax doesn't parse.
2. **No `await fetch()`.** The resolve hot path is sync. The Finicky
   `shortenerExpander` pattern can't run; resolve a shortener separately
   if you need it.
3. **No `finicky.*` namespace.** Grinch doesn't ship `finicky.matchHostnames`,
   `finicky.getModifierKeys`, `finicky.isAppRunning`, `finicky.notify`,
   `finicky.getBattery`, `finicky.getPowerInfo`, or `finicky.getSystemInfo`.
   Migrate to:
   - `finicky.matchHostnames(...)` → `domain(...)` (note: `domain` matches
     subdomains too; for exact-hostname-only use a regex like
     `/^github\.com$/`)
   - `finicky.getModifierKeys()` → `ctx.modifiers` inside a fn matcher
   - `finicky.isAppRunning(id)` → declarative `running(id)` matcher
   - The remaining stubs (notify / getBattery / getPowerInfo / getSystemInfo)
     never had meaningful implementations and have no replacement.
4. **`opener.windowTitle` requires Accessibility permission.** First launch
   prompts; before granting, the field returns `""` and rules depending on
   it silently no-op.
5. **`appType` is auto-detected, not declarative.** Names that look like
   bundle IDs (reverse-DNS) are treated as such; everything else goes
   through `NSWorkspace.fullPathForApplication`. You can still set
   `bundleId`/`id` explicitly.

Everything else — `domain`, `from`, `running`, `strip`, the `URL` polyfill,
arrays of matchers, `null` open, combined `{match, url, browser}` entries,
the `LegacyURLObject` rewrite return shape — is supported.

## License

MIT — see [LICENSE](LICENSE).
