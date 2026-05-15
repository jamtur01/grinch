// Engine: walks the JSValue export tree at config-load time and translates
// every match pattern + rewrite into a native Rust representation. The hot
// path then uses these directly — JS is only re-entered for user-written
// `(url, ctx)` functions, which are the explicit slow path.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::Message;
use objc2_foundation::{NSArray, NSString};
use objc2_javascript_core::{JSContext, JSType, JSValue};
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
    pub caps_lock: bool,
    /// macOS Fn / Globe key. Surfaced as both `fn` and `function` in JS for
    /// Finicky-v3-back-compat (Finicky exposes both names with the same
    /// value; we follow suit so configs that read either work unchanged).
    pub function: bool,
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
    /// Zero-based index of the rule whose matcher fired, or `None` for
    /// default-fallback / top-level rewriter Drop. Only the index is
    /// carried on the hot path — the corresponding name/label is looked
    /// up against `Engine.rules` inside the (cold) log writer so resolves
    /// without `logRequests` don't pay for a String clone per click.
    pub matched_rule: Option<usize>,
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
    /// Unwrap a corporate SafeLinks / URL-defense wrapper. Recognises the
    /// Microsoft Defender, Teams, and Proofpoint wrapper shapes; passes
    /// through on hosts it doesn't recognise. See `unwrap_safelink`.
    Safelinks,
    /// Unwrap a Microsoft Teams launcher URL
    /// (`teams.microsoft.com/dl/launcher/launcher.html?url=…`) into the
    /// native `msteams:` scheme. Pass-through on every other host.
    /// See `unwrap_teams_launcher`.
    TeamsLauncher,
}

enum Target {
    Browser(Rc<BrowserSpec>),
    Fn(UserFn),
    Suppress,
}

/// A run of consecutive rules whose `matchers` is exactly one fn — the
/// shape that's eligible for batched JS-side dispatch. At engine init we
/// compile a single JS function that runs all the matchers in sequence
/// and returns the first hit's offset (or -1). One bridge crossing
/// replaces N — measured at ~700 ns saved per skipped matcher. See
/// `analyse_fn_matcher_runs` + `compile_fn_matcher_dispatcher`.
struct FnMatcherRun {
    /// Inclusive index of the first rule in the run.
    start: usize,
    /// Exclusive end — rule indices `[start, end)` are covered.
    end: usize,
    /// JS function: `(url, ctx) → number`. Returns the 0-based offset
    /// within the run of the first matching matcher, or -1.
    dispatcher: Retained<JSValue>,
    /// True if any matcher in the run takes a ctx arg. When false, we can
    /// skip the `__grinchMakeCtx` build and pass undefined for ctx.
    needs_ctx: bool,
}

struct Rule {
    matchers: Vec<Matcher>,
    /// If set, applied to the URL when the rule matches, before resolving target.
    /// Mirrors Finicky's combined `{match, url, browser}` handler entries.
    rewriter: Option<Rewriter>,
    target: Target,
    /// Optional user-supplied `name:` field on the rule entry. Surfaced in
    /// the JSONL request log under `matchedRule.name` and in `--list-rules`
    /// output. None when the user didn't tag the rule.
    name: Option<String>,
    /// Auto-derived label describing the matcher(s) — set even when `name`
    /// is None so logs always have something readable. For declarative
    /// matchers this is the source pattern (`"github.com"`, `"slack:*"`,
    /// `"domain:foo,bar"`); for fn matchers, the first ~60 chars of
    /// `f.toString()` collapsed to one line.
    label: String,
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
/// What `defaultBrowser` resolves to.
///
/// - `Static` = parsed at config load to a concrete spec (the common case).
/// - `Fn` = a user function called at resolve time when no rule matched.
///   Forces `needs_opener` / `needs_modifiers` / `needs_host` on, since the
///   fn might call `url.hostname` or read `ctx.opener`.
/// - `Suppress` = `defaultBrowser: null`. Finicky-compatible — when no
///   rule matches, nothing opens. Mirrors how a rule's `open: null`
///   suppresses an individual URL.
enum DefaultBrowser {
    Static(Rc<BrowserSpec>),
    Fn(UserFn),
    Suppress,
}

pub struct Engine {
    default_browser: DefaultBrowser,
    browsers: std::collections::HashMap<String, Rc<BrowserSpec>>,
    rewrites: Vec<RewriteRule>,
    rules: Vec<Rule>,
    /// Pre-compiled JS dispatchers for runs of fn-only rules (rules whose
    /// `matchers` is exactly one `Matcher::Fn`). Empty for configs that
    /// have no such runs of length ≥ 2; non-empty configs save N–1
    /// JSC bridge crossings per such run on resolves where none of those
    /// rules match. See `FnMatcherRun` for the per-run details.
    fn_matcher_runs: Vec<FnMatcherRun>,
    /// `rule_to_run[i] = Some(j)` iff rule i is covered by
    /// `fn_matcher_runs[j]`. Pre-built at engine init so the resolve
    /// loop can answer "is this rule index inside a dispatched run?"
    /// in O(1) instead of scanning the runs vector each iteration.
    /// Empty (Vec of None) when there are no runs.
    rule_to_run: Vec<Option<usize>>,
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
    /// True if a fn matcher / rewriter / target with ctx exists. When this
    /// is false but `needs_opener` is true (= `from()`-only configs),
    /// AppDelegate uses the lite `frontmost_opener_id` path that skips
    /// `localizedName` + `executableURL` IPC.
    needs_opener_full: bool,
    /// True if any rule reads modifier flags (any user fn predicate, since
    /// fns can read ctx.modifiers). AppDelegate skips
    /// current_modifier_flags() when this is false.
    needs_modifiers: bool,
    /// True if any rule uses `domain()` or a bare-hostname matcher. When
    /// false, `quick_host` (lowercased hostname extract) is skipped on every
    /// resolve — saves ~30-50 ns for configs that route purely on regex /
    /// wildcard / fn matchers.
    needs_host: bool,
    /// Parsed `options` block — the few keys Grinch actually acts on.
    options: OptionsConfig,
    /// Per-resolve JSONL log file. `None` when `options.logRequests` is
    /// off, otherwise a lazy-opened append writer at
    /// `~/Library/Logs/Grinch/Grinch_<engine-init-timestamp>.log`. The
    /// file is created on first write so a flag-on-but-no-traffic engine
    /// doesn't litter empty files.
    log_writer: RefCell<Option<LogWriter>>,
    /// Cached JSValue strings for opener fields (bundleId / name / path).
    /// Most clicks come from the same handful of openers (Mail, Slack,
    /// Outlook…), and the JSC bridge crossing for NSString::from_str +
    /// JSValue::valueWithObject is ~500 ns per call. Caching by Rust string
    /// turns repeated builds into a refcount bump on the cached `Retained`.
    /// Reset implicitly when Engine is rebuilt on config reload — the
    /// JSContext goes with it, taking the cached JSValues along.
    opener_str_cache: RefCell<std::collections::HashMap<String, Retained<JSValue>>>,
    /// Cached `true` / `false` `JSValue`s — referenced by every ctx
    /// build (six modifier flags). Each `js_bool(ctx, b)` is a JSC bridge
    /// crossing of ~100-300 ns; replacing them with refcount-bumped
    /// clones of these cached values saves up to ~2 µs per ctx build.
    js_true: Retained<JSValue>,
    js_false: Retained<JSValue>,
}

#[derive(Debug)]
pub enum EngineError {
    MissingDefault,
    /// One of the prelude globals (`RegExp`, `Function`, `URL`, or a
    /// `__grinch*` helper) was missing or null when the engine tried to
    /// look it up. Almost always caused by user config that overwrites
    /// or deletes the global before exporting.
    PreludeBroken {
        global: &'static str,
    },
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::MissingDefault => write!(
                f,
                "config has no `default` (or `defaultBrowser`) — \
                 add e.g. `default: \"Google Chrome\"` to module.exports"
            ),
            EngineError::PreludeBroken { global } => write!(
                f,
                "prelude global `{global}` is missing or null — your config \
                 likely overwrote or deleted it. Remove the assignment and \
                 reload."
            ),
        }
    }
}

impl Engine {
    pub fn new(loaded: LoadedConfig) -> Result<Self, EngineError> {
        let ctx = loaded.ctx;
        let exports = loaded.exports;

        // Prelude lookups — turn missing / null / undefined globals into
        // config-load errors rather than letting the engine wander off
        // with a broken constructor in hand. A user config that does
        // e.g. `RegExp = null;` before `module.exports = …` doesn't
        // currently crash init (eval_global returns Some(null-JSValue),
        // not None), but it produces opaque downstream throws like
        // "TypeError: null is not an object" on every regex matcher.
        // Failing fast here surfaces the real problem at reload time
        // and lets the previous engine stay in place via the existing
        // `match Engine::new {Err => log; keep prev}` path in AppDelegate.
        let regexp_ctor = require_global(&ctx, "RegExp")?;
        let function_ctor = require_global(&ctx, "Function")?;
        let rewrite_result_helper = require_global(&ctx, "__grinchRewriteResult")?;
        let make_ctx_helper = require_global(&ctx, "__grinchMakeCtx")?;
        let url_ctor = require_global(&ctx, "URL")?;

        install_window_title_callback(&ctx);

        // options block — Finicky-compat. Accept all five v4 keys without
        // erroring so configs ported across don't have to delete them.
        // Anything unknown logs a one-line warning per key.
        let options = key(&exports, "options")
            .filter(|opts| !is_undef_or_null(opts) && unsafe { opts.isObject() })
            .map(|opts| parse_options_block(&opts))
            .unwrap_or_default();

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
        // Three-way classification:
        //   - explicit `null` → Suppress (Finicky-compat: no rule + no
        //     default = nothing happens)
        //   - undefined (key returned a JSValue but it's undefined-typed,
        //     which `is_undef_or_null` catches) → MissingDefault error
        //   - fn → dynamic default
        //   - anything else → static
        let default_browser = if unsafe { default_val.isNull() } {
            DefaultBrowser::Suppress
        } else if unsafe { default_val.isUndefined() } {
            return Err(EngineError::MissingDefault);
        } else if is_function(&default_val, &function_ctor) {
            // Dynamic default browser (Finicky parity): a fn evaluated at
            // resolve time. Detected here at config load so we can mark
            // runtime needs upstream — a default fn always needs ctx (it can
            // read opener / modifiers / url) and a URL polyfill instance.
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

        // Pre-compile JS dispatchers for any runs of consecutive fn-only
        // rules in the rule list. Failure to build a dispatcher (e.g. JSC
        // OOM) silently falls back to the per-matcher path for that run —
        // we never want a perf-only optimisation to break load.
        let fn_matcher_runs = build_fn_matcher_runs(&ctx, &rules);
        let mut rule_to_run: Vec<Option<usize>> = vec![None; rules.len()];
        for (j, run) in fn_matcher_runs.iter().enumerate() {
            for slot in rule_to_run.iter_mut().take(run.end).skip(run.start) {
                *slot = Some(j);
            }
        }

        let mut needs = analyse_runtime_needs(&rewrites, &rules);
        // A dynamic default (fn) is always reachable when no rule matches,
        // and it could read any of url/opener/modifiers. Force them all on.
        if matches!(&default_browser, DefaultBrowser::Fn(_)) {
            needs.opener = true;
            needs.modifiers = true;
            needs.host = true;
        }

        // Cache true/false JSValues — every ctx build (slow path) reads
        // six modifier flags through these. Pre-built here so the hot
        // path is a refcount bump, not a fresh JSC bridge crossing.
        let js_true = js_bool(&ctx, true).ok_or(EngineError::PreludeBroken {
            global: "valueWithBool(true)",
        })?;
        let js_false = js_bool(&ctx, false).ok_or(EngineError::PreludeBroken {
            global: "valueWithBool(false)",
        })?;

        Ok(Self {
            default_browser,
            browsers,
            rewrites,
            rules,
            fn_matcher_runs,
            rule_to_run,
            ctx,
            rewrite_result_helper,
            make_ctx_helper,
            url_ctor,
            needs_opener: needs.opener,
            // Modifiers are only set by fn-with-ctx (see analyse_runtime_needs);
            // a `from()`-only config has needs_opener=true / needs_modifiers=false,
            // and needs only the bundle_id field of the opener.
            needs_opener_full: needs.modifiers,
            needs_modifiers: needs.modifiers,
            needs_host: needs.host,
            options,
            log_writer: RefCell::new(if options.log_requests {
                Some(LogWriter::new(
                    log_file_path(),
                    options.log_rotate_bytes,
                    options.log_rotate_days,
                ))
            } else {
                None
            }),
            opener_str_cache: RefCell::new(std::collections::HashMap::new()),
            js_true,
            js_false,
        })
    }

    /// True if AppDelegate should populate the opener (frontmost app +
    /// bundle ID/name/path/pid) before calling resolve(). False for
    /// declarative-only configs that never reference opener — saves
    /// ~100–500 µs of LaunchServices IPC per click.
    pub fn needs_opener(&self) -> bool {
        self.needs_opener
    }

    /// True if any rule reads opener fields beyond `bundle_id`. When false
    /// but `needs_opener()` is true, AppDelegate uses the lite
    /// `frontmost_opener_id` path that skips `localizedName` +
    /// `executableURL` IPC.
    pub fn needs_opener_full(&self) -> bool {
        self.needs_opener_full
    }

    /// True if AppDelegate should fetch modifier flags before calling
    /// resolve(). False for configs without any user fn matchers/rewriters
    /// (only those can read modifiers, via `ctx.modifiers`).
    pub fn needs_modifiers(&self) -> bool {
        self.needs_modifiers
    }

    /// True when `options.hideIcon` was set in the user config. Read once
    /// by AppDelegate at app launch to decide whether to create the
    /// menu-bar status item. Reloads don't toggle the icon mid-session
    /// (no NSStatusItem add/remove dance) — that's consistent with how
    /// most macOS background apps surface this setting.
    /// Human-readable lines describing the loaded rules — one per rule,
    /// in the order they're evaluated. Format:
    /// `<idx>: [<name>] <match-label> → <target-summary>`. The optional
    /// `[name]` segment is dropped when the rule has no user-supplied name.
    /// Used by `Grinch --list-rules`; safe to call any time.
    pub fn rule_listing(&self) -> Vec<String> {
        self.rules
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let name = r
                    .name
                    .as_ref()
                    .map(|n| format!("[{n}] "))
                    .unwrap_or_default();
                let target = describe_target(&r.target);
                format!("{i}: {name}{label} → {target}", label = r.label)
            })
            .collect()
    }

    pub fn hide_icon(&self) -> bool {
        self.options.hide_icon
    }

    /// Hot path: resolve a URL given the opener and modifier flags.
    ///
    /// Thin wrapper around `resolve_inner` that performs the (optional)
    /// `options.logRequests` write at a single place rather than at
    /// every Resolution-returning return inside the engine. Earlier
    /// versions threaded a `finish()` helper through 5+ return sites,
    /// which paid ~3 ns of move-by-value overhead even when logging was
    /// off (each `return self.finish(...)` had to relocate the
    /// `Resolution` through a function-call boundary). Wrapping the
    /// inner-loop result with a single conditional-write here keeps
    /// the inner branch-free on the resolve hot path.
    ///
    /// The log write itself is in a separate `#[cold]` helper so the
    /// compiler lays it out away from the resolve hot path (icache-
    /// friendly) and biases the predictor toward the log-off branch.
    #[inline]
    pub fn resolve<'u>(
        &self,
        url_string: &'u str,
        opener: &Opener,
        modifiers: ModifierFlags,
    ) -> Resolution<'u> {
        let res = self.resolve_inner(url_string, opener, modifiers);
        if self.options.log_requests {
            self.write_log_entry(url_string, opener, modifiers, &res);
        }
        res
    }

    #[cold]
    #[inline(never)]
    fn write_log_entry(
        &self,
        url_string: &str,
        opener: &Opener,
        modifiers: ModifierFlags,
        res: &Resolution<'_>,
    ) {
        // Look up the rule's name/label here — cold path, so the resolve
        // hot path doesn't pay for the String clone when logging is off.
        let matched = res.matched_rule.and_then(|idx| {
            self.rules.get(idx).map(|r| {
                let name = r.name.as_deref().unwrap_or(r.label.as_str());
                (idx, name)
            })
        });
        let entry = format_log_entry(url_string, opener, modifiers, res, matched);
        if let Some(w) = self.log_writer.borrow_mut().as_mut() {
            w.write(&entry);
        }
    }

    /// Inner resolve loop. Same shape as the pre-logging version: every
    /// Resolution-returning path returns directly. The outer `resolve`
    /// wrapper handles `options.logRequests`. `inline(always)` rather
    /// than `inline` so the optimiser collapses the wrapper-inner pair
    /// into a single function — measured to recover ~1 ns of floor
    /// latency vs the plain `#[inline]` hint.
    #[inline(always)]
    fn resolve_inner<'u>(
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
            &self.js_true,
            &self.js_false,
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
        //
        // Manual index management (rather than `for ... in enumerate`) so
        // we can jump past a whole run of fn-only rules in one dispatcher
        // call instead of N per-matcher bridge crossings. The dispatcher
        // for each run is pre-compiled at engine init — see
        // `build_fn_matcher_runs`. URL doesn't change during fn-matcher
        // iteration (rewrites only fire after a rule matches), so the
        // dispatcher result is consumed immediately and not cached
        // across iterations.
        let mut idx = 0;
        'rules: while idx < self.rules.len() {
            // If we're standing *inside* a fn-only run (start of run OR
            // resumed mid-run after a Target::Fn fall-through), dispatch
            // the remainder in one JS call. The dispatcher takes a
            // `start_offset` so a resume scan picks up after the rule
            // that just fell through — without it the engine would
            // revert to the per-matcher path for the rest of the run
            // and lose the batched-dispatch benefit.
            // O(1) lookup via the pre-built index. Pre-fix this was a
            // linear scan over fn_matcher_runs each iteration —
            // negligible for the dozens-of-rules configs Grinch sees
            // today, but the index keeps the per-resolve cost constant
            // as configs grow.
            if let Some(run) = self
                .rule_to_run
                .get(idx)
                .and_then(|r| r.map(|j| &self.fn_matcher_runs[j]))
            {
                let start_offset = idx - run.start;
                match call_fn_matcher_dispatcher(run, &rc, &current, start_offset) {
                    Some(offset) => {
                        idx = run.start + offset;
                        // Fall through — `idx` now points at the matched
                        // rule. Skip the standard any_match check (which
                        // would redundantly re-invoke the same fn) and
                        // jump straight to rule-processing.
                    }
                    None => {
                        idx = run.end;
                        continue 'rules;
                    }
                }
            } else if !any_match(&self.rules[idx].matchers, &current, host.as_deref(), &rc) {
                idx += 1;
                continue 'rules;
            }
            let rule = &self.rules[idx];
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
                    RewriteOutcome::Drop => return suppressed_at(Some(idx)),
                }
            }
            match &rule.target {
                Target::Browser(b) => {
                    return Resolution {
                        browser: Rc::clone(b),
                        url: current,
                        matched_rule: Some(idx),
                    };
                }
                Target::Suppress => {
                    return suppressed_at(Some(idx));
                }
                Target::Fn(uf) => {
                    let Some(args) = rc.fn_args(&current, uf.needs_ctx) else {
                        idx += 1;
                        continue 'rules;
                    };
                    let result = unsafe { uf.f.callWithArguments(Some(&args)) };
                    if let Some(r) = result {
                        // Combined null-or-undefined check via the C API —
                        // one call replaces two Obj-C dispatches per rule.
                        let kind = js_value_type(&self.ctx, &r);
                        if !matches!(kind, JSType::Null | JSType::Undefined) {
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
                                matched_rule: Some(idx),
                            };
                        }
                    }
                }
            }
            // Target::Fn fell through (null/undefined return or args
            // build failed) — advance to the next rule. Target::Browser
            // / Target::Suppress have returned by now; this `idx += 1`
            // is unreachable on those arms but cheap to guard the Fn path.
            idx += 1;
        }

        // Default fallback. Static = the pre-resolved spec; Fn = invoke
        // the user fn now with (url, ctx) and resolve its return through
        // the same machinery as a Target::Fn rule would. Suppress =
        // explicit `defaultBrowser: null`, mirrors `open: null` for rules.
        match &self.default_browser {
            DefaultBrowser::Static(b) => Resolution {
                browser: Rc::clone(b),
                url: current,
                matched_rule: None,
            },
            DefaultBrowser::Suppress => suppressed(),
            DefaultBrowser::Fn(uf) => 'fn_default: {
                if let Some(args) = rc.fn_args(&current, uf.needs_ctx) {
                    if let Some(r) = unsafe { uf.f.callWithArguments(Some(&args)) } {
                        if !unsafe { r.isUndefined() } && !unsafe { r.isNull() } {
                            let spec =
                                resolve_browser(&r, &self.browsers, false).unwrap_or_else(|| {
                                    Rc::new(BrowserSpec::from_bundle_id(
                                        js_to_string(&r).unwrap_or_default(),
                                    ))
                                });
                            break 'fn_default Resolution {
                                browser: spec,
                                url: current,
                                matched_rule: None,
                            };
                        }
                    }
                }
                // Fn returned null/undefined or args build failed — same
                // semantics as `open: null` (suppress). Resolution<'static>
                // coerces to Resolution<'u> via covariance.
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
    suppressed_at(None)
}

/// Same as [`suppressed`] but records which rule fired the suppression
/// (rule-rewriter Drop or `Target::Suppress`). `None` means no rule was
/// involved — top-level rewriter Drop or `defaultBrowser: null`.
fn suppressed_at(matched_rule: Option<usize>) -> Resolution<'static> {
    Resolution {
        browser: Rc::new(BrowserSpec::empty()),
        url: Cow::Borrowed("about:blank"),
        matched_rule,
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
    /// Pre-built `true` / `false` JSValues borrowed from Engine. Reused for
    /// every modifier flag in `build_ctx_object` so we never pay the
    /// `js_bool` JSC bridge cost on the slow path.
    js_true: &'a Retained<JSValue>,
    js_false: &'a Retained<JSValue>,
    opener: &'a Opener,
    modifiers: ModifierFlags,
    /// Per-resolve cache for `running()` matchers. Holds an `Arc` snapshot
    /// from the process-wide `running_apps_cached`, so subsequent
    /// `running_apps()` calls within one resolve avoid the Mutex roundtrip.
    /// The process-wide cache is kept fresh by NSWorkspace launch/terminate
    /// observers (`install_running_apps_observer`).
    running_cache: RefCell<Option<Arc<HashSet<String>>>>,
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
        js_true: &'a Retained<JSValue>,
        js_false: &'a Retained<JSValue>,
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
            js_true,
            js_false,
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

    fn running_apps(&self) -> Arc<HashSet<String>> {
        if let Some(c) = self.running_cache.borrow().as_ref() {
            return c.clone();
        }
        let fresh = crate::workspace::running_apps_cached();
        *self.running_cache.borrow_mut() = Some(fresh.clone());
        fresh
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
            self.js_true,
            self.js_false,
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
    ///
    /// Returns None when JSC can't allocate even a fallback stub — callers
    /// propagate None up to the resolve path, which skips the affected fn
    /// matcher rather than panicking the daemon.
    fn url_instance(&self, url: &str) -> Option<Retained<JSValue>> {
        if let Some((cached_url, instance)) = self.cached_url_instance.borrow().as_ref() {
            if cached_url.as_ref() == url {
                return Some(instance.clone());
            }
        }
        let v = build_url_instance(self.url_ctor, self.ctx, url)?;
        *self.cached_url_instance.borrow_mut() = Some((Box::from(url), v.clone()));
        Some(v)
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
            let url_instance = self.url_instance(url)?;
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
            let url_instance = self.url_instance(url)?;
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
        // `fn` and `function` carry the same value — Finicky uses both
        // names (with `function` as the v3-back-compat alias).
        format!(
            r#"{{"shift":{},"option":{},"command":{},"control":{},"capsLock":{},"fn":{},"function":{}}}"#,
            m.shift, m.option, m.command, m.control, m.caps_lock, m.function, m.function,
        )
    });

    install_one_arg_string(ctx, "__grinchIsAppRunning", |id| {
        // Mirrors Finicky: match against either bundle ID or localized
        // name (so `finicky.isAppRunning("Slack")` works in addition to
        // `finicky.isAppRunning("com.tinyspeck.slackmacgap")`).
        if crate::workspace::is_app_running(id) {
            "1".to_string()
        } else {
            "0".to_string()
        }
    });

    install_zero_arg_string(ctx, "__grinchGetSystemInfo", || {
        // [NSHost currentHost] gives the same two values Finicky exposes:
        //   - localizedName follows the user-set "Computer Name" (e.g.
        //     "James's MacBook Pro")
        //   - name is the canonical hostname (e.g. "jamtur01-mbp")
        // On a fresh Mac install both are the same; routing on either
        // is meaningful.
        let (localized, name) = crate::workspace::host_info();
        serde_json::json!({ "localizedName": localized, "name": name }).to_string()
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
///
/// Returns `None` only when JSC is in an unrecoverable state (every
/// evaluateScript call fails, even on a 2-byte literal). Callers up the
/// chain (fn_args → resolve) treat None as "fn matcher doesn't match"
/// rather than panicking the daemon. Pre-fix, the bottom of this function
/// `.expect()`'d the final evaluateScript and would panic the whole
/// process on a per-resolve JSC OOM.
fn build_url_instance(url_ctor: &JSValue, ctx: &JSContext, url: &str) -> Option<Retained<JSValue>> {
    if let Some(url_str) = js_string(ctx, url) {
        let url_str_obj: Retained<AnyObject> = unsafe { Retained::cast_unchecked(url_str) };
        let args = NSArray::from_retained_slice(&[url_str_obj]);
        if let Some(instance) = unsafe { url_ctor.constructWithArguments(Some(&args)) } {
            if !unsafe { instance.isUndefined() } && !unsafe { instance.isNull() } {
                return Some(instance);
            }
        }
    }
    // js_string failed (OOM) or `new URL(...)` returned undefined/null —
    // fall through to the stub-object path so user code can still
    // destructure { href } without crashing the resolve.
    let url_json = serde_json::to_string(url).unwrap_or_else(|_| "\"\"".to_string());
    let stub_src = format!(
        "({{ href: {url_json}, protocol: '', hostname: '', pathname: '', search: '', hash: '' }})"
    );
    let stub_ns = NSString::from_str(&stub_src);
    if let Some(v) = unsafe { ctx.evaluateScript(Some(&stub_ns)) } {
        return Some(v);
    }
    // Last-ditch: a literal empty object. If even this fails, JSC is
    // unable to evaluate anything — propagate None so the resolve path
    // skips this fn matcher without panicking.
    unsafe { ctx.evaluateScript(Some(&NSString::from_str("({})"))) }
}

#[allow(clippy::too_many_arguments)]
fn build_ctx_object(
    ctx: &JSContext,
    helper: &JSValue,
    opener_str_cache: &RefCell<std::collections::HashMap<String, Retained<JSValue>>>,
    js_true: &Retained<JSValue>,
    js_false: &Retained<JSValue>,
    url: &str,
    opener: &Opener,
    m: ModifierFlags,
) -> Option<Retained<JSValue>> {
    // URL changes per resolve (or per rewrite); not worth caching across
    // resolves. Opener fields stabilise (same Mail / Slack / Outlook over
    // and over) → engine's opener_str_cache. Modifier flags are bools and
    // we hold a single cached Retained<JSValue> per truth value on the
    // Engine — clones here are refcount bumps, not JSC bridge crossings.
    let bool_v = |b: bool| -> Retained<JSValue> {
        if b {
            js_true.clone()
        } else {
            js_false.clone()
        }
    };
    // js_string / cached_js_string return None on JSC OOM. Propagate
    // via `?` to the function's Option return; the caller treats that
    // as "fn matcher won't match" and continues with the next rule.
    let url_v = js_string(ctx, url)?;
    let opener_id_v = cached_js_string(ctx, opener_str_cache, &opener.bundle_id)?;
    let opener_name_v = cached_js_string(ctx, opener_str_cache, &opener.name)?;
    let opener_path_v = cached_js_string(ctx, opener_str_cache, &opener.path)?;
    // Fixed-size array (was a heap-allocated Vec<Retained<AnyObject>>).
    // NSArray::from_retained_slice takes a `&[Retained<T>]` so the array
    // coerces cleanly; no allocation between us and JSC's NSArray copy.
    let args_objs: [Retained<AnyObject>; 10] = [
        unsafe { Retained::cast_unchecked(url_v) },
        unsafe { Retained::cast_unchecked(opener_id_v) },
        unsafe { Retained::cast_unchecked(opener_name_v) },
        unsafe { Retained::cast_unchecked(opener_path_v) },
        unsafe { Retained::cast_unchecked(bool_v(m.shift)) },
        unsafe { Retained::cast_unchecked(bool_v(m.option)) },
        unsafe { Retained::cast_unchecked(bool_v(m.command)) },
        unsafe { Retained::cast_unchecked(bool_v(m.control)) },
        unsafe { Retained::cast_unchecked(bool_v(m.caps_lock)) },
        unsafe { Retained::cast_unchecked(bool_v(m.function)) },
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
    // Empty pattern would otherwise match every host with a trailing dot
    // (`"x."` ends_with `""` is true, and `hb.len() > 0 + 1` for any
    // 2+-char host). A user passing `domain("")` — or whose JS computed
    // an empty hostname before reaching the matcher — shouldn't get a
    // global wildcard out of it.
    if pattern.is_empty() {
        return false;
    }
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
            // Fast paths: most fn rewriters return either a literal string,
            // null/undefined, or a URL polyfill instance (mutated). Handling
            // those four in Rust skips the __grinchRewriteResult bridge
            // crossing — measured at ~400–600 ns per rewrite on the slow
            // path. Only LegacyURLObject (`{protocol, host, …}`) returns
            // fall through to the helper, which keeps a single canonical
            // implementation of the field-concatenation rules.
            match js_value_type(rc.ctx, &raw) {
                JSType::Null => return RewriteOutcome::Drop,
                JSType::Undefined => return RewriteOutcome::Unchanged,
                JSType::String => {
                    let Some(s) = js_to_string(&raw) else {
                        return RewriteOutcome::Unchanged;
                    };
                    return if s == url {
                        RewriteOutcome::Unchanged
                    } else {
                        RewriteOutcome::Changed(s)
                    };
                }
                JSType::Object => {
                    // URL instance OR anything else whose `.href` is a
                    // non-empty string — same fast path the helper takes.
                    if let Some(s) = read_nonempty_string_property(rc.ctx, &raw, "href") {
                        return if s == url {
                            RewriteOutcome::Unchanged
                        } else {
                            RewriteOutcome::Changed(s)
                        };
                    }
                    // Fall through to the helper for LegacyURLObject.
                }
                _ => {
                    // Numbers, booleans, symbols, bigints — Finicky-v4
                    // doesn't define semantics for these, but the JS
                    // helper coerces them. Defer to it for parity.
                }
            }
            let raw_obj: Retained<AnyObject> = unsafe { Retained::cast_unchecked(raw) };
            let helper_args = NSArray::from_retained_slice(&[raw_obj]);
            let Some(normalised) = (unsafe {
                rc.rewrite_result_helper
                    .callWithArguments(Some(&helper_args))
            }) else {
                return RewriteOutcome::Unchanged;
            };
            // Helper post-checks: it can still return null (drop),
            // undefined (passthrough), or a string (the rebuilt href).
            match js_value_type(rc.ctx, &normalised) {
                JSType::Null => RewriteOutcome::Drop,
                JSType::Undefined => RewriteOutcome::Unchanged,
                _ => match js_to_string(&normalised) {
                    Some(s) if s != url => RewriteOutcome::Changed(s),
                    _ => RewriteOutcome::Unchanged,
                },
            }
        }
        Rewriter::TeamsLauncher => match unwrap_teams_launcher(url) {
            Some(new_url) => RewriteOutcome::Changed(new_url),
            None => RewriteOutcome::Unchanged,
        },
        Rewriter::Safelinks => match unwrap_safelink(url) {
            Some(new_url) => RewriteOutcome::Changed(new_url),
            None => RewriteOutcome::Unchanged,
        },
    }
}

/// Unwrap a corporate "SafeLinks"-style URL wrapper to its real
/// destination. Recognises three of the most common shapes:
///
/// - Microsoft 365 Defender SafeLinks
///   (`*.safelinks.protection.outlook.com/?url=<encoded>&data=…`)
/// - Microsoft Teams external-link interstitial
///   (`statics.teams.cdn.office.net/evergreen-assets/safelinks/?url=…`)
/// - Proofpoint URL Defense v2
///   (`urldefense.proofpoint.com/v2/url?u=<encoded>&…`)
///
/// Returns `Some(unwrapped)` only when the host matches a recognised
/// wrapper AND the inner URL extracts + percent-decodes cleanly. Anything
/// else (unknown host, missing param, malformed encoding) returns `None`
/// so the rewriter passes the URL through untouched.
///
/// Idempotent: re-runs up to two unwrap passes so a double-wrapped link
/// (Defender forwarding to Proofpoint, etc.) lands at the real target.
fn unwrap_safelink(url: &str) -> Option<String> {
    let mut current = url.to_string();
    let mut changed = false;
    for _ in 0..2 {
        let Some(next) = unwrap_safelink_once(&current) else {
            break;
        };
        current = next;
        changed = true;
    }
    changed.then_some(current)
}

/// Unwrap a Microsoft Teams launcher URL into the native `msteams:` scheme.
///
/// Calendar invites and corporate share links commonly use the launcher
/// form (`https://teams.microsoft.com/dl/launcher/launcher.html?url=…`)
/// because it works on machines that don't have Teams installed (it opens
/// the web client). Users with Teams installed almost always want the
/// native client, which speaks the `msteams:` scheme — but you can't get
/// there directly from a calendar invite link without rewriting.
///
/// Returns the rebuilt `msteams:<path>` form on a recognised launcher
/// URL, or None for any other host/path so the caller treats it as a
/// pass-through.
fn unwrap_teams_launcher(url: &str) -> Option<String> {
    let host = quick_host(url)?;
    if host.as_ref() != "teams.microsoft.com" {
        return None;
    }
    let query_start = url.find('?')?;
    let scheme_end = url.find("://").map(|i| i + 3).unwrap_or(0);
    let path_start = url[scheme_end..query_start]
        .find('/')
        .map(|rel| scheme_end + rel)
        .unwrap_or(query_start);
    let path = &url[path_start..query_start];
    if !path.starts_with("/dl/launcher/launcher.html") {
        return None;
    }
    let query = &url[query_start + 1..];
    let query = query.split('#').next().unwrap_or(query);
    let encoded = find_query_param(query, "url")?;
    let decoded = percent_decode(encoded)?;
    if decoded.is_empty() {
        return None;
    }
    // The decoded value is a relative path starting with the Teams web
    // app's routing prefix `/_#/…` (e.g. `/_#/l/meetup-join/19:…`).
    // Strip the `/_#` so the result is `/l/…`, the canonical `msteams:`
    // path. If the prefix isn't present (older launcher format), use
    // the decoded path as-is.
    let inner = decoded.strip_prefix("/_#").unwrap_or(&decoded);
    Some(format!("msteams:{inner}"))
}

fn unwrap_safelink_once(url: &str) -> Option<String> {
    let host = quick_host(url)?;
    let query_start = url.find('?')?;
    // Path = everything between the authority and the `?`. `quick_host`
    // strips userinfo (`user@…`) and port (`:443`) from the host, so
    // `scheme_end + host.len()` would land mid-authority on URLs that
    // carry either — yielding a `path` slice like `":443/v2/url"`
    // instead of `"/v2/url"` and silently failing the Teams / Proofpoint
    // path-prefix checks. Locate the path by scanning forward from the
    // scheme for the first `/` that isn't part of `//`.
    let scheme_end = url.find("://").map(|i| i + 3).unwrap_or(0);
    let path_start = url[scheme_end..query_start]
        .find('/')
        .map(|rel| scheme_end + rel)
        .unwrap_or(query_start);
    let path = &url[path_start..query_start];
    // Drop any URL fragment from the query — SafeLinks wrappers don't use
    // fragments for the inner URL, but a stray `#` later in the query
    // shouldn't pollute the param search.
    let query = &url[query_start + 1..];
    let query = query.split('#').next().unwrap_or(query);

    let is_microsoft_safelinks = host.ends_with(".safelinks.protection.outlook.com")
        || host.as_ref() == "safelinks.protection.outlook.com";
    let is_teams_safelink = host.as_ref() == "statics.teams.cdn.office.net"
        && path.starts_with("/evergreen-assets/safelinks/");
    let is_proofpoint_v2 =
        host.as_ref() == "urldefense.proofpoint.com" && path.starts_with("/v2/url");

    let param = if is_microsoft_safelinks || is_teams_safelink {
        "url"
    } else if is_proofpoint_v2 {
        "u"
    } else {
        return None;
    };

    let encoded = find_query_param(query, param)?;
    let decoded = percent_decode(encoded)?;
    if decoded.is_empty() || !looks_like_url(&decoded) {
        return None;
    }
    Some(decoded)
}

fn find_query_param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    for kv in query.split('&') {
        // `continue` on valueless params (bare keys, e.g. `?secure&url=…`).
        // The prior implementation used `?` here, which would short-circuit
        // the entire scan the first time the query contained a key without
        // `=` — silently breaking SafeLinks unwrapping for URLs that mix
        // a flag-style param with the wrapped URL param.
        let Some((k, v)) = kv.split_once('=') else {
            continue;
        };
        if k == name {
            return Some(v);
        }
    }
    None
}

/// Percent-decode a query-string value. Returns None when the input contains
/// a malformed `%XX` escape or the decoded bytes aren't valid UTF-8.
/// Treats `+` as a literal `+` (not space) — SafeLinks wrappers use proper
/// percent-encoding throughout, and form-encoding `+→ ` translation would
/// corrupt encoded URLs that legitimately contain `+`.
fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hi = (bytes[i + 1] as char).to_digit(16)?;
            let lo = (bytes[i + 2] as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Cheap sanity check that the decoded string looks like a URL — at least a
/// scheme followed by `://`. Defends against wrappers whose `url` param
/// happens to carry something else (a tracking token, an email address)
/// from being routed as a URL.
fn looks_like_url(s: &str) -> bool {
    let Some(scheme_end) = s.find("://") else {
        return false;
    };
    let scheme = &s[..scheme_end];
    !scheme.is_empty()
        && scheme
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'-' || b == b'.')
}

// MARK: - Compilation

/// Parse a JS browser spec (string | object). Resolves app names to bundle
/// IDs; expands the `profile` shorthand for Chromium-family browsers.
/// Translate a (bundle_id, profile-name) pair into the launch args the
/// browser actually understands:
///
///   - Chromium family → `["--profile-directory=<dir>"]`, where `<dir>`
///     is the on-disk directory key. The user can supply either the
///     directory ("Profile 10") or the display name ("Work"); we resolve
///     through Local State.
///   - Firefox family  → `["-P", "<name>"]`. Firefox's profile name is
///     end-to-end the same string the user wrote; we just validate it's
///     known so an unrecognised name doesn't silently open the profile-
///     manager UI.
///   - Anything else   → `None` (caller logs a warning).
///
/// Returns `Some(args)` on a recognised family, `None` otherwise. The
/// caller is responsible for setting `creates_new_instance: true` when
/// using the returned args — without that, an already-running browser
/// instance would route the URL into its current window and ignore the
/// profile flag.
fn expand_profile_args(bundle_id: &str, profile: &str) -> Option<Vec<String>> {
    if profile.is_empty() {
        return None;
    }
    if crate::chromium::is_chromium(bundle_id) {
        let dir = crate::chromium::resolve_profile_dir(bundle_id, profile);
        return Some(vec![format!("--profile-directory={dir}")]);
    }
    if crate::firefox::is_firefox(bundle_id) {
        let name = crate::firefox::resolve_profile_name(bundle_id, profile);
        return Some(vec!["-P".to_string(), name]);
    }
    None
}

/// Heuristic: does this string look like an `.app` bundle path that
/// should resolve via `NSBundle.bundleWithURL` instead of going through
/// the LaunchServices display-name lookup? Mirrors Finicky's
/// `autodetectAppStringType` regex (`^(~?(?:\/[^/\n]+)+\/[^/\n]+\.app)$`)
/// with a cheaper byte-level check.
fn looks_like_app_path(s: &str) -> bool {
    s.ends_with(".app") && s.contains('/')
}

/// Expand a leading `~/` to `$HOME/`. No-op for any other input. Used
/// only for path-form browser specs; the Chromium / Firefox profile
/// path code already calls `std::env::var("HOME")` directly.
fn expand_tilde(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    s.to_string()
}

fn parse_browser_jsval(v: &JSValue) -> BrowserSpec {
    if unsafe { v.isString() } {
        let s = js_to_string(v).unwrap_or_default();
        // Path autodetect: bare-string browser specs that look like
        // `.app` paths skip the LaunchServices display-name lookup and
        // resolve via NSBundle directly. Matches Finicky's
        // autodetectAppStringType — anyone writing
        // `default: "/Applications/Arc.app"` (rather than the explicit
        // `{ name: "...", appType: "path" }` form) gets the right
        // behaviour. Checked before the Name:Profile shorthand because
        // a path can't reasonably carry a profile suffix.
        if looks_like_app_path(&s) {
            let bundle_id = crate::workspace::resolve_browser_path(&expand_tilde(&s));
            return BrowserSpec::from_bundle_id(bundle_id);
        }
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
                if let Some(args) = expand_profile_args(&bundle_id, profile) {
                    return BrowserSpec {
                        bundle_id,
                        args,
                        open_in_background: false,
                        creates_new_instance: true,
                    };
                }
                if !profile.is_empty() {
                    eprintln!(
                        "grinch: ignoring `:profile` shorthand for unrecognised browser \
                         family {bundle_id} (input was {s:?}; supported: Chromium, Firefox)"
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

    // `profile` field: expand to launch args appropriate for the browser
    // family — `--profile-directory=<dir>` for Chromium, `-P <name>` for
    // Firefox. Forces `creates_new_instance` so an already-running
    // browser doesn't route the URL into its current window and ignore
    // the profile flag.
    if let Some(profile) = key(v, "profile").and_then(|p| js_to_string(&p)) {
        if let Some(profile_args) = expand_profile_args(&bundle_id, &profile) {
            args.extend(profile_args);
            creates_new_instance = true;
        } else if !profile.is_empty() {
            eprintln!(
                "grinch: ignoring `profile` for unrecognised browser family \
                 {bundle_id} (profile = {profile}; supported: Chromium, Firefox)"
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

/// Outcome of parsing the user config's `options` block. Only fields
/// Grinch actually acts on appear here — the others (urlShorteners,
/// logRequests, checkForUpdates, keepRunning) are still accepted at
/// parse time but discarded (see `parse_options_block`).
#[derive(Default, Debug, Clone, Copy)]
pub struct OptionsConfig {
    /// Whether the menu-bar status item should be skipped at app launch.
    /// Read once by AppDelegate during `setup_menu_bar`; reloads won't
    /// hide or re-show the icon mid-session (consistent with most macOS
    /// background apps that surface this kind of toggle).
    pub hide_icon: bool,
    /// Whether to write a per-resolve JSONL log to
    /// `~/Library/Logs/Grinch/Grinch_<timestamp>.log`. Each line is one
    /// resolve: input URL, final URL after rewrites, target browser,
    /// opener bundle ID, and a Unix timestamp. Mirrors Finicky's
    /// `options.logRequests` semantics.
    pub log_requests: bool,
    /// Rotate the request log when it grows past this many bytes.
    /// `None` (the default) disables size-based rotation. Rotation
    /// renames the current file to `<path>.<iso-timestamp>` and starts
    /// a fresh empty file, so older entries are preserved on disk for
    /// post-mortem until the user prunes them.
    pub log_rotate_bytes: Option<u64>,
    /// Rotate the request log when it has been written to for this many
    /// days (since the file was opened or most-recently rotated).
    /// `None` disables time-based rotation. Combine with
    /// `log_rotate_bytes` to get "rotate on either trigger".
    pub log_rotate_days: Option<u32>,
}

/// Per-resolve JSONL log writer used when `options.logRequests` is on.
/// Opens the destination file lazily on first `write` so an engine that
/// never resolves doesn't create an empty log file. After a write
/// failure the writer marks itself failed and stops trying — better
/// than spamming stderr per resolve.
///
/// Rotation: when either `rotate_bytes` or `rotate_days` is set and the
/// corresponding threshold is exceeded, the current file is renamed to
/// `<path>.<iso-timestamp>` and a fresh file is opened on the next write.
/// `bytes_written` is tracked in-process (initialised from the existing
/// file's size on open) so rotation decisions don't stat() per write.
struct LogWriter {
    path: std::path::PathBuf,
    file: Option<std::fs::File>,
    failed: bool,
    rotate_bytes: Option<u64>,
    rotate_days: Option<u32>,
    bytes_written: u64,
    opened_at_unix: u64,
}

impl LogWriter {
    fn new(path: std::path::PathBuf, rotate_bytes: Option<u64>, rotate_days: Option<u32>) -> Self {
        Self {
            path,
            file: None,
            failed: false,
            rotate_bytes,
            rotate_days,
            bytes_written: 0,
            opened_at_unix: 0,
        }
    }

    fn write(&mut self, line: &str) {
        use std::io::Write;
        if self.failed {
            return;
        }
        // newline-terminated; writeln! appends one
        let about_to_write = line.len() as u64 + 1;
        if self.should_rotate(about_to_write, now_unix()) {
            self.rotate();
        }
        if self.file.is_none() {
            match Self::open(&self.path) {
                Ok((f, size)) => {
                    self.file = Some(f);
                    self.bytes_written = size;
                    self.opened_at_unix = now_unix();
                }
                Err(e) => {
                    eprintln!(
                        "grinch: couldn't open log file {}: {e} — disabling \
                         options.logRequests for this session",
                        self.path.display()
                    );
                    self.failed = true;
                    return;
                }
            }
        }
        if let Some(f) = self.file.as_mut() {
            if let Err(e) = writeln!(f, "{line}") {
                eprintln!(
                    "grinch: write to {} failed: {e} — disabling \
                     options.logRequests for this session",
                    self.path.display()
                );
                self.failed = true;
                self.file = None;
            } else {
                self.bytes_written += about_to_write;
            }
        }
    }

    /// True when writing `extra_bytes` more would push the file past
    /// `rotate_bytes`, OR `now` is past `rotate_days` since the file
    /// was opened. Pure function so it's testable without a real fs.
    fn should_rotate(&self, extra_bytes: u64, now: u64) -> bool {
        if self.file.is_none() {
            return false;
        }
        if let Some(cap) = self.rotate_bytes {
            if self.bytes_written.saturating_add(extra_bytes) > cap {
                return true;
            }
        }
        if let Some(days) = self.rotate_days {
            let secs = u64::from(days).saturating_mul(86_400);
            if now.saturating_sub(self.opened_at_unix) >= secs {
                return true;
            }
        }
        false
    }

    fn rotate(&mut self) {
        // Drop the file handle so the rename can complete on platforms
        // that hold it locked (not macOS, but cheap to do everywhere).
        self.file = None;
        let stamp = iso_timestamp_for_filename();
        let rotated = self.path.with_extension(format!("log.{stamp}"));
        if let Err(e) = std::fs::rename(&self.path, &rotated) {
            // Rename can fail under very-unusual conditions (the source
            // disappeared because someone deleted it externally, or
            // permissions changed). Log once and carry on — the next
            // write will lazily re-open the path; in the worst case we
            // keep appending to a file that has grown past the cap,
            // which is still better than dropping log lines.
            eprintln!(
                "grinch: log rotation rename {} → {} failed: {e}",
                self.path.display(),
                rotated.display()
            );
        }
        self.bytes_written = 0;
        self.opened_at_unix = now_unix();
    }

    fn open(path: &std::path::Path) -> std::io::Result<(std::fs::File, u64)> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let size = f.metadata().map(|m| m.len()).unwrap_or(0);
        Ok((f, size))
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One JSONL entry per resolve. Schema:
///
/// - `ts`: unix seconds with millisecond fractional precision
/// - `url` / `final`: input URL and post-rewrite URL (equal when no
///   rewriter fired)
/// - `rewritten`: bool — true iff `url != final`. Pre-computed so log
///   consumers don't have to string-compare.
/// - `browser` / `args`: target bundle id and launch args. Empty
///   `browser` (== suppressed) is emitted as-is so callers can
///   distinguish a hit from "open: null".
/// - `opener`: `{bundleId, name, pid}` of the app that sent the URL.
///   Bundle id is empty when neither the sender PID nor the frontmost
///   snapshot identified one (rare).
/// - `modifiers`: `{shift, option, command, control}` at resolve time —
///   the four keys Grinch's rules actually expose to JS.
/// - `matchedRule`: `{index, name}` for the rule whose matcher fired, where
///   `name` is the user-supplied `name:` field when present, otherwise an
///   auto-derived label (string pattern, `domain:foo,bar`, or first line of
///   the fn source for fn matchers). `null` when the URL fell through to
///   the default browser.
fn format_log_entry(
    input_url: &str,
    opener: &Opener,
    modifiers: ModifierFlags,
    res: &Resolution<'_>,
    matched: Option<(usize, &str)>,
) -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let final_url = res.url.as_ref();
    let matched_json = matched.map(|(idx, name)| serde_json::json!({"index": idx, "name": name}));
    let entry = serde_json::json!({
        "ts": ts,
        "url": input_url,
        "final": final_url,
        "rewritten": final_url != input_url,
        "browser": res.browser.bundle_id,
        "args": res.browser.args,
        "opener": {
            "bundleId": opener.bundle_id,
            "name": opener.name,
            "pid": opener.pid,
        },
        "modifiers": {
            "shift": modifiers.shift,
            "option": modifiers.option,
            "command": modifiers.command,
            "control": modifiers.control,
        },
        "matchedRule": matched_json,
    });
    entry.to_string()
}

/// Build a per-launch log path under `~/Library/Logs/Grinch/`. Falls back
/// to `/tmp/Grinch_<ts>.log` if `$HOME` isn't set (rare on macOS but
/// possible under sandboxed test runners). Filename uses an ISO-style
/// timestamp with colons replaced by dashes for filesystem safety.
fn log_file_path() -> std::path::PathBuf {
    let stem = format!("Grinch_{}.log", iso_timestamp_for_filename());
    let base = std::env::var("HOME")
        .map(|h| std::path::PathBuf::from(h).join("Library/Logs/Grinch"))
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp"));
    base.join(stem)
}

/// Format the current local time as `YYYY-MM-DDTHH-MM-SS` for use in
/// log filenames. Avoids colons (which some macOS Finder pickers
/// remap) and keeps things human-readable.
fn iso_timestamp_for_filename() -> String {
    use std::ffi::CStr;
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as libc::time_t)
        .unwrap_or(0);
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::localtime_r(&secs, &mut tm);
    }
    let mut buf = [0i8; 64];
    let n = unsafe {
        libc::strftime(
            buf.as_mut_ptr(),
            buf.len(),
            c"%Y-%m-%dT%H-%M-%S".as_ptr(),
            &tm,
        )
    };
    if n == 0 {
        return secs.to_string();
    }
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

/// Parse Finicky v4's `options` block. The five known keys are accepted
/// without error so a copied-over Finicky config doesn't break:
///
/// | Key | Grinch behaviour |
/// |---|---|
/// | `urlShorteners` | silently ignored — Finicky's hard-coded list isn't user-configurable there either; Grinch expects external expansion (see `examples/expand-shortener.sh`) |
/// | `logRequests`   | **honoured** — writes per-resolve JSONL to `~/Library/Logs/Grinch/Grinch_<timestamp>.log` |
/// | `checkForUpdates` | silently ignored — Grinch doesn't poll for updates |
/// | `keepRunning`   | silently ignored — Grinch is always resident |
/// | `hideIcon`      | **honoured** — propagated through `OptionsConfig` to AppDelegate, which skips menu-bar status item creation when set |
///
/// Unknown keys log a one-line warning so users can spot typos.
fn parse_options_block(opts: &JSValue) -> OptionsConfig {
    const KNOWN: &[&str] = &[
        "urlShorteners",
        "logRequests",
        "logRotateBytes",
        "logRotateDays",
        "checkForUpdates",
        "keepRunning",
        "hideIcon",
    ];
    let mut out = OptionsConfig::default();
    for (k, v) in iter_object(opts) {
        match k.as_str() {
            "hideIcon" => {
                out.hide_icon = unsafe { v.toBool() };
            }
            "logRequests" => {
                out.log_requests = unsafe { v.toBool() };
            }
            "logRotateBytes" => {
                // JS numbers are doubles; coerce to u64 with bounds-check
                // so a negative/NaN/infinity value disables rotation
                // rather than silently producing a giant cap.
                let n = unsafe { v.toDouble() };
                if n.is_finite() && n > 0.0 && n <= u64::MAX as f64 {
                    out.log_rotate_bytes = Some(n as u64);
                }
            }
            "logRotateDays" => {
                let n = unsafe { v.toDouble() };
                if n.is_finite() && n > 0.0 && n <= u32::MAX as f64 {
                    out.log_rotate_days = Some(n as u32);
                }
            }
            other if !KNOWN.contains(&other) => {
                eprintln!(
                    "grinch: unknown options.{other} — accepted keys are {}",
                    KNOWN.join(", ")
                );
            }
            // Known but inert keys: accept silently.
            _ => {}
        }
    }
    out
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
        let name = key(&item, "name")
            .and_then(|v| js_to_string(&v))
            .filter(|s| !s.is_empty());
        let label = derive_match_label(match_val.as_deref());
        out.push(Rule {
            matchers,
            rewriter,
            target,
            name,
            label,
        });
    }
    out
}

/// Build a human-readable label for a rule's `match:` value at parse time.
/// String / array matchers turn into themselves; `domain()`/`from()`/`running()`
/// objects render as `kind:items`; fn matchers fall back to the first line of
/// their source. Returns `"*"` for `match: () => true` shorthand (no match key).
fn derive_match_label(v: Option<&JSValue>) -> String {
    const MAX: usize = 80;
    let Some(v) = v else { return "*".to_string() };
    if is_undef_or_null(v) {
        return "*".to_string();
    }
    if unsafe { v.isString() } {
        return js_to_string(v).unwrap_or_default();
    }
    if unsafe { v.isArray() } {
        let count = js_array_len(v);
        let parts: Vec<String> = (0..count)
            .filter_map(|i| js_array_at(v, i))
            .map(|item| describe_single_matcher(&item))
            .collect();
        return truncate_label(&parts.join(" | "), MAX);
    }
    truncate_label(&describe_single_matcher(v), MAX)
}

/// Single-matcher description. Recognises the `domain()/from()/running()`
/// helper shape (objects with a `__type` tag set by the prelude) and falls
/// back to `f.toString()` for plain functions.
fn describe_single_matcher(v: &JSValue) -> String {
    if unsafe { v.isString() } {
        return js_to_string(v).unwrap_or_default();
    }
    if unsafe { v.isObject() } {
        if let Some(t) = key(v, "__type").and_then(|t| js_to_string(&t)) {
            let items_key = match t.as_str() {
                "domain" => "hosts",
                "from" | "running" => "apps",
                _ => "",
            };
            if !items_key.is_empty() {
                if let Some(arr) = key(v, items_key) {
                    let items = js_array_to_strings(&arr).join(",");
                    return format!("{t}:{items}");
                }
            }
            return t;
        }
        // Plain JS function: toString() returns the source. Collapse to a
        // single line so the label renders cleanly in JSONL / --list-rules.
        if let Some(src) = js_to_string(v) {
            let one_line = src.split('\n').map(str::trim).collect::<Vec<_>>().join(" ");
            return format!("fn: {one_line}");
        }
    }
    "?".to_string()
}

/// Short, human-readable rendering of a rule's target — used by
/// `rule_listing()` for `--list-rules` output.
fn describe_target(t: &Target) -> String {
    match t {
        Target::Browser(b) if b.bundle_id.is_empty() => "(suppress)".to_string(),
        Target::Browser(b) => {
            if b.args.is_empty() {
                b.bundle_id.clone()
            } else {
                format!("{} {}", b.bundle_id, b.args.join(" "))
            }
        }
        Target::Fn(_) => "fn".to_string(),
        Target::Suppress => "(suppress)".to_string(),
    }
}

fn truncate_label(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let prefix: String = s.chars().take(max_chars).collect();
    format!("{prefix}…")
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

        // Bare safelinks() marker — also "always run". The rewriter itself
        // no-ops on hosts it doesn't recognise, so leaving the matcher as
        // Always is correct.
        if is_marker(&item, "safelinks") {
            out.push(RewriteRule {
                matchers: vec![Matcher::Always],
                rewriter: Rewriter::Safelinks,
            });
            continue;
        }

        // Bare teams_launcher() marker — same shape as safelinks(): the
        // rewriter no-ops on hosts/paths it doesn't recognise, so an
        // Always matcher is correct.
        if is_marker(&item, "teams_launcher") {
            out.push(RewriteRule {
                matchers: vec![Matcher::Always],
                rewriter: Rewriter::TeamsLauncher,
            });
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
                match RegexBuilder::new(&pattern)
                    .case_insensitive(ignore_case)
                    .multi_line(multi_line)
                    .build()
                {
                    Ok(re) => return Some(Matcher::Regex(re)),
                    Err(e) => {
                        // The Rust `regex` crate doesn't speak JS-specific
                        // regex syntax (lookbehinds, `\b` in some contexts).
                        // Silently dropping the matcher meant rules whose
                        // only pattern was a regex would never fire with
                        // no diagnostic. Surface the failure at load time
                        // so users can port the pattern to a supported
                        // form (e.g. wildcards, fn matchers).
                        eprintln!(
                            "grinch: rule matcher regex /{pattern}/ failed to compile: \
                             {e}. The rule will never match — replace with a wildcard, \
                             a `domain()` helper, or a `(url, ctx) => …` fn matcher."
                        );
                    }
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
    // Private-use codepoint as the "this `*` was escaped" sentinel.
    // Previously U+0000 (NUL), which is a valid char in JS strings — a
    // pattern containing a literal NUL would have been misinterpreted as
    // a `\*`. Unicode private-use characters (U+E000..U+F8FF) are
    // guaranteed not to appear in real-world host patterns.
    const PLACEHOLDER: char = '\u{E000}';

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

    // Case-sensitive by default — Finicky's `matchWildcard` produces a
    // bare JS RegExp with no `/i` flag and matches via `RegExp.test`,
    // which is also case-sensitive by default. Earlier Grinch versions
    // forced case_insensitive(true) here, which silently diverged on any
    // mixed-case URL (e.g. `match: "GitHub.com/*"` matched
    // `https://github.com/path` in Grinch but not in Finicky).
    RegexBuilder::new(&anchored).build().ok()
}

fn pattern_has_protocol_prefix(pat: &str) -> bool {
    // RFC 3986 scheme: ALPHA *( ALPHA / DIGIT / "+" / "-" / "." ). First
    // char must be ASCII alpha (rejects `1foo:` and `:nocolon-prefix`).
    // Continuation chars allow + - . in addition to alnum, catching
    // `chrome-extension:`, `view-source:`, `git+https:`, `web+foo:` —
    // the previous (alnum-or-underscore-only) version mistakenly
    // classified those as having no protocol prefix and compiled them
    // to an unanchored regex. Underscore is also accepted in
    // continuation for backwards compatibility with configs that used
    // it (RFC doesn't allow it but it was accepted historically).
    let bytes = pat.as_bytes();
    let Some((first, rest)) = bytes.split_first() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    for c in rest {
        if c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.' | b'_') {
            continue;
        }
        return *c == b':';
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
pub(crate) fn quick_host(url: &str) -> Option<Cow<'_, str>> {
    // Opaque-scheme URIs like `mailto:user@example.com`, `tel:+1...`,
    // `about:blank`, `javascript:…` have no authority component — no
    // `//` after the scheme. Trying to derive a hostname out of them
    // produced wrong results: `about:blank` previously yielded `"about"`
    // (rfind(':') sliced off `:blank`), so a `domain("about")` matcher
    // would have unexpectedly matched it. Return None for any input
    // without `://`; callers that want to match by scheme should use
    // a wildcard / regex matcher.
    let scheme_end = url.find("://")?;
    let mut s = &url[scheme_end + 3..];
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
                Some(maybe_lowercase(host))
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
        Some(maybe_lowercase(s))
    }
}

/// Return `s` borrowed when it has no ASCII uppercase bytes, otherwise
/// allocate a lowercased copy. Most URLs in the wild have already-lowercase
/// hostnames, so this skips the `String` allocation on the common path.
fn maybe_lowercase(s: &str) -> Cow<'_, str> {
    if s.bytes().any(|b| b.is_ascii_uppercase()) {
        Cow::Owned(s.to_ascii_lowercase())
    } else {
        Cow::Borrowed(s)
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

    // First pass: scan kv pairs, track total + kept-byte count. We bail
    // before allocating if nothing matches — the common case for URLs
    // with a query but no tracking params. When we do allocate, the
    // exact byte count gives `String::with_capacity` no slack.
    let mut total = 0usize;
    let mut stripped = 0usize;
    let mut kept_bytes = 0usize;
    for kv in qs.split('&') {
        if kv.is_empty() {
            continue;
        }
        total += 1;
        let key = kv.split_once('=').map(|(k, _)| k).unwrap_or(kv);
        if exact.contains(key) || prefixes.iter().any(|p| key.starts_with(p)) {
            stripped += 1;
            continue;
        }
        // +1 for the '&' separator we'll prepend before all but the first
        // kept pair. Tracked here so we don't recompute on the write pass.
        kept_bytes += kv.len() + 1;
    }
    if stripped == 0 {
        return None;
    }
    let kept = total - stripped;

    // `kept_bytes` over-counts by exactly one — it adds a separator for
    // every kept pair, but we only emit N-1 separators. The leading '?'
    // we still need to write (when `kept > 0`) cancels that out, so the
    // total we'll write is `base.len() + kept_bytes + frag.len()` minus
    // one byte when no params survive.
    let cap = base.len() + frag.len() + kept_bytes.saturating_sub((kept == 0) as usize);
    let mut out = String::with_capacity(cap);
    out.push_str(base);
    if kept > 0 {
        out.push('?');
        let mut first = true;
        for kv in qs.split('&') {
            if kv.is_empty() {
                continue;
            }
            let key = kv.split_once('=').map(|(k, _)| k).unwrap_or(kv);
            if exact.contains(key) || prefixes.iter().any(|p| key.starts_with(p)) {
                continue;
            }
            if !first {
                out.push('&');
            }
            out.push_str(kv);
            first = false;
        }
    }
    out.push_str(frag);
    Some(out)
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

/// One-call JSValue type classification via the JSC C API
/// (`JSValueGetType`). Replaces a sequence of `isNull()` + `isUndefined()`
/// Obj-C dispatches with a single C call on the hot path; saves ~50–100 ns
/// per fn return check, which compounds when a config has multiple fn
/// matchers (each unmatched matcher's result goes through this).
#[inline]
fn js_value_type(ctx: &JSContext, v: &JSValue) -> JSType {
    unsafe { JSValue::r#type(ctx.JSGlobalContextRef(), v.JSValueRef()) }
}

/// Identify runs of consecutive rules whose `matchers` is exactly one
/// `Matcher::Fn`, then compile a JS dispatcher for each run of length ≥ 2.
/// Single-fn-matcher runs (length 1) aren't worth batching — the wrapper
/// would add overhead vs the direct call. Mixed-matcher rules (regex +
/// fn, domain() + fn, etc.) also stay on the per-matcher path; the
/// dispatcher only knows how to call fn matchers.
///
/// Returns an empty vec on JSC failures (factory eval, dispatcher call) —
/// the resolve path checks for run coverage by `start` index, so a
/// missing run silently falls through to the per-rule loop.
fn build_fn_matcher_runs(ctx: &JSContext, rules: &[Rule]) -> Vec<FnMatcherRun> {
    // Dispatcher signature: `(url, ctx, startOffset) -> int`. The third
    // arg lets the resolve loop resume scanning mid-run after a
    // Target::Fn returns null/undefined and the engine wants to try the
    // next matcher in the same run without falling back to the
    // per-matcher path (which would skip the batching benefit).
    let factory_src = r#"
        (function() {
            return function() {
                var ms = arguments;
                return function(url, ctx, startOffset) {
                    var start = (startOffset | 0);
                    if (start < 0) start = 0;
                    for (var i = start; i < ms.length; i++) {
                        try {
                            if (ms[i](url, ctx)) return i;
                        } catch (e) {
                            // Matcher threw — treat as no-match, same as the
                            // Rust loop's `result.map(...).unwrap_or(false)`.
                        }
                    }
                    return -1;
                };
            };
        })()
    "#;
    let factory_ns = NSString::from_str(factory_src);
    let Some(factory) = (unsafe { ctx.evaluateScript(Some(&factory_ns)) }) else {
        return Vec::new();
    };
    if unsafe { factory.isUndefined() } || unsafe { factory.isNull() } {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut i = 0;
    while i < rules.len() {
        if !is_fn_only_rule(&rules[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < rules.len() && is_fn_only_rule(&rules[i]) {
            i += 1;
        }
        let end = i;
        if end - start < 2 {
            continue;
        }
        // Collect the matcher fns + their needs_ctx flag.
        let mut needs_ctx = false;
        let mut matcher_objs: Vec<Retained<AnyObject>> = Vec::with_capacity(end - start);
        for r in &rules[start..end] {
            let Matcher::Fn(uf) = &r.matchers[0] else {
                unreachable!("is_fn_only_rule guarantees Matcher::Fn");
            };
            if uf.needs_ctx {
                needs_ctx = true;
            }
            matcher_objs.push(unsafe { Retained::cast_unchecked(uf.f.clone()) });
        }
        let args = NSArray::from_retained_slice(&matcher_objs);
        let Some(dispatcher) = (unsafe { factory.callWithArguments(Some(&args)) }) else {
            continue;
        };
        if unsafe { dispatcher.isUndefined() } || unsafe { dispatcher.isNull() } {
            continue;
        }
        out.push(FnMatcherRun {
            start,
            end,
            dispatcher,
            needs_ctx,
        });
    }
    out
}

fn is_fn_only_rule(rule: &Rule) -> bool {
    rule.matchers.len() == 1 && matches!(rule.matchers[0], Matcher::Fn(_))
}

/// Call a run's dispatcher, scanning from `start_offset` within the run.
/// Returns the 0-based offset of the first matching matcher at or after
/// `start_offset`, or None when no later matcher matches (or the dispatch
/// fails). The `start_offset` parameter lets the resolve loop resume
/// inside a run after a Target::Fn returns null/undefined — without it,
/// fall-through would revert to the per-matcher path and lose the
/// batched-dispatch benefit for the remainder of the run.
fn call_fn_matcher_dispatcher(
    run: &FnMatcherRun,
    rc: &ResolveCtx,
    url: &str,
    start_offset: usize,
) -> Option<usize> {
    let url_instance = rc.url_instance(url)?;
    let ctx_val = if run.needs_ctx {
        rc.ctx_object()?
    } else {
        unsafe { JSValue::valueWithUndefinedInContext(Some(rc.ctx)) }?
    };
    // ctx_val is `Retained<JSValue>`; same shape whether real ctx or undef.
    let start_val =
        unsafe { JSValue::valueWithDouble_inContext(start_offset as f64, Some(rc.ctx)) }?;
    let url_obj: Retained<AnyObject> = unsafe { Retained::cast_unchecked(url_instance) };
    let ctx_obj: Retained<AnyObject> = unsafe { Retained::cast_unchecked(ctx_val) };
    let start_obj: Retained<AnyObject> = unsafe { Retained::cast_unchecked(start_val) };
    let args = NSArray::from_retained_slice(&[url_obj, ctx_obj, start_obj]);
    let result = unsafe { run.dispatcher.callWithArguments(Some(&args)) }?;
    let n = unsafe { result.toInt32() };
    if n < 0 {
        None
    } else {
        Some(n as usize)
    }
}

/// Read a string property from a JSValue object and return it only when
/// the value is *actually* a non-empty string (not `undefined`, not a
/// stringified other type). Used in the fn-rewriter fast path to extract
/// `.href` from URL polyfill instances without crossing into the
/// `__grinchRewriteResult` JS helper. None on missing/wrong-type/empty.
///
/// The JSType filter is load-bearing: `objectForKeyedSubscript` on a
/// missing property returns a JSValue of type `undefined`, which would
/// otherwise `toString()` into the literal "undefined" — and routing
/// "undefined" as a URL is exactly the kind of bug an opaque fast path
/// is prone to.
fn read_nonempty_string_property(ctx: &JSContext, v: &JSValue, key: &str) -> Option<String> {
    let key_ns = NSString::from_str(key);
    let key_ref: &AnyObject = &key_ns;
    let prop = unsafe { v.objectForKeyedSubscript(Some(key_ref)) }?;
    // Property access can trigger a throwing getter — JSC stashes the
    // thrown value on `ctx.exception` and returns a JS-undefined here.
    // The type check below correctly rejects the undefined, but the
    // exception state would persist through any subsequent JSC call
    // in the same resolve (next matcher, next rewriter), producing
    // confusing "matcher mysteriously returned false" symptoms. Clear
    // it so downstream calls see a fresh context.
    if unsafe { ctx.exception() }.is_some() {
        unsafe { ctx.setException(None) };
        return None;
    }
    if js_value_type(ctx, &prop) != JSType::String {
        return None;
    }
    let s = unsafe { prop.toString() }?.to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn js_string(ctx: &JSContext, s: &str) -> Option<Retained<JSValue>> {
    let ns = NSString::from_str(s);
    let any: &AnyObject = &ns;
    // `valueWithObject_inContext` returns Option in the bindings and
    // documented as infallible by Apple, but JSC can return null under
    // hard memory pressure. Returning Option lets the resolve hot path
    // suppress the click cleanly (matcher returns false, ctx build
    // returns None, engine continues) instead of panicking the process.
    unsafe { JSValue::valueWithObject_inContext(Some(any), Some(ctx)) }
}

/// Soft cap on per-cache entry counts. The interning caches in Grinch
/// are bounded in practice by the number of distinct apps that send
/// URLs (≤ a few dozen on any real machine), but a config whose dynamic
/// `open` fn or opener path varies per click could grow them without
/// bound. Stop *inserting* once the map crosses this threshold so the
/// cache size plateaus at a known limit; misses past the threshold pay
/// the lookup cost but the daemon can't be made to OOM via cache growth.
const STRING_CACHE_SOFT_CAP: usize = 1024;

/// Cached `js_string` keyed by the Rust `&str`. Cache hit returns a
/// refcount bump; miss allocates the JSValue and stores it. Used for
/// strings that repeat across resolves (opener fields), not per-call
/// inputs (URL).
fn cached_js_string(
    ctx: &JSContext,
    cache: &RefCell<std::collections::HashMap<String, Retained<JSValue>>>,
    s: &str,
) -> Option<Retained<JSValue>> {
    if let Some(v) = cache.borrow().get(s) {
        return Some(v.clone());
    }
    let v = js_string(ctx, s)?;
    // Insertion-guard: don't grow past the soft cap. Past the cap, hot
    // entries (already in the map) keep returning refcount bumps; cold
    // entries fall through and rebuild every time, which is fine — the
    // realistic ceiling on opener identities is in the dozens.
    let mut cache_mut = cache.borrow_mut();
    if cache_mut.len() < STRING_CACHE_SOFT_CAP {
        cache_mut.insert(s.to_string(), v.clone());
    }
    Some(v)
}

fn js_bool(ctx: &JSContext, b: bool) -> Option<Retained<JSValue>> {
    // Same OOM rationale as js_string. Engine::new propagates failure
    // here as EngineError::PreludeBroken; per-resolve callers can `?`
    // through to the build_ctx_object Option.
    unsafe { JSValue::valueWithBool_inContext(b, Some(ctx)) }
}

unsafe fn eval_global(ctx: &JSContext, name: &str) -> Option<Retained<JSValue>> {
    let key_ns = NSString::from_str(name);
    let key_ref: &AnyObject = &key_ns;
    unsafe { ctx.objectForKeyedSubscript(Some(key_ref)) }
}

/// Like `eval_global` but treats missing / null / undefined values as a
/// `PreludeBroken` error. Used during engine init for the constructors
/// and prelude helpers we need; the call sites would otherwise propagate
/// a null `Retained<JSValue>` into downstream `isInstanceOf` / call
/// operations and produce opaque "null is not an object" stderr per
/// click without ever failing the load.
fn require_global(ctx: &JSContext, name: &'static str) -> Result<Retained<JSValue>, EngineError> {
    let v = unsafe { eval_global(ctx, name) }.ok_or(EngineError::PreludeBroken { global: name })?;
    if unsafe { v.isNull() } || unsafe { v.isUndefined() } {
        return Err(EngineError::PreludeBroken { global: name });
    }
    Ok(v)
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
///
/// Uses `Object.keys(v)` rather than `v.toDictionary()`. The dictionary
/// path recursively converts every value to its NS* equivalent, which
/// stack-overflows on a circular config like `var x = {}; x.self = x;
/// module.exports = { browsers: x };`. `Object.keys` returns only the
/// own enumerable property *names* — no value walk — so circular values
/// are safe; we re-fetch each value via subscript afterwards (one JSC
/// bridge crossing per key, fine because this is engine-init only).
fn iter_object(v: &JSValue) -> Vec<(String, Retained<JSValue>)> {
    if !unsafe { v.isObject() } {
        return vec![];
    }
    let Some(ctx) = (unsafe { v.context() }) else {
        return vec![];
    };
    let Some(object_ctor) = (unsafe { eval_global(&ctx, "Object") }) else {
        return vec![];
    };
    let Some(keys_fn) = key(&object_ctor, "keys") else {
        return vec![];
    };
    let v_clone: Retained<AnyObject> = unsafe { Retained::cast_unchecked(v.retain()) };
    let args = NSArray::from_retained_slice(&[v_clone]);
    let Some(keys_array) = (unsafe { keys_fn.callWithArguments(Some(&args)) }) else {
        return vec![];
    };
    let Some(length_jsv) = key(&keys_array, "length") else {
        return vec![];
    };
    let length = unsafe { length_jsv.toUInt32() } as usize;
    let mut out = Vec::with_capacity(length);
    for i in 0..length {
        let Some(name_jsv) = (unsafe { keys_array.valueAtIndex(i) }) else {
            continue;
        };
        let Some(name_ns) = (unsafe { name_jsv.toString() }) else {
            continue;
        };
        let name = name_ns.to_string();
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

    #[test]
    fn quick_host_returns_none_for_opaque_scheme_uris() {
        // Regression: opaque-scheme URIs (no `//` after the scheme) have
        // no authority component, so there's no hostname to extract.
        // The pre-fix code did `rfind(':')` on the remainder, which
        // produced "mailto" / "about" / "tel" for inputs like the
        // ones below — a `domain("about")` matcher then unexpectedly
        // matched `about:blank` and similar. Should return None across
        // the board so callers fall back to wildcard / regex matching.
        assert_eq!(quick_host("about:blank"), None);
        assert_eq!(quick_host("mailto:user@example.com"), None);
        assert_eq!(quick_host("tel:+15551234567"), None);
        assert_eq!(quick_host("javascript:void(0)"), None);
        assert_eq!(quick_host("slack:channel?team=foo"), None);
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

    #[test]
    fn host_matches_empty_pattern_is_not_a_wildcard() {
        // An empty pattern would otherwise match any 2+-char host with a
        // trailing dot (`"x." ends_with ""` is true). Reject explicitly so
        // a config that passed `domain("")` doesn't get a global wildcard.
        assert!(!host_matches("github.com", ""));
        assert!(!host_matches("a.b.example", ""));
        assert!(!host_matches("x.", ""));
        assert!(!host_matches("", ""));
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

    // -------- unwrap_safelink --------

    #[test]
    fn safelink_unwraps_microsoft_defender_wrapper() {
        let wrapped = "https://emea01.safelinks.protection.outlook.com/?url=https%3A%2F%2Fdocs.example.com%2Fpage&data=tracking";
        assert_eq!(
            unwrap_safelink(wrapped).as_deref(),
            Some("https://docs.example.com/page")
        );
    }

    #[test]
    fn safelink_unwraps_apex_safelinks_host() {
        // Some tenants emit URLs straight off `safelinks.protection.outlook.com`
        // without a regional subdomain — must match the same as the subdomain form.
        let wrapped =
            "https://safelinks.protection.outlook.com/?url=https%3A%2F%2Fdocs.example.com%2Fpage";
        assert_eq!(
            unwrap_safelink(wrapped).as_deref(),
            Some("https://docs.example.com/page")
        );
    }

    #[test]
    fn safelink_unwraps_teams_evergreen_safelink() {
        let wrapped = "https://statics.teams.cdn.office.net/evergreen-assets/safelinks/?url=https%3A%2F%2Fexample.com%2Ffoo%3Fa%3D1";
        assert_eq!(
            unwrap_safelink(wrapped).as_deref(),
            Some("https://example.com/foo?a=1")
        );
    }

    #[test]
    fn safelink_unwraps_proofpoint_v2() {
        let wrapped =
            "https://urldefense.proofpoint.com/v2/url?u=https%3A%2F%2Fexample.com%2Fa&d=foo&c=bar";
        assert_eq!(
            unwrap_safelink(wrapped).as_deref(),
            Some("https://example.com/a")
        );
    }

    #[test]
    fn safelink_passes_through_unrelated_hosts() {
        // Untouched URLs return None so the rewriter pipeline emits
        // RewriteOutcome::Unchanged (no allocation).
        assert!(unwrap_safelink("https://example.com/?url=https%3A%2F%2Felsewhere/").is_none());
        assert!(unwrap_safelink("https://example.com/page").is_none());
    }

    #[test]
    fn safelink_passes_through_teams_path_mismatch() {
        // The Teams CDN host serves more than just safelinks — only the
        // `/evergreen-assets/safelinks/` path qualifies for unwrapping.
        let unrelated =
            "https://statics.teams.cdn.office.net/evergreen-assets/other/?url=https%3A%2F%2Fexample.com";
        assert!(unwrap_safelink(unrelated).is_none());
    }

    #[test]
    fn safelink_rejects_malformed_inner_url() {
        // Decoded value isn't a valid URL — must pass through, not route as one.
        let bad = "https://safelinks.protection.outlook.com/?url=not-a-url";
        assert!(unwrap_safelink(bad).is_none());

        // Decoded value missing entirely.
        let empty = "https://safelinks.protection.outlook.com/?url=";
        assert!(unwrap_safelink(empty).is_none());
    }

    #[test]
    fn safelink_rejects_invalid_percent_escape() {
        // %ZZ is not valid hex — decoder bails, wrapper passes through.
        let bad = "https://safelinks.protection.outlook.com/?url=https%ZZ";
        assert!(unwrap_safelink(bad).is_none());
    }

    #[test]
    fn safelink_unwraps_teams_url_with_explicit_port() {
        // Regression: path was computed via scheme_end + host.len(), but
        // quick_host strips the `:443` port. The resulting `path` slice
        // started with `":443/evergreen-assets/safelinks/"` instead of
        // `"/evergreen-assets/safelinks/"`, so the Teams path-prefix
        // check failed and the URL silently routed un-unwrapped.
        let wrapped = "https://statics.teams.cdn.office.net:443/evergreen-assets/safelinks/?url=https%3A%2F%2Fexample.com%2F";
        assert_eq!(
            unwrap_safelink(wrapped).as_deref(),
            Some("https://example.com/")
        );
    }

    #[test]
    fn safelink_unwraps_proofpoint_url_with_userinfo() {
        // Same shape as the port regression but with `user@`. quick_host
        // strips userinfo as well, so path slicing must locate `/` by
        // scanning, not by host length.
        let wrapped =
            "https://x@urldefense.proofpoint.com/v2/url?u=https%3A%2F%2Fexample.com%2F&d=tag";
        assert_eq!(
            unwrap_safelink(wrapped).as_deref(),
            Some("https://example.com/")
        );
    }

    #[test]
    fn safelink_unwraps_with_valueless_param_before_url() {
        // Regression: the pre-fix find_query_param early-returned None the
        // moment it hit a kv pair without `=`, so a SafeLinks URL that
        // carried a flag-style param (`?secure&url=…`) silently failed to
        // unwrap and routed as the wrapper host. Fix: skip valueless pairs
        // and keep scanning for `name`.
        let wrapped = "https://emea01.safelinks.protection.outlook.com/?secure&url=https%3A%2F%2Fexample.com%2F";
        assert_eq!(
            unwrap_safelink(wrapped).as_deref(),
            Some("https://example.com/")
        );
    }

    #[test]
    fn teams_launcher_unwraps_to_msteams_scheme() {
        // The shape calendar invites use: a launcher URL whose `url`
        // query param is a percent-encoded relative path starting with
        // the Teams web app's `/_#` routing prefix. Strip the prefix
        // and prepend `msteams:` to get the native-app URL.
        let wrapped = "https://teams.microsoft.com/dl/launcher/launcher.html?\
                       url=%2F_%23%2Fl%2Fmeetup-join%2F19%3Ameeting_abc&\
                       type=meetup-join&deeplinkId=x&directDl=true";
        assert_eq!(
            unwrap_teams_launcher(wrapped).as_deref(),
            Some("msteams:/l/meetup-join/19:meeting_abc")
        );
    }

    #[test]
    fn teams_launcher_handles_decoded_url_without_routing_prefix() {
        // Older launcher format that doesn't include the `/_#` web-app
        // routing prefix — the decoded path is already canonical.
        let wrapped = "https://teams.microsoft.com/dl/launcher/launcher.html?\
                       url=%2Fl%2Fchannel%2F19%3Achannel123%2FGeneral";
        assert_eq!(
            unwrap_teams_launcher(wrapped).as_deref(),
            Some("msteams:/l/channel/19:channel123/General")
        );
    }

    #[test]
    fn teams_launcher_passes_through_unrelated_hosts() {
        // Other Teams URLs (the direct `/l/…` form) aren't launcher
        // wrappers — they need a different rewrite. Same host but
        // different path → pass-through, not an attempt-to-unwrap.
        assert!(
            unwrap_teams_launcher("https://teams.microsoft.com/l/meetup-join/19:meeting_abc")
                .is_none()
        );
        // Unrelated host.
        assert!(unwrap_teams_launcher(
            "https://example.com/dl/launcher/launcher.html?url=%2Fl%2Ffoo"
        )
        .is_none());
        // Right host, wrong path.
        assert!(
            unwrap_teams_launcher("https://teams.microsoft.com/other/path?url=%2Fl%2Ffoo")
                .is_none()
        );
    }

    #[test]
    fn teams_launcher_rejects_empty_or_malformed_inner_url() {
        // No `url` param → can't unwrap.
        assert!(unwrap_teams_launcher(
            "https://teams.microsoft.com/dl/launcher/launcher.html?type=meetup-join"
        )
        .is_none());
        // Empty `url` param.
        assert!(unwrap_teams_launcher(
            "https://teams.microsoft.com/dl/launcher/launcher.html?url="
        )
        .is_none());
        // Malformed percent escape — decoder bails.
        assert!(unwrap_teams_launcher(
            "https://teams.microsoft.com/dl/launcher/launcher.html?url=%ZZ"
        )
        .is_none());
    }

    #[test]
    fn safelink_handles_double_wrap_up_to_two_levels() {
        // Defender → Proofpoint chain. The Defender layer's `url` param
        // contains a percent-encoded Proofpoint URL; safelinks() should
        // unwrap both passes and yield the innermost link.
        let inner = "https://example.com/landing";
        let proofpoint = format!(
            "https://urldefense.proofpoint.com/v2/url?u={}&d=tag",
            urlencode(inner)
        );
        let defender = format!(
            "https://emea01.safelinks.protection.outlook.com/?url={}",
            urlencode(&proofpoint)
        );
        assert_eq!(unwrap_safelink(&defender).as_deref(), Some(inner));
    }

    /// Test-local URL-encoder for the double-wrap fixture. Encodes everything
    /// outside ASCII alphanumerics — heavier than necessary but trivially
    /// correct, and tests don't need to be efficient.
    fn urlencode(s: &str) -> String {
        let mut out = String::with_capacity(s.len() * 3);
        for &b in s.as_bytes() {
            if b.is_ascii_alphanumeric() {
                out.push(b as char);
            } else {
                out.push_str(&format!("%{:02X}", b));
            }
        }
        out
    }

    // -------- pattern_has_protocol_prefix --------

    #[test]
    fn pattern_has_protocol_prefix_recognises_schemes() {
        assert!(pattern_has_protocol_prefix("slack:"));
        assert!(pattern_has_protocol_prefix("https://x"));
        assert!(pattern_has_protocol_prefix("custom_scheme:foo"));
        // RFC-3986 scheme chars: + - . in continuation. Previously rejected,
        // making patterns like `chrome-extension:*` compile as unanchored.
        assert!(pattern_has_protocol_prefix("chrome-extension:foo"));
        assert!(pattern_has_protocol_prefix("view-source:bar"));
        assert!(pattern_has_protocol_prefix("git+https:baz"));
        assert!(pattern_has_protocol_prefix("web+foo:qux"));
    }

    #[test]
    fn pattern_has_protocol_prefix_rejects_non_schemes() {
        assert!(!pattern_has_protocol_prefix("slack"));
        assert!(!pattern_has_protocol_prefix(""));
        assert!(!pattern_has_protocol_prefix(":nocolon-prefix"));
        assert!(!pattern_has_protocol_prefix("zoom.us/j/*"));
        // RFC: scheme must start with ALPHA. Previously alnum was accepted.
        assert!(!pattern_has_protocol_prefix("1foo:bar"));
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
    fn wildcard_is_case_sensitive_matching_finicky() {
        // Finicky's matchWildcard produces a JS RegExp without the /i
        // flag — RegExp.test is case-sensitive by default. Mirror that.
        assert!(matches_pat("zoom.us/j/*", "https://zoom.us/j/abc"));
        // Same path, mixed case host — must NOT match without /i.
        assert!(!matches_pat("zoom.us/j/*", "HTTPS://ZOOM.US/J/abc"));
        // Path case must also be respected.
        assert!(matches_pat(
            "github.com/Org/*",
            "https://github.com/Org/repo"
        ));
        assert!(!matches_pat(
            "github.com/Org/*",
            "https://github.com/org/repo"
        ));
    }

    // -------- analyse_runtime_needs --------

    fn rule_with_matchers(ms: Vec<Matcher>) -> Rule {
        Rule {
            matchers: ms,
            rewriter: None,
            target: Target::Suppress,
            name: None,
            label: "test".to_string(),
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
        try_build_engine(user_src).expect("engine init failed")
    }

    /// Variant that returns the Result so tests can assert on
    /// EngineError variants (e.g. PreludeBroken when a hostile config
    /// trashes a prelude global).
    fn try_build_engine(user_src: &str) -> Result<Engine, EngineError> {
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

        Engine::new(LoadedConfig { exports, ctx })
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
                    logRequests: false, // tested for real in
                                        // options_log_requests_writes_jsonl_per_resolve;
                                        // false here to avoid creating a log
                                        // file at whatever HOME the parallel
                                        // test runner happens to have set
                    checkForUpdates: false,
                    keepRunning: true,
                    hideIcon: false,
                },
            };"#,
        );
        assert_eq!(resolve(&e, "https://x/").0, "com.apple.Safari");
    }

    #[test]
    fn options_hideicon_parses_to_engine_accessor() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                options: { hideIcon: true },
            };"#,
        );
        assert!(e.hide_icon());
    }

    #[test]
    fn options_hideicon_default_is_false() {
        let e = build_engine(r#"module.exports = { default: "com.apple.Safari" };"#);
        assert!(!e.hide_icon());

        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                options: { hideIcon: false },
            };"#,
        );
        assert!(!e.hide_icon());
    }

    #[test]
    fn rule_listing_describes_each_rule_with_index_and_target() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                browsers: { work: { name: "com.google.Chrome", profile: "Work" } },
                rules: [
                    { match: "github.com", open: "com.google.Chrome", name: "code-hosts" },
                    { match: "slack:*", open: "com.tinyspeck.slackmacgap" },
                    { match: (url) => url.searchParams.has("incognito"), open: null },
                ],
            };"#,
        );
        let lines = e.rule_listing();
        assert_eq!(lines.len(), 3, "expected three rules: {lines:?}");
        // user-supplied name takes precedence, target is the bundle id
        assert_eq!(lines[0], "0: [code-hosts] github.com → com.google.Chrome");
        // no name → auto-derived label from the string pattern
        assert_eq!(lines[1], "1: slack:* → com.tinyspeck.slackmacgap");
        // fn matcher → first line of f.toString(); open:null → "(suppress)"
        assert!(
            lines[2].starts_with("2: fn:") && lines[2].ends_with("→ (suppress)"),
            "fn rule line had unexpected shape: {}",
            lines[2]
        );
    }

    #[test]
    fn matched_rule_in_log_uses_user_name_when_present() {
        let tmp = unique_tmp("log-name");
        let _ = std::fs::remove_dir_all(&tmp);
        with_home(&tmp, || {
            let e = build_engine(
                r#"module.exports = {
                    default: "com.apple.Safari",
                    options: { logRequests: true },
                    rules: [{ match: "github.com", open: "com.google.Chrome", name: "code-hosts" }],
                };"#,
            );
            assert_eq!(resolve(&e, "https://github.com/").0, "com.google.Chrome");
        });
        let log_dir = tmp.join("Library/Logs/Grinch");
        let entries: Vec<_> = std::fs::read_dir(&log_dir)
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        let body = std::fs::read_to_string(entries[0].path()).unwrap();
        let row: serde_json::Value = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        assert_eq!(row["matchedRule"]["index"], 0);
        assert_eq!(row["matchedRule"]["name"], "code-hosts");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// HOME is process-global. The two log tests serialise via this
    /// mutex so neither sees the other's HOME mid-engine-init. Other
    /// integration tests don't read HOME from inside Engine::new (no
    /// log_requests) so they don't need the lock.
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Run `f` with HOME pointed at `home` for its duration, holding the
    /// shared HOME_LOCK. The Engine's log writer is lazy and opens the
    /// file on first write, so HOME must still point at the test tmpdir
    /// when resolves happen — hence holding the lock around the whole
    /// engine-and-resolves block, not just the construction call.
    fn with_home<R>(home: &std::path::Path, f: impl FnOnce() -> R) -> R {
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", home);
        }
        let out = f();
        unsafe {
            match prev {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
        out
    }

    /// Build a guaranteed-unique tmp-dir path. Per-test pid+name+counter
    /// to avoid cross-test pollution if a previous run left junk behind
    /// or another parallel test happens to compose the same path.
    fn unique_tmp(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("grinch-{}-{}-{}", name, std::process::id(), n))
    }

    #[test]
    fn options_log_requests_writes_jsonl_per_resolve() {
        let tmp = unique_tmp("log-on");
        let _ = std::fs::remove_dir_all(&tmp);

        with_home(&tmp, || {
            let e = build_engine(
                r#"module.exports = {
                    default: "com.apple.Safari",
                    options: { logRequests: true },
                    rules: [{ match: "github.com", open: "com.google.Chrome" }],
                };"#,
            );
            assert_eq!(resolve(&e, "https://github.com/").0, "com.google.Chrome");
            assert_eq!(resolve(&e, "https://example.com/").0, "com.apple.Safari");
        });

        let log_dir = tmp.join("Library/Logs/Grinch");
        let entries: Vec<_> = std::fs::read_dir(&log_dir)
            .unwrap_or_else(|e| panic!("expected log dir at {}: {e}", log_dir.display()))
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(entries.len(), 1, "expected exactly one log file");
        let body = std::fs::read_to_string(entries[0].path()).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2, "expected two log lines, got: {body}");
        let row0: serde_json::Value = serde_json::from_str(lines[0]).expect("line 0 is JSON");
        let row1: serde_json::Value = serde_json::from_str(lines[1]).expect("line 1 is JSON");
        // Rule-hit row: matchedRule object with index + auto-derived name,
        // opener nested, modifiers nested with all four booleans,
        // rewritten = false.
        assert_eq!(row0["url"], "https://github.com/");
        assert_eq!(row0["final"], "https://github.com/");
        assert_eq!(row0["rewritten"], false);
        assert_eq!(row0["browser"], "com.google.Chrome");
        assert_eq!(row0["matchedRule"]["index"], 0);
        assert_eq!(row0["matchedRule"]["name"], "github.com");
        assert!(row0["opener"].is_object(), "opener should be an object");
        assert!(row0["opener"]["bundleId"].is_string());
        assert!(row0["opener"]["name"].is_string());
        assert!(row0["opener"]["pid"].is_number());
        assert_eq!(row0["modifiers"]["shift"], false);
        assert_eq!(row0["modifiers"]["option"], false);
        assert_eq!(row0["modifiers"]["command"], false);
        assert_eq!(row0["modifiers"]["control"], false);
        // Default-fallback row: matchedRule = null, browser = default.
        assert_eq!(row1["url"], "https://example.com/");
        assert_eq!(row1["browser"], "com.apple.Safari");
        assert!(
            row1["matchedRule"].is_null(),
            "matchedRule should be null when default fired"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn cached_js_string_stops_inserting_past_soft_cap() {
        // Build a context, hand it a cache that's already at the cap,
        // and verify the next insert is a no-op. The lookup must still
        // succeed (returns a fresh JSValue) — only growth is capped.
        let ctx: Retained<JSContext> = unsafe { JSContext::new() };
        let cache = RefCell::new(std::collections::HashMap::new());
        // Pre-fill to the cap with synthetic entries.
        for i in 0..STRING_CACHE_SOFT_CAP {
            let key = format!("preload_{i}");
            let v = js_string(&ctx, &key).expect("js_string ok");
            cache.borrow_mut().insert(key, v);
        }
        assert_eq!(cache.borrow().len(), STRING_CACHE_SOFT_CAP);
        // New miss → still returns a JSValue but doesn't grow the map.
        let v = cached_js_string(&ctx, &cache, "post_cap").expect("returns value");
        assert!(unsafe { v.isString() });
        assert_eq!(
            cache.borrow().len(),
            STRING_CACHE_SOFT_CAP,
            "cache must not grow past the soft cap"
        );
        // Existing key still hits → no allocation.
        let hit = cached_js_string(&ctx, &cache, "preload_0").expect("returns value");
        assert!(unsafe { hit.isString() });
    }

    #[test]
    fn log_writer_should_rotate_unit() {
        // Pure-function rotation predicate — verifies bytes-based and
        // time-based thresholds independently, without touching the fs.
        let mut w = LogWriter::new(
            std::path::PathBuf::from("/tmp/never-opened.log"),
            Some(1024),
            Some(7),
        );
        // No file open yet → never rotates (rotation rebinds bytes_written
        // when the new file opens; nothing to rotate before that).
        assert!(!w.should_rotate(2048, 1_000_000));
        // Pretend a file is open with some bytes already written.
        w.file = Some(
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(std::env::temp_dir().join("grinch-log-rotate-unit.tmp"))
                .unwrap(),
        );
        w.bytes_written = 1000;
        w.opened_at_unix = 1_000_000;
        // Under both thresholds: no rotation.
        assert!(!w.should_rotate(20, 1_000_000));
        // Adding 25 bytes would push past 1024.
        assert!(w.should_rotate(25, 1_000_000));
        // Time threshold (7 days = 604_800s): exactly at the threshold rotates.
        w.bytes_written = 0;
        assert!(w.should_rotate(1, 1_000_000 + 604_800));
        // Just under: no rotation.
        assert!(!w.should_rotate(1, 1_000_000 + 604_799));
    }

    #[test]
    fn log_rotates_on_size_threshold() {
        // End-to-end: configure a 200-byte cap and write enough lines to
        // trigger a rotation. After the test we expect (a) a rotated
        // file with the .log.<timestamp> suffix containing the early
        // lines, and (b) a fresh active file with the later ones.
        let tmp = unique_tmp("log-rotate");
        let _ = std::fs::remove_dir_all(&tmp);
        with_home(&tmp, || {
            let e = build_engine(
                r#"module.exports = {
                    default: "com.apple.Safari",
                    options: { logRequests: true, logRotateBytes: 200 },
                };"#,
            );
            // Each log line is ~250 bytes; the first write opens the
            // file (size 0, 0 + ~250 > 200 — wait, the should_rotate
            // check skips when file is None, so the FIRST line lands
            // un-rotated). The SECOND write sees bytes_written=~250 +
            // ~250 = ~500 > 200 → rotates before writing the second
            // line. Drive enough resolves to trigger at least one
            // rotation regardless of exact line length.
            for _ in 0..5 {
                let _ = resolve(&e, "https://example.com/");
            }
        });
        let log_dir = tmp.join("Library/Logs/Grinch");
        let entries: Vec<_> = std::fs::read_dir(&log_dir)
            .unwrap()
            .filter_map(|r| r.ok())
            .map(|e| e.path())
            .collect();
        assert!(
            entries.len() >= 2,
            "expected at least one rotated log file + the active one, got: {entries:?}"
        );
        let has_rotated = entries
            .iter()
            .any(|p| p.to_string_lossy().contains(".log."));
        assert!(
            has_rotated,
            "expected a .log.<timestamp> rotated file in: {entries:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn options_log_requests_off_writes_nothing() {
        let tmp = unique_tmp("log-off");
        let _ = std::fs::remove_dir_all(&tmp);

        with_home(&tmp, || {
            let e = build_engine(r#"module.exports = { default: "com.apple.Safari" };"#);
            let _ = resolve(&e, "https://x/");
        });

        let log_dir = tmp.join("Library/Logs/Grinch");
        if log_dir.exists() {
            // Debug: dump what's there so we can see what actually got
            // written if this fails again.
            let listing: Vec<_> = std::fs::read_dir(&log_dir)
                .map(|d| {
                    d.filter_map(|r| r.ok())
                        .map(|e| e.path().display().to_string())
                        .collect()
                })
                .unwrap_or_default();
            panic!(
                "log dir was created with logRequests off (path: {}, contents: {:?})",
                log_dir.display(),
                listing,
            );
        }
        let _ = std::fs::remove_dir_all(&tmp);
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
    fn default_browser_null_is_explicit_suppress() {
        // Finicky-compat: `defaultBrowser: null` means "do nothing if no
        // rule matches" rather than being a config error. Mirrors how
        // a rule's `open: null` suppresses an individual URL.
        let e = build_engine(
            r#"module.exports = {
                default: null,
                rules: [{ match: "github.com", open: "com.google.Chrome" }],
            };"#,
        );
        // Match → routes normally.
        assert_eq!(resolve(&e, "https://github.com/").0, "com.google.Chrome");
        // No match → suppressed via the about:blank sentinel.
        let (browser, url) = resolve(&e, "https://other.com/");
        assert_eq!(browser, "");
        assert_eq!(url, "about:blank");
    }

    #[test]
    fn dynamic_default_browser_returning_null_suppresses() {
        let e = build_engine(r#"module.exports = { default: () => null };"#);
        let (browser, url) = resolve(&e, "https://x/");
        assert_eq!(browser, "");
        assert_eq!(url, "about:blank");
    }

    #[test]
    fn export_default_es_module_syntax_works() {
        // Verify the loader's preprocess step kicks in and the user
        // can write Finicky-v4-style `export default { … }` without
        // converting to module.exports first. We stage the same way
        // the loader does — preprocess + wrap — and run through
        // build_engine's existing pipeline.
        use crate::helpers::preprocess_es_module_syntax;
        let src = preprocess_es_module_syntax(
            r#"export default {
                default: "com.apple.Safari",
                rules: [{ match: "github.com", open: "com.google.Chrome" }],
            };"#,
        )
        .unwrap();
        let e = build_engine(&src);
        assert_eq!(resolve(&e, "https://github.com/").0, "com.google.Chrome");
        assert_eq!(resolve(&e, "https://example.com/").0, "com.apple.Safari");
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
    fn rewriter_fn_returning_undefined_passes_through() {
        // Finicky v4 contract: undefined return = leave the URL alone.
        // Distinct from null (drop). Pin both behaviours together.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [
                    { match: () => true, url: () => undefined },
                ],
            };"#,
        );
        let (browser, url) = resolve(&e, "https://example.com/path?q=1");
        assert_eq!(browser, "com.apple.Safari");
        assert_eq!(url, "https://example.com/path?q=1");
    }

    #[test]
    fn rewriter_fn_with_no_explicit_return_is_pass_through() {
        // Functions with no `return` statement implicitly return undefined,
        // which the prelude maps to "no change". Same as the explicit
        // undefined return.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [
                    { match: () => true, url: () => { /* no return */ } },
                ],
            };"#,
        );
        let (_, url) = resolve(&e, "https://x.example/path");
        assert_eq!(url, "https://x.example/path");
    }

    #[test]
    fn rewriter_fn_returning_url_with_no_changes_is_pass_through() {
        // Returning the URL instance unchanged should yield the same href.
        // Tests both the URL-instance return path and the
        // `if s == url` shortcut in apply_rewrite.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [
                    { match: () => true, url: (url) => url },
                ],
            };"#,
        );
        let (_, url) = resolve(&e, "https://example.com/path?q=1");
        assert_eq!(url, "https://example.com/path?q=1");
    }

    #[test]
    fn dispatcher_resumes_after_target_fn_returns_null_in_run() {
        // Three consecutive fn-only rules. Rule 0's matcher fires but its
        // target fn returns null (Finicky `open: () => null` shape for
        // "rule matched but skip routing"). Rule 1's target fn returns
        // null too. Rule 2's target fn returns a real browser.
        //
        // Pre-fix: dispatcher matched rule 0, fell through; resolve loop
        // advanced idx to 1, didn't see a run starting at 1, fell back
        // to the per-matcher path for the rest of the run. Correct, but
        // lost the batched-dispatch perf benefit for the resume.
        //
        // Now: the resolve loop detects we're still INSIDE the run and
        // re-calls the dispatcher with start_offset = idx - run.start,
        // so the JS-side scan picks up at the next matcher.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [
                    { match: (url) => url.hostname === "github.com",
                      open: (url) => null },
                    { match: (url) => url.hostname === "github.com",
                      open: (url) => null },
                    { match: (url) => url.hostname === "github.com",
                      open: (url) => "com.google.Chrome" },
                ],
            };"#,
        );
        assert_eq!(resolve(&e, "https://github.com/foo").0, "com.google.Chrome");
    }

    #[test]
    fn invalid_regex_matcher_drops_and_warns_but_engine_still_loads() {
        // The Rust regex crate doesn't support JS lookbehind `(?<=…)`.
        // Pre-fix, compile_matcher silently dropped the matcher and the
        // rule loaded with `matchers: []`, meaning the rule never fired
        // with no diagnostic. Verify the engine still loads (we don't
        // panic the config-load on a bad regex — other rules might be
        // fine), the bad rule is inert, and a later valid rule fires
        // as a fallback.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [
                    // Lookbehind: unsupported by `regex` crate.
                    { match: /(?<=test\.)github\.com/, open: "com.brave.Browser" },
                    // Fallback that should still match the URL.
                    { match: "github.com", open: "com.google.Chrome" },
                ],
            };"#,
        );
        assert_eq!(resolve(&e, "https://github.com/foo").0, "com.google.Chrome");
    }

    #[test]
    fn rewriter_with_throwing_href_getter_doesnt_poison_next_matcher() {
        // Regression: when a fn rewriter returns an object whose .href
        // getter throws, the JSC bridge stashes the thrown value on
        // ctx.exception. The fast-path bypass correctly rejected the
        // bad object (type check), but didn't clear the exception state
        // — so the *next* JS call in the same resolve (the next matcher
        // or the helper fall-through) inherited the exception and
        // produced "unexpected fall-through to default" symptoms.
        //
        // The setup: a fn rewriter returns `{get href() { throw … }}`.
        // The fast-path read of `.href` triggers the throw. After the
        // exception is cleared, the next rule's matcher runs and routes
        // normally.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [
                    {
                        match: "trigger.example.com",
                        url: (url) => ({ get href() { throw new Error("nope"); } }),
                    },
                ],
                rules: [
                    { match: "trigger.example.com", open: "com.google.Chrome" },
                ],
            };"#,
        );
        // The rewrite returns a poisoned object; fast-path bypass sees
        // the throwing getter, rejects, leaves URL unchanged. The rule's
        // matcher then evaluates against the original URL and fires.
        assert_eq!(
            resolve(&e, "https://trigger.example.com/").0,
            "com.google.Chrome"
        );
    }

    #[test]
    fn url_polyfill_parses_ipv6_host_literal() {
        // Regression: the URL polyfill regex's hostname class was
        // `[^:\/?#]*`, which stopped at the first `:` inside an IPv6
        // literal — `https://[::1]:8080/path` parsed with hostname=`[`
        // and the rest of the address leaked into pathname. A user fn
        // matcher reading `url.hostname` could never match an IPv6 URL
        // correctly. After the fix, the regex alternates between an
        // IPv6-bracket branch and the bare-host branch.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: (url) => url.hostname === "[::1]" && url.port === "8080",
                    open: "com.google.Chrome",
                }],
            };"#,
        );
        assert_eq!(
            resolve(&e, "https://[::1]:8080/path").0,
            "com.google.Chrome"
        );
    }

    #[test]
    fn url_polyfill_serialises_ipv6_round_trip() {
        // After parsing IPv6, rebuildHref must keep the brackets so
        // `url.href` round-trips. Verify via a no-op rewrite that
        // returns the polyfill instance — Grinch reads .href via the
        // fast-path bypass and resolves with that string.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [{
                    match: (url) => url.hostname === "[2001:db8::1]",
                    url: (url) => url,
                }],
            };"#,
        );
        let (_, url) = resolve(&e, "https://[2001:db8::1]/api?q=1");
        assert_eq!(url, "https://[2001:db8::1]/api?q=1");
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
    fn fn_matcher_dispatcher_picks_first_matching_rule_in_a_run() {
        // Four consecutive fn-only rules — the second one matches. The
        // dispatcher must return offset 1 (not 0, 2, or 3) so the right
        // rule fires. Regression test for the build_fn_matcher_runs path.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [
                    { match: (url) => url.hostname === "miss-a", open: "com.apple.Mail" },
                    { match: (url) => url.hostname === "github.com", open: "com.google.Chrome" },
                    { match: (url) => url.hostname === "miss-c", open: "com.brave.Browser" },
                    { match: (url) => url.hostname === "miss-d", open: "com.microsoft.edgemac" },
                ],
            };"#,
        );
        let (b, _) = resolve(&e, "https://github.com/foo");
        assert_eq!(b, "com.google.Chrome");
    }

    #[test]
    fn fn_matcher_dispatcher_falls_through_to_default_when_nothing_matches() {
        // Same shape as the slow-native bench. No matcher matches; dispatcher
        // returns -1 and the engine skips past the whole run to the default.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [
                    { match: (url, ctx) => ctx.opener && ctx.opener.bundleId === "a.example",
                      open: "com.google.Chrome" },
                    { match: (url, ctx) => ctx.opener && ctx.opener.bundleId === "b.example",
                      open: "com.google.Chrome" },
                    { match: (url, ctx) => ctx.opener && ctx.opener.bundleId === "c.example",
                      open: "com.google.Chrome" },
                    { match: (url, ctx) => ctx.opener && ctx.opener.bundleId === "d.example",
                      open: "com.google.Chrome" },
                ],
            };"#,
        );
        assert_eq!(resolve(&e, "https://x/").0, "com.apple.Safari");
    }

    #[test]
    fn fn_matcher_dispatcher_isolates_throwing_matcher_from_neighbours() {
        // A matcher that throws in the middle of a run must not poison
        // matchers around it — the dispatcher's per-matcher try/catch
        // treats a throw as no-match, same as the per-matcher path's
        // `result.map(...).unwrap_or(false)`.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [
                    { match: (url) => url.hostname === "miss-a", open: "com.apple.Mail" },
                    { match: (url) => { throw new Error("boom"); }, open: "com.brave.Browser" },
                    { match: (url) => url.hostname === "github.com", open: "com.google.Chrome" },
                ],
            };"#,
        );
        assert_eq!(resolve(&e, "https://github.com/foo").0, "com.google.Chrome");
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
    fn ctx_modifiers_includes_caps_lock_and_function() {
        // Pin the v4 shape: ctx.modifiers exposes seven keys.
        // shift/option/command/control/capsLock/fn/function — fn and
        // function carry the same value (Finicky-style alias).
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: () => true,
                    open: (url, ctx) => "k:" + Object.keys(ctx.modifiers).sort().join(","),
                }],
            };"#,
        );
        // Sorted: capsLock, command, control, fn, function, option, shift.
        assert_eq!(
            resolve(&e, "https://x/").0,
            "k:capsLock,command,control,fn,function,option,shift",
        );
    }

    #[test]
    fn ctx_modifiers_caps_lock_value_propagates() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: (url, ctx) => ctx.modifiers.capsLock,
                    open: "com.google.Chrome",
                }],
            };"#,
        );
        // No caps lock — falls through to default.
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
        // Caps lock on — matches.
        let caps = ModifierFlags {
            caps_lock: true,
            ..ModifierFlags::default()
        };
        assert_eq!(
            resolve_with(&e, "https://x/", &Opener::default(), caps).0,
            "com.google.Chrome",
        );
    }

    #[test]
    fn ctx_modifiers_function_alias_matches_fn() {
        // Finicky exposes both `fn` and `function` with the same value.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: (url, ctx) => ctx.modifiers.fn === ctx.modifiers.function,
                    open: "com.google.Chrome",
                }],
            };"#,
        );
        assert_eq!(resolve(&e, "https://x/").0, "com.google.Chrome");
    }

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
        assert!(e.needs_opener_full());
    }

    #[test]
    fn config_that_trashes_prelude_global_returns_error_not_panic() {
        // Hostile/buggy config that nukes a prelude global should produce
        // a clean EngineError so the previous engine survives a SIGHUP
        // reload, not a panic that tears down the process.
        let result = try_build_engine(
            r#"RegExp = null;
               module.exports = { default: "com.apple.Safari" };"#,
        );
        match result {
            Err(EngineError::PreludeBroken { global }) => assert_eq!(global, "RegExp"),
            Err(other) => panic!("wrong error variant: {other:?}"),
            Ok(_) => panic!("expected PreludeBroken, got Ok"),
        }
    }

    #[test]
    fn config_with_circular_browsers_map_does_not_stack_overflow() {
        // Regression: iter_object used to call v.toDictionary() which
        // recursively converted every value to its NS* equivalent and
        // blew the stack on circular references. The Object.keys path
        // walks names only — circular *values* are safe; we just hand
        // the JSValue back to parse_browser_jsval, which reads specific
        // keys (name/id/profile/...) without deep traversal.
        let e = build_engine(
            r#"var x = {};
               x.self = x;
               module.exports = {
                 default: "com.apple.Safari",
                 browsers: { broken: x },
               };"#,
        );
        // Resolves without panicking; broken-browser entry is a no-op spec
        // (no `name`/`id`), so the rule below falls through to the default.
        assert_eq!(resolve(&e, "https://x/").0, "com.apple.Safari");
    }

    #[test]
    fn config_that_nulls_make_ctx_helper_returns_error_not_panic() {
        // Function-declaration globals can't be `delete`d (non-configurable),
        // but can be assigned over.
        let result = try_build_engine(
            r#"globalThis.__grinchMakeCtx = null;
               module.exports = { default: "com.apple.Safari" };"#,
        );
        match result {
            Err(EngineError::PreludeBroken { global }) => assert_eq!(global, "__grinchMakeCtx"),
            Err(other) => panic!("wrong error variant: {other:?}"),
            Ok(_) => panic!("expected PreludeBroken, got Ok"),
        }
    }

    #[test]
    fn from_matcher_needs_opener_but_not_full() {
        // `from()` matchers only read opener.bundle_id — AppDelegate can use
        // the lite `frontmost_opener_id` path that skips localizedName /
        // executableURL IPC.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: from("com.tinyspeck.slackmacgap"),
                    open: "com.google.Chrome",
                }],
            };"#,
        );
        assert!(e.needs_opener());
        assert!(!e.needs_modifiers());
        assert!(!e.needs_opener_full());
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
    fn legacy_url_string_returns_href() {
        // url.urlString is the v3 alias for url.href. Shim warns and
        // returns the same value.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: (url) => url.urlString === url.href,
                    open: "com.google.Chrome",
                }],
            };"#,
        );
        assert_eq!(resolve(&e, "https://x/path").0, "com.google.Chrome");
    }

    #[test]
    fn legacy_url_url_returns_legacy_object_shape() {
        // url.url returns a plain LegacyURLObject. Verify the shape.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: (url) => {
                        var u = url.url;
                        return u.protocol === "https"
                            && u.hostname === "github.com"
                            && u.pathname === "/x"
                            && u.search === "q=1"
                            && u.hash === "frag";
                    },
                    open: "com.google.Chrome",
                }],
            };"#,
        );
        assert_eq!(
            resolve(&e, "https://github.com/x?q=1#frag").0,
            "com.google.Chrome",
        );
    }

    #[test]
    fn legacy_url_opener_returns_active_opener_with_warning() {
        // Match Finicky v4: url.opener warns and returns the live opener.
        // The opener publishes onto a per-resolve global from
        // `__grinchMakeCtx`, so we need a 2-arg fn (which triggers ctx
        // build) for the value to be set. The matcher reads `url.opener`
        // and checks the bundle ID — without the warn-and-return shim
        // this would have thrown.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: (url, _ctx) => url.opener && url.opener.bundleId === "com.x",
                    open: "com.google.Chrome",
                }],
            };"#,
        );
        let known = opener("com.x", "X");
        let (browser, _) = resolve_with(&e, "https://x/", &known, ModifierFlags::default());
        assert_eq!(browser, "com.google.Chrome");
    }

    #[test]
    fn legacy_url_keys_throws_with_helpful_message() {
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: (url) => {
                        try { url.keys; return false; }
                        catch (e) {
                            return e.message.indexOf("ctx.modifiers") !== -1
                                && e.message.indexOf("getModifierKeys") !== -1;
                        }
                    },
                    open: "com.google.Chrome",
                }],
            };"#,
        );
        assert_eq!(resolve(&e, "https://x/").0, "com.google.Chrome");
    }

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
    fn polyfill_preserves_opaque_scheme_on_rewrite() {
        // Regression: rebuildHref used to unconditionally emit `scheme://...`,
        // turning `mailto:user@example.com` into `mailto://user@example.com`.
        // Verify the opaque schemes round-trip through a rewrite that
        // returns the URL object unchanged.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [{ match: () => true, url: (url) => url }],
            };"#,
        );
        assert_eq!(
            resolve(&e, "mailto:user@example.com").1,
            "mailto:user@example.com"
        );
        assert_eq!(resolve(&e, "tel:+15551234567").1, "tel:+15551234567");
        assert_eq!(resolve(&e, "javascript:void(0)").1, "javascript:void(0)");
        // Hierarchical schemes still get the `//`.
        assert_eq!(
            resolve(&e, "https://example.com/path").1,
            "https://example.com/path"
        );
    }

    #[test]
    fn polyfill_searchparams_value_with_equals_signs() {
        // Regression: split("=") + kv[1] used to truncate values containing
        // `=` (signed tokens, base64 payloads, nested query strings). The
        // WHATWG split-on-first-= behaviour preserves the full value.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: () => true,
                    open: (url) => "v:" + url.searchParams.get("token"),
                }],
            };"#,
        );
        assert_eq!(resolve(&e, "https://x/?token=a=b=c&q=1").0, "v:a=b=c");
    }

    #[test]
    fn polyfill_searchparams_immune_to_object_prototype_pollution() {
        // Regression: `_m: {}` exposed every URL's searchParams to
        // Object.prototype mutations — `Object.prototype.utm = ["x"]`
        // injected a phantom "utm" entry into every URL. Object.create(null)
        // backing object has no prototype, so for-in only enumerates own
        // keys.
        let e = build_engine(
            r#"Object.prototype.utm = ["polluted"];
               module.exports = {
                 default: "com.apple.Safari",
                 rules: [{
                   match: () => true,
                   open: (url) => "n:" + url.searchParams.size +
                                  ",has:" + (url.searchParams.has("utm") ? "yes" : "no"),
                 }],
               };"#,
        );
        // Clean URL: zero own keys, no "utm".
        assert_eq!(resolve(&e, "https://x/").0, "n:0,has:no");
        // Real ?utm=… still works.
        assert_eq!(resolve(&e, "https://x/?utm=real").0, "n:1,has:yes");
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
    fn parse_browser_jsval_firefox_profile_resolves_via_p_flag() {
        // Firefox-family bundle with a profile string should produce
        // `-P <name>` args, not `--profile-directory=…`. We can't easily
        // observe the args without a real BrowserSpec accessor, but we
        // can at least check the engine accepts the config without
        // erroring (Firefox profile validation logs to stderr if the
        // name is unknown but doesn't fail the load).
        let e = build_engine(
            r#"module.exports = {
                default: { name: "org.mozilla.firefox", profile: "Work" },
            };"#,
        );
        // Bundle ID survives unchanged.
        assert_eq!(resolve(&e, "https://x/").0, "org.mozilla.firefox");
    }

    #[test]
    fn parse_browser_jsval_firefox_profile_via_shorthand_string() {
        let e = build_engine(r#"module.exports = { default: "org.mozilla.firefox:Work" };"#);
        assert_eq!(resolve(&e, "https://x/").0, "org.mozilla.firefox");
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
    fn browser_spec_string_path_autodetects_via_nsbundle() {
        // Finicky-compat: a bare-string browser spec that looks like an
        // .app path (ends with .app + contains /) goes through NSBundle
        // directly, no `appType: "path"` required. Use Safari since it
        // ships with macOS in /Applications/Safari.app.
        let e = build_engine(r#"module.exports = { default: "/Applications/Safari.app" };"#);
        assert_eq!(resolve(&e, "https://x/").0, "com.apple.Safari");
    }

    #[test]
    fn browser_spec_string_path_with_tilde_expands_home() {
        // Tilde expansion in the path. Hard to test against a real ~
        // path without polluting the home directory, so use the engine
        // fixture's HOME-override mutex to point HOME at /Applications,
        // then refer to ~/Safari.app — should resolve to the same bundle
        // ID as /Applications/Safari.app does in the test above.
        with_home(std::path::Path::new("/Applications"), || {
            let e = build_engine(r#"module.exports = { default: "~/Safari.app" };"#);
            assert_eq!(resolve(&e, "https://x/").0, "com.apple.Safari");
        });
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
    fn finicky_is_app_running_returns_false_for_unknown_input() {
        // Pass an obviously-bogus identifier that matches no bundle ID
        // and no localized name. Verifies the bridge round-trips
        // (JS call → Rust workspace lookup → string return → JS bool
        // coerce) and that the localized-name comparison branch is
        // exercised — `is_app_running` walks every running app checking
        // BOTH `bundleIdentifier` and `localizedName` against the input
        // before returning false. (The "true" case is environment-
        // dependent — headless CI runners may not have Finder/Dock/etc.
        // running — so we don't pin a specific app here.)
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: () => true,
                    open: () =>
                        finicky.isAppRunning("definitely-not-installed-xyz123-fake") ? "yes" : "no",
                }],
            };"#,
        );
        assert_eq!(resolve(&e, "https://x/").0, "no");
    }

    #[test]
    fn finicky_is_app_running_returns_true_for_known_running_app() {
        // Round-trip the bridge against an app the workspace itself
        // confirms is running. If the workspace returns no apps at all
        // (sandboxed test env), skip — the previous test already
        // covered the false-path; this one's about the true path.
        let running = crate::workspace::running_app_bundle_ids();
        let Some(known) = running.iter().next().cloned() else {
            eprintln!("skipping: no running apps detected on this host");
            return;
        };
        // Pass the known bundle ID through the JS bridge and back.
        let src = format!(
            r#"module.exports = {{
                default: "com.apple.Safari",
                rules: [{{
                    match: () => true,
                    open: () => finicky.isAppRunning("{known}") ? "yes" : "no",
                }}],
            }};"#,
        );
        let e = build_engine(&src);
        assert_eq!(resolve(&e, "https://x/").0, "yes");
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
    fn finicky_get_power_info_is_dedup_stub() {
        // The stub returns the same shape on every call. The one-time
        // console.warn is observable on stderr but doesn't affect the
        // return value; verify the structure is stable across repeated
        // calls so the dedup flag doesn't accidentally cache a
        // different first-call return.
        let e = build_engine(
            r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: () => true,
                    open: () => {
                        var a = finicky.getPowerInfo();
                        var b = finicky.getPowerInfo();
                        return "same:" + (a.isCharging === b.isCharging
                            && a.isConnected === b.isConnected
                            && a.percentage === b.percentage);
                    },
                }],
            };"#,
        );
        assert_eq!(resolve(&e, "https://x/").0, "same:true");
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
