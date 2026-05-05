// Engine: walks the JSValue export tree at config-load time and translates
// every match pattern + rewrite into a native Rust representation. The hot
// path then uses these directly — JS is only re-entered for user-written
// `(url, ctx)` functions, which are the explicit slow path.

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

pub struct Resolution {
    /// `Rc<BrowserSpec>` so the resolve hot path is a refcount bump
    /// instead of cloning the inner String + Vec on every match. Callers
    /// can still treat it as `&BrowserSpec` via auto-deref.
    pub browser: Rc<BrowserSpec>,
    pub url: String,
}

enum Matcher {
    Always,
    Regex(Regex),
    Domain(Vec<String>),
    From(Vec<String>),
    Running(Vec<String>),
    Fn(Retained<JSValue>),
}

enum Rewriter {
    Drop,
    Strip {
        exact: HashSet<String>,
        prefixes: Vec<String>,
    },
    Literal(String),
    Fn(Retained<JSValue>),
}

enum Target {
    Browser(Rc<BrowserSpec>),
    Fn(Retained<JSValue>),
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
/// (see `running_cache`, `default_browser`, `Rule.target`). The engine is
/// only ever exercised on the main run loop (Apple Event dispatch is
/// main-thread-only on macOS), and `CURRENT_OPENER_PID` likewise assumes
/// a single in-flight resolve. Don't try to call `.resolve()` from a
/// background thread — it'll fail to compile.
pub struct Engine {
    default_browser: Rc<BrowserSpec>,
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
    running_cache: RefCell<Option<HashSet<String>>>,
    /// True if any rule reads opener (via `from()` matcher or any user fn
    /// predicate/rewrite/target — fns might dereference ctx.opener).
    /// AppDelegate skips frontmost_opener() when this is false, saving 4
    /// LaunchServices/IPC round-trips per click.
    needs_opener: bool,
    /// True if any rule reads modifier flags (any user fn predicate, since
    /// fns can read ctx.modifiers). AppDelegate skips
    /// current_modifier_flags() when this is false.
    needs_modifiers: bool,
}

#[derive(Debug)]
pub enum EngineError {
    MissingDefault,
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
        let url_ctor = unsafe { eval_global(&ctx, "URL") }.expect("prelude URL constructor missing");

        install_window_title_callback(&ctx);

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
        let default_browser = resolve_browser(&default_val, &browsers).unwrap_or_else(|| {
            Rc::new(BrowserSpec::from_bundle_id(
                js_to_string(&default_val).unwrap_or_default(),
            ))
        });

        // rewrites
        let rewrites = key(&exports, "rewrite")
            .map(|arr| parse_rewrite_array(&arr, &function_ctor))
            .unwrap_or_default();

        // rules — accept Finicky's `handlers` as well as Grinch's `rules`
        let rules_val = key(&exports, "rules").or_else(|| key(&exports, "handlers"));
        let rules = rules_val
            .map(|arr| parse_rule_array(&arr, &browsers, &regexp_ctor, &function_ctor))
            .unwrap_or_default();

        let (needs_opener, needs_modifiers) = analyse_runtime_needs(&rewrites, &rules);

        Ok(Self {
            default_browser,
            browsers,
            rewrites,
            rules,
            ctx,
            rewrite_result_helper,
            make_ctx_helper,
            url_ctor,
            running_cache: RefCell::new(None),
            needs_opener,
            needs_modifiers,
        })
    }

    /// True if AppDelegate should populate the opener (frontmost app +
    /// bundle ID/name/path/pid) before calling resolve(). False for
    /// declarative-only configs that never reference opener — saves
    /// ~100–500 µs of LaunchServices IPC per click.
    pub fn needs_opener(&self) -> bool { self.needs_opener }

    /// True if AppDelegate should fetch modifier flags before calling
    /// resolve(). False for configs without any user fn matchers/rewriters
    /// (only those can read modifiers, via `ctx.modifiers`).
    pub fn needs_modifiers(&self) -> bool { self.needs_modifiers }

    /// Hot path: resolve a URL given the opener and modifier flags.
    pub fn resolve(&self, url_string: &str, opener: &Opener, modifiers: ModifierFlags) -> Resolution {
        // Stash the opener's PID so the __grinchFetchWindowTitle block can find
        // the right process if user code accesses opener.windowTitle. Cheap
        // unconditional write; the AX call only fires on JS access.
        CURRENT_OPENER_PID.store(opener.pid, Ordering::Relaxed);

        let mut current = url_string.to_string();
        let mut host = quick_host(&current);
        let rc = ResolveCtx::new(
            &self.ctx,
            &self.rewrite_result_helper,
            &self.make_ctx_helper,
            &self.url_ctor,
            &self.running_cache,
            opener,
            modifiers,
            url_string,
        );

        // Global rewrites — apply every matching one in order.
        for rw in &self.rewrites {
            if any_match(&rw.matchers, &current, host.as_deref(), &rc) {
                match apply_rewrite(&rw.rewriter, &current, &rc) {
                    RewriteOutcome::Changed(s) => {
                        current = s;
                        host = quick_host(&current);
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
                        current = s;
                        host = quick_host(&current);
                    }
                    RewriteOutcome::Unchanged => {}
                    RewriteOutcome::Drop => return suppressed(),
                }
            }
            match &rule.target {
                Target::Browser(b) => {
                    return Resolution { browser: Rc::clone(b), url: current };
                }
                Target::Suppress => {
                    return suppressed();
                }
                Target::Fn(f) => {
                    let Some(args) = rc.fn_args(&current) else { continue };
                    let result = unsafe { f.callWithArguments(Some(&args)) };
                    if let Some(r) = result {
                        if !unsafe { r.isUndefined() } && !unsafe { r.isNull() } {
                            let spec = resolve_browser(&r, &self.browsers).unwrap_or_else(|| {
                                Rc::new(BrowserSpec::from_bundle_id(
                                    js_to_string(&r).unwrap_or_default(),
                                ))
                            });
                            return Resolution { browser: spec, url: current };
                        }
                    }
                }
            }
        }

        Resolution {
            browser: Rc::clone(&self.default_browser),
            url: current,
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
fn analyse_runtime_needs(rewrites: &[RewriteRule], rules: &[Rule]) -> (bool, bool) {
    fn matchers_need(ms: &[Matcher]) -> (bool, bool) {
        let mut o = false;
        let mut m = false;
        for matcher in ms {
            match matcher {
                Matcher::From(_) => o = true,
                Matcher::Fn(_) => {
                    o = true;
                    m = true;
                }
                Matcher::Always
                | Matcher::Regex(_)
                | Matcher::Domain(_)
                | Matcher::Running(_) => {}
            }
        }
        (o, m)
    }
    fn rewriter_needs(r: &Rewriter) -> (bool, bool) {
        match r {
            Rewriter::Fn(_) => (true, true),
            _ => (false, false),
        }
    }

    let mut needs_opener = false;
    let mut needs_modifiers = false;

    for rw in rewrites {
        let (o, m) = matchers_need(&rw.matchers);
        needs_opener |= o;
        needs_modifiers |= m;
        let (o, m) = rewriter_needs(&rw.rewriter);
        needs_opener |= o;
        needs_modifiers |= m;
    }
    for rule in rules {
        let (o, m) = matchers_need(&rule.matchers);
        needs_opener |= o;
        needs_modifiers |= m;
        if let Some(rw) = &rule.rewriter {
            let (o, m) = rewriter_needs(rw);
            needs_opener |= o;
            needs_modifiers |= m;
        }
        if matches!(&rule.target, Target::Fn(_)) {
            needs_opener = true;
            needs_modifiers = true;
        }
    }

    (needs_opener, needs_modifiers)
}

fn suppressed() -> Resolution {
    Resolution {
        browser: Rc::new(BrowserSpec::empty()),
        url: "about:blank".to_string(),
    }
}

// MARK: - Resolve context (per-call)

struct ResolveCtx<'a> {
    ctx: &'a JSContext,
    rewrite_result_helper: &'a JSValue,
    make_ctx_helper: &'a JSValue,
    url_ctor: &'a JSValue,
    opener: &'a Opener,
    modifiers: ModifierFlags,
    running_cache: &'a RefCell<Option<HashSet<String>>>,
    /// URL passed to resolve() — exposed to user fns as `ctx.url` /
    /// `ctx.originalUrl`. Stays constant for the entire resolve even if
    /// rewrites fire; user code reads the *current* URL via the first arg.
    original_url: &'a str,
    /// ctx object — built lazily on first fn call, then reused. Opener and
    /// modifiers are constant for a resolve, so this never needs invalidating.
    cached_ctx: RefCell<Option<Retained<JSValue>>>,
    /// fn args NSArray for the current URL string. Invalidated when the URL
    /// changes between rewrites; cached_ctx is preserved across that. The
    /// key is `Box<str>` (not `String`) to halve the per-cache allocation
    /// footprint — we never push to it, so the capacity field is dead weight.
    fn_args_cache: RefCell<Option<(Box<str>, Retained<NSArray>)>>,
}

impl<'a> ResolveCtx<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        ctx: &'a JSContext,
        rewrite_result_helper: &'a JSValue,
        make_ctx_helper: &'a JSValue,
        url_ctor: &'a JSValue,
        running_cache: &'a RefCell<Option<HashSet<String>>>,
        opener: &'a Opener,
        modifiers: ModifierFlags,
        original_url: &'a str,
    ) -> Self {
        Self {
            ctx,
            rewrite_result_helper,
            make_ctx_helper,
            url_ctor,
            opener,
            modifiers,
            running_cache,
            original_url,
            cached_ctx: RefCell::new(None),
            fn_args_cache: RefCell::new(None),
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
            self.original_url,
            self.opener,
            self.modifiers,
        )?;
        *self.cached_ctx.borrow_mut() = Some(v.clone());
        Some(v)
    }

    /// Build the args for a user `(url, ctx) => ...` invocation. First arg is
    /// a URL instance (Finicky-compatible — supports .href, .hostname, .protocol,
    /// .searchParams etc.); second arg is the cached ctx object with opener
    /// and modifiers. NSArray itself is cached while the URL string is unchanged.
    /// Returns None if the prelude is broken — callers treat that as a fn that
    /// doesn't match (rather than panicking).
    fn fn_args(&self, url: &str) -> Option<Retained<NSArray>> {
        if let Some((cached_url, args)) = self.fn_args_cache.borrow().as_ref() {
            if cached_url.as_ref() == url {
                return Some(args.clone());
            }
        }
        let url_instance = build_url_instance(self.url_ctor, self.ctx, url);
        let ctx_val = self.ctx_object()?;
        let url_obj: Retained<AnyObject> = unsafe { Retained::cast_unchecked(url_instance) };
        let ctx_obj: Retained<AnyObject> = unsafe { Retained::cast_unchecked(ctx_val) };
        let args = NSArray::from_retained_slice(&[url_obj, ctx_obj]);
        *self.fn_args_cache.borrow_mut() = Some((Box::from(url), args.clone()));
        Some(args)
    }
}

/// Install a block as `__grinchFetchWindowTitle` on the JSContext. The block
/// reads CURRENT_OPENER_PID (set by resolve()) and calls into the AX API.
/// Lazy: the JS getter on opener.windowTitle only invokes this when user code
/// reads it, so configs that don't touch windowTitle pay nothing.
fn install_window_title_callback(ctx: &JSContext) {
    // Block returns +1-retained NSString*. JSC's objc bridge expects the same
    // ABI as a method returning `id`, so a `*mut NSString` with retain count
    // bumped is exactly right; JSC autoreleases on the JS side.
    let block = RcBlock::new(|| -> *mut NSString {
        let pid = CURRENT_OPENER_PID.load(Ordering::Relaxed);
        let title = frontmost_window_title(pid);
        Retained::into_raw(NSString::from_str(&title))
    });
    // SAFETY: A block is an Objective-C object (NSBlock). `&Block<F>` is
    // ABI-compatible with a block pointer, which is itself a valid `id`.
    // JSC accepts blocks as JS-callable functions via the standard objc bridge.
    let block_obj: &AnyObject = unsafe {
        &*(&*block as *const block2::Block<dyn Fn() -> *mut NSString> as *const AnyObject)
    };
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
    url: &str,
    opener: &Opener,
    m: ModifierFlags,
) -> Option<Retained<JSValue>> {
    let url_v = js_string(ctx, url);
    let opener_id_v = js_string(ctx, &opener.bundle_id);
    let opener_name_v = js_string(ctx, &opener.name);
    let opener_path_v = js_string(ctx, &opener.path);
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
        Matcher::Fn(f) => {
            let Some(args) = rc.fn_args(url) else { return false };
            let result = unsafe { f.callWithArguments(Some(&args)) };
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
    hb.len() > pb.len() + 1
        && hb[hb.len() - pb.len() - 1] == b'.'
        && hb.ends_with(pb)
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
        Rewriter::Fn(f) => {
            let Some(args) = rc.fn_args(url) else { return RewriteOutcome::Unchanged };
            let Some(raw) = (unsafe { f.callWithArguments(Some(&args)) }) else {
                return RewriteOutcome::Unchanged;
            };
            // Normalise via __grinchRewriteResult: handles string | URL |
            // LegacyURLObject | null in one place, returning a string href
            // or JS null.
            let raw_obj: Retained<AnyObject> = unsafe { Retained::cast_unchecked(raw) };
            let helper_args = NSArray::from_retained_slice(&[raw_obj]);
            let Some(normalised) = (unsafe { rc.rewrite_result_helper.callWithArguments(Some(&helper_args)) }) else {
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
        return BrowserSpec::from_bundle_id(resolve_browser_identifier(&s));
    }
    if !unsafe { v.isObject() } {
        return BrowserSpec::empty();
    }

    // Bundle ID source: `id`, `bundleId`, or `name`. `appType` (Finicky) is
    // read but doesn't change behavior — we always normalise to a bundle ID.
    let raw_id = key(v, "id")
        .or_else(|| key(v, "bundleId"))
        .or_else(|| key(v, "name"))
        .and_then(|x| js_to_string(&x))
        .unwrap_or_default();
    let bundle_id = resolve_browser_identifier(&raw_id);

    let mut args = key(v, "args").map(|a| js_array_to_strings(&a)).unwrap_or_default();
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

    BrowserSpec { bundle_id, args, open_in_background, creates_new_instance }
}

fn resolve_browser(
    v: &JSValue,
    browsers: &std::collections::HashMap<String, Rc<BrowserSpec>>,
) -> Option<Rc<BrowserSpec>> {
    if unsafe { v.isString() } {
        let s = js_to_string(v)?;
        if let Some(named) = browsers.get(&s) {
            return Some(Rc::clone(named));
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
        let Some(item) = js_array_at(arr, i) else { continue };
        let match_val = key(&item, "match");
        // `open` (Grinch) and `browser` (Finicky) are aliases.
        let open_val = key(&item, "open").or_else(|| key(&item, "browser"));
        let url_val = key(&item, "url");
        let matchers = compile_matchers(match_val.as_deref(), regexp_ctor, function_ctor);

        // Optional per-rule rewriter (combined entry).
        let rewriter = url_val.as_ref().and_then(|uv| compile_rewriter(uv, function_ctor));

        // Target: `open: null` → suppress; fn → Fn; resolvable browser → Browser.
        // If `open`/`browser` is absent but a `url` rewrite IS present, that's
        // a pure rewrite-on-match (no routing change) — treat as default-target.
        let target = match open_val.as_ref() {
            Some(ov) if unsafe { ov.isNull() } => Target::Suppress,
            Some(ov) if is_function(ov, function_ctor) => Target::Fn(ov.clone()),
            Some(ov) => match resolve_browser(ov, browsers) {
                Some(b) => Target::Browser(b),
                None => continue,
            },
            None => {
                // No browser specified. If there's also no url rewrite, the
                // entry is malformed — skip. Otherwise treat as a rewrite
                // that doesn't affect routing (target = "fall through to
                // next rule"). We model "fall through" by NOT emitting a
                // Rule at all; instead push a global RewriteRule. But we
                // can't push to rewrites from here, so for simplicity skip.
                continue;
            }
        };
        out.push(Rule { matchers, rewriter, target });
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
        let Some(item) = js_array_at(arr, i) else { continue };

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
        let Some(rewriter) = compile_rewriter(&uv, function_ctor) else { continue };
        out.push(RewriteRule { matchers, rewriter });
    }
    out
}

fn compile_rewriter(v: &JSValue, function_ctor: &JSValue) -> Option<Rewriter> {
    if unsafe { v.isNull() } {
        return Some(Rewriter::Drop);
    }
    if is_function(v, function_ctor) {
        return Some(Rewriter::Fn(v.retain()));
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

fn compile_matcher(
    v: &JSValue,
    regexp_ctor: &JSValue,
    function_ctor: &JSValue,
) -> Option<Matcher> {
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
        // Regex literal /.../ — compile via the regex crate.
        if is_instance_of(v, regexp_ctor) {
            if let Some(pattern) = key(v, "source").and_then(|p| js_to_string(&p)) {
                if let Ok(re) = RegexBuilder::new(&pattern).case_insensitive(true).build() {
                    return Some(Matcher::Regex(re));
                }
            }
        }
        if is_function(v, function_ctor) {
            return Some(Matcher::Fn(v.retain()));
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

    RegexBuilder::new(&anchored).case_insensitive(true).build().ok()
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
/// lowercased hostname or None. Handles `http(s)://`, `//`, `scheme:host`.
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
    let Some(t) = key(v, "__type") else { return false };
    js_to_string(&t).as_deref() == Some(ty)
}

/// Iterate the keys of a JS object as Rust strings, returning (key, value).
/// Values are re-fetched as JSValues so we don't lose JSValue identity (which
/// `JSValue::toDictionary` would erase by recursively converting to NS*).
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
        let Ok(s) = any_key.downcast::<NSString>() else { continue };
        let name = s.to_string();
        if let Some(val) = key(v, &name) {
            out.push((name, val));
        }
    }
    out
}
