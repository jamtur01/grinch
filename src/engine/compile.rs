// Auto-split from the former monolithic engine.rs. Child of `engine`, so
// `use super::*;` pulls in the shared types, std imports, and the sibling
// modules' items that `engine` re-exports via `pub(crate) use`.
use super::*;

pub(crate) fn parse_rule_array(
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
            if !items_key.is_empty()
                && let Some(arr) = key(v, items_key)
            {
                let items = js_array_to_strings(&arr).join(",");
                return format!("{t}:{items}");
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
pub(crate) fn describe_target(t: &Target) -> String {
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

pub(crate) fn parse_rewrite_array(arr: &JSValue, function_ctor: &JSValue) -> Vec<RewriteRule> {
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

pub(crate) fn compile_rewriter(v: &JSValue, function_ctor: &JSValue) -> Option<Rewriter> {
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
            if let Some(item) = js_array_at(v, i)
                && let Some(m) = compile_matcher(&item, regexp_ctor, function_ctor)
            {
                ms.push(m);
            }
        }
        return ms;
    }
    compile_matcher(v, regexp_ctor, function_ctor)
        .map(|m| vec![m])
        .unwrap_or_default()
}

pub(crate) fn compile_matcher(
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
        if let Some(t) = key(v, "__type")
            && !unsafe { t.isUndefined() }
            && let Some(name) = js_to_string(&t)
        {
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
        // Regex literal /.../ — compile via the regex crate. Honour the JS
        // RegExp's `ignoreCase` (`i`) and `multiline` (`m`) flags. Finicky
        // matches via native RegExp.test on url.href, which respects all the
        // flags the user wrote; mirror that. Earlier versions of Grinch
        // forced case-insensitive matching, which was a silent semantic
        // divergence from Finicky and from JS's own `.test()` behaviour.
        if is_instance_of(v, regexp_ctor)
            && let Some(pattern) = key(v, "source").and_then(|p| js_to_string(&p))
        {
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
pub(crate) fn compile_wildcard(pattern: &str) -> Option<Regex> {
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

pub(crate) fn pattern_has_protocol_prefix(pat: &str) -> bool {
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
pub(crate) fn build_fn_matcher_runs(ctx: &JSContext, rules: &[Rule]) -> Vec<FnMatcherRun> {
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
                            // Report through the app-owned diagnostic sink;
                            // this catch prevents JSC's exception handler from
                            // seeing batched matcher failures.
                            try {
                                if (typeof __grinchRuntimeError === "function") {
                                    var message = String(e);
                                    if (e && e.line) message += " (line " + e.line + ")";
                                    __grinchRuntimeError(message);
                                }
                            } catch (_) {}
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
