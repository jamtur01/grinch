# Grinch

A small, fast native macOS browser router. Set it as your default browser
and it routes each URL to the right one based on rules in
`~/.config/grinch.js` (or `~/.grinch.js`).

Most Finicky **v4** configs work in Grinch unchanged. (Finicky v3 configs
need updating — see the
[v4 migration guide](https://github.com/johnste/finicky/wiki/Migration-guide)
upstream, then [Differences from Finicky](#differences-from-finicky) below
for the rest.) Inspired by both [Finicky](https://github.com/johnste/finicky)
and [Finch](https://github.com/expelledboy/finch).

- **~1500 LOC Rust** + a small embedded JS prelude
- **~16 MB** resident memory, **~1.5 MB** universal binary
- Native `JavaScriptCore` for config eval — no Electron, no bundler, no transpiler
- Single signed and notarized DMG
- Config is real JavaScript — simple cases look like data, full power available
- Hot-path resolve in nanoseconds; full click-to-browser pipeline in single-digit milliseconds

## Install

Requires macOS 13 or later. The release build is a universal binary
(Apple Silicon + Intel) signed and notarized with a Developer ID, so
Gatekeeper won't warn on first launch.

### From a release (recommended)

Grab the latest `Grinch-vX.Y.Z.dmg` from
[Releases](https://github.com/jamtur01/grinch/releases/latest), open it,
and drag `Grinch.app` onto the `Applications` shortcut shown in the DMG
window.

Or from the terminal:

```sh
DMG=$(curl -fsSL https://api.github.com/repos/jamtur01/grinch/releases/latest \
  | grep -oE '"browser_download_url": "[^"]*\.dmg"' | cut -d'"' -f4)
curl -fsSL "$DMG" -o /tmp/grinch.dmg
hdiutil attach -nobrowse -quiet /tmp/grinch.dmg
ditto "/Volumes/Grinch "*/Grinch.app /Applications/Grinch.app
hdiutil detach "/Volumes/Grinch "* -quiet
open /Applications/Grinch.app
```

Always install from the DMG to `/Applications` directly. Running
`Grinch.app` out of `~/Downloads` (or anywhere else) triggers Gatekeeper
translocation, which makes the same app appear multiple times in the
default-browser picker.

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
```

`export default { ... }` works as-is (auto-rewritten to `module.exports`).
You'll still need to convert `await fetch()` calls; see
"Differences from Finicky" below.

## Configuration

Drop a JavaScript file at `~/.config/grinch.js` (or `~/.grinch.js`). It must
export a config object via CommonJS:

```js
module.exports = {
  default: ...,           // required (browser spec, fn, or null for "do nothing")
  browsers: { ... },      // optional: named-browser dictionary
  rewrite: [ ... ],       // optional: URL rewriters, applied in order
  rules: [ ... ],         // optional: routing rules, first match wins
  options: { ... },       // optional: Finicky-compat options block (parsed, mostly inert)
};
```

Finicky-style aliases are accepted everywhere: `defaultBrowser`, `handlers`,
`browser` work identically to `default`, `rules`, `open`.

The `options` block accepts Finicky v4's five keys. Two are wired up:

- **`hideIcon: true`** — skip the menu-bar status item at app launch.
  Useful when you don't want the 🎄 in your menu bar. Reloads don't
  toggle the icon mid-session; restart Grinch to apply.
- **`logRequests: true`** — write a JSONL trace to
  `~/Library/Logs/Grinch/Grinch_<timestamp>.log` with one line per
  resolve: `{"ts": <unix-secs>, "url": "<input>", "final":
  "<after-rewrites>", "browser": "<bundle-id>", "args": [...],
  "opener": "<bundle-id>"}`. The file is opened lazily on the first
  resolve and appended to thereafter; one file per app launch.
  Useful for figuring out *why* a particular click went where it did
  without enabling the broader `GRINCH_DEBUG=1` stderr trace.

The other three are inert: `urlShorteners` (expects
[external expansion](#working-with-url-shorteners)), `checkForUpdates`
(Grinch doesn't poll), `keepRunning` (Grinch is always resident).
Unknown keys log a one-line warning.

### Browser specs

A browser is one of:

| Form | Means |
|---|---|
| `"Google Chrome"` | App display name; Grinch resolves to bundle ID at config-load |
| `"com.google.Chrome"` | Bundle ID (any reverse-DNS string is treated as one) |
| `"Google Chrome:Work"` | `Name:Profile` shorthand (Finicky-compatible) — splits on the first `:`, expands the suffix to `--profile-directory=Work` (Chromium) or `-P Work` (Firefox). Only applied to literal config strings; fn-returned strings are treated opaquely |
| `"/Applications/Foo.app"` or `"~/Apps/Bar.app"` | Path autodetect (Finicky-compatible) — bare-string spec ending in `.app` is resolved via `NSBundle` directly, no `appType: "path"` required. Useful for browsers outside `/Applications` or not registered with LaunchServices |
| `{ name: "..." }` | Same as a bare string |
| `{ name: "Google Chrome", profile: "Work" }` | Profile shorthand — expanded to `--profile-directory=Work` (Chromium-family) or `-P Work` (Firefox-family) |
| `{ name: "...", args: ["--incognito"] }` | Bundle ID + extra launch args |
| `{ name: "...", openInBackground: true }` | Don't activate (keep focus where it is) |
| `{ name: "/Applications/Foo.app", appType: "path" }` | Path to an `.app` bundle — Grinch reads `CFBundleIdentifier` directly. Useful for browsers outside `/Applications` or not registered with LaunchServices |
| `{ name: "...", appType: "bundleId" }` | Trust the value as a bundle ID, skip the LaunchServices display-name fallback |
| `{ appType: "none" }` | Explicit no-op browser. Same effect as `open: null` |
| `(url, ctx) => "..."` | Dynamic — return any of the above. Works for `defaultBrowser` too (Finicky-compatible): a fn default is invoked at resolve time when no rule matched |
| `null` | Suppress: do nothing. Works as a rule's `open: null` AND as `defaultBrowser: null` (Finicky-compat) — when no rule matches and the default is null, nothing opens |

The `profile` shorthand is auto-expanded for the Chromium family (Chrome,
Brave, Edge, Vivaldi, Arc, Opera, Chromium) and the Firefox family
(Firefox, Firefox Developer Edition, Firefox Nightly, Waterfox, LibreWolf).
Chromium profiles can be referenced by either their on-disk directory
("Profile 10") or their display name ("Work") — Grinch resolves through
Chrome's `Local State`. Firefox profiles use the name from `profiles.ini`;
unknown names log a warning naming the known profiles. Other browsers'
`profile` is silently dropped with a load-time warning.

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
| `"github.com"` | hostname, exactly or as subdomain | Bare strings without `*` or `/` are hostname patterns. Most common form. **Differs from Finicky**: see "Bare-string matcher semantics" below. |
| `"*.slack.com/*"` | wildcard, full URL | Strings containing `*` or `/` compile to a Finicky-style anchored regex |
| `"zoom.us/j/*"` | wildcard with implicit `https?://` prefix | |
| `"slack:*"` | URLs with the slack scheme | |
| `domain("a.com", "b.com")` | any of the listed hostnames or their subdomains | Compiled to a single byte-level check |
| `finicky.matchHostnames("github.com")` | exact hostname only — does NOT match subdomains | Finicky-compatible matcher fn. Use this when you specifically need exact-hostname semantics. |
| `from("com.tinyspeck.slackmacgap")` | URL was opened by this app | Caller bundle ID; matches `ctx.opener.bundleId` |
| `running("us.zoom.xos")` | this app is currently running | Lazily computed once per resolve |

#### Bare-string matcher semantics

Grinch's bare string `"github.com"` is a **hostname-and-subdomain shortcut**:
it matches `https://github.com/`, `https://api.github.com/`, and
`https://gist.github.com/` alike. This is the most common case for routing
configs and is the friendliest default.

Finicky v4's same syntax is different — a bare string with no `*` is matched
as an `===` against `url.href` (the full URL). So `match: "github.com"`
in Finicky would never fire on a real URL, and users reach for
`finicky.matchHostnames("github.com")` (exact hostname) or `domain()`
helpers instead.

If you want the strict Finicky semantics on a port, use either:

- `finicky.matchHostnames("github.com")` for exact hostname matching, **or**
- `/^github\.com$/` (regex anchored to the full URL — no, wait: that's the
  full URL not just hostname; use `(url) => url.hostname === "github.com"`
  for the hostname-only check).

If you want subdomain matching across multiple hosts at once:

- `domain("github.com", "gitlab.com")` — Grinch's helper, matches each host
  AND its subdomains, compiled to a single byte-level check.
| `/regex/` | regex against full URL | Honours `i` and `m` flags from the JS literal (matches Finicky / native `RegExp.test`); without `i`, matching is case-sensitive |
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
| `null` | Drop the URL (suppress, open nothing) |
| `undefined` (or `return;`) | Pass-through — leave the URL unchanged. Matches Finicky v4 |

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
  opener: {                      // OR null — see below
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

`ctx.opener` is `null` when the source app couldn't be detected (e.g. the
URL came from a non-app dispatcher). Always guard with
`if (ctx.opener) { ... }` or optional chaining (`ctx.opener?.bundleId`)
in fns that read it. This matches Finicky v4's `options.opener` semantics.

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

`console.log/warn/error/info/debug` are wired to stderr with a
`grinch [level]:` prefix — call them from anywhere in your config to
trace why a rule did or didn't fire. Objects are JSON-stringified for
inspection-style debugging.

### Menu bar

Click the 🎄 in the menu bar:

| Item | Action |
|---|---|
| **Open Config** (⌘O) | Opens the active config file in your default `.js` handler (VS Code / Cursor / etc.). |
| **Reload Config** (⌘R) | Re-evaluates the config without relaunching. Equivalent to `kill -HUP $(pgrep -f Grinch.app/Contents/MacOS/Grinch)`. |
| **Start at Login** | Toggles `SMAppService.mainApp` registration. Off by default; the entry also appears in System Settings → General → Login Items so users can disable it from there. |
| **Quit Grinch** (⌘Q) | Exit. |

## Working with URL shorteners

Grinch's resolve loop is synchronous on purpose, so it can't follow
redirects from inside a rule (`await fetch()` doesn't run; see
[Differences from Finicky](#differences-from-finicky)). For shortener
hosts (`bit.ly`, `t.co`, `lnkd.in`, `ow.ly`, …) you have two practical
options.

### Option 1: route shorteners to a sensible default

Just send them to whichever browser you'd usually open links in. The
final destination's host won't be visible to your match rules, but the
browser opens normally and you don't pay any extra latency.

```js
{
  match: domain("bit.ly", "t.co", "lnkd.in", "ow.ly", "buff.ly", "tinyurl.com"),
  open: browsers.personal,
}
```

### Option 2: pre-expand outside Grinch

The companion script
[`examples/expand-shortener.sh`](examples/expand-shortener.sh) follows
the redirect chain with `curl --location --head` (capped at 5 s) and
then re-opens the final URL through `open(1)`. Grinch sees the
expanded form and routes it through your normal rules — the shortener
host never reaches your `match:` logic.

```sh
chmod +x examples/expand-shortener.sh
examples/expand-shortener.sh "https://bit.ly/3GyNJpL"
```

Hook it into whichever launcher you already use:

- **Raycast / Alfred**: bind to a hotkey, paste the URL from the
  clipboard, run the script.
- **Hammerspoon**: register a `hs.urlevent` handler and shell out to
  the script with the URL as the argument.
- **Shortcuts.app**: wrap as a Quick Action that takes URLs from the
  share sheet and runs `expand-shortener.sh "$1"`.
- **Plain terminal**: `examples/expand-shortener.sh "$(pbpaste)"` after
  copying.

The trade-off is that shortener clicks now pay a network round-trip
(50–500 ms typical), but the expansion happens *outside* Grinch's
hot path so the rest of your routing stays in the microsecond range.

## Commands

```sh
make build                          # build Grinch.app
make run                            # build + register + launch
make test URL="https://..."         # dry-run a URL through the rules
make clean
```

The binary also has `--version` (prints the crate version), `--test <url>`
(dry-run a URL through the rules), and `--bench N <url>` (in-process resolve
benchmarking).

## Performance

A few benchmark data points from `bench/run.sh`. Worth knowing that
real-world click-to-browser latency is dominated by macOS plumbing
(Apple Event dispatch + `NSWorkspace.openApplicationAtURL`, both in
the few-millisecond range), so engine-only numbers don't translate
1:1 into a faster-feeling click — but they're a useful window into
what the engine is doing on its own.

Apple Silicon, macOS 25, release build, median of 10 runs at 100k–200k
iterations per workload. Configs and URLs in `bench/configs/`.

### Hot path (declarative-only configs)

Workloads that hit the rules-array — domain matchers, regex, wildcards.
No JS bridge crossings; the URL string is borrowed (`Cow::Borrowed`)
for the entire resolve when no rewrite fires; `quick_host` is skipped
when the config has no host-using matcher and borrows the host slice
when it's already lowercase ASCII.

| Workload | ns/op |
|---|---:|
| Floor: empty rules, no rewrite | 5 |
| Default fallback, no query | 67 |
| Default fallback, strip removes a param | 187 |
| Bare-hostname match (`"github.com"`) | 42 |
| `domain()` match | 48 |
| Regex match | 22 |
| Wildcard match (`"zoom.us/j/*"`) | 30 |
| 50 bare-hostname rules, last one wins | 295 |

### Slow path (configs with `(url, ctx) => …` fn matchers)

User-written predicates and rewrites cross into JavaScriptCore. URL-only
predicates (`(url) => …`) skip the `__grinchMakeCtx` build and skip the
LaunchServices IPC for `frontmost_opener()` upstream — only fns declaring
a second formal arg pay for ctx. The first JS-bridge call in a resolve
costs ~3 µs (URL polyfill + cached opener-field JSValues); subsequent
fn calls within the same resolve reuse the cached args. Ctx build itself
reuses pre-built `true`/`false` JSValues for modifier flags.

| Workload | ns/op |
|---|---:|
| Native rule wins early (no fn fires) | 42 |
| Drop URL via `() => null` (url-only) | 2,597 |
| HTTP→HTTPS via URL mutation (url-only) | 4,327 |
| `?browser=` dynamic open fn (url-only matcher) | 4,855 |
| 4 fn matchers reading `ctx.opener` | 5,159 |
| Full Slack-web → `slack://` rewrite | 5,494 |

### Footprint

| | Grinch | Finch | Finicky |
|---|---:|---:|---:|
| Resident memory | 15.5 MB | 14.6 MB | 142.5 MB |
| Peak memory | 16.6 MB | 15.5 MB | 391.2 MB |
| Source LOC | ~1,500 | ~700 | ~2,900 |
| JS engine | system JSC | n/a (Swift DSL) | bundled goja |
| Bundled UI | menu bar only | menu bar only | WebView config app |

Finicky bundles a WebKit instance for its config UI, which accounts for
the bulk of its memory footprint.

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

Grinch tracks **Finicky v4** (the current line, with `defaultBrowser` /
`handlers` / `rewrite`). Finicky v3 configs are best ported through
[Finicky's migration guide](https://github.com/johnste/finicky/wiki/Migration-guide)
first, but for the most common v3 leftovers Grinch ships compatibility
shims:

- `url.urlString`, `url.url`, and `url.opener` warn-and-return — the
  values are usable, just deprecated. `urlString` returns `url.href`;
  `url.url` returns the legacy `{protocol, hostname, …}` object;
  `url.opener` returns the live opener (the same `{bundleId, name,
  path}` object you'd get via `ctx.opener` in a 2-arg matcher fn) or
  `null` if no opener is available. Each logs a one-line
  `console.warn` pointing at the v4 equivalent.
- `url.keys` throws with a helpful message pointing at `ctx.modifiers`
  and `finicky.getModifierKeys()`. (Throwing rather than warn-and-
  returning because v3's `url.keys` had a different shape from v4's
  `ctx.modifiers`, and silently returning the wrong shape would cause
  routes to misfire.)

If you're porting a Finicky v4 config, these are the places you'll need

If you're porting a Finicky v4 config, these are the places you'll need
to adjust:

1. **`export default { ... }` works; `import` / named `export` don't.**
   Grinch preprocesses `export default <expr>` into `module.exports = <expr>`
   at config-load, so paste-and-go from a Finicky v4 config works. ES module
   `import` lines and named exports (`export const`, `export function`, etc.)
   error out with a config-load message pointing at `module.exports` —
   JSC evaluates the file as a script, so there's nowhere for an import to
   resolve. Inline what you need or pre-process before invoking Grinch.
2. **No `await fetch()`.** The resolve hot path is sync. The Finicky
   `shortenerExpander` pattern can't run; resolve a shortener separately
   if you need it — see [Working with URL shorteners](#working-with-url-shorteners)
   below.
3. **`finicky.*` namespace is shipped, with one stub.** All eight v4
   methods are present:
   - `finicky.matchHostnames(matchers)` — exact-hostname matcher fn
     (Finicky-compatible). For subdomain matching use Grinch's `domain(...)`.
   - `finicky.matchDomains(matchers)` — deprecated alias, warns and delegates.
   - `finicky.getModifierKeys()` — real values from CG event flags
     (shift/option/command/control/capsLock/fn/function).
   - `finicky.isAppRunning(id)` — matches against bundle ID OR localized name.
   - `finicky.getSystemInfo()` — `{localizedName, name}` from `[NSHost currentHost]`.
   - `finicky.getPowerInfo()` — **stub** that returns placeholder values
     (`{isCharging:false, isConnected:true, percentage:null}`) and emits a
     one-time `console.warn` on first call. Real IOKit hookup is on the TODO
     list; routing on actual battery state isn't supported yet.
   - `finicky.notify` and `finicky.getBattery` — inert stubs that match
     Finicky's own deprecated behaviour (`notify` logs an error pointing at
     `console.log`; `getBattery` returns dummies pointing at `getPowerInfo`).
4. **`opener.windowTitle` requires Accessibility permission.** First launch
   prompts; before granting, the field returns `""` and rules depending on
   it silently no-op.
5. **`appType` is honoured, but autodetected when omitted.** All four
   Finicky values work: `"appName"` (the default — display-name lookup),
   `"bundleId"` (skip the display-name fallback), `"path"` (read
   `CFBundleIdentifier` from a `.app` bundle path), and `"none"`
   (no-op browser, same as `open: null`). When you don't set `appType`,
   Grinch autodetects: a reverse-DNS string is treated as a bundle ID,
   anything else as a display name.

Everything else — `domain`, `from`, `running`, `strip`, the `URL` polyfill,
arrays of matchers, `null` open, combined `{match, url, browser}` entries,
the `LegacyURLObject` rewrite return shape — is supported.

## License

MIT — see [LICENSE](LICENSE).
