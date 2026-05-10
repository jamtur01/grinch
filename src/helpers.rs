// JS prelude evaluated once at config load.
//
// Everything here is engineered to keep the resolve() hot path in Rust:
// helpers return DATA (marker objects), not functions, so engine.rs
// translates them to native matchers/rewriters at config-load time.
// Bridge crossings on the hot path are only paid for user-written
// `(url, ctx) => ...` predicates and rewrites — the explicit slow path.

pub const JS_PRELUDE: &str = r##"
// ---------- Mutable URL polyfill (lazy searchParams, no DOM, no IDNA) ----------
//
// Backs the URL constructor used by user code. Mutating .protocol/.hostname/etc.
// is supported; href is rebuilt on access from the current field values.
// Accessors (href, host, search, searchParams) and toString/toJSON live on
// URL.prototype, defined once per JSContext at engine init. Construction is
// just `parseInto(this, href)` — no per-instance defineProperty cost on the
// slow path.
//
// **Note** for users iterating URL fields: prototype-defined accessors don't
// show up in `Object.keys(url)` or get copied by `Object.assign({}, url)`.
// The data fields (protocol, username, password, hostname, port, pathname,
// hash) are still own-enumerable. Use `url.href` to serialise.
(function(g) {
  if (g.URL && g.URL.__grinchPolyfill) return;

  var URL_RE = /^([a-z][a-z0-9+.-]*:)(?:\/\/(?:([^:@\/]*)(?::([^@\/]*))?@)?([^:\/?#]*)(?::(\d+))?)?([^?#]*)(\?[^#]*)?(#.*)?$/i;

  function parseInto(self, href) {
    var m = URL_RE.exec(href);
    if (!m) throw new TypeError("Invalid URL: " + href);
    self.protocol = m[1];
    self.username = m[2] || "";
    self.password = m[3] || "";
    self.hostname = m[4] || "";
    self.port     = m[5] || "";
    self.pathname = m[6] || "";
    self._search  = m[7] || "";
    self.hash     = m[8] || "";
    self.__sp = null;
  }

  function rebuildHref(u) {
    var p = u.protocol || "https";
    if (p && p.charAt(p.length - 1) !== ":") p += ":";
    var auth = "";
    if (u.username || u.password) {
      auth = u.username || "";
      if (u.password) auth += ":" + u.password;
      auth += "@";
    }
    var host = (u.hostname || "") + (u.port ? ":" + u.port : "");
    var search = u._search || "";
    if (search && search.charAt(0) !== "?") search = "?" + search;
    var hash = u.hash || "";
    if (hash && hash.charAt(0) !== "#") hash = "#" + hash;
    return p + "//" + auth + host + (u.pathname || "") + search + hash;
  }

  function makeSearchParams(u) {
    var sp = { _m: {} };
    var s = u._search;
    if (s && s.length > 1) {
      var pairs = s.slice(1).split("&");
      for (var i = 0; i < pairs.length; i++) {
        if (!pairs[i]) continue;
        var kv = pairs[i].split("=");
        var k = decodeURIComponent(kv[0]);
        var v = kv[1] ? decodeURIComponent(kv[1].replace(/\+/g, ' ')) : "";
        (sp._m[k] = sp._m[k] || []).push(v);
      }
    }
    function serialize() {
      var p = [], k, i;
      for (k in sp._m) for (i = 0; i < sp._m[k].length; i++)
        p.push(encodeURIComponent(k) + "=" + encodeURIComponent(sp._m[k][i]));
      return p.join("&");
    }
    function commit() {
      var str = serialize();
      u._search = str ? "?" + str : "";
    }
    sp.get      = function(k) { return sp._m[k] ? sp._m[k][0] : null; };
    sp.getAll   = function(k) { return sp._m[k] ? sp._m[k].slice() : []; };
    sp.has      = function(k) { return !!sp._m[k]; };
    sp.set      = function(k, v) { sp._m[k] = [String(v)]; commit(); };
    sp.append   = function(k, v) { (sp._m[k] = sp._m[k] || []).push(String(v)); commit(); };
    sp.delete   = function(k) { delete sp._m[k]; commit(); };
    sp.toString = serialize;

    // Iteration: WHATWG URLSearchParams returns an iterator that yields
    // pairs in insertion order, with multi-value keys yielding once per
    // value. We materialise the list eagerly because the underlying
    // `_m` is a plain object (no insertion-order guarantee in spec, but
    // V8/JSC both honour it for string keys); spec compliance for callers
    // that care about ordering of mixed-key insertion is best-effort.
    function snapshotPairs() {
      var pairs = [];
      for (var k in sp._m) {
        for (var i = 0; i < sp._m[k].length; i++) {
          pairs.push([k, sp._m[k][i]]);
        }
      }
      return pairs;
    }
    function makeIter(mapPair) {
      var pairs = snapshotPairs();
      var i = 0;
      var iter = {
        next: function() {
          if (i >= pairs.length) return { value: undefined, done: true };
          var v = mapPair(pairs[i]);
          i++;
          return { value: v, done: false };
        },
      };
      // Iterables must return themselves from @@iterator so they can be
      // re-fed into for...of without losing state.
      if (typeof Symbol !== "undefined" && Symbol.iterator) {
        iter[Symbol.iterator] = function() { return iter; };
      }
      return iter;
    }
    sp.entries = function() { return makeIter(function(p) { return p; }); };
    sp.keys    = function() { return makeIter(function(p) { return p[0]; }); };
    sp.values  = function() { return makeIter(function(p) { return p[1]; }); };
    sp.forEach = function(cb, thisArg) {
      var pairs = snapshotPairs();
      for (var i = 0; i < pairs.length; i++) {
        // WHATWG signature: callback(value, key, parent).
        cb.call(thisArg, pairs[i][1], pairs[i][0], sp);
      }
    };
    if (typeof Symbol !== "undefined" && Symbol.iterator) {
      sp[Symbol.iterator] = sp.entries;
    }
    Object.defineProperty(sp, "size", {
      get: function() {
        var n = 0;
        for (var k in sp._m) n += sp._m[k].length;
        return n;
      },
    });

    return sp;
  }

  function URL(href) {
    parseInto(this, href);
  }

  // Accessors live on the prototype. Each is defined once at engine init
  // rather than four times per `new URL()` call.
  Object.defineProperty(URL.prototype, "search", {
    get: function() { return this._search; },
    set: function(v) {
      v = String(v);
      if (v && v.charAt(0) !== "?") v = "?" + v;
      this._search = v;
      this.__sp = null;
    },
    enumerable: true,
  });

  Object.defineProperty(URL.prototype, "host", {
    get: function() { return this.hostname + (this.port ? ":" + this.port : ""); },
    set: function(v) {
      var parts = String(v).split(":");
      this.hostname = parts[0].toLowerCase();
      this.port = parts[1] || "";
    },
    enumerable: true,
  });

  Object.defineProperty(URL.prototype, "href", {
    get: function() { return rebuildHref(this); },
    set: function(v) { parseInto(this, String(v)); },
    enumerable: true,
  });

  Object.defineProperty(URL.prototype, "searchParams", {
    get: function() {
      if (this.__sp) return this.__sp;
      this.__sp = makeSearchParams(this);
      return this.__sp;
    },
    enumerable: true,
  });

  URL.prototype.toString = function() { return rebuildHref(this); };
  URL.prototype.toJSON   = function() { return rebuildHref(this); };

  URL.__grinchPolyfill = true;
  g.URL = URL;
})(this);

// ---------- Marker-returning helpers (compiled to native by Rust) ----------

// Match URLs whose hostname is one of the given hosts, or a subdomain thereof.
//   domain("github.com")           → matches github.com, *.github.com
//   domain("a.com", "b.com")       → matches either
function domain() {
  var hosts = [];
  for (var i = 0; i < arguments.length; i++) hosts.push(String(arguments[i]).toLowerCase());
  return { __type: "domain", hosts: hosts };
}

// Match when the calling app is one of these bundle IDs.
//   from("com.tinyspeck.slackmacgap")
function from() {
  var apps = [];
  for (var i = 0; i < arguments.length; i++) apps.push(String(arguments[i]));
  return { __type: "from", apps: apps };
}

// Match when any of these apps is currently running.
//   running("us.zoom.xos")
function running() {
  var apps = [];
  for (var i = 0; i < arguments.length; i++) apps.push(String(arguments[i]));
  return { __type: "running", apps: apps };
}

// Rewrite that strips the named query params. Supports trailing * for prefix.
//   strip("utm_source", "fbclid")
//   strip("utm_*")                    → strips utm_source, utm_medium, ...
function strip() {
  var params = [];
  for (var i = 0; i < arguments.length; i++) params.push(String(arguments[i]));
  return { __type: "strip", params: params };
}

// Build the ctx object passed to user `(url, ctx) => ...` predicates.
//
// ctx is built once per resolve and reused across all fn callbacks within
// it. As a result `ctx.url` (alias `ctx.originalUrl`) is the URL passed to
// resolve() — i.e. the input URL, not the URL after rewrites have run.
// User code that needs the *current* URL should read it from the first
// argument (a URL instance), which is rebuilt per fn call.
//
// opener.windowTitle is a getter on Opener.prototype (lazy): the fetch is a
// ~5 ms XPC call into the opener app via the Accessibility API, so we never
// pay for it unless a rule's matcher actually reads ctx.opener.windowTitle.
// Defining it on the prototype rather than per-instance avoids a
// per-resolve Object.defineProperty call.
function __grinchOpener(bundleId, name, path) {
  this.bundleId = bundleId;
  this.name = name;
  this.path = path;
}
Object.defineProperty(__grinchOpener.prototype, "windowTitle", {
  get: function() {
    // Installed by Rust at engine init; calls back into workspace::frontmost_window_title.
    return (typeof __grinchFetchWindowTitle === "function") ? __grinchFetchWindowTitle() : "";
  },
  enumerable: true,
});

function __grinchMakeCtx(url, openerBundleId, openerName, openerPath,
                         shift, option, command, control, capsLock, fn) {
  // Match Finicky v4 semantics: opener is `null` when the source app is
  // unknown, not an object full of empty strings. Lets configs do
  // `if (ctx.opener) { ... }` truthiness checks the same way they would
  // in Finicky. Grinch passes empty strings from Rust when
  // frontmost_opener() can't determine the source app; treat any all-
  // empty triple as "unknown".
  var opener = (openerBundleId || openerName || openerPath)
    ? new __grinchOpener(openerBundleId, openerName, openerPath)
    : null;
  return {
    url: url,
    originalUrl: url,
    opener: opener,
    // `fn` and `function` carry the same value — Finicky exposes both
    // names with `function` as the v3-back-compat alias. Mirror that
    // so configs reading either work.
    modifiers: {
      shift: shift, option: option, command: command, control: control,
      capsLock: capsLock, fn: fn, function: fn,
    },
  };
}

// Normalise the result of a user rewrite function. Three outcomes the
// Rust side distinguishes via JSValue.isNull / .isUndefined:
//   - null      → drop the URL (suppress)
//   - undefined → leave the URL unchanged (Finicky's "no rewrite" return)
//   - string    → use as the new URL
//   - URL/LegacyURLObject → serialise to a string href
function __grinchRewriteResult(v) {
  if (v === null) return null;            // drop
  if (v === undefined) return undefined;  // pass-through, no change
  if (typeof v === "string") return v;
  if (typeof v === "object") {
    // URL instance (or anything with a non-empty .href).
    if (typeof v.href === "string" && v.href) return v.href;
    // LegacyURLObject — concatenate fields.
    var proto = v.protocol || "https";
    if (proto.charAt(proto.length - 1) === ":") proto = proto.slice(0, -1);
    var auth = "";
    if (v.username || v.password) {
      auth = v.username || "";
      if (v.password) auth += ":" + v.password;
      auth += "@";
    }
    var host = v.host;
    if (!host) {
      host = v.hostname || "";
      if (v.port != null && v.port !== "") host += ":" + v.port;
    }
    var path = v.pathname || "";
    var search = "";
    if (v.search) search = (v.search.charAt(0) === "?" ? v.search : "?" + v.search);
    var hash = "";
    if (v.hash) hash = (v.hash.charAt(0) === "#" ? v.hash : "#" + v.hash);
    return proto + "://" + auth + host + path + search + hash;
  }
  return String(v);
}

// ---------- console bridge to Rust → stderr ----------
// JSC has no built-in console. Each level is a thin shim that joins its
// varargs into a single string and hands it to a Rust block (installed at
// engine init) that prints to stderr with a `grinch [level]:` prefix.
//
// Objects are JSON-stringified for inspection-style debugging; primitives
// are stringified normally. `JSON.stringify` can throw on circular refs,
// in which case we fall back to plain `String(v)`.
function __grinchFormatArgs(args) {
  var parts = [];
  for (var i = 0; i < args.length; i++) {
    var v = args[i];
    if (typeof v === "string") {
      parts.push(v);
    } else if (v && typeof v === "object") {
      try { parts.push(JSON.stringify(v)); } catch (_) { parts.push(String(v)); }
    } else {
      parts.push(String(v));
    }
  }
  return parts.join(" ");
}

console = {
  log:   function() { if (typeof __grinchConsoleLog   === "function") __grinchConsoleLog(__grinchFormatArgs(arguments)); },
  warn:  function() { if (typeof __grinchConsoleWarn  === "function") __grinchConsoleWarn(__grinchFormatArgs(arguments)); },
  error: function() { if (typeof __grinchConsoleError === "function") __grinchConsoleError(__grinchFormatArgs(arguments)); },
  info:  function() { if (typeof __grinchConsoleInfo  === "function") __grinchConsoleInfo(__grinchFormatArgs(arguments)); },
  debug: function() { if (typeof __grinchConsoleDebug === "function") __grinchConsoleDebug(__grinchFormatArgs(arguments)); },
};

// ---------- finicky.* compatibility namespace ----------
//
// Mirrors Finicky v4's `finicky.*` global so configs ported across don't
// have to be rewritten. Pure-JS helpers live here; the OS-touching ones
// (getModifierKeys, isAppRunning, getSystemInfo, getPowerInfo) call into
// Rust blocks installed by `install_finicky_callbacks` — typeof guards
// keep these safe even on a JSContext where the blocks aren't installed
// (e.g. integration tests that haven't called the installer).
//
// Notable semantic point: `matchHostnames` is *exact* hostname match —
// `finicky.matchHostnames("github.com")` does NOT match `api.github.com`.
// That's the inverse of Grinch's bare-string matcher (`match: "github.com"`),
// which matches subdomains too. Use `domain()` for subdomain semantics.
var finicky = {
  matchHostnames: function(matchers) {
    var arr = Array.isArray(matchers) ? matchers : [matchers];
    return function(url) {
      var h = url.hostname;
      for (var i = 0; i < arr.length; i++) {
        var m = arr[i];
        if (typeof m === "string") {
          if (m === h) return true;
        } else if (m instanceof RegExp) {
          if (m.test(h)) return true;
        } else {
          throw new TypeError("finicky.matchHostnames: unrecognised matcher type: " + typeof m);
        }
      }
      return false;
    };
  },

  matchDomains: function(matchers) {
    console.warn("finicky.matchDomains is deprecated; use finicky.matchHostnames");
    return finicky.matchHostnames(matchers);
  },

  notify: function() {
    console.error("finicky.notify is not implemented in Grinch — use console.log instead");
  },

  getBattery: function() {
    console.error("finicky.getBattery is deprecated — use finicky.getPowerInfo");
    return { isCharging: false, isPluggedIn: false, chargePercentage: 0 };
  },

  getModifierKeys: function() {
    if (typeof __grinchGetModifierKeys === "function") {
      try { return JSON.parse(__grinchGetModifierKeys()); } catch (_) {}
    }
    return { shift: false, option: false, command: false, control: false,
             capsLock: false, fn: false, function: false };
  },

  isAppRunning: function(id) {
    if (typeof __grinchIsAppRunning === "function") {
      return __grinchIsAppRunning(String(id)) === "1";
    }
    return false;
  },

  getSystemInfo: function() {
    if (typeof __grinchGetSystemInfo === "function") {
      try { return JSON.parse(__grinchGetSystemInfo()); } catch (_) {}
    }
    return { localizedName: "", name: "" };
  },

  getPowerInfo: function() {
    if (typeof __grinchGetPowerInfo === "function") {
      try { return JSON.parse(__grinchGetPowerInfo()); } catch (_) {}
    }
    return { isCharging: false, isConnected: false, percentage: null };
  },
};

// ---------- CommonJS scaffolding ----------
var __grinchModule = { exports: {} };
"##;

// Wrap user source so module/exports are scoped locally and don't pollute globals.
//
// The `{` and the user source share line 1 deliberately: any `\n` between
// them would push every line of user code down by one in JSC's source map,
// so an error on user line 5 would be reported as line 6. JS doesn't care
// whether the brace is on its own line.
pub fn wrap_user_config(src: &str) -> String {
    format!("(function(module, exports) {{ {src}\n}})(__grinchModule, __grinchModule.exports);")
}
