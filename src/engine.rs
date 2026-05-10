// Engine: walks the JSValue export tree at config-load time and translates
// every match pattern + rewrite into a native Rust representation. The hot
// path then uses these directly — JS is only re-entered for user-written
// `(url, ctx)` functions, which are the explicit slow path.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::atomic::{AtomicI32, Ordering};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::Message;
use objc2_foundation::{NSArray, NSString};
use objc2_javascript_core::{JSContext, JSValue};
use regex::{Regex, RegexBuilder};

use crate::loader::LoadedConfig;
use crate::workspace::{frontmost_window_title, resolve_browser_identifier, Opener};

/// PID of the current resolve()'s opener. Read by the __grinchFetchWindowTitle
/// block when user code accesses opener.windowTitle. Set on the main thread
/// at the start of each resolve(); the runtime is single-threaded (Apple Event
/// dispatch happens only on the main thread), so a plain atomic suffices.
static CURRENT_OPENER_PID: AtomicI32 = AtomicI32::new(0);

#[derive(Clone, Debug)]
pub struct BrowserSpec {
    pub bundle_id: String,
    pub args: Vec<String>,
    pub open_in_background: bool,
    /// Force LaunchServices to spawn a new application instance instead of
    /// routing the URL into a running one. Set when a Chromium profile has
    /// been chosen — without this, Chrome's existing window steals the URL
    /// and ignores the `--profile-directory=` flag.
    pub creates_new_instance: bool,
}

impl BrowserSpec {
    fn empty() -> Self {
        Self::from_bundle_id(String::new())
    }

    /// Construct a `BrowserSpec` for the given bundle ID with the no-args
    /// defaults (no extra args, foreground activate, no force-new-instance).
    /// Centralises the default-fields tail so callers don't repeat them.
    fn from_bundle_id(bundle_id: String) -> Self {
        Self {
            bundle_id,
            args: vec![],
            open_in_background: false,
            creates_new_instance: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ModifierFlags {
    pub shift: bool,
    pub option: bool,
    pub command: bool,
    pub control: bool,
}

pub struct Resolution<'u> {
    /// `Rc<BrowserSpec>` so the resolve hot path is a refcount bump
    /// instead of cloning the inner String + Vec on every match. Callers
    /// can still treat it as `&BrowserSpec` via auto-deref.
    pub browser: Rc<BrowserSpec>,
    /// Borrowed from the input URL when no rewrite fired (the common case),
    /// owned otherwise. Avoids ~one heap allocation per resolve on the
    /// declarative-only fast path.
    pub url: Cow<'u, str>,
}

/// User-supplied JS callback packaged with the metadata we sniff at config
/// load.
///
/// **The ctx-passing contract**: Grinch supplies the second positional arg
/// (`ctx`) only when the function declares two-or-more formal parameters
/// (`f.length >= 2`). With `f.length` of 0 or 1, the fn is treated as
/// url-only — Grinch skips `__grinchMakeCtx` *and* skips the LaunchServices
/// IPC for `frontmost_opener()` / `current_modifier_flags()` upstream.
///
/// Patterns this contract changes:
/// - `function() { … arguments[1] … }` — ctx slot is now always undefined.
/// - `(...args) => args[1]…` — same.
/// - `(url, ctx = {}) => …` — `f.length` is 1 (default params don't count),
///   so user code sees the JS default `{}`, not Grinch's ctx.
///
/// The trade-off favours the common case (declarative configs that use
/// either `(url) =>` or `(url, ctx) =>`) at the cost of these rare patterns.
/// If you need ctx in a fn with a default param, name the param explicitly:
/// `(url, ctx) => { ctx = ctx || {}; … }`.
struct UserFn {
    f: Retained<JSValue>,
    needs_ctx: bool,
}

impl UserFn {
    fn new(f: Retained<JSValue>) -> Self {
        let needs_ctx = fn_needs_ctx(&f);
        if !needs_ctx {
            warn_if_fn_might_read_ctx(&f);
        }
        Self { f, needs_ctx }
    }
}

/// Read `f.length` (declared formal parameter count) and apply the
/// ctx-passing contract documented on `UserFn`.
fn fn_needs_ctx(f: &JSValue) -> bool {
    let key_ns = NSString::from_str("length");
    let key_ref: &AnyObject = &key_ns;
    let len_val = match unsafe { f.objectForKeyedSubscript(Some(key_ref)) } {
        Some(v) => v,
        None => return true,
    };
    let len = unsafe { len_val.toUInt32() };
    len >= 2
}

/// Hint for the silent-failure case: when a fn has `length < 2` but its
/// source mentions `ctx` or `arguments`, the user probably expected ctx
/// to be passed. Most likely culprit is a default-param signature like
/// `(url, ctx = {}) => …` — JS's `f.length` excludes params with defaults,
/// so Grinch's arity sniffer treats it as url-only and the user's `ctx`
/// reference silently sees the JS default `{}`. Emit a one-line hint so
/// they can fix it (drop the default, or add the second arg explicitly).
///
/// False positives (a fn with a literal `"ctx"` or `arguments` string)
/// are tolerable — the message is a hint, not an error.
fn warn_if_fn_might_read_ctx(f: &JSValue) {
    let Some(src) = (unsafe { f.toString() }) else {
        return;
    };
    let src = src.to_string();
    if !src.contains("ctx") && !src.contains("arguments") {
        return;
    }
    let snippet: String = src.chars().take(80).collect::<String>().replace('\n', " ");
    eprintln!(
        "grinch: fn `{snippet}…` references `ctx` or `arguments` but declares \
         fewer than 2 formal parameters — Grinch passes ctx only when the fn \
         signature names a second arg (e.g. `(url, ctx) => …`). Default params \
         like `(url, ctx = {{}}) => …` count as one for `f.length` and won't \
         receive ctx. Add the second arg explicitly if you intended to read it."
    );
}

enum Matcher {
    Always,
    Regex(Regex),
    Domain(Vec<String>),
    From(Vec<String>),
    Running(Vec<String>),
    Fn(UserFn),
}

enum Rewriter {
    Drop,
    Strip {
        exact: HashSet<String>,
        prefixes: Vec<String>,
    },
    Literal(String),
    Fn(UserFn),
}

enum Target {
    Browser(Rc<BrowserSpec>),
    Fn(UserFn),
    Suppress,
}

struct Rule {
    matchers: Vec<Matcher>,
    /// If set, applied to the URL when the rule matches, before resolving target.
    /// Mirrors Finicky's combined `{match, url, browser}` handler entries.
    rewriter: Option<Rewriter>,
    target: Target,
}

struct RewriteRule {
    matchers: Vec<Matcher>,
    rewriter: Rewriter,
}

/// The Grinch routing engine.
///
/// **Thread safety**: `Engine` is intentionally not `Send` or `Sync`. It uses
/// `RefCell` and `Rc` for cheap interior mutability and refcount bumps
/// (see `default_browser`, `Rule.target`). The engine is only ever
/// exercised on the main run loop (Apple Event dispatch is main-thread-only
/// on macOS), and `CURRENT_OPENER_PID` likewise assumes a single in-flight
/// resolve. Don't try to call `.resolve()` from a background thread — it'll
/// fail to compile.
/// What `defaultBrowser` resolves to. Static = parsed at config load to a
/// concrete spec (the common case). Fn = a user function called at resolve
/// time when no rule matched. The fn variant means we always need ctx
/// available, so it forces `needs_opener` / `needs_modifiers` / `needs_host`
/// on (host because the fn might call `url.hostname` against the URL).
enum DefaultBrowser {
    Static(Rc<BrowserSpec>),
    Fn(UserFn),
}

pub struct Engine {
    default_browser: DefaultBrowser,
    browsers: std::collections::HashMap<String, Rc<BrowserSpec>>,
    rewrites: Vec<RewriteRule>,
    rules: Vec<Rule>,
    /// JSContext owns every JSValue we still hold after compilation (user
    /// predicate functions, prelude helpers). Must outlive them.
    ctx: Retained<JSContext>,
    /// Cached `__grinchRewriteResult` JS function for normalising user
    /// rewrite return values to a string href or null.
    rewrite_result_helper: Retained<JSValue>,
    /// Cached `__grinchMakeCtx` JS function — looked up once at engine init
    /// rather than re-fetched via objectForKeyedSubscript on each fn call.
    make_ctx_helper: Retained<JSValue>,
    /// Cached `URL` constructor — used to build URL instances for the first
    /// arg of user fn predicates/rewrites (Finicky-compatible signature).
    url_ctor: Retained<JSValue>,
    /// True if any rule reads opener (via `from()` matcher or any user fn
    /// predicate/rewrite/target — fns might dereference ctx.opener).
    /// AppDelegate skips frontmost_opener() when this is false, saving 4
    /// LaunchServices/IPC round-trips per click.
    needs_opener: bool,
    /// True if any rule reads modifier flags (any user fn predicate, since
    /// fns can read ctx.modifiers). AppDelegate skips
    /// current_modifier_flags() when this is false.
    needs_modifiers: bool,
    /// True if any rule uses `domain()` or a bare-hostname matcher. When
    /// false, `quick_host` (lowercased hostname extract) is skipped on every
    /// resolve — saves ~30-50 ns for configs that route purely on regex /
    /// wildcard / fn matchers.
    needs_host: bool,
    /// Cached JSValue strings for opener fields (bundleId / name / path).
    /// Most clicks come from the same handful of openers (Mail, Slack,
    /// Outlook…), and the JSC bridge crossing for NSString::from_str +
    /// JSValue::valueWithObject is ~500 ns per call. Caching by Rust string
    /// turns repeated builds into a refcount bump on the cached `Retained`.
    /// Reset implicitly when Engine is rebuilt on config reload — the
    /// JSContext goes with it, taking the cached JSValues along.
    opener_str_cache: RefCell<std::collections::HashMap<String, Retained<JSValue>>>,
}

#[derive(Debug)]
pub enum EngineError {
    MissingDefault,
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::MissingDefault => write!(
                f,
                "config has no `default` (or `defaultBrowser`) — \
                 add e.g. `default: \"Google Chrome\"` to module.exports"
            ),
        }
    }
}

impl Engine {
    pub fn new(loaded: LoadedConfig) -> Result<Self, EngineError> {
        let ctx = loaded.ctx;
        let exports = loaded.exports;

        let regexp_ctor = unsafe { eval_global(&ctx, "RegExp") }.expect("RegExp ctor");
        let function_ctor = unsafe { eval_global(&ctx, "Function") }.expect("Function ctor");
        let rewrite_result_helper = unsafe { eval_global(&ctx, "__grinchRewriteResult") }
            .expect("prelude __grinchRewriteResult missing");
        let make_ctx_helper = unsafe { eval_global(&ctx, "__grinchMakeCtx") }
            .expect("prelude __grinchMakeCtx missing");
        let url_ctor =
            unsafe { eval_global(&ctx, "URL") }.expect("prelude URL constructor missing");

        install_window_title_callback(&ctx);

        // options block — Finicky-compat. Accept all five v4 keys without
        // erroring so configs ported across don't have to delete them.
        // Anything unknown logs a one-line warning per key.
        if let Some(opts) = key(&exports, "options") {
            if !is_undef_or_null(&opts) && unsafe { opts.isObject() } {
                parse_options_block(&opts);
            }
        }

        // browsers
        let mut browsers: std::collections::HashMap<String, Rc<BrowserSpec>> =
            std::collections::HashMap::new();
        if let Some(b) = key(&exports, "browsers") {
            if !is_undef_or_null(&b) {
                for (k, v) in iter_object(&b) {
                    browsers.insert(k, Rc::new(parse_browser_jsval(&v)));
                }
            }
        }

        // default — accept Finicky's `defaultBrowser` as well as Grinch's `default`
        let default_val = key(&exports, "default")
            .or_else(|| key(&exports, "defaultBrowser"))
            .ok_or(EngineError::MissingDefault)?;
        if is_undef_or_null(&default_val) {
            return Err(EngineError::MissingDefault);
        }
        // Dynamic default browser (Finicky parity): a fn evaluated at
        // resolve time. Detected here at config load so we can mark
        // runtime needs upstream — a default fn always needs ctx (it can
        // read opener / modifiers / url) and a URL polyfill instance.
        let default_browser = if is_function(&default_val, &function_ctor) {
            DefaultBrowser::Fn(UserFn::new(default_val.retain()))
        } else {
            let spec = resolve_browser(&default_val, &browsers, true).unwrap_or_else(|| {
                Rc::new(BrowserSpec::from_bundle_id(
                    js_to_string(&default_val).unwrap_or_default(),
                ))
            });
            DefaultBrowser::Static(spec)
        };

        // rewrites
        let rewrites = key(&exports, "rewrite")
            .map(|arr| parse_rewrite_array(&arr, &function_ctor))
            .unwrap_or_default();

        // rules — accept Finicky's `handlers` as well as Grinch's `rules`
        let rules_val = key(&exports, "rules").or_else(|| key(&exports, "handlers"));
        let rules = rules_val
            .map(|arr| parse_rule_array(&arr, &browsers, &regexp_ctor, &function_ctor))
            .unwrap_or_default();

        let mut needs = analyse_runtime_needs(&rewrites, &rules);
        // A dynamic default (fn) is always reachable when no rule matches,
        // and it could read any of url/opener/modifiers. Force them all on.
        if matches!(&default_browser, DefaultBrowser::Fn(_)) {
            needs.opener = true;
            needs.modifiers = true;
            needs.host = true;
        }

        Ok(Self {
            default_browser,
            browsers,
            rewrites,
            rules,
            ctx,
            rewrite_result_helper,
            make_ctx_helper,
            url_ctor,
            needs_opener: needs.opener,
            needs_modifiers: needs.modifiers,
            needs_host: needs.host,
            opener_str_cache: RefCell::new(std::collections::HashMap::new()),
        })
    }

    /// True if AppDelegate should populate the opener (frontmost app +
    /// bundle ID/name/path/pid) before calling resolve(). False for
    /// declarative-only configs that never reference opener — saves
    /// ~100–500 µs of LaunchServices IPC per click.
    pub fn needs_opener(&self) -> bool {
        self.needs_opener
    }

    /// True if AppDelegate should fetch modifier flags before calling
    /// resolve(). False for configs without any user fn matchers/rewriters
    /// (only those can read modifiers, via `ctx.modifiers`).
    pub fn needs_modifiers(&self) -> bool {
        self.needs_modifiers
    }

    /// Hot path: resolve a URL given the opener and modifier flags.
    pub fn resolve<'u>(
        &self,
        url_string: &'u str,
        opener: &Opener,
        modifiers: ModifierFlags,
    ) -> Resolution<'u> {
        // Stash the opener's PID so the __grinchFetchWindowTitle block can find
        // the right process if user code accesses opener.windowTitle. Cheap
        // unconditional write; the AX call only fires on JS access.
        CURRENT_OPENER_PID.store(opener.pid, Ordering::Relaxed);

        // Borrow until a rewrite fires; then own. Saves one heap allocation
        // on every resolve that doesn't rewrite the URL.
        let mut current: Cow<'u, str> = Cow::Borrowed(url_string);
        // quick_host allocates a lowercased String; skip it entirely when
        // the config has no host-using matchers (regex/wildcard/fn-only).
        let mut host = if self.needs_host {
            quick_host(&current)
        } else {
            None
        };
        let rc = ResolveCtx::new(
            &self.ctx,
            &self.rewrite_result_helper,
            &self.make_ctx_helper,
            &self.url_ctor,
            &self.opener_str_cache,
            opener,
            modifiers,
            url_string,
        );

        // Global rewrites — apply every matching one in order.
        for rw in &self.rewrites {
            if any_match(&rw.matchers, &current, host.as_deref(), &rc) {
                match apply_rewrite(&rw.rewriter, &current, &rc) {
                    RewriteOutcome::Changed(s) => {
                        current = Cow::Owned(s);
                        host = if self.needs_host {
                            quick_host(&current)
                        } else {
                            None
                        };
                    }
                    RewriteOutcome::Unchanged => {}
                    RewriteOutcome::Drop => return suppressed(),
                }
            }
        }

        // Handlers — first match wins. A matched rule may carry its own
        // rewriter (Finicky-style combined entry); apply it before resolving
        // the target.
        for rule in &self.rules {
            if !any_match(&rule.matchers, &current, host.as_deref(), &rc) {
                continue;
            }
            if let Some(rw) = &rule.rewriter {
                match apply_rewrite(rw, &current, &rc) {
                    RewriteOutcome::Changed(s) => {
                        current = Cow::Owned(s);
                        host = if self.needs_host {
                            quick_host(&current)
                        } else {
                            None
                        };
                    }
                    RewriteOutcome::Unchanged => {}
                    RewriteOutcome::Drop => return suppressed(),
                }
            }
            match &rule.target {
                Target::Browser(b) => {
                    return Resolution {
                        browser: Rc::clone(b),
                        url: current,
                    };
                }
                Target::Suppress => {
                    return suppressed();
                }
                Target::Fn(uf) => {
                    let Some(args) = rc.fn_args(&current, uf.needs_ctx) else {
                        continue;
                    };
                    let result = unsafe { uf.f.callWithArguments(Some(&args)) };
                    if let Some(r) = result {
                        if !unsafe { r.isUndefined() } && !unsafe { r.isNull() } {
                            // Runtime fn return: don't apply Name:Profile shorthand —
                            // an opaque debug string like "t:function" must stay literal.
                            let spec =
                                resolve_browser(&r, &self.browsers, false).unwrap_or_else(|| {
                                    Rc::new(BrowserSpec::from_bundle_id(
                                        js_to_string(&r).unwrap_or_default(),
                                    ))
                                });
                            return Resolution {
                                browser: spec,
                                url: current,
                            };
                        }
                    }
                }
            }
        }

        // Default fallback. Static = the pre-resolved spec; Fn = invoke
        // the user fn now with (url, ctx) and resolve its return through
        // the same machinery as a Target::Fn rule would. Fall back to a
        // suppressed-style empty spec if the fn returns null/undefined or
        // its return doesn't resolve to a browser — better than panicking.
        match &self.default_browser {
            DefaultBrowser::Static(b) => Resolution {
                browser: Rc::clone(b),
                url: current,
            },
            DefaultBrowser::Fn(uf) => {
                if let Some(args) = rc.fn_args(&current, uf.needs_ctx) {
                    if let Some(r) = unsafe { uf.f.callWithArguments(Some(&args)) } {
                        if !unsafe { r.isUndefined() } && !unsafe { r.isNull() } {
                            let spec =
                                resolve_browser(&r, &self.browsers, false).unwrap_or_else(|| {
                                    Rc::new(BrowserSpec::from_bundle_id(
                                        js_to_string(&r).unwrap_or_default(),
                                    ))
                                });
                            return Resolution {
                                browser: spec,
                                url: current,
                            };
                        }
                    }
                }
                // Fn returned null/undefined or args build failed — same
                // semantics as `open: null` (suppress).
                suppressed()
            }
        }
    }
}

/// Outcome of a rewrite: drop the URL, leave it unchanged, or replace it.
/// Distinguishing Unchanged from Changed lets the resolve loop skip a
/// String allocation when a rewriter (e.g. strip on a URL with no query
/// string) produces no actual change.
enum RewriteOutcome {
    Unchanged,
    Changed(String),
    Drop,
}

/// Walk every matcher/rewriter/target in the compiled config and decide
/// whether the AppDelegate needs to populate opener / modifiers before
/// calling resolve(). Conservative: any fn variant counts (we can't
/// statically inspect what a JS function reads), and Matcher::From
/// requires opener.bundle_id specifically.
#[derive(Debug, PartialEq, Eq)]
struct RuntimeNeeds {
    opener: bool,
    modifiers: bool,
    host: bool,
}

fn analyse_runtime_needs(rewrites: &[RewriteRule], rules: &[Rule]) -> RuntimeNeeds {
    // Only fns that declare a second arg can read ctx, so they're the only
    // ones that force us to populate opener + modifiers. A url-only fn
    // (`(url) => …`) sees `undefined` if we pass it nothing for ctx, so
    // skipping the opener IPC is safe.
    //
    // `host` is needed only by Matcher::Domain (the bare-hostname / domain()
    // path). Regex/wildcard matchers regex against the full URL string and
    // never look at the host slot.
    fn matchers_need(ms: &[Matcher], n: &mut RuntimeNeeds) {
        for matcher in ms {
            match matcher {
                Matcher::From(_) => n.opener = true,
                Matcher::Fn(uf) if uf.needs_ctx => {
                    n.opener = true;
                    n.modifiers = true;
                }
                Matcher::Domain(_) => n.host = true,
                Matcher::Always | Matcher::Regex(_) | Matcher::Running(_) | Matcher::Fn(_) => {}
            }
        }
    }
    fn rewriter_needs(r: &Rewriter, n: &mut RuntimeNeeds) {
        if let Rewriter::Fn(uf) = r {
            if uf.needs_ctx {
                n.opener = true;
                n.modifiers = true;
            }
        }
    }

    let mut n = RuntimeNeeds {
        opener: false,
        modifiers: false,
        host: false,
    };

    for rw in rewrites {
        matchers_need(&rw.matchers, &mut n);
        rewriter_needs(&rw.rewriter, &mut n);
    }
    for rule in rules {
        matchers_need(&rule.matchers, &mut n);
        if let Some(rw) = &rule.rewriter {
            rewriter_needs(rw, &mut n);
        }
        if let Target::Fn(uf) = &rule.target {
            if uf.needs_ctx {
                n.opener = true;
                n.modifiers = true;
            }
        }
    }

    n
}

fn suppressed() -> Resolution<'static> {
    Resolution {
        browser: Rc::new(BrowserSpec::empty()),
        url: Cow::Borrowed("about:blank"),
    }
}

// MARK: - Resolve context (per-call)

struct ResolveCtx<'a> {
    ctx: &'a JSContext,
    rewrite_result_helper: &'a JSValue,
    make_ctx_helper: &'a JSValue,
    url_ctor: &'a JSValue,
    /// Cached opener-field JSValues (bundleId/name/path → cached
    /// `Retained<JSValue>`). Lives on Engine; we only borrow it.
    opener_str_cache: &'a RefCell<std::collections::HashMap<String, Retained<JSValue>>>,
    opener: &'a Opener,
    modifiers: ModifierFlags,
    /// Per-resolve cache for `running()` matchers. Built lazily on first
    /// `running_apps()` access, dropped at end of resolve. Lifetime-of-Engine
    /// caching looked tempting but goes stale — apps start/quit between
    /// clicks and `running()` would lie until the next config reload.
    running_cache: RefCell<Option<HashSet<String>>>,
    /// URL passed to resolve() — exposed to user fns as `ctx.url` /
    /// `ctx.originalUrl`. Stays constant for the entire resolve even if
    /// rewrites fire; user code reads the *current* URL via the first arg.
    original_url: &'a str,
    /// ctx object — built lazily on first fn call, then reused. Opener and
    /// modifiers are constant for a resolve, so this never needs invalidating.
    cached_ctx: RefCell<Option<Retained<JSValue>>>,
    /// Cached URL polyfill instance. Built once per URL string seen during
    /// the resolve and reused by both fn-args cache slots, so a url-only
    /// fn matcher and a url+ctx fn matcher share one `new URL()` cost.
    cached_url_instance: RefCell<Option<(Box<str>, Retained<JSValue>)>>,
    /// fn args NSArray for the current URL string when the fn declares
    /// `(url, ctx) => …`. Invalidated when the URL changes between rewrites;
    /// cached_ctx is preserved across that. `Box<str>` (not `String`)
    /// halves the per-cache allocation footprint — capacity is dead weight.
    fn_args_cache_full: RefCell<Option<(Box<str>, Retained<NSArray>)>>,
    /// fn args NSArray for url-only fns (`(url) => …`). One-element NSArray
    /// containing just the URL instance — no ctx, so we never trigger the
    /// `__grinchMakeCtx` path or pay the opener-IPC cost upstream.
    fn_args_cache_url_only: RefCell<Option<(Box<str>, Retained<NSArray>)>>,
}

impl<'a> ResolveCtx<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        ctx: &'a JSContext,
        rewrite_result_helper: &'a JSValue,
        make_ctx_helper: &'a JSValue,
        url_ctor: &'a JSValue,
        opener_str_cache: &'a RefCell<std::collections::HashMap<String, Retained<JSValue>>>,
        opener: &'a Opener,
        modifiers: ModifierFlags,
        original_url: &'a str,
    ) -> Self {
        Self {
            ctx,
            rewrite_result_helper,
            make_ctx_helper,
            url_ctor,
            opener_str_cache,
            opener,
            modifiers,
            running_cache: RefCell::new(None),
            original_url,
            cached_ctx: RefCell::new(None),
            cached_url_instance: RefCell::new(None),
            fn_args_cache_full: RefCell::new(None),
            fn_args_cache_url_only: RefCell::new(None),
        }
    }

    fn running_apps(&self) -> std::cell::Ref<'_, HashSet<String>> {
        if self.running_cache.borrow().is_none() {
            *self.running_cache.borrow_mut() = Some(crate::workspace::running_app_bundle_ids());
        }
        std::cell::Ref::map(self.running_cache.borrow(), |o| o.as_ref().unwrap())
    }

    /// Lazily-built ctx object. Reused across all fn invocations within a
    /// resolve — opener and modifiers don't change, and ctx.url is pinned
    /// to the original (pre-rewrite) URL by design. Returns None if the
    /// prelude helper is broken; caller treats that as fn-doesn't-match.
    fn ctx_object(&self) -> Option<Retained<JSValue>> {
        if let Some(c) = self.cached_ctx.borrow().as_ref() {
            return Some(c.clone());
        }
        let v = build_ctx_object(
            self.ctx,
            self.make_ctx_helper,
            self.opener_str_cache,
            self.original_url,
            self.opener,
            self.modifiers,
        )?;
        *self.cached_ctx.borrow_mut() = Some(v.clone());
        Some(v)
    }

    /// Cached URL polyfill instance for `url`. Both fn-args paths share it,
    /// so a config that mixes url-only and url+ctx fns pays for `new URL()`
    /// once per URL string per resolve, not once per fn call.
    fn url_instance(&self, url: &str) -> Retained<JSValue> {
        if let Some((cached_url, instance)) = self.cached_url_instance.borrow().as_ref() {
            if cached_url.as_ref() == url {
                return instance.clone();
            }
        }
        let v = build_url_instance(self.url_ctor, self.ctx, url);
        *self.cached_url_instance.borrow_mut() = Some((Box::from(url), v.clone()));
        v
    }

    /// Build the args for a user fn invocation. When `needs_ctx` is true, the
    /// args are `[urlInstance, ctxObject]` (Finicky-compatible 2-arg form);
    /// otherwise `[urlInstance]` alone, and `__grinchMakeCtx` is never called.
    /// Returns None if the prelude is broken — callers treat that as a fn that
    /// doesn't match (rather than panicking).
    fn fn_args(&self, url: &str, needs_ctx: bool) -> Option<Retained<NSArray>> {
        if needs_ctx {
            if let Some((cached_url, args)) = self.fn_args_cache_full.borrow().as_ref() {
                if cached_url.as_ref() == url {
                    return Some(args.clone());
                }
            }
            let url_instance = self.url_instance(url);
            let ctx_val = self.ctx_object()?;
            let url_obj: Retained<AnyObject> = unsafe { Retained::cast_unchecked(url_instance) };
            let ctx_obj: Retained<AnyObject> = unsafe { Retained::cast_unchecked(ctx_val) };
            let args = NSArray::from_retained_slice(&[url_obj, ctx_obj]);
            *self.fn_args_cache_full.borrow_mut() = Some((Box::from(url), args.clone()));
            Some(args)
        } else {
            if let Some((cached_url, args)) = self.fn_args_cache_url_only.borrow().as_ref() {
                if cached_url.as_ref() == url {
                    return Some(args.clone());
                }
            }
            let url_instance = self.url_instance(url);
            let url_obj: Retained<AnyObject> = unsafe { Retained::cast_unchecked(url_instance) };
            let args = NSArray::from_retained_slice(&[url_obj]);
            *self.fn_args_cache_url_only.borrow_mut() = Some((Box::from(url), args.clone()));
            Some(args)
        }
    }
}

/// Install a block as `__grinchFetchWindowTitle` on the JSContext. The block
/// reads CURRENT_OPENER_PID (set by resolve()) and calls into the AX API.
/// Lazy: the JS getter on opener.windowTitle only invokes this when user code
/// reads it, so configs that don't touch windowTitle pay nothing.
/// Install five `__grinchConsole*` blocks that the prelude wires up to
/// `console.log/warn/error/info/debug`. Each block takes a single
/// already-formatted string (the prelude joins varargs JS-side) and prints
/// it to stderr with a `grinch [level]:` prefix.
///
/// Called from the loader after the prelude evaluates but before user
/// config evaluates, so top-level `console.log()` calls in the user file
/// land on the wired blocks rather than the prelude's `typeof` no-op
/// fallback. Without this ordering, configs that call `console.log("…")`
/// at module scope got silent drops — debugging a non-firing rule was
/// painful.
/// Manual Obj-C block encoding for `void (^)(NSString *)`. JSC will only
/// auto-bridge a block to a JS function if it carries `_Block_signature`
/// metadata — objc2's `RcBlock::new` uses `NoBlockEncoding`, which omits
/// it. With this encoding string in place (`v16@?0@8` on 64-bit), JSC
/// reads the signature and exposes the block as a callable JS function;
/// without it, the block stays an opaque `NSBlock` and JS-side calls
/// throw "is not a function".
struct OneStringArgEncoding;
unsafe impl block2::ManualBlockEncoding for OneStringArgEncoding {
    type Arguments = (*mut NSString,);
    type Return = ();
    const ENCODING_CSTR: &'static std::ffi::CStr = if cfg!(target_pointer_width = "64") {
        c"v16@?0@8"
    } else {
        c"v8@?0@4"
    };
}

pub(crate) fn install_console_callbacks(ctx: &JSContext) {
    fn install(ctx: &JSContext, key: &str, level: &'static str) {
        let block =
            RcBlock::with_encoding::<_, _, _, OneStringArgEncoding>(move |msg: *mut NSString| {
                if msg.is_null() {
                    return;
                }
                // SAFETY: JSC owns the NSString; we just borrow it for one call.
                let s = unsafe { (*msg).to_string() };
                eprintln!("grinch [{level}]: {s}");
            });
        let block_ref: &block2::Block<_> = &block;
        let block_obj: &AnyObject = unsafe { &*(block_ref as *const _ as *const AnyObject) };
        let key_ns = NSString::from_str(key);
        let key_ref: &objc2_foundation::NSObject = &key_ns;
        unsafe {
            ctx.setObject_forKeyedSubscript(Some(block_obj), Some(key_ref));
        }
        drop(block);
    }
    install(ctx, "__grinchConsoleLog", "log");
    install(ctx, "__grinchConsoleWarn", "warn");
    install(ctx, "__grinchConsoleError", "error");
    install(ctx, "__grinchConsoleInfo", "info");
    install(ctx, "__grinchConsoleDebug", "debug");
}

/// Manual encoding for `NSString * (^)(void)` — block returning id, no
/// args. Same JSC reason as the console encoding: without a signature,
/// JSC sees an opaque NSBlock and JS-side `typeof` returns "object",
/// silently dropping the call. The previous implementation looked
/// correct but was effectively dead code; opener.windowTitle just
/// returned "" because the JS-side fallback (`typeof === "function"`)
/// failed.
struct ZeroArgIdReturnEncoding;
unsafe impl block2::ManualBlockEncoding for ZeroArgIdReturnEncoding {
    type Arguments = ();
    type Return = *mut NSString;
    const ENCODING_CSTR: &'static std::ffi::CStr = if cfg!(target_pointer_width = "64") {
        c"@8@?0"
    } else {
        c"@4@?0"
    };
}

/// Manual encoding for `NSString * (^)(NSString *)` — used for
/// `finicky.isAppRunning`'s underlying bridge (returns "1"/"0" so the
/// JS wrapper can coerce to boolean cheaply, no JSON parse needed).
struct OneStringArgIdReturnEncoding;
unsafe impl block2::ManualBlockEncoding for OneStringArgIdReturnEncoding {
    type Arguments = (*mut NSString,);
    type Return = *mut NSString;
    const ENCODING_CSTR: &'static std::ffi::CStr = if cfg!(target_pointer_width = "64") {
        c"@16@?0@8"
    } else {
        c"@8@?0@4"
    };
}

fn install_window_title_callback(ctx: &JSContext) {
    // Block return follows ARC's id-returning convention: autoreleased, not
    // +1 retained. JSC's Obj-C bridge calls objc_retainAutoreleasedReturnValue
    // on the result; pairing an autorelease here means the retain counts
    // balance. Returning Retained::into_raw (a +1 pointer) leaks the NSString
    // every time user code reads opener.windowTitle.
    let block = RcBlock::with_encoding::<_, _, _, ZeroArgIdReturnEncoding>(|| -> *mut NSString {
        let pid = CURRENT_OPENER_PID.load(Ordering::Relaxed);
        let title = frontmost_window_title(pid);
        Retained::autorelease_return(NSString::from_str(&title))
    });
    // SAFETY: A block is an Objective-C object (NSBlock). `&Block<F>` is
    // ABI-compatible with a block pointer, which is itself a valid `id`.
    // JSC accepts blocks as JS-callable functions via the standard objc bridge.
    let block_ref: &block2::Block<_> = &block;
    let block_obj: &AnyObject = unsafe { &*(block_ref as *const _ as *const AnyObject) };
    let key_ns = NSString::from_str("__grinchFetchWindowTitle");
    // JSContext::setObject_forKeyedSubscript takes the key as &NSObject
    // (NSCopying-typed historically), unlike the JSValue variant which takes
    // &AnyObject. NSString -> NSObject deref-coerces in argument position.
    let key_ref: &objc2_foundation::NSObject = &key_ns;
    unsafe {
        ctx.setObject_forKeyedSubscript(Some(block_obj), Some(key_ref));
    }
    // setObject_forKeyedSubscript copies the block into JSC's value table;
    // dropping our RcBlock here is safe — JSC keeps it alive for the lifetime
    // of the JSContext.
    drop(block);
}

/// Install Rust-side bridges for the `finicky.*` helpers that need access
/// to OS state. The JS-side `finicky` namespace (defined in the prelude)
/// wraps each one with a `typeof` guard and a parse-or-default fallback,
/// so configs run even on a JSContext where these aren't installed (e.g.
/// the integration-test fixture before it explicitly calls this fn).
///
/// All bridges return *strings* — JSON for the dict-shaped helpers,
/// "1"/"0" for the boolean. Returning NSDictionary directly would mean
/// constructing one Rust-side, which is more code than this is worth.
pub(crate) fn install_finicky_callbacks(ctx: &JSContext) {
    fn install_zero_arg_string(ctx: &JSContext, key: &str, body: impl Fn() -> String + 'static) {
        let block =
            RcBlock::with_encoding::<_, _, _, ZeroArgIdReturnEncoding>(move || -> *mut NSString {
                Retained::autorelease_return(NSString::from_str(&body()))
            });
        let block_ref: &block2::Block<_> = &block;
        let block_obj: &AnyObject = unsafe { &*(block_ref as *const _ as *const AnyObject) };
        let key_ns = NSString::from_str(key);
        let key_ref: &objc2_foundation::NSObject = &key_ns;
        unsafe {
            ctx.setObject_forKeyedSubscript(Some(block_obj), Some(key_ref));
        }
        drop(block);
    }

    fn install_one_arg_string(ctx: &JSContext, key: &str, body: impl Fn(&str) -> String + 'static) {
        let block = RcBlock::with_encoding::<_, _, _, OneStringArgIdReturnEncoding>(
            move |arg: *mut NSString| -> *mut NSString {
                let s = if arg.is_null() {
                    String::new()
                } else {
                    unsafe { (*arg).to_string() }
                };
                Retained::autorelease_return(NSString::from_str(&body(&s)))
            },
        );
        let block_ref: &block2::Block<_> = &block;
        let block_obj: &AnyObject = unsafe { &*(block_ref as *const _ as *const AnyObject) };
        let key_ns = NSString::from_str(key);
        let key_ref: &objc2_foundation::NSObject = &key_ns;
        unsafe {
            ctx.setObject_forKeyedSubscript(Some(block_obj), Some(key_ref));
        }
        drop(block);
    }

    install_zero_arg_string(ctx, "__grinchGetModifierKeys", || {
        let m = crate::workspace::current_modifier_flags();
        // capsLock/fn/function aren't surfaced by ModifierFlags today;
        // emit `false` for parity with Finicky's shape so callers don't
        // hit "undefined.shift" style errors on those keys.
        format!(
            r#"{{"shift":{},"option":{},"command":{},"control":{},"capsLock":false,"fn":false,"function":false}}"#,
            m.shift, m.option, m.command, m.control,
        )
    });

    install_one_arg_string(ctx, "__grinchIsAppRunning", |id| {
        // Match against either bundle ID or localized name, like Finicky.
        // `running_app_bundle_ids` only returns bundle IDs today; the
        // localized-name comparison would need a different NSWorkspace
        // call. Document this gap; common case (bundle ID) works.
        if crate::workspace::running_app_bundle_ids().contains(id) {
            "1".to_string()
        } else {
            "0".to_string()
        }
    });

    install_zero_arg_string(ctx, "__grinchGetSystemInfo", || {
        let mut buf = [0u8; 256];
        let ret = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut _, buf.len()) };
        let host = if ret == 0 {
            let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            String::from_utf8_lossy(&buf[..len]).into_owned()
        } else {
            String::new()
        };
        // Finicky returns localizedName + name from NSHost; we just return
        // gethostname() for both — it's good enough for routing decisions
        // like "is this my work laptop?".
        let json = serde_json::json!({ "localizedName": host, "name": host });
        json.to_string()
    });

    install_zero_arg_string(ctx, "__grinchGetPowerInfo", || {
        // IOKit IOPSCopyPowerSourcesInfo would give real values, but the
        // call surface is heavy and most routing configs don't read this.
        // Return a sensible-shape stub; the JS wrapper logs an info note
        // the first time it's called so users know to file an issue if
        // they actually need this.
        r#"{"isCharging":false,"isConnected":true,"percentage":null}"#.to_string()
    });
}

/// Build a URL polyfill instance via `new URL(urlString)`. If the URL fails
/// to parse (e.g. exotic scheme), fall back to a plain object so user code
/// destructuring `{ href }` doesn't crash.
fn build_url_instance(url_ctor: &JSValue, ctx: &JSContext, url: &str) -> Retained<JSValue> {
    let url_str = js_string(ctx, url);
    let url_str_obj: Retained<AnyObject> = unsafe { Retained::cast_unchecked(url_str) };
    let args = NSArray::from_retained_slice(&[url_str_obj]);
    if let Some(instance) = unsafe { url_ctor.constructWithArguments(Some(&args)) } {
        if !unsafe { instance.isUndefined() } && !unsafe { instance.isNull() } {
            return instance;
        }
    }
    // Parse failed (URL polyfill threw); return a stub object with .href set
    // so user code can still destructure. serde_json gives us a JSON string
    // literal that's also valid JS — Rust's debug-format `{:?}` would emit
    // \u{X} escapes which don't parse as JS string escapes.
    let url_json = serde_json::to_string(url).unwrap_or_else(|_| "\"\"".to_string());
    let stub_src = format!(
        "({{ href: {url_json}, protocol: '', hostname: '', pathname: '', search: '', hash: '' }})"
    );
    let stub_ns = NSString::from_str(&stub_src);
    unsafe { ctx.evaluateScript(Some(&stub_ns)) }
        .or_else(|| {
            // Last-ditch: a literal empty object. evaluateScript on a 2-byte
            // input failing means the JSContext is fundamentally broken, but
            // we'd still rather return *something* than panic.
            unsafe { ctx.evaluateScript(Some(&NSString::from_str("({})"))) }
        })
        .expect("JSContext can't evaluate `({})` — context is broken")
}

fn build_ctx_object(
    ctx: &JSContext,
    helper: &JSValue,
    opener_str_cache: &RefCell<std::collections::HashMap<String, Retained<JSValue>>>,
    url: &str,
    opener: &Opener,
    m: ModifierFlags,
) -> Option<Retained<JSValue>> {
    // URL changes per resolve (or per rewrite); not worth caching across
    // resolves. Opener fields stabilise — same Mail / Slack / Outlook over
    // and over — so they go through the engine's cache.
    let url_v = js_string(ctx, url);
    let opener_id_v = cached_js_string(ctx, opener_str_cache, &opener.bundle_id);
    let opener_name_v = cached_js_string(ctx, opener_str_cache, &opener.name);
    let opener_path_v = cached_js_string(ctx, opener_str_cache, &opener.path);
    let shift_v = js_bool(ctx, m.shift);
    let option_v = js_bool(ctx, m.option);
    let command_v = js_bool(ctx, m.command);
    let control_v = js_bool(ctx, m.control);
    let args_objs: Vec<Retained<AnyObject>> = vec![
        unsafe { Retained::cast_unchecked(url_v) },
        unsafe { Retained::cast_unchecked(opener_id_v) },
        unsafe { Retained::cast_unchecked(opener_name_v) },
        unsafe { Retained::cast_unchecked(opener_path_v) },
        unsafe { Retained::cast_unchecked(shift_v) },
        unsafe { Retained::cast_unchecked(option_v) },
        unsafe { Retained::cast_unchecked(command_v) },
        unsafe { Retained::cast_unchecked(control_v) },
    ];
    let args = NSArray::from_retained_slice(&args_objs);
    let result = unsafe { helper.callWithArguments(Some(&args)) };
    if result.is_none() {
        // Helper returned null (likely the user's config replaced or broke
        // the prelude). Warn once and let the caller fall through — the fn
        // matcher/rewriter that needed this ctx will simply not match.
        static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            eprintln!(
                "grinch: __grinchMakeCtx returned null — fn matchers won't match \
                 until the config is fixed (the prelude helper appears to have been \
                 overridden)."
            );
        }
    }
    result
}

// MARK: - Match dispatch

fn any_match(matchers: &[Matcher], url: &str, host: Option<&str>, rc: &ResolveCtx) -> bool {
    matchers.iter().any(|m| matches(m, url, host, rc))
}

fn matches(m: &Matcher, url: &str, host: Option<&str>, rc: &ResolveCtx) -> bool {
    match m {
        Matcher::Always => true,
        Matcher::Regex(re) => re.is_match(url),
        Matcher::Domain(hosts) => match host {
            Some(host) => hosts.iter().any(|h| host_matches(host, h)),
            None => false,
        },
        Matcher::From(apps) => apps.iter().any(|a| a == &rc.opener.bundle_id),
        Matcher::Running(apps) => {
            let runs = rc.running_apps();
            apps.iter().any(|a| runs.contains(a))
        }
        Matcher::Fn(uf) => {
            let Some(args) = rc.fn_args(url, uf.needs_ctx) else {
                return false;
            };
            let result = unsafe { uf.f.callWithArguments(Some(&args)) };
            result.map(|v| unsafe { v.toBool() }).unwrap_or(false)
        }
    }
}

/// True if `host` is exactly `pattern` or a subdomain of `pattern`.
/// Allocation-free: does the dot-boundary check on bytes directly rather
/// than allocating a `.{pattern}` string per call.
#[inline]
fn host_matches(host: &str, pattern: &str) -> bool {
    if host == pattern {
        return true;
    }
    let hb = host.as_bytes();
    let pb = pattern.as_bytes();
    hb.len() > pb.len() + 1 && hb[hb.len() - pb.len() - 1] == b'.' && hb.ends_with(pb)
}

/// Apply a rewriter. Returns Changed(new_url) when the URL was rewritten,
/// Unchanged when the rewriter matched but produced no change (e.g. strip
/// against a URL with no query), and Drop when the URL should be suppressed.
fn apply_rewrite(r: &Rewriter, url: &str, rc: &ResolveCtx) -> RewriteOutcome {
    match r {
        Rewriter::Drop => RewriteOutcome::Drop,
        Rewriter::Strip { exact, prefixes } => match strip_params(url, exact, prefixes) {
            Some(new_url) => RewriteOutcome::Changed(new_url),
            None => RewriteOutcome::Unchanged,
        },
        Rewriter::Literal(s) => {
            if s == url {
                RewriteOutcome::Unchanged
            } else {
                RewriteOutcome::Changed(s.clone())
            }
        }
        Rewriter::Fn(uf) => {
            let Some(args) = rc.fn_args(url, uf.needs_ctx) else {
                return RewriteOutcome::Unchanged;
            };
            let Some(raw) = (unsafe { uf.f.callWithArguments(Some(&args)) }) else {
                return RewriteOutcome::Unchanged;
            };
            // Normalise via __grinchRewriteResult: handles string | URL |
            // LegacyURLObject | null in one place, returning a string href
            // or JS null.
            let raw_obj: Retained<AnyObject> = unsafe { Retained::cast_unchecked(raw) };
            let helper_args = NSArray::from_retained_slice(&[raw_obj]);
            let Some(normalised) = (unsafe {
                rc.rewrite_result_helper
                    .callWithArguments(Some(&helper_args))
            }) else {
                return RewriteOutcome::Unchanged;
            };
            if unsafe { normalised.isNull() } || unsafe { normalised.isUndefined() } {
                return RewriteOutcome::Drop;
            }
            let Some(s) = js_to_string(&normalised) else {
                return RewriteOutcome::Unchanged;
            };
            if s == url {
                RewriteOutcome::Unchanged
            } else {
                RewriteOutcome::Changed(s)
            }
        }
    }
}

// MARK: - Compilation

/// Parse a JS browser spec (string | object). Resolves app names to bundle
/// IDs; expands the `profile` shorthand for Chromium-family browsers.
fn parse_browser_jsval(v: &JSValue) -> BrowserSpec {
    if unsafe { v.isString() } {
        let s = js_to_string(v).unwrap_or_default();
        // Finicky's "Name:Profile" shorthand: a colon separates the app
        // name (or bundle ID) from a profile name. Bundle IDs use `.` not
        // `:`, so a `:` after the first character is unambiguously the
        // shorthand separator. We deliberately don't parse it for URL-
        // scheme matchers (those go through compile_matcher, a different
        // code path).
        if let Some(idx) = s.find(':') {
            // Don't split on a leading `:` (would give an empty name).
            if idx > 0 {
                let (name, rest) = s.split_at(idx);
                let profile = &rest[1..]; // skip the ':' itself
                let bundle_id = resolve_browser_identifier(name);
                if !profile.is_empty() && crate::chromium::is_chromium(&bundle_id) {
                    let dir = crate::chromium::resolve_profile_dir(&bundle_id, profile);
                    return BrowserSpec {
                        bundle_id,
                        args: vec![format!("--profile-directory={dir}")],
                        open_in_background: false,
                        creates_new_instance: true,
                    };
                }
                // Non-Chromium with a profile suffix: warn and fall back
                // to the bare name (matches the object-form behaviour).
                if !profile.is_empty() {
                    eprintln!(
                        "grinch: ignoring `:profile` shorthand for non-Chromium browser \
                         {bundle_id} (input was {s:?})"
                    );
                }
                return BrowserSpec::from_bundle_id(bundle_id);
            }
        }
        return BrowserSpec::from_bundle_id(resolve_browser_identifier(&s));
    }
    if !unsafe { v.isObject() } {
        return BrowserSpec::empty();
    }

    // appType: "none" → no-op browser (same as `open: null`). Skip the
    // identifier resolution entirely.
    if let Some(t) = key(v, "appType").and_then(|x| js_to_string(&x)) {
        if t == "none" {
            return BrowserSpec::empty();
        }
    }

    // Bundle ID source: `id`, `bundleId`, or `name`. The resolver dispatches
    // on `appType` when present:
    //   - "path"     → treat the value as a filesystem path, look up its
    //                  CFBundleIdentifier directly.
    //   - "bundleId" → use the value verbatim (skip the LaunchServices
    //                  display-name fallback).
    //   - "appName"  → look up via NSWorkspace's app-by-display-name path.
    //   - default    → autodetect (existing behaviour).
    let raw_id = key(v, "id")
        .or_else(|| key(v, "bundleId"))
        .or_else(|| key(v, "name"))
        .and_then(|x| js_to_string(&x))
        .unwrap_or_default();
    let app_type = key(v, "appType").and_then(|x| js_to_string(&x));
    let bundle_id = match app_type.as_deref() {
        Some("path") => crate::workspace::resolve_browser_path(&raw_id),
        Some("bundleId") => raw_id.clone(),
        // "appName" goes through the same code path as autodetect — both end
        // up at fullPathForApplication. The explicit appType lets the user
        // skip the bundle-ID fast path when the name happens to look like
        // one (rare but possible).
        _ => resolve_browser_identifier(&raw_id),
    };

    let mut args = key(v, "args")
        .map(|a| js_array_to_strings(&a))
        .unwrap_or_default();
    let mut creates_new_instance = false;

    // Chromium-family `profile` field: expand to --profile-directory=<dir>.
    // `profile` may be either the on-disk directory name ("Profile 10") or
    // the user-facing display name ("Convergint") — we resolve through
    // Chrome's Local State to make both work.
    if let Some(profile) = key(v, "profile").and_then(|p| js_to_string(&p)) {
        if !profile.is_empty() && crate::chromium::is_chromium(&bundle_id) {
            let dir = crate::chromium::resolve_profile_dir(&bundle_id, &profile);
            args.push(format!("--profile-directory={dir}"));
            // When a profile is requested we MUST spawn a new application
            // instance — without this, an already-running Chrome routes the
            // URL into its active window and ignores the profile flag.
            creates_new_instance = true;
        } else if !profile.is_empty() {
            eprintln!(
                "grinch: ignoring `profile` for non-Chromium browser {bundle_id} (profile = {profile})"
            );
        }
    }

    let open_in_background = key(v, "openInBackground")
        .map(|b| unsafe { b.toBool() })
        .unwrap_or(false);

    BrowserSpec {
        bundle_id,
        args,
        open_in_background,
        creates_new_instance,
    }
}

/// Resolve a JSValue to a BrowserSpec.
///
/// `apply_string_shorthand` controls whether bare-string browser specs are
/// parsed for the Finicky `"Name:Profile"` shorthand. `true` for config-
/// load callers (default browser, rule `open`/`browser` literals), `false`
/// for runtime callers (Target::Fn return values) — fn-returned strings
/// should be treated opaquely so a debug string like `"t:function"` doesn't
/// get split on `:`.
fn resolve_browser(
    v: &JSValue,
    browsers: &std::collections::HashMap<String, Rc<BrowserSpec>>,
    apply_string_shorthand: bool,
) -> Option<Rc<BrowserSpec>> {
    if unsafe { v.isString() } {
        let s = js_to_string(v)?;
        // Browsers-map lookup uses the string verbatim (the user wrote
        // `open: "work"` referring to a key in the map, not a literal app
        // name). The map key never contains a `:` shorthand, so this
        // check goes first.
        if let Some(named) = browsers.get(&s) {
            return Some(Rc::clone(named));
        }
        if apply_string_shorthand {
            // `parse_browser_jsval`'s string branch handles bare-name +
            // "Name:Profile" shorthand.
            return Some(Rc::new(parse_browser_jsval(v)));
        }
        return Some(Rc::new(BrowserSpec::from_bundle_id(
            resolve_browser_identifier(&s),
        )));
    }
    if unsafe { v.isObject() } {
        return Some(Rc::new(parse_browser_jsval(v)));
    }
    None
}

/// Parse Finicky v4's `options` block. The five known keys are accepted
/// without error so a copied-over Finicky config doesn't break:
///
/// | Key | Grinch behaviour |
/// |---|---|
/// | `urlShorteners` | silently ignored — Finicky's hard-coded list isn't user-configurable there either; Grinch expects external expansion (see `examples/expand-shortener.sh`) |
/// | `logRequests`   | silently ignored — Grinch uses `GRINCH_DEBUG=1` for trace logs to stderr |
/// | `checkForUpdates` | silently ignored — Grinch doesn't poll for updates |
/// | `keepRunning`   | silently ignored — Grinch is always resident |
/// | `hideIcon`      | silently ignored for now (will be implemented in a future commit) |
///
/// Unknown keys log a one-line warning so users can spot typos. Doesn't
/// affect the engine's runtime behaviour today; existing in code is
/// purely about not erroring on valid Finicky config.
fn parse_options_block(opts: &JSValue) {
    const KNOWN: &[&str] = &[
        "urlShorteners",
        "logRequests",
        "checkForUpdates",
        "keepRunning",
        "hideIcon",
    ];
    for (k, _v) in iter_object(opts) {
        if !KNOWN.contains(&k.as_str()) {
            eprintln!(
                "grinch: unknown options.{k} — accepted keys are urlShorteners, \
                 logRequests, checkForUpdates, keepRunning, hideIcon"
            );
        }
    }
}

fn parse_rule_array(
    arr: &JSValue,
    browsers: &std::collections::HashMap<String, Rc<BrowserSpec>>,
    regexp_ctor: &JSValue,
    function_ctor: &JSValue,
) -> Vec<Rule> {
    if is_undef_or_null(arr) {
        return vec![];
    }
    let count = js_array_len(arr);
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let Some(item) = js_array_at(arr, i) else {
            continue;
        };
        let match_val = key(&item, "match");
        // `open` (Grinch) and `browser` (Finicky) are aliases.
        let open_val = key(&item, "open").or_else(|| key(&item, "browser"));
        let url_val = key(&item, "url");
        let matchers = compile_matchers(match_val.as_deref(), regexp_ctor, function_ctor);

        // Optional per-rule rewriter (combined entry).
        let rewriter = url_val
            .as_ref()
            .and_then(|uv| compile_rewriter(uv, function_ctor));

        // Target: `open: null` → suppress; fn → Fn; resolvable browser → Browser.
        // If `open`/`browser` is absent but a `url` rewrite IS present, that's
        // a pure rewrite-on-match (no routing change) — treat as default-target.
        let target = match open_val.as_ref() {
            Some(ov) if unsafe { ov.isNull() } => Target::Suppress,
            Some(ov) if is_function(ov, function_ctor) => Target::Fn(UserFn::new(ov.clone())),
            Some(ov) => match resolve_browser(ov, browsers, true) {
                // Empty bundle_id = explicit no-op browser (e.g. via
                // `appType: "none"`). Normalise to Target::Suppress so the
                // resolve path's URL handling matches `open: null` exactly,
                // including the "about:blank" Resolution.url.
                Some(b) if b.bundle_id.is_empty() => Target::Suppress,
                Some(b) => Target::Browser(b),
                None => {
                    eprintln!(
                        "grinch: rules[{i}] has unresolvable `open` (not a string, \
                         object, or browser key) — entry ignored"
                    );
                    continue;
                }
            },
            None => {
                if rewriter.is_some() {
                    eprintln!(
                        "grinch: rules[{i}] has `url:` but no `open:` — move it \
                         to the top-level `rewrite:` array if you want it to \
                         apply globally; rules entries need an `open` to route"
                    );
                } else {
                    eprintln!("grinch: rules[{i}] has neither `open` nor `url` — entry ignored");
                }
                continue;
            }
        };
        out.push(Rule {
            matchers,
            rewriter,
            target,
        });
    }
    out
}

fn parse_rewrite_array(arr: &JSValue, function_ctor: &JSValue) -> Vec<RewriteRule> {
    if is_undef_or_null(arr) {
        return vec![];
    }
    let count = js_array_len(arr);
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let Some(item) = js_array_at(arr, i) else {
            continue;
        };

        // Bare strip(...) marker (no match field) — treat as "always run".
        if is_marker(&item, "strip") {
            if let Some(r) = compile_strip(&item) {
                out.push(RewriteRule {
                    matchers: vec![Matcher::Always],
                    rewriter: r,
                });
            }
            continue;
        }

        let match_val = key(&item, "match");
        let url_val = key(&item, "url");
        // RegExp matchers don't appear in rewrite arrays under any common
        // pattern, but pass the ctor through compile_matchers anyway so
        // /literal/ regex is accepted.
        let matchers = compile_matchers(match_val.as_deref(), function_ctor, function_ctor);
        let Some(uv) = url_val else { continue };
        let Some(rewriter) = compile_rewriter(&uv, function_ctor) else {
            continue;
        };
        out.push(RewriteRule { matchers, rewriter });
    }
    out
}

fn compile_rewriter(v: &JSValue, function_ctor: &JSValue) -> Option<Rewriter> {
    if unsafe { v.isNull() } {
        return Some(Rewriter::Drop);
    }
    if is_function(v, function_ctor) {
        return Some(Rewriter::Fn(UserFn::new(v.retain())));
    }
    if let Some(s) = js_to_string(v) {
        return Some(Rewriter::Literal(s));
    }
    None
}

fn compile_matchers(
    v: Option<&JSValue>,
    regexp_ctor: &JSValue,
    function_ctor: &JSValue,
) -> Vec<Matcher> {
    let Some(v) = v else { return vec![] };
    if is_undef_or_null(v) {
        return vec![];
    }
    if unsafe { v.isArray() } {
        let count = js_array_len(v);
        let mut ms = Vec::with_capacity(count);
        for i in 0..count {
            if let Some(item) = js_array_at(v, i) {
                if let Some(m) = compile_matcher(&item, regexp_ctor, function_ctor) {
                    ms.push(m);
                }
            }
        }
        return ms;
    }
    compile_matcher(v, regexp_ctor, function_ctor)
        .map(|m| vec![m])
        .unwrap_or_default()
}

fn compile_matcher(v: &JSValue, regexp_ctor: &JSValue, function_ctor: &JSValue) -> Option<Matcher> {
    // String → either a wildcard pattern (if it contains * or /) or a bare
    // hostname shorthand for a domain-and-subdomain match.
    if unsafe { v.isString() } {
        let s = js_to_string(v)?;
        if s.contains('*') || s.contains('/') {
            return compile_wildcard(&s).map(Matcher::Regex);
        }
        // ASCII lowercase to match `quick_host`'s lowercasing of the URL's
        // host. URL hostnames are ASCII per the URL spec; using the
        // Unicode-aware to_lowercase() on either side could produce mismatches
        // on IDN inputs.
        return Some(Matcher::Domain(vec![s.to_ascii_lowercase()]));
    }
    if unsafe { v.isObject() } {
        if let Some(t) = key(v, "__type") {
            if !unsafe { t.isUndefined() } {
                if let Some(name) = js_to_string(&t) {
                    match name.as_str() {
                        "domain" => {
                            if let Some(arr) = key(v, "hosts") {
                                let hosts: Vec<String> = js_array_to_strings(&arr)
                                    .into_iter()
                                    .map(|s| s.to_ascii_lowercase())
                                    .collect();
                                return Some(Matcher::Domain(hosts));
                            }
                        }
                        "from" => {
                            if let Some(arr) = key(v, "apps") {
                                return Some(Matcher::From(js_array_to_strings(&arr)));
                            }
                        }
                        "running" => {
                            if let Some(arr) = key(v, "apps") {
                                return Some(Matcher::Running(js_array_to_strings(&arr)));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        // Regex literal /.../ — compile via the regex crate. Honour the JS
        // RegExp's `ignoreCase` (`i`) and `multiline` (`m`) flags. Finicky
        // matches via native RegExp.test on url.href, which respects all the
        // flags the user wrote; mirror that. Earlier versions of Grinch
        // forced case-insensitive matching, which was a silent semantic
        // divergence from Finicky and from JS's own `.test()` behaviour.
        if is_instance_of(v, regexp_ctor) {
            if let Some(pattern) = key(v, "source").and_then(|p| js_to_string(&p)) {
                let ignore_case = key(v, "ignoreCase")
                    .map(|p| unsafe { p.toBool() })
                    .unwrap_or(false);
                let multi_line = key(v, "multiline")
                    .map(|p| unsafe { p.toBool() })
                    .unwrap_or(false);
                if let Ok(re) = RegexBuilder::new(&pattern)
                    .case_insensitive(ignore_case)
                    .multi_line(multi_line)
                    .build()
                {
                    return Some(Matcher::Regex(re));
                }
            }
        }
        if is_function(v, function_ctor) {
            return Some(Matcher::Fn(UserFn::new(v.retain())));
        }
    }
    None
}

fn compile_strip(v: &JSValue) -> Option<Rewriter> {
    let arr = key(v, "params")?;
    let params = js_array_to_strings(&arr);
    if params.is_empty() {
        eprintln!("grinch: strip() called with no arguments — rewriter will never strip anything");
    }
    let mut exact = HashSet::new();
    let mut prefixes = Vec::new();
    for p in params {
        if let Some(stripped) = p.strip_suffix('*') {
            prefixes.push(stripped.to_string());
        } else {
            exact.insert(p);
        }
    }
    Some(Rewriter::Strip { exact, prefixes })
}

/// Port of Finicky's `matchWildcard`. Compiles a glob-style pattern to a
/// case-insensitive regex anchored at both ends. `*` is non-greedy `.*?`;
/// `\*` is a literal asterisk; patterns without a leading protocol/asterisk
/// get an optional `(?:https?:|...)?(?://)?` prefix so e.g. `"zoom.us/j/*"`
/// matches both bare and protocol-prefixed URLs.
fn compile_wildcard(pattern: &str) -> Option<Regex> {
    const PLACEHOLDER: char = '\u{0000}';

    // Step 1: replace escaped asterisks with a sentinel.
    let mut work = pattern.replace("\\*", &PLACEHOLDER.to_string());

    // Step 2: escape regex special chars except `*`.
    let mut escaped = String::with_capacity(work.len() + 16);
    for c in work.chars() {
        if matches!(
            c,
            '.' | '+' | '?' | '^' | '$' | '{' | '}' | '(' | ')' | '|' | '[' | ']' | '\\'
        ) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    work = escaped;

    // Step 3: protocol-prefix logic. If the pattern has a `\w+:` prefix, treat
    // it as protocol-anchored; otherwise (and unless it starts with `*`)
    // prepend an optional protocol matcher.
    let starts_with_protocol = pattern_has_protocol_prefix(pattern);
    if !starts_with_protocol {
        if !pattern.starts_with('*') {
            work = format!("(?:https?:|ftp:|mailto:|file:|tel:|sms:|data:)?(?://)?{work}");
        }
    } else {
        work = work.replace('/', "\\/");
        if work.ends_with("\\/\\/") {
            work.push_str(".*");
        }
    }

    // Step 4: replace remaining `*` with non-greedy `.*?`.
    work = work.replace('*', ".*?");

    // Step 5: restore escaped asterisks as literal `\*`.
    work = work.replace(PLACEHOLDER, "\\*");

    // Step 6: anchor.
    let anchored = format!("^{work}$");

    RegexBuilder::new(&anchored)
        .case_insensitive(true)
        .build()
        .ok()
}

fn pattern_has_protocol_prefix(pat: &str) -> bool {
    let bytes = pat.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_alphanumeric() || c == b'_' {
            i += 1;
            continue;
        }
        return c == b':' && i > 0;
    }
    false
}

// MARK: - URL utilities (hot-path inline parsing)

/// Extract hostname from a URL string without a full URL parser. Returns
/// lowercased hostname or None. Handles fully-qualified URLs (`http(s)://`,
/// `scheme://host`); protocol-relative `//host` forms aren't supported
/// because LaunchServices only delivers absolute URLs to URL handlers.
/// Bracketed IPv6 literals (`[::1]`, `[::1]:8080`) are returned with their
/// brackets intact, which is also what `domain()` matchers compare against.
/// Hostnames are ASCII per the URL spec, so we use `to_ascii_lowercase` —
/// faster than the Unicode-aware `to_lowercase` and good enough.
#[inline]
pub(crate) fn quick_host(url: &str) -> Option<String> {
    let mut s = url;
    if let Some(idx) = s.find("://") {
        s = &s[idx + 3..];
    }
    if let Some(idx) = s.find(['/', '?', '#']) {
        s = &s[..idx];
    }
    if let Some(at) = s.rfind('@') {
        s = &s[at + 1..];
    }
    // IPv6 literal: keep [..] intact, strip only a trailing :port. Doing
    // rfind(':') unconditionally would slice into the address itself
    // (`[::1]` → `[:`).
    if s.starts_with('[') {
        if let Some(end) = s.find(']') {
            let host = &s[..end + 1];
            return if host.len() <= 2 {
                None
            } else {
                Some(host.to_ascii_lowercase())
            };
        }
        return None;
    }
    if let Some(colon) = s.rfind(':') {
        s = &s[..colon];
    }
    if s.is_empty() {
        None
    } else {
        Some(s.to_ascii_lowercase())
    }
}

/// Strip query parameters. Returns Some(rebuilt) when at least one param was
/// removed; None when the URL had no query or no matching params (so the
/// caller can avoid an unnecessary String allocation).
pub(crate) fn strip_params(
    url: &str,
    exact: &HashSet<String>,
    prefixes: &[String],
) -> Option<String> {
    let q = url.find('?')?;
    let base = &url[..q];
    let rest = &url[q + 1..];
    let (qs, frag) = if let Some(h) = rest.find('#') {
        (&rest[..h], &rest[h..])
    } else {
        (rest, "")
    };

    let mut total = 0usize;
    let mut kept: Vec<&str> = Vec::new();
    for kv in qs.split('&') {
        if kv.is_empty() {
            continue;
        }
        total += 1;
        let key = kv.split_once('=').map(|(k, _)| k).unwrap_or(kv);
        if exact.contains(key) {
            continue;
        }
        if prefixes.iter().any(|p| key.starts_with(p)) {
            continue;
        }
        kept.push(kv);
    }

    if kept.len() == total {
        // Nothing was stripped — caller can keep the original URL.
        return None;
    }

    Some(if kept.is_empty() {
        format!("{base}{frag}")
    } else {
        format!("{base}?{}{frag}", kept.join("&"))
    })
}

// MARK: - JSValue helpers

/// Look up a property by name. Returns None for missing/undefined fields so
/// callers can use `.or_else` chains and pattern-match on Some(value).
/// Explicit `null` (e.g. `open: null`) returns Some(null_value) — distinguishable
/// via `.isNull()`.
fn key(v: &JSValue, name: &str) -> Option<Retained<JSValue>> {
    if !unsafe { v.isObject() } {
        return None;
    }
    let key_ns = NSString::from_str(name);
    let key_ref: &AnyObject = &key_ns;
    let result = unsafe { v.objectForKeyedSubscript(Some(key_ref)) }?;
    if unsafe { result.isUndefined() } {
        return None;
    }
    Some(result)
}

fn is_undef_or_null(v: &JSValue) -> bool {
    unsafe { v.isUndefined() || v.isNull() }
}

fn js_to_string(v: &JSValue) -> Option<String> {
    let s = unsafe { v.toString() }?;
    Some(s.to_string())
}

fn js_array_len(v: &JSValue) -> usize {
    let len = key(v, "length");
    len.map(|n| unsafe { n.toInt32() } as usize).unwrap_or(0)
}

fn js_array_at(v: &JSValue, i: usize) -> Option<Retained<JSValue>> {
    unsafe { v.valueAtIndex(i) }
}

fn js_array_to_strings(v: &JSValue) -> Vec<String> {
    let count = js_array_len(v);
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        if let Some(item) = js_array_at(v, i) {
            if let Some(s) = js_to_string(&item) {
                out.push(s);
            }
        }
    }
    out
}

fn js_string(ctx: &JSContext, s: &str) -> Retained<JSValue> {
    let ns = NSString::from_str(s);
    let any: &AnyObject = &ns;
    unsafe { JSValue::valueWithObject_inContext(Some(any), Some(ctx)) }
        .expect("valueWithObject returned null")
}

/// Cached `js_string` keyed by the Rust `&str`. Cache hit returns a
/// refcount bump; miss allocates the JSValue and stores it. Used for
/// strings that repeat across resolves (opener fields), not per-call
/// inputs (URL).
fn cached_js_string(
    ctx: &JSContext,
    cache: &RefCell<std::collections::HashMap<String, Retained<JSValue>>>,
    s: &str,
) -> Retained<JSValue> {
    if let Some(v) = cache.borrow().get(s) {
        return v.clone();
    }
    let v = js_string(ctx, s);
    cache.borrow_mut().insert(s.to_string(), v.clone());
    v
}

fn js_bool(ctx: &JSContext, b: bool) -> Retained<JSValue> {
    unsafe { JSValue::valueWithBool_inContext(b, Some(ctx)) }.expect("valueWithBool null")
}

unsafe fn eval_global(ctx: &JSContext, name: &str) -> Option<Retained<JSValue>> {
    let key_ns = NSString::from_str(name);
    let key_ref: &AnyObject = &key_ns;
    unsafe { ctx.objectForKeyedSubscript(Some(key_ref)) }
}

fn is_function(v: &JSValue, function_ctor: &JSValue) -> bool {
    let any: &AnyObject = function_ctor;
    unsafe { v.isInstanceOf(Some(any)) }
}

fn is_instance_of(v: &JSValue, ctor: &JSValue) -> bool {
    let any: &AnyObject = ctor;
    unsafe { v.isInstanceOf(Some(any)) }
}

fn is_marker(v: &JSValue, ty: &str) -> bool {
    if !unsafe { v.isObject() } {
        return false;
    }
    let Some(t) = key(v, "__type") else {
        return false;
    };
    js_to_string(&t).as_deref() == Some(ty)
}

/// Iterate the keys of a JS object as Rust strings, returning (key, value).
/// Values are re-fetched as JSValues so we don't lose JSValue identity (which
/// `JSValue::toDictionary` would erase by recursively converting to NS*).
/// The double bridge crossing is fine here — only called from `Engine::new`
/// against the small `browsers:` map, never on the resolve hot path.
fn iter_object(v: &JSValue) -> Vec<(String, Retained<JSValue>)> {
    let dict = match unsafe { v.toDictionary() } {
        Some(d) => d,
        None => return vec![],
    };
    let keys = dict.allKeys();
    let count = keys.count();
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let any_key = keys.objectAtIndex(i);
        let Ok(s) = any_key.downcast::<NSString>() else {
            continue;
        };
        let name = s.to_string();
        if let Some(val) = key(v, &name) {
            out.push((name, val));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------- quick_host --------

    #[test]
    fn quick_host_basic() {
        assert_eq!(
            quick_host("http://example.com/path"),
            Some("example.com".into())
        );
        assert_eq!(
            quick_host("https://example.com:443/"),
            Some("example.com".into())
        );
        assert_eq!(
            quick_host("ftp://files.example/x"),
            Some("files.example".into())
        );
    }

    #[test]
    fn quick_host_strips_userinfo() {
        assert_eq!(
            quick_host("https://user:pw@host.example/x"),
            Some("host.example".into()),
        );
        assert_eq!(
            quick_host("https://user@host.example/x"),
            Some("host.example".into()),
        );
    }

    #[test]
    fn quick_host_lowercases_ascii() {
        assert_eq!(
            quick_host("HTTP://Example.COM/"),
            Some("example.com".into())
        );
    }

    #[test]
    fn quick_host_query_and_fragment() {
        assert_eq!(
            quick_host("https://x.example?a=b"),
            Some("x.example".into())
        );
        assert_eq!(
            quick_host("https://x.example#frag"),
            Some("x.example".into())
        );
    }

    #[test]
    fn quick_host_handles_ipv6_literals() {
        // Regression: the rfind(':') stripper used to chop the colons inside
        // the brackets, returning "[:" for any [::1]-style URL.
        assert_eq!(quick_host("http://[::1]/"), Some("[::1]".into()));
        assert_eq!(quick_host("http://[::1]:8080/path"), Some("[::1]".into()));
        assert_eq!(
            quick_host("http://[2001:db8::1]:443/"),
            Some("[2001:db8::1]".into()),
        );
        assert_eq!(quick_host("http://user@[::1]:80/"), Some("[::1]".into()),);
    }

    #[test]
    fn quick_host_empty_or_garbage() {
        assert_eq!(quick_host(""), None);
        assert_eq!(quick_host("file:///etc/hosts"), None); // empty host
        assert_eq!(quick_host("http://"), None);
    }

    // -------- host_matches --------

    #[test]
    fn host_matches_exact_and_subdomain() {
        assert!(host_matches("github.com", "github.com"));
        assert!(host_matches("api.github.com", "github.com"));
        assert!(host_matches("a.b.github.com", "github.com"));
    }

    #[test]
    fn host_matches_rejects_prefix_collisions() {
        // "notgithub.com" must NOT match pattern "github.com" — the previous
        // implementation needed a literal dot before the suffix.
        assert!(!host_matches("notgithub.com", "github.com"));
        assert!(!host_matches("github.com.evil", "github.com"));
        assert!(!host_matches("", "github.com"));
    }

    // -------- strip_params --------

    fn strset<const N: usize>(items: [&str; N]) -> HashSet<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn strip_params_exact_match() {
        let r = strip_params("https://x/?utm_source=a&q=1", &strset(["utm_source"]), &[]);
        assert_eq!(r.as_deref(), Some("https://x/?q=1"));
    }

    #[test]
    fn strip_params_prefix_wildcard() {
        let r = strip_params(
            "https://x/?utm_a=1&utm_b=2&keep=ok",
            &strset([]),
            &["utm_".to_string()],
        );
        assert_eq!(r.as_deref(), Some("https://x/?keep=ok"));
    }

    #[test]
    fn strip_params_returns_none_when_unchanged() {
        // Caller relies on None to skip the rebuild allocation.
        assert!(strip_params("https://x/?q=1", &strset(["missing"]), &[]).is_none());
        assert!(strip_params("https://x", &strset(["utm_source"]), &[]).is_none());
    }

    #[test]
    fn strip_params_preserves_fragment() {
        let r = strip_params("https://x/?utm=1&q=ok#anchor", &strset(["utm"]), &[]);
        assert_eq!(r.as_deref(), Some("https://x/?q=ok#anchor"));
    }

    #[test]
    fn strip_params_when_only_param_is_stripped() {
        let r = strip_params("https://x/?utm=1#frag", &strset(["utm"]), &[]);
        assert_eq!(r.as_deref(), Some("https://x/#frag"));
    }

    #[test]
    fn strip_params_handles_value_less_keys() {
        // `?a&b=1` — `a` has no `=`. Stripping `a` leaves `b=1`.
        let r = strip_params("https://x/?a&b=1", &strset(["a"]), &[]);
        assert_eq!(r.as_deref(), Some("https://x/?b=1"));
    }

    // -------- pattern_has_protocol_prefix --------

    #[test]
    fn pattern_has_protocol_prefix_recognises_schemes() {
        assert!(pattern_has_protocol_prefix("slack:"));
        assert!(pattern_has_protocol_prefix("https://x"));
        assert!(pattern_has_protocol_prefix("custom_scheme:foo"));
    }

    #[test]
    fn pattern_has_protocol_prefix_rejects_non_schemes() {
        assert!(!pattern_has_protocol_prefix("slack"));
        assert!(!pattern_has_protocol_prefix(""));
        assert!(!pattern_has_protocol_prefix(":nocolon-prefix"));
        assert!(!pattern_has_protocol_prefix("zoom.us/j/*"));
    }

    // -------- compile_wildcard --------

    fn matches_pat(pat: &str, url: &str) -> bool {
        let re = compile_wildcard(pat).unwrap_or_else(|| panic!("compile failed: {pat}"));
        re.is_match(url)
    }

    #[test]
    fn wildcard_bare_hostname_pattern() {
        // The Finicky-style protocol prefix is auto-prepended.
        assert!(matches_pat("zoom.us/j/*", "https://zoom.us/j/123"));
        assert!(matches_pat("zoom.us/j/*", "zoom.us/j/123"));
        assert!(!matches_pat(
            "zoom.us/j/*",
            "https://other.com/zoom.us/j/123"
        ));
    }

    #[test]
    fn wildcard_subdomain_star() {
        assert!(matches_pat("*.zoom.us/j/*", "https://x.zoom.us/j/y"));
        // Bare zoom.us shouldn't match the *. variant.
        assert!(!matches_pat("*.zoom.us/j/*", "https://zoom.us/j/y"));
    }

    #[test]
    fn wildcard_protocol_anchored() {
        assert!(matches_pat("slack:*", "slack://channel?team=foo"));
        assert!(matches_pat("mailto:*", "mailto:a@b.example"));
        // http: pattern shouldn't match https URLs.
        assert!(!matches_pat(
            "http://example.com/*",
            "https://example.com/x"
        ));
    }

    #[test]
    fn wildcard_escaped_asterisk_is_literal() {
        // \* must match a literal *, not act as a wildcard.
        assert!(matches_pat(r"foo\*bar", "foo*bar"));
        assert!(!matches_pat(r"foo\*bar", "fooXbar"));
    }

    #[test]
    fn wildcard_match_all() {
        assert!(matches_pat("*", "https://anything.example/at/all"));
        assert!(matches_pat("*", ""));
    }

    #[test]
    fn wildcard_case_insensitive() {
        assert!(matches_pat("zoom.us/j/*", "HTTPS://ZOOM.US/J/abc"));
    }

    // -------- analyse_runtime_needs --------

    fn rule_with_matchers(ms: Vec<Matcher>) -> Rule {
        Rule {
            matchers: ms,
            rewriter: None,
            target: Target::Suppress,
        }
    }

    #[test]
    fn analyse_needs_empty() {
        assert_eq!(
            analyse_runtime_needs(&[], &[]),
            RuntimeNeeds {
                opener: false,
                modifiers: false,
                host: false
            },
        );
    }

    #[test]
    fn analyse_needs_declarative_only() {
        let rules = vec![
            rule_with_matchers(vec![Matcher::Always]),
            rule_with_matchers(vec![Matcher::Running(vec!["app".into()])]),
        ];
        assert_eq!(
            analyse_runtime_needs(&[], &rules),
            RuntimeNeeds {
                opener: false,
                modifiers: false,
                host: false
            },
        );
    }

    #[test]
    fn analyse_needs_domain_sets_host_only() {
        let rules = vec![rule_with_matchers(vec![Matcher::Domain(vec!["x".into()])])];
        assert_eq!(
            analyse_runtime_needs(&[], &rules),
            RuntimeNeeds {
                opener: false,
                modifiers: false,
                host: true
            },
        );
    }

    #[test]
    fn analyse_needs_from_requires_opener_only() {
        let rules = vec![rule_with_matchers(vec![Matcher::From(vec!["x".into()])])];
        assert_eq!(
            analyse_runtime_needs(&[], &rules),
            RuntimeNeeds {
                opener: true,
                modifiers: false,
                host: false
            },
        );
    }
}

#[cfg(test)]
mod integration_tests {
    //! End-to-end tests that build a real `Engine` from a JS config string,
    //! then exercise `resolve()` with synthetic openers and modifiers. The
    //! fixture (`build_engine`) creates a fresh `JSContext` per test so
    //! parallel test execution doesn't share JS-side state.
    //!
    //! These tests cover the parse + resolve pipeline (matchers, rewriters,
    //! targets, browser specs, ctx semantics, URL polyfill, fn-arity skip)
    //! that the pure-Rust unit tests in `mod tests` above can't reach
    //! without a JSC fixture.
    use super::*;
    use crate::helpers::{wrap_user_config, JS_PRELUDE};
    use crate::loader::LoadedConfig;
    use crate::workspace::Opener;

    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_foundation::NSString;
    use objc2_javascript_core::JSContext;

    /// Build an `Engine` from a JS config source. Each call gets its own
    /// `JSContext` (and its own JavaScriptCore VM) so two parallel tests
    /// can't see each other's globals. Panics on any JSC error — caller's
    /// job to keep the synthetic config valid.
    fn build_engine(user_src: &str) -> Engine {
        let ctx: Retained<JSContext> = unsafe { JSContext::new() };

        let prelude_ns = NSString::from_str(JS_PRELUDE);
        unsafe { ctx.evaluateScript(Some(&prelude_ns)) }.expect("prelude evaluation returned null");

        // Match the loader's ordering: install bridges between prelude eval
        // and user-config eval so top-level `console.log` / `finicky.*`
        // calls in the user source land on real Rust hooks.
        super::install_console_callbacks(&ctx);
        super::install_finicky_callbacks(&ctx);

        let wrapped = wrap_user_config(user_src);
        let wrapped_ns = NSString::from_str(&wrapped);
        unsafe { ctx.evaluateScript(Some(&wrapped_ns)) }
            .expect("user config evaluation returned null");

        let module_key = NSString::from_str("__grinchModule");
        let module_ref: &AnyObject = &module_key;
        let module = unsafe { ctx.objectForKeyedSubscript(Some(module_ref)) }
            .expect("__grinchModule missing from global");
        let exports_key = NSString::from_str("exports");
        let exports_ref: &AnyObject = &exports_key;
        let exports = unsafe { module.objectForKeyedSubscript(Some(exports_ref)) }
            .expect("__grinchModule.exports missing");

        Engine::new(LoadedConfig { exports, ctx }).expect("engine init failed")
    }

    /// Synthetic opener for tests. `pid = 0` short-circuits any AX/IPC
    /// lookups (see `frontmost_window_title`) so tests stay hermetic.
    fn opener(bundle_id: &str, name: &str) -> Opener {
        Opener {
            bundle_id: bundle_id.to_string(),
            name: name.to_string(),
            path: String::new(),
            pid: 0,
        }
    }

    /// Resolve and return `(browser_bundle_id, final_url)` so tests can
    /// assert on plain strings.
    fn resolve(engine: &Engine, url: &str) -> (String, String) {
        let res = engine.resolve(url, &Opener::default(), ModifierFlags::default());
        (res.browser.bundle_id.clone(), res.url.into_owned())
    }

    fn resolve_with(
        engine: &Engine,
        url: &str,
        opener: &Opener,
        modifiers: ModifierFlags,
    ) -> (String, String) {
        let res = engine.resolve(url, opener, modifiers);
        (res.browser.bundle_id.clone(), res.url.into_owned())
    }

    // ---------- Engine end-to-end ----------

    #[test]
    fn default_browser_fires_when_no_rules() {
        let e = build_engine(r#"module.exports = { default: "com.apple.Safari" };"#);
        let (browser, url) = resolve(&e, "https://example.com/");
        assert_eq!(browser, "com.apple.Safari");
        assert_eq!(url, "https://example.com/");
    }

    #[test]
    fn options_block_with_all_known_keys_is_accepted() {
        // Finicky-config compat: the entire options block should be
        // accepted without erroring even though Grinch implements none
        // of these today. Verify by checking that the engine builds
        // (build_engine would panic if Engine::new returned Err) and
        // that resolve still works.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                options: {
                    urlShorteners: ["bit.ly", "t.co"],
                    logRequests: true,
                    checkForUpdates: false,
                    keepRunning: true,
                    hideIcon: false,
                },
            };"#,
        );
        assert_eq!(resolve(&e, "https://x/").0, "com.apple.Safari");
    }

    #[test]
    fn options_block_with_unknown_key_does_not_error() {
        // Unknown option keys log a stderr warning but must not break
        // engine init. The user's config still loads and resolves.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                options: { thisIsNotARealOption: 42 },
            };"#,
        );
        assert_eq!(resolve(&e, "https://x/").0, "com.apple.Safari");
    }

    #[test]
    fn dynamic_default_browser_fn_returning_string() {
        // Finicky-style dynamic default: defaultBrowser is a fn evaluated
        // at resolve time when no rule matched.
        let e = build_engine(
            r#"module.exports = {
                default: (url) =>
                    url.hostname === "internal.corp" ? "com.apple.Safari" : "com.google.Chrome",
            };"#,
        );
        assert_eq!(resolve(&e, "https://internal.corp/x").0, "com.apple.Safari");
        assert_eq!(resolve(&e, "https://github.com/x").0, "com.google.Chrome");
    }

    #[test]
    fn dynamic_default_browser_fn_with_ctx() {
        // Default fn can read ctx (opener / modifiers). Dynamic-default
        // configs always have needs_opener / needs_modifiers / needs_host
        // forced on so the IPC happens upstream.
        let e = build_engine(
            r#"module.exports = {
                default: (url, ctx) =>
                    ctx.modifiers.shift ? "com.google.Chrome" : "com.apple.Safari",
            };"#,
        );
        assert!(e.needs_opener());
        assert!(e.needs_modifiers());
        assert_eq!(
            resolve_with(
                &e,
                "https://x/",
                &Opener::default(),
                ModifierFlags::default()
            )
            .0,
            "com.apple.Safari",
        );
        let with_shift = ModifierFlags {
            shift: true,
            ..ModifierFlags::default()
        };
        assert_eq!(
            resolve_with(&e, "https://x/", &Opener::default(), with_shift).0,
            "com.google.Chrome",
        );
    }

    #[test]
    fn dynamic_default_browser_returning_null_suppresses() {
        let e = build_engine(r#"module.exports = { default: () => null };"#);
        let (browser, url) = resolve(&e, "https://x/");
        assert_eq!(browser, "");
        assert_eq!(url, "about:blank");
    }

    #[test]
    fn defaultbrowser_alias_works() {
        // Finicky-style key name should be accepted as well.
        let e = build_engine(r#"module.exports = { defaultBrowser: "com.apple.Safari" };"#);
        assert_eq!(resolve(&e, "https://x/").0, "com.apple.Safari");
    }

    #[test]
    fn handlers_alias_for_rules() {
        // Finicky's `handlers` should be accepted as a synonym for `rules`.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                handlers: [{ match: "x", open: "com.google.Chrome" }],
            };"#,
        );
        assert_eq!(resolve(&e, "https://x/").0, "com.google.Chrome");
    }

    #[test]
    fn first_matching_rule_wins() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [
                    { match: "github.com", open: "com.google.Chrome" },
                    { match: "github.com", open: "com.apple.Mail" },
                ],
            };"#,
        );
        assert_eq!(resolve(&e, "https://github.com/").0, "com.google.Chrome");
    }

    #[test]
    fn falls_through_to_default_when_no_rule_matches() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{ match: "github.com", open: "com.google.Chrome" }],
            };"#,
        );
        assert_eq!(resolve(&e, "https://example.com/").0, "com.apple.Safari");
    }

    // ---------- compile_matcher per variant ----------

    #[test]
    fn matcher_bare_hostname_matches_subdomain() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{ match: "github.com", open: "com.google.Chrome" }],
            };"#,
        );
        assert_eq!(resolve(&e, "https://github.com/").0, "com.google.Chrome");
        assert_eq!(
            resolve(&e, "https://api.github.com/").0,
            "com.google.Chrome"
        );
        assert_eq!(resolve(&e, "https://other.com/").0, "com.apple.Safari");
    }

    #[test]
    fn matcher_domain_helper_handles_multiple_hosts() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{ match: domain("github.com", "gitlab.com"),
                          open: "com.google.Chrome" }],
            };"#,
        );
        assert_eq!(resolve(&e, "https://github.com/").0, "com.google.Chrome");
        assert_eq!(resolve(&e, "https://x.gitlab.com/").0, "com.google.Chrome");
        assert_eq!(resolve(&e, "https://other.com/").0, "com.apple.Safari");
    }

    #[test]
    fn matcher_regex_against_full_url() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{ match: /github\.com\/(paymentology|tutuka)\//,
                          open: "com.google.Chrome" }],
            };"#,
        );
        assert_eq!(
            resolve(&e, "https://github.com/paymentology/grinch").0,
            "com.google.Chrome"
        );
        assert_eq!(resolve(&e, "https://github.com/").0, "com.apple.Safari");
    }

    #[test]
    fn matcher_regex_default_is_case_sensitive() {
        // Regression: previously Grinch forced case_insensitive(true) on
        // every regex. Now matches Finicky / native JS RegExp.test, which
        // is case-sensitive unless the `i` flag is set.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{ match: /github\.com/, open: "com.google.Chrome" }],
            };"#,
        );
        assert_eq!(resolve(&e, "https://github.com/").0, "com.google.Chrome");
        // Same domain, mixed case — must NOT match without /i.
        assert_eq!(resolve(&e, "https://GitHub.com/").0, "com.apple.Safari");
    }

    #[test]
    fn matcher_regex_i_flag_makes_it_case_insensitive() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{ match: /github\.com/i, open: "com.google.Chrome" }],
            };"#,
        );
        assert_eq!(resolve(&e, "https://github.com/").0, "com.google.Chrome");
        assert_eq!(resolve(&e, "https://GitHub.com/").0, "com.google.Chrome");
        assert_eq!(resolve(&e, "https://GITHUB.COM/").0, "com.google.Chrome");
    }

    #[test]
    fn matcher_wildcard_with_implicit_protocol_prefix() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{ match: "zoom.us/j/*", open: "us.zoom.xos" }],
            };"#,
        );
        assert_eq!(resolve(&e, "https://zoom.us/j/123").0, "us.zoom.xos");
        assert_eq!(resolve(&e, "zoom.us/j/123").0, "us.zoom.xos");
        assert_eq!(resolve(&e, "https://other.com/").0, "com.apple.Safari");
    }

    #[test]
    fn matcher_from_reads_opener_bundle_id() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{ match: from("com.tinyspeck.slackmacgap"),
                          open: "com.google.Chrome" }],
            };"#,
        );
        let slack = opener("com.tinyspeck.slackmacgap", "Slack");
        let (browser, _) = resolve_with(&e, "https://x/", &slack, ModifierFlags::default());
        assert_eq!(browser, "com.google.Chrome");

        let mail = opener("com.apple.Mail", "Mail");
        let (browser, _) = resolve_with(&e, "https://x/", &mail, ModifierFlags::default());
        assert_eq!(browser, "com.apple.Safari");
    }

    #[test]
    fn matcher_array_is_or() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{ match: ["github.com", "gitlab.com"],
                          open: "com.google.Chrome" }],
            };"#,
        );
        assert_eq!(resolve(&e, "https://github.com/").0, "com.google.Chrome");
        assert_eq!(resolve(&e, "https://gitlab.com/").0, "com.google.Chrome");
        assert_eq!(resolve(&e, "https://other.com/").0, "com.apple.Safari");
    }

    #[test]
    fn matcher_fn_url_only() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{ match: (url) => url.searchParams.get("browser") === "chrome",
                          open: "com.google.Chrome" }],
            };"#,
        );
        assert_eq!(
            resolve(&e, "https://x/?browser=chrome").0,
            "com.google.Chrome"
        );
        assert_eq!(
            resolve(&e, "https://x/?browser=other").0,
            "com.apple.Safari"
        );
        assert_eq!(resolve(&e, "https://x/").0, "com.apple.Safari");
    }

    #[test]
    fn matcher_fn_with_ctx_reads_opener_and_modifiers() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: (url, ctx) =>
                        ctx.opener.bundleId === "com.outlook.X" && ctx.modifiers.shift,
                    open: "com.google.Chrome",
                }],
            };"#,
        );
        let outlook = opener("com.outlook.X", "Outlook");
        let no_shift = ModifierFlags::default();
        let with_shift = ModifierFlags {
            shift: true,
            ..ModifierFlags::default()
        };
        assert_eq!(
            resolve_with(&e, "https://x/", &outlook, no_shift).0,
            "com.apple.Safari",
        );
        assert_eq!(
            resolve_with(&e, "https://x/", &outlook, with_shift).0,
            "com.google.Chrome",
        );
    }

    // ---------- compile_rewriter per variant ----------

    #[test]
    fn rewriter_strip_removes_named_params() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [strip("utm_source", "utm_medium", "fbclid")],
            };"#,
        );
        let (_, url) = resolve(&e, "https://x/?utm_source=a&q=1&fbclid=xyz");
        assert_eq!(url, "https://x/?q=1");
    }

    #[test]
    fn rewriter_strip_prefix_wildcard() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [strip("utm_*")],
            };"#,
        );
        let (_, url) = resolve(&e, "https://x/?utm_a=1&utm_b=2&keep=ok");
        assert_eq!(url, "https://x/?keep=ok");
    }

    #[test]
    fn rewriter_literal_string_replaces_url() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [{ match: "old.example.com/*",
                            url: "https://new.example.com/" }],
            };"#,
        );
        let (_, url) = resolve(&e, "https://old.example.com/path");
        assert_eq!(url, "https://new.example.com/");
    }

    #[test]
    fn rewriter_fn_returning_string() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [{ match: "*.medium.com/*",
                            url: (url) => "https://scribe.rip" + url.pathname }],
            };"#,
        );
        let (_, url) = resolve(&e, "https://x.medium.com/some-article");
        assert_eq!(url, "https://scribe.rip/some-article");
    }

    #[test]
    fn rewriter_fn_returning_url_instance_via_mutation() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [{
                    match: (url) => url.protocol === "http:",
                    url: (url) => { url.protocol = "https:"; return url; },
                }],
            };"#,
        );
        let (_, url) = resolve(&e, "http://example.com/path");
        assert_eq!(url, "https://example.com/path");
    }

    #[test]
    fn rewriter_fn_returning_legacy_object_concatenates_fields() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [{
                    match: "*.slack.com/archives/*",
                    url: (url) => ({ protocol: "slack", host: "channel",
                                     pathname: "", search: "team=foo" }),
                }],
            };"#,
        );
        let (_, url) = resolve(&e, "https://acme.slack.com/archives/C0/p1");
        assert_eq!(url, "slack://channel?team=foo");
    }

    #[test]
    fn rewriter_fn_returning_null_drops_url() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [{ match: (url) => url.hostname === "tracking.example.com",
                            url: () => null }],
            };"#,
        );
        let (browser, url) = resolve(&e, "https://tracking.example.com/pixel");
        assert_eq!(browser, ""); // suppress
        assert_eq!(url, "about:blank");
    }

    #[test]
    fn rewriter_chain_applies_in_order() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [
                    strip("utm_source"),
                    {
                        match: (url) => url.protocol === "http:",
                        url: (url) => { url.protocol = "https:"; return url; },
                    },
                ],
            };"#,
        );
        let (_, url) = resolve(&e, "http://example.com/?utm_source=a&q=1");
        assert_eq!(url, "https://example.com/?q=1");
    }

    // ---------- Targets ----------

    #[test]
    fn target_null_suppresses() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{ match: "tracking.com", open: null }],
            };"#,
        );
        let (browser, url) = resolve(&e, "https://tracking.com/pixel");
        assert_eq!(browser, "");
        assert_eq!(url, "about:blank");
    }

    #[test]
    fn target_fn_returning_string() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{ match: (url) => true, open: (url) => "com.google.Chrome" }],
            };"#,
        );
        assert_eq!(resolve(&e, "https://x/").0, "com.google.Chrome");
    }

    #[test]
    fn target_fn_returning_browser_object() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{ match: (url) => true,
                          open: (url) => ({ name: "com.google.Chrome",
                                            args: ["--incognito"] }) }],
            };"#,
        );
        assert_eq!(resolve(&e, "https://x/").0, "com.google.Chrome");
    }

    #[test]
    fn target_browser_key_lookup_against_browsers_map() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                browsers: { work: { name: "com.google.Chrome", args: ["--guest"] } },
                rules: [{ match: "x.com", open: "work" }],
            };"#,
        );
        assert_eq!(resolve(&e, "https://x.com/").0, "com.google.Chrome");
    }

    #[test]
    fn target_browser_alias_finicky_browser_field() {
        // Finicky uses `browser:` where Grinch uses `open:` — should accept both.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{ match: "x.com", browser: "com.google.Chrome" }],
            };"#,
        );
        assert_eq!(resolve(&e, "https://x.com/").0, "com.google.Chrome");
    }

    // ---------- Combined entries ----------

    #[test]
    fn combined_match_url_open_rewrites_then_routes() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: "itunes.apple.com/app/*",
                    url: (url) => "https://apps.apple.com" + url.pathname,
                    open: "com.apple.AppStore",
                }],
            };"#,
        );
        let (browser, url) = resolve(&e, "https://itunes.apple.com/app/123");
        assert_eq!(browser, "com.apple.AppStore");
        assert_eq!(url, "https://apps.apple.com/app/123");
    }

    // ---------- ctx semantics ----------

    #[test]
    fn ctx_url_pinned_to_input_after_global_rewrite() {
        // ctx.url stays as the original input even when global rewrites have
        // mutated the URL — by design, so handlers can branch on the click.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [{ match: (url) => true,
                            url: (url) => "https://rewritten.com/" }],
                rules: [{
                    match: (url, ctx) => ctx.url === "https://original.com/",
                    open: "com.google.Chrome",
                }],
            };"#,
        );
        let (browser, url) = resolve(&e, "https://original.com/");
        assert_eq!(browser, "com.google.Chrome");
        assert_eq!(url, "https://rewritten.com/");
    }

    #[test]
    fn ctx_originalurl_aliases_url() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: (url, ctx) => ctx.url === ctx.originalUrl,
                    open: "com.google.Chrome",
                }],
            };"#,
        );
        assert_eq!(resolve(&e, "https://x/").0, "com.google.Chrome");
    }

    // ---------- UserFn arity contract ----------

    #[test]
    fn arity_url_only_clears_runtime_needs() {
        // A url-only matcher must NOT mark needs_opener / needs_modifiers,
        // so AppDelegate skips frontmost_opener() and current_modifier_flags()
        // entirely on real clicks.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{ match: (url) => url.hostname === "x",
                          open: "com.google.Chrome" }],
            };"#,
        );
        assert!(!e.needs_opener());
        assert!(!e.needs_modifiers());
        assert_eq!(resolve(&e, "https://x/").0, "com.google.Chrome");
    }

    #[test]
    fn arity_with_ctx_marks_runtime_needs() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: (url, ctx) => ctx.opener.bundleId === "x",
                    open: "com.google.Chrome",
                }],
            };"#,
        );
        assert!(e.needs_opener());
        assert!(e.needs_modifiers());
    }

    #[test]
    fn arity_zero_treated_as_url_only() {
        // `() => null` is length 0 — Grinch's contract is `length >= 2 → ctx`,
        // so length 0 is treated as url-only too.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [{ match: "x", url: () => null }],
            };"#,
        );
        assert!(!e.needs_opener());
        let (browser, url) = resolve(&e, "https://x");
        assert_eq!(browser, "");
        assert_eq!(url, "about:blank");
    }

    #[test]
    fn arity_default_param_is_treated_as_url_only_per_contract() {
        // (url, ctx = {}) — JS's `f.length` excludes default-param slots, so
        // it reads as 1, and Grinch's contract treats it as url-only. The
        // user's default `{}` kicks in. Documented footgun; this test pins
        // the behaviour so we notice if it ever changes.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: (url, ctx = {}) => (ctx.opener && ctx.opener.bundleId) === "x",
                    open: "com.google.Chrome",
                }],
            };"#,
        );
        assert!(!e.needs_opener());
        // Even with a "real" opener, the matcher sees ctx = {} (its default),
        // so `ctx.opener` is undefined and the rule never fires.
        let real = opener("x", "X");
        assert_eq!(
            resolve_with(&e, "https://x/", &real, ModifierFlags::default()).0,
            "com.apple.Safari",
        );
    }

    // ---------- URL polyfill ----------

    #[test]
    fn polyfill_url_round_trips_full_href() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [{ match: (url) => true, url: (url) => url.href }],
            };"#,
        );
        let (_, url) = resolve(&e, "https://user:pw@example.com:8443/path?q=1#frag");
        assert_eq!(url, "https://user:pw@example.com:8443/path?q=1#frag");
    }

    #[test]
    fn polyfill_searchparams_set_and_delete_propagate() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [{
                    match: (url) => true,
                    url: (url) => {
                        url.searchParams.delete("utm_source");
                        url.searchParams.set("added", "1");
                        return url;
                    },
                }],
            };"#,
        );
        let (_, url) = resolve(&e, "https://x/?utm_source=a&q=1");
        // searchParams iteration order is implementation-defined for `set`
        // on a brand-new key, so check the components rather than full eq.
        assert!(!url.contains("utm_source"));
        assert!(url.contains("q=1"));
        assert!(url.contains("added=1"));
    }

    #[test]
    fn polyfill_searchparams_size_property() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: () => true,
                    open: (url) => "n:" + url.searchParams.size,
                }],
            };"#,
        );
        assert_eq!(resolve(&e, "https://x/?a=1&b=2&c=3").0, "n:3");
        assert_eq!(resolve(&e, "https://x/").0, "n:0");
    }

    #[test]
    fn polyfill_searchparams_for_of_iterates_pairs() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: () => true,
                    open: (url) => {
                        var keys = [];
                        for (var pair of url.searchParams) keys.push(pair[0]);
                        return "k:" + keys.join(",");
                    },
                }],
            };"#,
        );
        assert_eq!(resolve(&e, "https://x/?a=1&b=2&c=3").0, "k:a,b,c");
    }

    #[test]
    fn polyfill_searchparams_for_each_with_value_key_args() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: () => true,
                    open: (url) => {
                        var seen = [];
                        url.searchParams.forEach(function(value, key) {
                            seen.push(key + "=" + value);
                        });
                        return "p:" + seen.join("|");
                    },
                }],
            };"#,
        );
        assert_eq!(resolve(&e, "https://x/?a=1&b=2").0, "p:a=1|b=2");
    }

    #[test]
    fn polyfill_searchparams_keys_values_iterators() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: () => true,
                    open: (url) => {
                        var ks = []; var vs = [];
                        for (var k of url.searchParams.keys())   ks.push(k);
                        for (var v of url.searchParams.values()) vs.push(v);
                        return ks.join(",") + "/" + vs.join(",");
                    },
                }],
            };"#,
        );
        assert_eq!(resolve(&e, "https://x/?a=1&b=2").0, "a,b/1,2");
    }

    #[test]
    fn polyfill_hostname_setter_propagates_to_href() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [{ match: (url) => true,
                            url: (url) => { url.hostname = "moved.com"; return url; } }],
            };"#,
        );
        let (_, url) = resolve(&e, "https://original.com/path");
        assert_eq!(url, "https://moved.com/path");
    }

    // ---------- Parse-side warnings ----------

    #[test]
    fn parse_browser_jsval_handles_args_and_openinbackground() {
        // Object form with both fields. We can't directly read BrowserSpec,
        // but we can verify it routes correctly and the engine accepted it.
        let e = build_engine(
            r#"module.exports = {
                default: { name: "com.spotify.client", openInBackground: true,
                           args: ["--no-fork"] },
            };"#,
        );
        assert_eq!(resolve(&e, "https://x/").0, "com.spotify.client");
    }

    #[test]
    fn browser_spec_string_with_profile_shorthand() {
        // Finicky-style "Name:Profile" shorthand. Splits on first `:`
        // when the prefix resolves to a Chromium-family browser.
        let e = build_engine(r#"module.exports = { default: "com.google.Chrome:Work" };"#);
        // Browser ID survives unchanged; profile expansion is into args
        // (not directly observable from resolve()'s public surface, but
        // we can at least verify the bundle ID is right).
        assert_eq!(resolve(&e, "https://x/").0, "com.google.Chrome");
    }

    #[test]
    fn browser_spec_string_with_no_colon_unchanged() {
        let e = build_engine(r#"module.exports = { default: "com.google.Chrome" };"#);
        assert_eq!(resolve(&e, "https://x/").0, "com.google.Chrome");
    }

    #[test]
    fn parse_browser_jsval_apptype_none_suppresses() {
        // appType: "none" is Finicky's explicit no-op browser. Should
        // behave identically to `open: null`.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: "tracking.com",
                    open: { name: "ignored", appType: "none" },
                }],
            };"#,
        );
        let (browser, url) = resolve(&e, "https://tracking.com/");
        assert_eq!(browser, "");
        assert_eq!(url, "about:blank");
    }

    #[test]
    fn parse_browser_jsval_apptype_path_resolves_to_bundle_id() {
        // appType: "path" — point at a real, always-installed system app
        // and assert we recover its bundle ID. Safari ships with macOS,
        // so /Applications/Safari.app exists in CI and on every dev box.
        let e = build_engine(
            r#"module.exports = {
                default: { name: "/Applications/Safari.app", appType: "path" },
            };"#,
        );
        assert_eq!(resolve(&e, "https://x/").0, "com.apple.Safari");
    }

    #[test]
    fn parse_browser_jsval_apptype_bundleid_skips_lookup() {
        // appType: "bundleId" trusts the value verbatim. Even an unknown ID
        // is preserved — the eventual open call is what would fail visibly.
        let e = build_engine(
            r#"module.exports = {
                default: { name: "com.totally.fake", appType: "bundleId" },
            };"#,
        );
        assert_eq!(resolve(&e, "https://x/").0, "com.totally.fake");
    }

    #[test]
    fn parse_browser_jsval_accepts_id_alias_for_bundleid() {
        let e = build_engine(
            r#"module.exports = {
                default: { id: "com.google.Chrome" },
            };"#,
        );
        assert_eq!(resolve(&e, "https://x/").0, "com.google.Chrome");
    }

    // ---------- console wiring ----------

    #[test]
    fn console_callbacks_are_callable_functions() {
        // typeof should be "function" for all five levels — proves the
        // manual block-encoding registration is reaching JSC's bridge.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: () => true,
                    open: () =>
                        typeof __grinchConsoleLog + "/" +
                        typeof __grinchConsoleWarn + "/" +
                        typeof __grinchConsoleError + "/" +
                        typeof __grinchConsoleInfo + "/" +
                        typeof __grinchConsoleDebug,
                }],
            };"#,
        );
        let (browser, _) = resolve(&e, "https://x/");
        assert_eq!(browser, "function/function/function/function/function");
    }

    #[test]
    fn console_log_inside_fn_matcher_does_not_throw() {
        // Calling console.log from a user fn must not throw; the matcher
        // must still be able to return its value. We use the matcher's
        // return to signal success.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: (url) => {
                        console.log("matched", url.hostname);
                        return url.hostname === "example.com";
                    },
                    open: "com.google.Chrome",
                }],
            };"#,
        );
        assert_eq!(resolve(&e, "https://example.com/").0, "com.google.Chrome");
        assert_eq!(resolve(&e, "https://other.com/").0, "com.apple.Safari");
    }

    // ---------- finicky.* namespace ----------

    // ---------- ctx.opener nullability ----------

    #[test]
    fn ctx_opener_is_null_when_opener_unknown() {
        // Default Opener (all-empty strings, pid 0) signals "no opener
        // detected" — ctx.opener should be JS null, matching Finicky v4.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: (url, ctx) => ctx.opener === null,
                    open: "com.google.Chrome",
                }],
            };"#,
        );
        let unknown = Opener::default(); // all empty strings
        let (browser, _) = resolve_with(&e, "https://x/", &unknown, ModifierFlags::default());
        assert_eq!(browser, "com.google.Chrome");
    }

    #[test]
    fn ctx_opener_is_object_when_opener_known() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: (url, ctx) => ctx.opener && ctx.opener.bundleId === "com.x",
                    open: "com.google.Chrome",
                }],
            };"#,
        );
        let known = opener("com.x", "X");
        let (browser, _) = resolve_with(&e, "https://x/", &known, ModifierFlags::default());
        assert_eq!(browser, "com.google.Chrome");
    }

    #[test]
    fn finicky_namespace_is_present_with_all_v4_methods() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: () => true,
                    open: () =>
                        typeof finicky.matchHostnames + "/" +
                        typeof finicky.matchDomains + "/" +
                        typeof finicky.notify + "/" +
                        typeof finicky.getBattery + "/" +
                        typeof finicky.getModifierKeys + "/" +
                        typeof finicky.isAppRunning + "/" +
                        typeof finicky.getSystemInfo + "/" +
                        typeof finicky.getPowerInfo,
                }],
            };"#,
        );
        assert_eq!(
            resolve(&e, "https://x/").0,
            "function/function/function/function/function/function/function/function",
        );
    }

    #[test]
    fn finicky_match_hostnames_is_exact_not_subdomain() {
        // Critical semantic: matchHostnames is === on hostname, NOT
        // subdomain-matching. This is the inverse of Grinch's bare-string
        // matcher. Pin the behaviour.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: finicky.matchHostnames("github.com"),
                    open: "com.google.Chrome",
                }],
            };"#,
        );
        assert_eq!(resolve(&e, "https://github.com/").0, "com.google.Chrome");
        assert_eq!(resolve(&e, "https://api.github.com/").0, "com.apple.Safari");
    }

    #[test]
    fn finicky_match_hostnames_accepts_array_and_regex() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: finicky.matchHostnames(["github.com", /^gitlab\./]),
                    open: "com.google.Chrome",
                }],
            };"#,
        );
        assert_eq!(resolve(&e, "https://github.com/").0, "com.google.Chrome");
        assert_eq!(resolve(&e, "https://gitlab.com/").0, "com.google.Chrome");
        assert_eq!(resolve(&e, "https://example.com/").0, "com.apple.Safari");
        // Subdomain still doesn't match the exact-hostname string.
        assert_eq!(resolve(&e, "https://api.github.com/").0, "com.apple.Safari");
    }

    #[test]
    fn finicky_get_system_info_returns_shaped_object() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: () => true,
                    open: () => {
                        var info = finicky.getSystemInfo();
                        return "k:" + Object.keys(info).sort().join(",");
                    },
                }],
            };"#,
        );
        // The Rust bridge fills both fields with gethostname() output;
        // we can't predict the value, just the shape.
        assert_eq!(resolve(&e, "https://x/").0, "k:localizedName,name");
    }

    #[test]
    fn finicky_get_modifier_keys_returns_full_v4_shape() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: () => true,
                    open: () => "k:" + Object.keys(finicky.getModifierKeys()).sort().join(","),
                }],
            };"#,
        );
        // capsLock, command, control, fn, function, option, shift — sorted.
        assert_eq!(
            resolve(&e, "https://x/").0,
            "k:capsLock,command,control,fn,function,option,shift",
        );
    }

    #[test]
    fn finicky_is_app_running_returns_boolean() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: () => true,
                    open: () => "t:" + typeof finicky.isAppRunning("com.apple.finder"),
                }],
            };"#,
        );
        assert_eq!(resolve(&e, "https://x/").0, "t:boolean");
    }

    #[test]
    fn finicky_notify_is_inert_stub() {
        // Calling notify must not throw, must return undefined; matches
        // Finicky's deprecated stub behaviour.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: () => true,
                    open: () => "v:" + (typeof finicky.notify() === "undefined"),
                }],
            };"#,
        );
        assert_eq!(resolve(&e, "https://x/").0, "v:true");
    }

    #[test]
    fn fetch_window_title_bridge_is_a_function() {
        // Regression for the same _Block_signature issue that bit console:
        // without ManualBlockEncoding, JSC saw __grinchFetchWindowTitle as
        // an opaque NSBlock and the JS-side getter fell through to "".
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: () => true,
                    open: () => "t:" + typeof __grinchFetchWindowTitle,
                }],
            };"#,
        );
        assert_eq!(resolve(&e, "https://x/").0, "t:function");
    }

    #[test]
    fn console_log_handles_objects_and_primitives() {
        // The prelude's `__grinchFormatArgs` must not throw on mixed types
        // — number, string, object, null, undefined.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: () => {
                        console.log("string", 42, { a: 1 }, null, undefined);
                        return true;
                    },
                    open: "com.google.Chrome",
                }],
            };"#,
        );
        assert_eq!(resolve(&e, "https://x/").0, "com.google.Chrome");
    }
}
