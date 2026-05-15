# Grinch

A small, fast native macOS browser router. Set it as your default browser
and it routes each URL to the right one based on rules in
`~/.grinch.js`, `~/.config/grinch.js`, or `~/.config/grinch/grinch.js`
(checked in that order; first found wins).

Most Finicky **v4** configs work in Grinch unchanged. (Finicky v3 configs
need updating — see the
[v4 migration guide](https://github.com/johnste/finicky/wiki/Migration-guide)
upstream, then [Differences from Finicky](#differences-from-finicky) below
for the rest.) Inspired by both [Finicky](https://github.com/johnste/finicky)
and [Finch](https://github.com/expelledboy/finch).

- **~1500 LOC Rust** + a small embedded JS prelude
- **~16 MB** resident memory, **~1.5 MB** universal binary
- Native `JavaScriptCore` for config eval — no Electron, no bundler, no transpiler
- Single DMG, universal binary (Apple Silicon + Intel)
- Config is real JavaScript — simple cases look like data, full power available
- Hot-path resolve in nanoseconds; full click-to-browser pipeline in single-digit milliseconds

## Install

Requires macOS 13 or later. The release build is a universal binary
(Apple Silicon + Intel).

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

Drop a JavaScript file at one of (checked in this order, first found wins):

1. `~/.grinch.js` — legacy / dotfile
2. `~/.config/grinch.js` — flat XDG
3. `~/.config/grinch/grinch.js` — XDG subdir, mirrors Finicky's layout
4. `/Library/Application Support/Grinch/grinch.js` — system-wide, last in
   the search order so user paths always win. Intended for MDM-managed
   Macs and shared workstations where a baseline config gets dropped
   centrally without per-user provisioning.

It must export a config object via CommonJS:

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
  resolve. The file is opened lazily on the first resolve and appended
  to thereafter; one file per app launch. Useful for figuring out *why*
  a particular click went where it did without enabling the broader
  `GRINCH_DEBUG=1` stderr trace.

  Pair with **`logRotateBytes: <n>`** and/or **`logRotateDays: <n>`** to
  cap the log's growth. Rotation renames the current file to
  `<original-name>.log.<iso-timestamp>` and starts a fresh empty file;
  both triggers can be combined (whichever fires first wins). Default:
  no rotation, file grows until you delete it.

  ```json
  {
    "ts": 1778518645.634,
    "url": "https://example.com/",
    "final": "https://example.com/",
    "rewritten": false,
    "browser": "com.google.Chrome",
    "args": ["--profile-directory=Profile 10"],
    "opener": {
      "bundleId": "com.tinyspeck.slackmacgap",
      "name": "Slack",
      "pid": 731
    },
    "modifiers": {"shift": true, "option": false, "command": false, "control": false},
    "matchedRule": {"index": 11, "name": "shift-override"}
  }
  ```

  Field notes:
  - `rewritten` — true iff `final != url` (a rewrite fired).
  - `opener` — the app that *sent* the URL, identified via the GURL Apple
    Event's sender PID. Empty `bundleId` means neither the sender PID
    nor the frontmost-app fallback identified one (rare).
  - `matchedRule` — `{index, name}` of the rule whose matcher fired, or
    `null` when the URL fell through to `default`. `name` is the rule's
    user-supplied `name:` if present, otherwise an auto-derived label
    (string pattern, `domain:foo,bar`, or first line of the fn source
    for fn matchers). Pair with `Grinch --list-rules` to map indices
    to their full source.

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
| `safelinks()` | Unwrap corporate SafeLinks / URL-defense wrappers — see below |
| `teams_launcher()` | Unwrap MS Teams launcher URLs to `msteams:` — see below |
| `{ match: ..., url: "https://..." }` | Replace URL when match hits |
| `{ match: ..., url: (url, ctx) => ... }` | Transform URL via JS |
| `{ match: ..., url: () => null }` | Drop the URL (suppress, open nothing) |

`safelinks()` is a bare top-level entry (no `match:` field) that recognises
the most common corporate URL wrappers and extracts the real destination:

- **Microsoft 365 Defender SafeLinks** — `*.safelinks.protection.outlook.com/?url=…`
- **Microsoft Teams external-link interstitial** — `statics.teams.cdn.office.net/evergreen-assets/safelinks/?url=…`
- **Proofpoint URL Defense v2** — `urldefense.proofpoint.com/v2/url?u=…`
- **Proofpoint URL Defense v3** — `urldefense.com/v3/__<encoded>__;<marker>!!…` — uses `*` placeholders with a base64-URL replacement stream (and `**X` run-length markers for runs of 2–65)

Pass-through on every other host, so it's safe at the top of the rewrite
chain. Composes cleanly with `strip()` — `[safelinks(), strip("utm_*")]`
unwraps a Defender-tracked Outlook link, then strips `utm_*` off the
inner URL. Double-wrapped chains (Defender → Proofpoint and similar) are
unwrapped up to two levels deep.

`teams_launcher()` is a separate bare top-level entry that handles a
different Microsoft URL shape — the Teams launcher landing page that
calendar invites and corporate share links commonly use:

```
https://teams.microsoft.com/dl/launcher/launcher.html?url=%2F_%23%2Fl%2Fmeetup-join%2F19%3A…
```

The decoded `url` param is a relative path (`/_#/l/meetup-join/19:…`)
rather than an absolute URL, so it doesn't fit `safelinks()`'s
"hidden absolute URL" model. `teams_launcher()` strips the web-app
routing prefix and rebuilds the URL as `msteams:/l/meetup-join/19:…`,
which opens in the native Teams app. Pass-through on every other host
and path. Use it alongside `safelinks()`:

```js
rewrite: [ safelinks(), teams_launcher(), strip("utm_*") ]
```

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
  { match: ..., open: ..., name: "label" },     // optional human label (see below)
]
```

`open` (Grinch) and `browser` (Finicky) are aliases.

Each rule entry accepts an optional **`name`** string. It doesn't affect
routing — it labels the rule in `Grinch --list-rules` output and in the
`matchedRule.name` field of the `logRequests` JSONL. Useful when chasing
"why did this click go there?" through a config with a dozen fn matchers,
since the auto-derived label for fn rules is just the first line of
`f.toString()`.

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

`ctx.opener` is identified via the GURL Apple Event's sender PID
(`keySenderPIDAttr`), so it survives LaunchServices activating Grinch
ahead of our open-URL callback — the frontmost-app heuristic would
otherwise report Grinch itself once macOS shifted focus. `ctx.opener`
is `null` only when the event lacks the sender attribute or the sending
process exited between event delivery and lookup. Always guard with
`if (ctx.opener) { ... }` or optional chaining (`ctx.opener?.bundleId`)
in fns that read it. This matches Finicky v4's `options.opener` semantics.

`opener.windowTitle` is a lazy getter. The first time a rule reads it,
Grinch fetches the focused window title via the Accessibility API (~5 ms
XPC call). Configs that never reference `windowTitle` pay nothing. On first
launch, Grinch will prompt for Accessibility permission; until granted,
`windowTitle` returns `""`.

### Globals

Grinch installs the marker helpers — `domain()`, `from()`, `running()`,
`strip()` — the `URL` polyfill, and the Finicky-compatible `finicky.*`
namespace (see [Differences from Finicky](#differences-from-finicky)
for the full inventory). For most routing decisions you can pick either
the Grinch-native or the Finicky-style form:

| Want | Grinch-native | Finicky-style |
|---|---|---|
| Match hostname or subdomain | `domain("github.com", ...)` or `"github.com"` | n/a (use bare string) |
| Match exact hostname only | n/a (use bare string with no `.`) | `finicky.matchHostnames("github.com")` |
| Match by opener bundle ID | `from("com.microsoft.Outlook")` | `(url, ctx) => ctx.opener.bundleId === "..."` |
| Match if app is running | `running("us.zoom.xos")` | `finicky.isAppRunning("Zoom")` |
| Read modifier keys | `(url, ctx) => ctx.modifiers.shift` | `finicky.getModifierKeys()` |
| Read opener metadata | `ctx.opener.{bundleId, name, path, windowTitle}` | (same — `ctx.opener` is shared) |

`console.log/warn/error/info/debug` are wired to stderr with a
`grinch [level]:` prefix — call them from anywhere in your config to
trace why a rule did or didn't fire. Objects are JSON-stringified for
inspection-style debugging.

### Menu bar

Click the 🎄 in the menu bar:

| Item | Action |
|---|---|
| **Grinch vX.Y.Z** | Disabled label at the top showing the running binary's version. Matches `Grinch --version`. |
| **Open Config** (⌘O) | Opens the active config file in your default `.js` handler (VS Code / Cursor / etc.). |
| **Reload Config** (⌘R) | Re-evaluates the config without relaunching. Equivalent to `kill -HUP $(pgrep -f Grinch.app/Contents/MacOS/Grinch)`. |
| **Start at Login** | Toggles `SMAppService.mainApp` registration. Off by default; the entry also appears in System Settings → General → Login Items so users can disable it from there. |
| **Quit Grinch** (⌘Q) | Exit. |

If a reload fails (syntax error, unreadable file, missing `default`),
the menu bar icon flips to **⚠️** and a non-clickable "Config error:
…" item appears at the top of the menu with the first line of the
failure. The previous engine stays in place so routing keeps working
until the config is fixed; the next successful reload restores 🎄.

## SSO / OAuth popups

Apps that use `ASWebAuthenticationSession` for sign-in (Slack login,
Claude Desktop login, many corporate OAuth flows, password-manager
extensions) don't go through the regular `http://` default-browser
handoff. macOS routes them to a separate "trusted browser" via
`ASWebAuthenticationSessionWebBrowserSessionManager` and falls back
to Safari for any app that doesn't declare
`ASWebAuthenticationSessionWebBrowserSupportCapabilities` in its
Info.plist — which is what was happening before Grinch v0.5
[(Finicky has the same bug)](https://github.com/johnste/finicky/issues/405).

Grinch declares the capability and registers a session handler that
forwards the auth URL through the same `engine.resolve()` machinery
that handles regular clicks. The user's chosen browser opens the URL
as a normal tab. When the browser eventually navigates to the
callback URL (a custom scheme like `slack://oauth-callback?token=…`,
or an HTTPS Universal-Links callback), Grinch's URL / activity
handlers match the URL against pending sessions via the framework's
`ASWebAuthenticationSessionCallback.matchesURL:` and call
`completeWithCallbackURL:` — letting the originating app's
session-API completion handler fire normally and dismissing the
auth dialog cleanly.

**Caveats.** Grinch declares `IsSupported: true` but not
`EphemeralBrowserSessionIsSupported`: we route to the user's regular
browser tab, which inherits their existing cookies and profile
state, so the isolation Apple promises for
`shouldUseEphemeralSession`-flagged requests isn't actually
delivered. Apps that need ephemeral sessions see the missing key
and fall back to a non-ephemeral flow. Apps that don't request
ephemeral (the vast majority) are unaffected.

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

The binary also accepts:

| Flag | Effect |
|---|---|
| `--version` | Print the crate version. |
| `--test <url>` | Dry-run a URL through the rules. `grinch:<inner>` URLs are unwrapped, so `--test grinch:tel:+15551234567` exercises the routing for `tel:+15551234567`. |
| `--bench N <url>` | In-process resolve benchmarking, N iterations. |
| `--list-rules` | Print the loaded rules with their indices, labels, and targets — pair with `logRequests` to map `matchedRule.index` back to the entry in your config. |
| `--list-browsers` | List every app registered to handle `https://` URLs, one bundle ID per line with its display name. Useful for finding the right bundle ID when writing a config. |
| `--validate` | Load the config and print whether it parses cleanly. Exits 0 on success, 1 on any load error (with the captured message + the path it was reading). Designed for editor save-hooks and CI. |

Beyond the standard `http` / `https` / `mailto` Grinch handles natively,
the bundle also registers `tel:`, `webcal:`, and `feed:` so those schemes
route through the same rules engine. A custom `grinch:` scheme lets
external tools invoke Grinch's resolver explicitly:

```sh
open grinch:https://example.com/path        # route through your rules
open 'grinch:tel:+15551234567'              # route a non-web URL
```

The handler strips the `grinch:` prefix before resolve, so the inner
URL is what your rules match against. Useful for Shortcuts, AppleScript,
and `open(1)` flows where you want to route through Grinch even if it
isn't the system default browser.

## Performance

A few benchmark data points from `bench/run.sh`. Worth knowing that
real-world click-to-browser latency is dominated by macOS plumbing
(Apple Event dispatch + `NSWorkspace.openApplicationAtURL`, both in
the few-millisecond range), so engine-only numbers don't translate
1:1 into a faster-feeling click — but they're a useful window into
what the engine is doing on its own.

Apple Silicon, macOS 26, release build, median of 10 runs at 100k–200k
iterations per workload. Configs and URLs in `bench/configs/`.

### Hot path (declarative-only configs)

Workloads that hit the rules-array — domain matchers, regex, wildcards.
No JS bridge crossings; the URL string is borrowed (`Cow::Borrowed`)
for the entire resolve when no rewrite fires; `quick_host` is skipped
when the config has no host-using matcher and borrows the host slice
when it's already lowercase ASCII.

| Workload | ns/op |
|---|---:|
| Floor: empty rules, no rewrite | 6 |
| Default fallback, no query | 69 |
| Default fallback, strip removes a param | 194 |
| Bare-hostname match (`"github.com"`) | 44 |
| `domain()` match | 50 |
| Regex match | 24 |
| Wildcard match (`"zoom.us/j/*"`) | 32 |
| 50 bare-hostname rules, last one wins | 302 |

### Slow path (configs with `(url, ctx) => …` fn matchers)

User-written predicates and rewrites cross into JavaScriptCore. URL-only
predicates (`(url) => …`) skip the `__grinchMakeCtx` build and skip the
LaunchServices IPC for `frontmost_opener()` upstream — only fns declaring
a second formal arg pay for ctx. The first JS-bridge call in a resolve
costs ~2.5 µs (URL polyfill + cached opener-field JSValues); subsequent
fn calls within the same resolve reuse the cached args. Ctx build itself
reuses pre-built `true`/`false` JSValues for modifier flags.

A few smaller wins compound on this path: `apply_rewrite` short-circuits
in Rust for the common fn-return shapes (string, null, undefined, URL
polyfill instance with non-empty `.href`) instead of always going
through the `__grinchRewriteResult` JS helper — the LegacyURLObject
case still does. Result-checks use `JSValueGetType` (one C call) in
place of paired `isNull()` + `isUndefined()` Obj-C dispatches. And
runs of two-or-more consecutive fn-only rules are batched into a
single pre-compiled JS dispatcher at engine init, so a config with N
fn matchers that all fall through pays for one JS bridge crossing
instead of N — the *"4 fn matchers reading `ctx.opener`"* row below
exercises that path.

| Workload | ns/op |
|---|---:|
| Native rule wins early (no fn fires) | 44 |
| Drop URL via `() => null` (url-only) | 2,400 |
| HTTP→HTTPS via URL mutation (url-only) | 3,725 |
| `?browser=` dynamic open fn (url-only matcher) | 4,640 |
| 4 fn matchers reading `ctx.opener` | 4,745 |
| Full Slack-web → `slack://` rewrite | 5,750 |

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
3. **`finicky.*` namespace is shipped; three of the eight methods are stubbed.**
   All eight v4 methods are present:
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
   - `finicky.notify(...)` — **stub**; logs a `console.error` pointing at
     `console.log` and returns. macOS notifications aren't wired up.
   - `finicky.getBattery()` — **stub**; matches Finicky's own deprecation
     by logging an error pointing at `getPowerInfo` and returning dummies.
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
