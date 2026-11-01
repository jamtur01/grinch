<p align="center">
  <img src="brand/grinch-icon.svg" width="160" height="160" alt="Grinch logo">
</p>

# Grinch

Grinch chooses which browser or app opens each link on your Mac. Set it as
your default browser, then give it a few rules.

For example, you can open GitHub links in Chrome, send everything else to
Safari, and use a separate browser profile for work. Links from Slack, Mail,
and other apps follow the same rules automatically.

Grinch is inspired by [Finicky](https://github.com/johnste/finicky) and
[Finch](https://github.com/expelledboy/finch), two other macOS apps for routing
links. If you already use Finicky, most v4 configs work with Grinch; see
[Differences from Finicky](#differences-from-finicky) when moving your config.

Get started with [installation](#install) and
[your first config](#set-up-your-first-config), then explore
[rules](#rules), [browser targets](#browser-specs), and
[rewrites](#url-rewrites). For troubleshooting, see the
[menu bar](#menu-bar), [diagnostic log](#diagnostic-log), and
[commands](#commands).

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

### Updates

Grinch checks for updates automatically. To check now, choose **Check for
Updates…** from the menu bar. You can turn automatic checks off with **Check
for Updates Automatically** in the same menu.

### With Homebrew

A community-maintained cask is available from
[`bric3/tap`](https://github.com/bric3/homebrew-tap/blob/main/Casks/grinch.rb):

```sh
brew tap bric3/tap
brew install --cask grinch
```

The cask is maintained outside the Grinch project.

### From source

Requires Rust stable via rustup. Put `~/.cargo/bin` first on `PATH`; see
[the contributor prerequisites](CONTRIBUTING.md#prerequisites) to verify the tools.

```sh
git clone https://github.com/jamtur01/grinch
cd grinch
make run
```

The default source build doesn't include automatic updates.

Pass `UNIVERSAL=1` to `make build` to produce a fat binary locally
(needs both rustup targets: `rustup target add aarch64-apple-darwin
x86_64-apple-darwin`).

### Set up your first config

1. If you already have a Grinch config, edit that file. Otherwise create
   `~/.config/grinch.js` with the example below, creating `~/.config` first
   if needed. Replace the browser targets if you use different apps.
   If your changes don’t take effect, see [which config Grinch uses](#if-grinch-uses-a-different-config).
2. Launch Grinch from Applications. Its eyes appear in the menu bar.
3. In **System Settings → Desktop & Dock → Default web browser**, select
   **Grinch**. Links opened by other apps now go through your rules.
4. After changing the config, choose **Reload Config** from the Grinch menu.
   If a reload fails, Grinch keeps the last successfully loaded config.

```js
module.exports = {
  default: "com.apple.Safari",
  rules: [
    { match: "github.com", open: "com.google.Chrome" },
  ],
};
```

This sends `github.com` and its subdomains to Chrome, and everything else to
Safari. The apps you name must be installed. Grinch can open other apps too,
such as Zoom or Slack; a “browser” target can be any installed app that can
handle the URL.

To check a decision without opening a browser, run:

```sh
/Applications/Grinch.app/Contents/MacOS/Grinch --test "https://github.com/"
```

Other examples below use `Grinch` as shorthand for that binary. For an installed
app, run this in your shell first, or use the full path in each command:

```sh
alias Grinch='/Applications/Grinch.app/Contents/MacOS/Grinch'
```

Only use configuration files you trust: Grinch evaluates them as JavaScript.

## Configuration

Grinch receives a URL, applies every matching [rewrite](#url-rewrites) in
order, and then checks [rules](#rules) from top to bottom. The first matching
rule selects a [browser or app](#browser-specs). If none matches, `default`
selects the destination. A rule or default can also suppress a URL. A dynamic
`open` function that returns `null` or `undefined` falls through to the next
rule.

Start with the example above. The full config can also define named browsers,
rewrites, and options:

```js
module.exports = {
  default: "com.apple.Safari", // required; use null to open nothing
  browsers: {},               // optional named browser targets
  rewrite: [],                // optional URL transformations
  rules: [],                  // optional routing rules
  options: {},                // optional app and logging settings
};
```

[`examples/grinch.example.js`](examples/grinch.example.js) is a feature
reference with more examples.

### Rules

`rules` is an array. Each rule has a `match` condition and an
`open` destination. Grinch checks them in order and stops at the first match:

```js
rules: [
  { match: "github.com", open: "com.google.Chrome", name: "work" },
  { match: "example.com", open: null }, // suppress this URL
],
```

A rule can also specify `url` to rewrite a matching URL before opening it.
See [URL rewrites](#url-rewrites) for the supported return values.
Unlike a literal `open: null`, a function that returns `null` or `undefined`
does not suppress the URL: Grinch continues to the next rule.

Each rule entry accepts an optional **`name`** string. It doesn't affect
routing — it labels the rule in `Grinch --list-rules` output and in the
`matchedRule.name` field of diagnostic-log `resolve` events. Useful when chasing
"why did this click go there?" through a config with a dozen fn matchers,
since the auto-derived label for fn rules is just the first line of
`f.toString()`.

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
| `/regex/` | regex against the full URL | Supports the `i` and `m` flags. |
| `(url, ctx) => bool` | a user-defined condition | See [the context object](#the-ctx-object). |

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

For an exact hostname, use `finicky.matchHostnames("github.com")` or a
function such as `(url) => url.hostname === "github.com"`. A regular
expression is tested against the full URL, not just the hostname.

If you want subdomain matching across multiple hosts at once:

- `domain("github.com", "gitlab.com")` — Grinch's helper, matches each host
  AND its subdomains, compiled to a single byte-level check.

Helper return values like `domain(...)`/`from(...)`/`running(...)`/`strip(...)`
are *data*, not functions — Grinch recognises the marker shape at config-load
and compiles to native Rust matchers/rewriters. The JS bridge is only crossed
on the hot path for user-written `(url, ctx) => ...` predicates.

### Browser specs

A browser target tells Grinch where to open a link. Most of the time, use the
app's name or bundle ID in `default` or a rule's `open`:

```js
{ match: "github.com", open: "Google Chrome" },
{ match: "zoom.us", open: "us.zoom.xos" },
```

The first rule opens GitHub in Chrome; the second sends Zoom links to the Zoom
app. Use `Grinch --list-browsers` to find the bundle IDs of apps registered to
open web links.

#### Choose a profile

If you use separate work and personal profiles, specify the profile by name:

```js
{ match: "github.com", open: { name: "Google Chrome", profile: "Work" } },
```

Grinch supports profiles in Chromium-based browsers, including Chrome, Brave,
Edge, Arc, Vivaldi and Chromium, and Firefox-based browsers, including Firefox,
LibreWolf, Waterfox and Zen. `"Google Chrome:Work"` is a shorter form for a
literal target. For Chrome, `"Default"` and `"Profile N"` can also identify a
profile by its directory name. Dynamic functions returning strings don't use
the `Name:Profile` shorthand.

If a Chrome profile can't be found, Grinch won't open the link in a potentially
wrong profile. Check the profile name and **Open Diagnostic Log**, then choose
**Reload Config** after fixing the name or the browser's profile data. If you
need the directory name, open `chrome://version` in that profile and look at
**Profile Path**. Firefox profile names are passed to Firefox even if Grinch
can't verify them; Firefox's newer profile-group names aren't resolved by Grinch.

#### Change how a link opens

Use an object when a link needs a profile or another launch option:

```js
{ match: "private.example.org", open: { name: "Google Chrome", incognito: true } },
{ match: "news.example.com", open: { name: "org.mozilla.firefox", openInNewWindow: true } },
```

`incognito` and `openInNewWindow` work with supported Chromium and Firefox
browsers; Safari doesn't support these options. Set `openInBackground: true`
to open a link without activating the target app. You can also pass browser
command-line flags with `args: ["--incognito"]`.

To open a particular app bundle, use its path, for example
`"/Applications/Foo.app"` or `{ name: "~/Apps/Foo.app", appType: "path" }`.
Use `{ name: "com.example.Browser", appType: "bundleId" }` to treat a name as
a bundle ID without an app-name lookup. A target of `null` or
`{ appType: "none" }` opens nothing. A function can choose a target dynamically;
a rule's function returning `null` or `undefined` tries the next rule.

#### Reuse a target

Give targets names if several rules should use the same browser or profile:

```js
const browsers = {
  personal: "com.apple.Safari",
  work: { name: "com.google.Chrome", profile: "Default" },
};

module.exports = {
  default: "personal",
  browsers,
  rules: [{ match: "github.com", open: "work" }],
};
```

The exported `browsers` map makes `"personal"` and `"work"` available by
name. You can also use the JavaScript reference `browsers.work` directly.

<details>
<summary>More about resolving browser profiles</summary>

Chrome display names are looked up in the browser's `Local State` file.
`Default` and `Profile N` (ASCII digits) are explicit directory names and need
no profile-file access. Other values must match a display name or directory key
in `Local State`. An unreadable file or unknown name records a `profile_error`
and suppresses the launch, including for dynamic targets; Grinch does not try a
later rule or the default. Reloading the config clears cached profile data and
read failures. Check file permissions if access is denied; Full Disk Access is a
broader workaround, not a requirement for every profile problem.

Firefox profiles use names from `profiles.ini`. Grinch logs unreadable files
and unknown names when an inventory is available, but still passes the name to
Firefox. Other browsers ignore `profile` with a warning. The full supported
browser-family lists are in `src/chromium.rs` and `src/firefox.rs`.

</details>

### URL rewrites

`rewrite` is an array of transformations applied before routing. Every
matching rewriter applies, in order. For example, this removes common tracking
parameters before the rules see the URL:

```js
rewrite: [strip("utm_*", "fbclid")],
```

The full set of forms is:

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
- **Proofpoint URL Defense v3** — `urldefense.com/v3/__<encoded>__;<marker>!!…` (plus the FedRAMP `urldefense.us` tenant) — uses `*` placeholders with a base64-URL replacement stream and `**X` run-length markers for runs of 2–65

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

A `url` rewrite function receives a URL instance as its first argument. If it
declares a second argument, it also receives [context](#the-ctx-object). It
can return:

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

### The `ctx` object

User-written matcher, rewrite, and browser functions receive the current URL
as their first argument. To receive `ctx` as the second argument, explicitly
declare two parameters, such as `(url, ctx) => ...`. A default parameter such
as `(url, ctx = {}) => ...` does not request context because JavaScript reports
its function length as one.

For example, this rule opens links from Mail in Safari:

```js
{ match: (url, ctx) => ctx.opener?.bundleId === "com.apple.mail",
  open: "com.apple.Safari" },
```

`ctx` contains:

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

Grinch uses the Apple Event's sender PID to identify `ctx.opener` when
available. If the sender cannot be identified, it falls back to the frontmost
app; if that also fails, `ctx.opener` is `null`. The fallback may not identify
the original sender. Always guard with
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
| Match exact hostname only | `(url) => url.hostname === "github.com"` | `finicky.matchHostnames("github.com")` |
| Match by opener bundle ID | `from("com.microsoft.Outlook")` | `(url, ctx) => ctx.opener?.bundleId === "..."` |
| Match if app is running | `running("us.zoom.xos")` | `finicky.isAppRunning("Zoom")` |
| Read modifier keys | `(url, ctx) => ctx.modifiers.shift` | `finicky.getModifierKeys()` |
| Read opener metadata | `ctx.opener.{bundleId, name, path, windowTitle}` | (same — `ctx.opener` is shared) |

`console.log/warn/error/info/debug` are wired to stderr with a
`grinch [level]:` prefix — call them from anywhere in your config to
trace why a rule did or didn't fire. Objects are JSON-stringified for
inspection-style debugging.

### Options

The `options` block accepts Finicky v4's keys plus Grinch's log settings:

- **`hideIcon: true`** — skip the menu-bar status item at app launch.
  Useful when you don't want Grinch's icon in your menu bar. Reloads don't
  toggle the icon mid-session; restart Grinch to apply. Automatic update
  checks still run without the icon. Reopen the running app from Spotlight or
  Finder to show a window with config, log, and update actions.
- **`checkForUpdates: false`** — turn off the daily update check (`true`
  turns it on). While the key is set, it overrides the **Check for Updates
  Automatically** menu item, which is greyed out. Without the key, the menu
  item decides. Grinch saves the value as its own preference, so after you
  remove the key the menu item starts from the last value the config set.
- **`logRequests: true`** — add a `resolve` event for every routed URL to
  the app's diagnostic log. Useful for figuring out *why* a particular
  click went where it did without enabling the broader `GRINCH_DEBUG=1`
  stderr trace.

  Pair with **`logRotateBytes: <n>`** and/or **`logRotateDays: <n>`** to
  cap the diagnostic log's growth. Rotation renames the current file to
  `<original-name>.log.<iso-timestamp>` and starts a fresh empty file.
  Same-second collisions append `.1`, `.2`, etc., preserving earlier archives.
  Both triggers can be combined (whichever fires first wins). Default:
  no rotation, file grows until you delete it.
- **`logFormat: "text"`** — write readable diagnostic events. The default,
  `"json"`, writes one JSON object per line. Invalid values leave the current
  format in place, record a config error, and print a warning; routing continues.
  An omitted, `null`, or `undefined` value selects the default JSON format.
  `Grinch --validate` shows the warning but still exits 0 for a valid config.

The compatibility options `urlShorteners` (expects
[external expansion](#working-with-url-shorteners)) and `keepRunning`
(Grinch is always resident) are accepted but have no effect.
Unknown keys log a one-line warning.

## Menu bar

Click the Grinch eyes in the menu bar:

| Item | Action |
|---|---|
| **Grinch X.Y.Z** | Disabled label at the top showing the running binary's version. Matches `Grinch --version`. |
| **Open Config** (⌘O) | Opens the active config file in your default `.js` handler (VS Code / Cursor / etc.). |
| **Reload Config** (⌘R) | Re-evaluates the config without relaunching. Equivalent to `kill -HUP $(pgrep -f Grinch.app/Contents/MacOS/Grinch)`. |
| **Open Diagnostic Log** | Opens the current diagnostic log. |
| **Start at Login** | Toggles `SMAppService.mainApp` registration. Off by default; the entry also appears in System Settings → General → Login Items so users can disable it from there. |
| **Check for Updates…** | Checks the latest release now and shows the result. See [Updates](#updates). |
| **Check for Updates Automatically** | Toggles the daily update check. Greyed out while `options.checkForUpdates` is set. |
| **Quit Grinch** (⌘Q) | Exit. |

At launch, Grinch terminates other running instances with the same bundle
identifier before installing its menu bar item, preventing duplicate instances.

When `hideIcon: true` hides the menu-bar icon, reopen the running Grinch app
from Spotlight or Finder to access **Open Config**, **Reload Config**,
**Open Diagnostic Log**, and **Check for Updates…**. The window also shows
config and profile errors. The update button is disabled when the updater is
unavailable or cannot currently check for updates. Closing the window leaves
Grinch running in the background.

If a reload fails (syntax error, unreadable file, missing `default`),
the menu bar icon flips to **⚠️** and a non-clickable "Config error:
…" item appears at the top of the menu with the first line of the
failure. The previous engine stays in place so routing keeps working
until the config is fixed; the next successful reload restores the Grinch
icon. The full failure is also written as a `config_error` event in the
diagnostic log.

### If Grinch uses a different config

If your edits don't take effect, choose **Open Config** from the menu to see
which file Grinch loaded. It uses the first existing file in this order:

1. `~/.grinch.js`
2. `~/.config/grinch.js`
3. `~/.config/grinch/grinch.js`
4. `/Library/Application Support/Grinch/grinch.js` (a system-wide fallback)

Edit the active file, then choose **Reload Config**. If it fails, the menu
shows the error and Grinch keeps the last successfully loaded config.
`Grinch --validate` can also report config errors and warnings in a terminal.

## Diagnostic log

Grinch keeps a diagnostic log per app launch at
`~/Library/Logs/Grinch/Grinch_<timestamp>.log`. Config-load errors,
runtime JavaScript exceptions, profile errors, and launch failures are recorded even when
`logRequests` is false; that option controls only the higher-volume `resolve`
events. The file opens
lazily on the first event or when you click **Open Diagnostic Log**. Rotation
settings take effect after the JavaScript config is evaluated and its options
are parsed, even if later engine construction fails. Changing `logFormat`
starts a new log file; earlier events remain in the previous file. Errors
before the first config's options are parsed use JSON. The menu opens the
current file. Files created by format changes use suffixes such as `.1.log`
and `.2.log` in place of the initial `.log` suffix.

Text format begins each event with the local date, UTC offset, and event type,
followed by its fields:

```text
2026-05-11T12:57:25-0400  resolve
  args: ["--profile-directory=Profile 10"]
  browser: com.google.Chrome
  final: https://example.com/
  matchedRule:
    index: 11
    name: shift-override
  modifiers:
    command: false
    control: false
    option: false
    shift: true
  opener:
    bundleId: com.tinyspeck.slackmacgap
    name: Slack
    pid: 731
  rewritten: false
  strategy: launch_new_instance
  url: https://example.com/
```

JSON format also includes the numeric Unix timestamp:

```json
{
  "date": "2026-05-11T12:57:25-0400",
  "event": "resolve",
  "ts": 1778518645.634,
  "url": "https://example.com/",
  "final": "https://example.com/",
  "rewritten": false,
  "browser": "com.google.Chrome",
  "args": ["--profile-directory=Profile 10"],
  "strategy": "launch_new_instance",
  "opener": {
    "bundleId": "com.tinyspeck.slackmacgap",
    "name": "Slack",
    "pid": 731
  },
  "modifiers": {
    "shift": true,
    "option": false,
    "command": false,
    "control": false
  },
  "matchedRule": {"index": 11, "name": "shift-override"}
}
```

Field notes:

- `date` — local date and time with UTC offset; `ts` remains Unix seconds.
- `event` — `resolve`, `config_error`, `runtime_js_error`, `profile_error`, or `launch_error`.
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

Config and JavaScript error events include the config path, when one was found, and the captured
JavaScript message. For example, this JSON event:

```json
{"date":"2026-05-11T12:57:25-0400","event":"config_error","message":"SyntaxError (line 4)","path":"/Users/alex/.grinch.js","ts":1778518645.634}
```

The same event in text format:

```text
2026-05-11T12:57:25-0400  config_error
  message: SyntaxError (line 4)
  path: /Users/alex/.grinch.js
```

`launch_error` records invalid URLs, missing browsers, and failures reported by
macOS after a launch request. It includes `url`, `browser`, `strategy`, and
`message`; native errors also include `errorDomain` and `errorCode`. These
events share the app's log and rotation settings, including after config reloads.
A `resolve` event records the routing decision; it does not confirm launch success.

Logs can contain complete URLs, including authentication tokens and personal data.
Redact them before sharing; report suspected vulnerabilities through the
[security policy](SECURITY.md).

`console.log/warn/error/info/debug` remain on stderr with their existing
`grinch [level]:` prefix; they are not copied into the diagnostic log.

`profile_error` events include the browser bundle ID, profile-data file path
(when available), and an actionable message. They report unreadable profile
files and unresolved profile names through **Open Diagnostic Log**.
Only launch-suppressing Chromium errors affect the status item; Firefox's
pass-through warnings remain in the diagnostic log. The first suppressing
profile error after a config load changes the menu-bar icon to
**⚠️** and displays **Profile error: …**, including errors first discovered by
dynamic browser targets on a click. Hover over the error for the full message
or open the diagnostic log. A config-load error takes precedence in the menu;
a successful reload clears the profile warning unless a profile error recurs.

## SSO / OAuth popups

Apps that use `ASWebAuthenticationSession` for sign-in send their URLs through
AuthenticationServices rather than ordinary URL delivery. Grinch routes these
requests using the same browser rules as other links.

Grinch opens the initial authentication URL in the browser selected by your
rules as a normal tab. It does not transfer the authentication session to that
browser or observe its subsequent navigation.

Requests received during startup wait for the initial configuration load attempt.
If configuration is unavailable, routing suppresses the URL, or the browser cannot
be opened, Grinch finishes the authentication request with an error. Requests
canceled during startup do not launch a browser.

**Callback completion is best effort.** A callback must be delivered back to
Grinch for it to complete the pending session. Grinch matches incoming callback
URLs against pending requests and returns matching URLs to the originating app.
Registering a callback with `ASWebAuthenticationSession` does not make Chrome
send it to Grinch. Apps that rely solely on the session callback can remain
waiting after sign-in, even though the authentication page opened successfully.

On macOS 13 through 14.3, callback matching uses the legacy custom
scheme API. HTTPS host/path callbacks require macOS 14.4 or later.

The handler installs in `applicationWillFinishLaunching:`, before the
runloop accepts events, so a request delivered during launch itself
(third-party app calls into AuthenticationServices while Grinch is
starting) doesn't fall through to Safari while we're still booting.
Grinch also declares `CallbackURLMatchingIsSupported: true` —
some auth clients (notably Slack) gate SSO routing on that capability
and skip browsers that don't advertise it.

**Caveats.** Grinch declares `IsSupported: true` but not
`EphemeralBrowserSessionIsSupported`: we route to the user's regular
browser tab, which inherits their existing cookies and profile
state, so the isolation Apple promises for
`shouldUseEphemeralSession`-flagged requests isn't actually
delivered. macOS may handle requests for ephemeral sessions in Safari instead;
those requests bypass Grinch's routing rules.

## Chrome opens a link in a new window

When a rule selects a Chrome profile, Chrome can open a new window even if
that profile already has a normal window. This can happen if you most recently
used a Chrome web app, popup, or DevTools window for that profile.

To reuse an existing window, activate an ordinary Chrome window for that profile
before opening the link. This is tracked in the
[Chromium issue for this behavior](https://issues.chromium.org/issues/541287632).

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
redirects from bit.ly, t.co, goo.gl, lnkd.in, ow.ly, buff.ly, and tinyurl.com
(including their `www.` forms), with at most ten requests in five seconds.
It opens the first destination outside those hosts through `open(1)`, leaving
login redirects and cookies to your browser. Grinch routes that destination
normally. Unknown hosts pass through; expansion failures retain the original URL.

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
| `--list-rules` | Print rule indices, labels, and targets. |
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

A third shape, `grinch://open/<base64-url>`, accepts a `btoa()`-encoded
inner URL. This is the envelope shape Finicky's published Chrome and
Firefox addons emit; pointing those addons at `grinch://open/…` instead
of `finicky://open/…` makes the "Open with Finicky" right-click menu
route through Grinch without it needing to be the default browser. Both
the standard (`+`/`/`) and URL-safe (`-`/`_`) base64 alphabets are
accepted, with optional `=` padding.

## Performance

Release builds on Apple Silicon, median of ten runs. These benchmarks measure
URL routing, excluding browser launch and page loading.

Run `bench/run.sh` to reproduce. Workloads are in `bench/configs/`; setup and
full results are in the [benchmark notes](bench/README.md).

### Hot path (declarative-only configs)

Workloads that hit the rules-array — domain matchers, regex, wildcards.
No JS bridge crossings; the URL string is borrowed (`Cow::Borrowed`)
for the entire resolve when no rewrite fires; `quick_host` is skipped
when the config has no host-using matcher and borrows the host slice
when it's already lowercase ASCII.

| Workload | ns/op |
|---|---:|
| Floor: empty rules, no rewrite | 6.6 |
| Default fallback, no query | 64.2 |
| Default fallback, strip removes a param | 179.5 |
| Bare-hostname match (`"github.com"`) | 39.6 |
| `domain()` match | 46.1 |
| Regex match | 23.1 |
| Wildcard match (`"zoom.us/j/*"`) | 31.6 |
| 50 bare-hostname rules, last one matches | 268.1 |

### Slow path (configs with `(url, ctx) => …` fn matchers)

User-written predicates and rewrites run in JavaScriptCore. Functions that
explicitly declare a second parameter also request the context object and its
opener and modifier information. Use these functions when a declarative matcher
or rewrite does not express the rule you need.

| Workload | ns/op |
|---|---:|
| Native rule wins early (no fn fires) | 40.1 |
| Drop URL via `() => null` (url-only) | 2,422.9 |
| HTTP→HTTPS via URL mutation (url-only) | 3,955.6 |
| `?browser=` dynamic open fn (url-only matcher) | 4,539.8 |
| 4 fn matchers reading `ctx.opener` | 5,454 |
| Full Slack-web → `slack://` rewrite | 5,839.3 |

### Footprint

These are historical measurements, not a comparison of the latest releases.
Memory use varies with configuration and workload.

| | Grinch | Finch | Finicky |
|---|---:|---:|---:|
| Resident memory | 15.5 MB | 14.6 MB | 142.5 MB |
| Peak memory | 16.6 MB | 15.5 MB | 391.2 MB |
| JS engine | system JSC | system JSC | bundled goja |
| Bundled UI | menu bar only | menu bar only | WebView config app |

Finicky bundles a WebKit instance for its config UI, which accounts for
the bulk of its memory footprint.

## Differences from Finicky

Grinch supports many **Finicky v4** configs. It accepts `export default` as
well as `module.exports`; `defaultBrowser`, `handlers`, and `browser` are
aliases for `default`, `rules`, and a rule's `open`. Finicky v3 configs are
best ported through
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
   if you need it — see [Working with URL shorteners](#working-with-url-shorteners).
3. **`finicky.*` namespace is shipped without `notify`, `getPowerInfo`, or
   `getBattery`.** Calling one of those throws a `TypeError`: at the top
   level of a config that fails the load; inside a matcher or `open` fn the
   rule is skipped and a `runtime_js_error` event is logged. Five v4 methods
   are present, plus one Grinch-specific helper:
   - `finicky.matchHostnames(matchers)` — exact-hostname matcher fn
     (Finicky-compatible). For subdomain matching use Grinch's `domain(...)`.
   - `finicky.matchDomains(matchers)` — deprecated alias, warns and delegates.
   - `finicky.getModifierKeys()` — real values from CG event flags
     (shift/option/command/control/capsLock/fn/function).
   - `finicky.isAppRunning(id)` — matches against bundle ID OR localized name.
   - `finicky.getRunningBrowsers()` — Grinch-specific helper.
     Returns an array of currently-running known-browser bundle IDs in family-
     table order. Use with `Array.prototype.find` for first-running-of-preference
     routing: `prefs.find(b => running.includes(b)) || "com.apple.Safari"`.
   - `finicky.getSystemInfo()` — `{localizedName, name}` from `[NSHost currentHost]`.
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

## Support and security

Only the [latest published release](https://github.com/jamtur01/grinch/releases/latest)
is supported for issues and security fixes. Upgrade and reproduce the problem
before filing a bug; older releases do not receive backported fixes.

For bug reports and feature requests, see [CONTRIBUTING.md](CONTRIBUTING.md).
Report vulnerabilities privately using [SECURITY.md](SECURITY.md), not a public issue.

## License

MIT — see [LICENSE](LICENSE).
