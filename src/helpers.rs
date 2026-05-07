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
// opener.windowTitle is a getter (lazy): only fetched when user code reads
// it. The fetch is a ~5ms XPC call into the opener app via the Accessibility
// API, so we never pay for it unless a rule's matcher actually accesses it.
function __grinchMakeCtx(url, openerBundleId, openerName, openerPath, shift, option, command, control) {
  var modifiers = { shift: shift, option: option, command: command, control: control };
  var opener = { bundleId: openerBundleId, name: openerName, path: openerPath };
  Object.defineProperty(opener, "windowTitle", {
    get: function() {
      // Installed by Rust at engine init; calls back into workspace::frontmost_window_title.
      return (typeof __grinchFetchWindowTitle === "function") ? __grinchFetchWindowTitle() : "";
    },
    enumerable: true,
  });
  return {
    url: url,
    originalUrl: url,
    opener: opener,
    modifiers: modifiers,
  };
}

// Normalise the result of a user rewrite function to a string href, or null
// if the URL should be dropped. Accepts: string | URL instance | LegacyURLObject
// {protocol, host?, hostname?, port?, pathname?, search?, hash?, ...} | null.
function __grinchRewriteResult(v) {
  if (v == null) return null;                 // drop
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

// ---------- console shim (no-op) ----------
// JSC has no built-in console; without this, user code calling console.log
// would throw. We discard output rather than bridging back to Rust — keeping
// the slow path slow path. Set __grinchSilenceConsole = false to debug.
if (typeof console === "undefined") {
  console = {
    log:   function() {},
    warn:  function() {},
    error: function() {},
    info:  function() {},
    debug: function() {},
  };
}

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
