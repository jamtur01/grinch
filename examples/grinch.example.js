// Grinch browser router — exhaustive syntax reference.
// https://github.com/jamtur01/grinch
//
// This file is intended as a feature inventory: every supported matcher,
// rewriter, browser-spec form, and helper has at least one example. For a
// minimal real-world config, copy what you need.
//
// Reload after editing:  kill -HUP $(pgrep -f Grinch.app/Contents/MacOS/Grinch)

// ---------- Browser specs ----------

const browsers = {
  // Bare-string forms — auto-detected (looks like a bundle ID = used directly,
  // looks like an app name = looked up via NSWorkspace).
  zen: "app.zen-browser.zen",                               // bundle ID
  chrome: "Google Chrome",                                  // app name
  firefox: "Firefox",                                       // app name

  // Object form — `name` accepts either; `id`/`bundleId` are explicit.
  brave: { name: "com.brave.Browser" },
  arc: { id: "company.thebrowser.Browser" },

  // Finicky-compatible "Name:Profile" string shorthand. Splits on the
  // first `:` for Chromium-family browsers; equivalent to
  // { name: "Google Chrome", profile: "Work" }.
  workShort: "Google Chrome:Work",

  // `profile` shorthand. Chromium-family expands to --profile-directory=<dir>;
  // Firefox-family expands to -P <name>. Recognised for Chrome, Brave, Edge,
  // Vivaldi, Arc, Opera, Chromium (Chromium family) and Firefox, Firefox
  // Developer Edition, Firefox Nightly, Waterfox, LibreWolf (Firefox family).
  personal: { name: "Google Chrome", profile: "Personal" },
  work: { name: "Google Chrome", profile: "Work" },
  firefoxWork: { name: "org.mozilla.firefox", profile: "Work" },

  // Custom launch args.
  incognito: { name: "Google Chrome", args: ["--incognito"] },

  // Open in the background (don't activate the target app).
  spotifyBackground: { name: "com.spotify.client", openInBackground: true },

  // appType: "path" — point at an .app bundle directly. Useful for
  // browsers outside /Applications or not registered with LaunchServices.
  custom: { name: "/Applications/MyBrowser.app", appType: "path" },

  // appType: "none" — explicit no-op browser. Same effect as `open: null`,
  // but can be referenced by key from rules below.
  doNothing: { appType: "none" },
};

// Apps too — same forms.
const apps = {
  zoom: "us.zoom.xos",
  slack: "com.tinyspeck.slackmacgap",
  appStore: "com.apple.AppStore",
  spotify: "com.spotify.client",
};

module.exports = {
  // ---------- Default browser (required) ----------
  // Either `default` (Grinch) or `defaultBrowser` (Finicky alias) works.
  // Accepts any browser-spec form including a fn — a dynamic default
  // is invoked at resolve time when no rule matched, e.g.:
  //   default: (url, ctx) =>
  //     ctx.modifiers.shift ? browsers.work : browsers.personal,
  default: browsers.personal,

  // Top-level browsers map — looked up by key in `open: "<key>"`.
  browsers: browsers,

  // ---------- Rewrites — applied in order, every match fires ----------
  rewrite: [
    // strip(): remove query params. Trailing `*` is a prefix wildcard.
    // Returns a marker object compiled to native byte-level filtering.
    strip("utm_source", "utm_medium", "utm_campaign", "utm_term",
          "utm_content", "fbclid", "gclid", "mc_eid", "ref", "referrer"),
    strip("utm_*"),

    // safelinks(): unwrap Microsoft Defender / Teams / Proofpoint
    // (v2 + v3, including FedRAMP `urldefense.us`) URL wrappers in one
    // line. Pass-through on every other host, so it's safe to leave on.
    // Composes with strip() — put safelinks() first so utm_* cleanup
    // runs on the inner URL, not the wrapper.
    safelinks(),

    // teams_launcher(): unwrap Teams' web-app launcher URLs (the
    // `teams.microsoft.com/dl/launcher/launcher.html?url=…` form that
    // produces a one-tab-per-click stub) into their canonical
    // `msteams:` deep links. Sits alongside safelinks() because the
    // launcher is path-routed (no `?url=`-style query wrapper), so
    // safelinks() doesn't catch it.
    teams_launcher(),

    // Conditional literal rewrite.
    {
      match: "old.example.com/*",
      url: "https://new.example.com/",
    },

    // Conditional fn rewrite — return string.
    {
      match: ["*.medium.com/*", "medium.com/*"],
      url: (url) => "https://scribe.rip" + url.pathname + url.search,
    },

    // URL-instance mutation. The polyfill rebuilds .href on next read after
    // .protocol/.hostname/.pathname/.searchParams updates.
    {
      match: (url) => url.protocol === "http:",
      url: (url) => { url.protocol = "https:"; return url; },
    },

    // Hostname rewrite via mutation.
    {
      match: (url) => url.hostname.endsWith("buildinglink.com"),
      url: (url) => { url.hostname = "7metrotech.buildinglink.com"; return url; },
    },

    // searchParams mutation propagates back into .search and .href.
    {
      match: "*.youtube.com/watch*",
      url: (url) => {
        url.searchParams.delete("list");
        url.searchParams.delete("index");
        return url;
      },
    },

    // Return a `{protocol, host, pathname, search, hash}` object — Grinch
    // concatenates the fields. Useful for cross-scheme rewrites.
    {
      match: ["statics.teams.cdn.office.net/evergreen-assets/safelinks/*"],
      url: (url) => {
        const inner = url.searchParams.get("url");
        return inner ? new URL(decodeURIComponent(inner)) : url;
      },
    },
    {
      match: "*.slack.com/archives/*",
      url: (url) => {
        const m = /\/archives\/(?<channel>\w+)(?:\/(?<msg>p\d+))?/.exec(url.pathname);
        if (!m) return url;
        let search = "team=" + (url.hostname.split(".")[0]);
        search += "&id=" + m.groups.channel;
        if (m.groups.msg) {
          const t = m.groups.msg;
          search += "&message=" + t.slice(1, 11) + "." + t.slice(11);
        }
        return { protocol: "slack", host: "channel", pathname: "", search: search };
      },
    },

    // Microsoft Teams meeting/channel/file links → msteams: scheme so
    // they open in the native Teams app instead of the web client.
    //
    // **Subtle:** Teams' custom URI scheme is single-slash
    // (`msteams:/l/meetup-join/…`), NOT double-slash, because the OS
    // treats it as opaque (like `mailto:` or `tel:`). To produce that
    // shape, return a plain object with ONLY `protocol` + `pathname`
    // (+ optional `search`). DON'T spread the input URL:
    //
    //   ❌ url: (url) => ({ ...url, protocol: 'msteams' })
    //       // → msteams://teams.microsoft.com/l/…  (broken double-slash)
    //   ❌ url: (url) => { url.protocol = 'msteams'; return url; }
    //       // → same; in-place mutation keeps the original authority
    //   ✅ url: (url) => ({ protocol: 'msteams',
    //                       pathname: url.pathname,
    //                       search: url.search })
    //       // → msteams:/l/…  (correct single-slash opaque form)
    //
    // The `{protocol, pathname}` LegacyURLObject shape triggers
    // Grinch's "no authority → emit `scheme:path` not `scheme://path`"
    // path. The spread / mutate forms carry `hostname` through and
    // produce double-slash output.
    {
      match: ["teams.microsoft.com/l/*", "*.teams.microsoft.com/l/*"],
      url: (url) => ({
        protocol: "msteams",
        pathname: url.pathname,
        search: url.search,
      }),
    },

    // Return null/undefined to drop the URL entirely.
    {
      match: (url) => url.hostname === "tracking.example.com",
      url: () => null,
    },
  ],

  // ---------- Rules — first match wins ----------
  // Order matters: put more-specific patterns first; broader catch-alls last.
  rules: [
    // ----- Match types -----

    // Regex literal — matched against the full URL. Honours the JS `i` and
    // `m` flags from the literal (matches Finicky / native RegExp.test);
    // omit /i if you want case-sensitive matching. Placed before the bare-
    // hostname `github.com` rule so path-specific patterns win over the
    // broader hostname match.
    { match: /github\.com\/(paymentology|tutuka)\//i, open: browsers.work },
    { match: /\.(figma|notion)\.(com|so)/i,           open: browsers.work },

    // Wildcard string with `*`. Implicitly anchored; `(?:https?:)?(?://)?` is
    // prepended unless the pattern starts with `*` or a protocol prefix.
    { match: "zoom.us/j/*", open: apps.zoom },
    { match: "*.zoom.us/j/*", open: apps.zoom },

    // Protocol-anchored wildcard.
    { match: "slack:*", open: apps.slack },
    { match: "mailto:*", open: "com.apple.mail" },

    // Array of matchers — OR semantics.
    {
      match: ["stackoverflow.com", "stackexchange.com", "*.stackexchange.com/*"],
      open: browsers.personal,
    },

    // domain() helper — same as bare strings but multiple at once.
    {
      match: domain("paymentology.atlassian.net", "tutuka.atlassian.net",
                    "datadoghq.com", "miro.com", "pagerduty.com"),
      open: browsers.work,
    },

    // domain() — matches an exact hostname or any subdomain. Compiles to
    // native byte-level checks at config load.
    {
      match: domain("gist.github.com"),
      open: browsers.personal,
    },
    {
      match: domain("open.spotify.com"),
      open: apps.spotify,
    },

    // Bare hostname string — matches github.com AND any subdomain
    // (api.github.com, gist.github.com, ...). Placed after the regex /
    // wildcard rules so they take precedence on overlapping URLs.
    //
    // **Differs from Finicky v4**: there, a bare string is matched as
    // an exact `===` on `url.href` (the full URL), so `match: "github.com"`
    // would never fire. Use `finicky.matchHostnames("github.com")` if
    // you specifically need the Finicky-style exact-hostname match.
    { match: "github.com", open: browsers.personal },

    // Finicky-style exact-hostname matcher: does NOT match subdomains.
    // `api.github.com` and `gist.github.com` would NOT route here.
    { match: finicky.matchHostnames("news.ycombinator.com"), open: browsers.personal },

    // from() — match the bundle ID of the app that opened the URL.
    //
    // The optional `name:` field labels this rule in --list-rules output
    // and in matchedRule.name of the logRequests JSONL. Doesn't affect
    // routing. Especially handy for fn matchers, whose auto-derived
    // label is just the first line of `f.toString()`.
    {
      match: from("com.microsoft.Outlook"),
      open: browsers.work,
      name: "outlook-to-work",
    },

    // running() — match if any of these apps is currently running. Lazy:
    // the running-apps set is built on first use within a resolve and cached.
    {
      match: ["*.zoom.us/*", running("us.zoom.xos")],
      open: apps.zoom,
    },

    // ----- Predicate fns and ctx -----

    // Read query params.
    {
      match: (url) => url.searchParams.has("browser"),
      open: (url) => {
        switch (url.searchParams.get("browser")) {
          case "work": return browsers.work;
          case "private": return browsers.incognito;
          default: return browsers.personal;
        }
      },
    },

    // ctx.opener (bundleId, name, path). Optional-chain `?.` because
    // ctx.opener is null when the source app couldn't be detected (e.g.
    // a URL handed in via the command line); without `?.`, those clicks
    // would throw "undefined is not an object" and silently default-route.
    {
      match: (url, ctx) => ctx.opener?.bundleId === "com.microsoft.teams2",
      open: browsers.work,
    },
    {
      match: (url, ctx) => ctx.opener?.name === "Mail",
      open: browsers.personal,
    },
    {
      match: (url, ctx) => ctx.opener?.path.includes("/Visual Studio Code.app/"),
      open: browsers.work,
    },

    // ctx.opener.windowTitle (lazy — needs Accessibility permission).
    {
      match: (url, ctx) =>
        ctx.opener?.bundleId === "com.tinyspeck.slackmacgap" &&
        (ctx.opener?.windowTitle || "").includes("Convergint"),
      open: browsers.work,
    },

    // ctx.modifiers — read modifier-key state at the moment of the click.
    { match: (url, ctx) => ctx.modifiers.shift, open: browsers.work },
    { match: (url, ctx) => ctx.modifiers.option, open: browsers.incognito },

    // ctx.url is the original (input) URL — useful when a chain of rewrites
    // has changed the URL but you want to inspect what the user clicked.
    { match: (url, ctx) => ctx.originalUrl.startsWith("file://"), open: browsers.personal },

    // ----- Combined entries: match + url + open -----
    // When a rule has both `url` and `open`, the rewrite runs only if the
    // rule matches, then the rewritten URL is opened in the chosen browser.
    {
      match: ["itunes.apple.com/app/*", domain("apps.apple.com")],
      url: (url) => {
        if (url.hostname === "apps.apple.com") return url;
        return {
          protocol: url.protocol,
          host: "apps.apple.com",
          pathname: "/us" + url.pathname,
          search: "",
        };
      },
      open: apps.appStore,
    },

    // ----- Browser spec forms in rules -----

    // String key (looked up in top-level browsers map).
    { match: "news.ycombinator.com", open: "personal" },

    // Inline browser spec object.
    { match: "perplexity.ai", open: { name: "Google Chrome", args: ["--guest"] } },

    // Dynamic — fn returns any browser spec form.
    {
      match: "*.atlassian.net/*",
      open: (url) => {
        if (url.hostname.startsWith("paymentology.")) return browsers.work;
        return browsers.personal;
      },
    },

    // null = suppress (open nothing). Useful for intentional drops.
    { match: "annoying-tracker.example.com", open: null },

    // Combined entry, dropping the URL entirely (rule's `url:` returns null).
    {
      match: "internal-only.corp.example.com",
      url: () => null,
      open: null,
    },

    // Shorteners (bit.ly, t.co, lnkd.in, …): Grinch can't follow redirects
    // from inside resolve() — that's a network round-trip and resolve() is
    // sync by design (see "Performance" in the README). Two practical
    // options:
    //
    // 1. Accept it: route shorteners to a sensible default, knowing you
    //    can't decide on the *real* destination's host until after the
    //    browser opens it.
    {
      match: domain("bit.ly", "t.co", "lnkd.in", "ow.ly", "buff.ly", "tinyurl.com"),
      open: browsers.personal,
    },
    //
    // 2. Pre-expand outside Grinch: the companion script
    //    `examples/expand-shortener.sh` follows redirects via curl and
    //    re-opens the final URL through `open(1)`, so Grinch sees the
    //    expanded form and routes it through your normal rules.
    //
    //    Hook the script into your usual launcher (Raycast hotkey, Alfred
    //    workflow, Hammerspoon binding, Shortcuts.app Quick Action, etc.).
    //    See the script's header comment for setup notes.
  ],
};
