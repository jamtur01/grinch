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

// Submodules carved out of the original monolith. `engine` re-exports each
// via `pub(crate) use` so every intra-crate path (and the test modules'
// `use super::*;`) resolves the moved items unqualified.
mod compile;
mod jsbridge;
mod logging;
mod rewrite;
mod spec;
mod urlparse;

pub(crate) use compile::*;
pub(crate) use jsbridge::*;
pub(crate) use logging::*;
pub(crate) use rewrite::*;
pub(crate) use spec::*;
pub(crate) use urlparse::*;

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

/// Pure description of how [`crate::workspace::open_url`] will hand a resolved
/// [`BrowserSpec`] to macOS. Extracted from the side-effecting NSWorkspace call
/// so the launch *decision* — which strategy, which flags, which exact argv —
/// is unit-testable without a running app, and so the chosen strategy can be
/// recorded per-resolve in the request log. That log line is the only way to
/// correlate the intermittent Chrome window-reuse behaviour with what Grinch
/// actually asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchPlan {
    /// `open: null` / empty bundle id — nothing is launched.
    Suppress,
    /// No custom args: route the URL to the app via
    /// `openURLs:withApplicationAtURL:`. LaunchServices delivers it to a
    /// running instance (the window-reusing path); there are no profile /
    /// incognito flags to honour in this case.
    OpenUrls { activates: bool },
    /// Custom args present (a Chromium/Firefox profile, `--incognito`, or
    /// `--new-window`): launch via `openApplicationAtURL:` passing `args` (the
    /// spec's args followed by the URL) on the command line so the flags reach
    /// the browser's argv. `new_instance` forces a fresh application instance —
    /// required because macOS drops `configuration.arguments` when it delivers
    /// to an already-running instance.
    LaunchApplication {
        args: Vec<String>,
        new_instance: bool,
        activates: bool,
    },
}

impl LaunchPlan {
    /// Decide the launch plan for `spec` opening `url`. Pure — no IO, no
    /// AppKit — so it can be asserted directly in tests.
    pub fn from_spec(spec: &BrowserSpec, url: &str) -> LaunchPlan {
        if spec.bundle_id.is_empty() {
            return LaunchPlan::Suppress;
        }
        let activates = !spec.open_in_background;
        if spec.args.is_empty() {
            LaunchPlan::OpenUrls { activates }
        } else {
            let mut args = spec.args.clone();
            args.push(url.to_string());
            LaunchPlan::LaunchApplication {
                args,
                new_instance: spec.creates_new_instance,
                activates,
            }
        }
    }

    /// Stable, log-friendly identifier for the chosen strategy. Recorded in the
    /// per-resolve request log so intermittent new-window behaviour can be
    /// correlated with the launch path Grinch selected.
    pub fn strategy(&self) -> &'static str {
        match self {
            LaunchPlan::Suppress => "suppress",
            LaunchPlan::OpenUrls { .. } => "open_urls",
            LaunchPlan::LaunchApplication {
                new_instance: true, ..
            } => "launch_new_instance",
            LaunchPlan::LaunchApplication {
                new_instance: false,
                ..
            } => "launch_application",
        }
    }
}

#[cfg(test)]
mod launch_plan_tests {
    use super::*;

    fn spec(bundle: &str, args: &[&str], background: bool, new_instance: bool) -> BrowserSpec {
        BrowserSpec {
            bundle_id: bundle.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            open_in_background: background,
            creates_new_instance: new_instance,
        }
    }

    #[test]
    fn empty_bundle_suppresses() {
        let p = LaunchPlan::from_spec(&spec("", &[], false, false), "https://x/");
        assert_eq!(p, LaunchPlan::Suppress);
        assert_eq!(p.strategy(), "suppress");
    }

    #[test]
    fn no_args_uses_open_urls() {
        let p = LaunchPlan::from_spec(&spec("com.apple.Safari", &[], false, false), "https://x/");
        assert_eq!(p, LaunchPlan::OpenUrls { activates: true });
        assert_eq!(p.strategy(), "open_urls");
    }

    #[test]
    fn background_disables_activation() {
        let p = LaunchPlan::from_spec(&spec("com.apple.Safari", &[], true, false), "https://x/");
        assert_eq!(p, LaunchPlan::OpenUrls { activates: false });
    }

    #[test]
    fn profile_appends_url_after_flag_without_new_window() {
        let p = LaunchPlan::from_spec(
            &spec(
                "com.google.Chrome",
                &["--profile-directory=Profile 10"],
                false,
                true,
            ),
            "https://example.com/",
        );
        match p {
            LaunchPlan::LaunchApplication {
                args,
                new_instance,
                activates,
            } => {
                assert!(new_instance, "a profile launch must force a new instance");
                assert!(activates);
                assert_eq!(
                    args,
                    vec![
                        "--profile-directory=Profile 10".to_string(),
                        "https://example.com/".to_string(),
                    ],
                    "args must be the profile flag followed by the URL"
                );
                assert!(
                    !args.iter().any(|a| a == "--new-window"),
                    "a plain profile must not add --new-window"
                );
            }
            other => panic!("expected LaunchApplication, got {other:?}"),
        }
    }

    #[test]
    fn profile_strategy_is_new_instance() {
        let p = LaunchPlan::from_spec(
            &spec(
                "com.google.Chrome",
                &["--profile-directory=Default"],
                false,
                true,
            ),
            "https://x/",
        );
        assert_eq!(p.strategy(), "launch_new_instance");
    }

    #[test]
    fn incognito_and_new_window_flags_pass_through() {
        let p = LaunchPlan::from_spec(
            &spec(
                "com.google.Chrome",
                &["--incognito", "--new-window"],
                false,
                true,
            ),
            "https://x/",
        );
        let LaunchPlan::LaunchApplication {
            args, new_instance, ..
        } = p
        else {
            panic!("expected LaunchApplication");
        };
        assert!(new_instance);
        assert!(args.iter().any(|a| a == "--incognito"));
        assert!(args.iter().any(|a| a == "--new-window"));
        assert_eq!(args.last().unwrap(), "https://x/", "URL is always last");
    }

    #[test]
    fn args_without_new_instance_is_launch_application() {
        let p = LaunchPlan::from_spec(
            &spec("org.mozilla.firefox", &["-P", "work"], false, false),
            "https://x/",
        );
        assert_eq!(p.strategy(), "launch_application");
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
pub(crate) struct UserFn {
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

pub(crate) enum Matcher {
    Always,
    Regex(Regex),
    Domain(Vec<String>),
    From(Vec<String>),
    Running(Vec<String>),
    Fn(UserFn),
}

pub(crate) enum Rewriter {
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

pub(crate) enum Target {
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
pub(crate) struct FnMatcherRun {
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

pub(crate) struct Rule {
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

pub(crate) struct RewriteRule {
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

    /// True when `options.logRequests` is enabled for the active config.
    pub fn request_logging_enabled(&self) -> bool {
        self.options.log_requests
    }

    /// Ensure the active request log exists and return its path.
    ///
    /// Returns `Ok(None)` when request logging is disabled. The explicit
    /// create lets the menu open an empty log before the first URL arrives
    /// without changing the normal lazy-open behavior.
    pub fn ensure_request_log(&self) -> std::io::Result<Option<std::path::PathBuf>> {
        let mut writer_ref = self.log_writer.borrow_mut();
        let Some(writer) = writer_ref.as_mut() else {
            return Ok(None);
        };
        writer.ensure_file().map(Some)
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

#[cfg(test)]
mod tests;

#[cfg(test)]
mod integration_tests;
