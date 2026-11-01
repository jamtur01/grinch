// Auto-split from the former monolithic engine.rs. Child of `engine`, so
// `use super::*;` pulls in the shared types, std imports, and the sibling
// modules' items that `engine` re-exports via `pub(crate) use`.
use super::*;

pub(crate) const RULE_KEYS: &[&str] = &["match", "open", "browser", "url", "name"];

pub(crate) fn parse_rule_array(
    array: (&JSValue, usize),
    browsers: &std::collections::HashMap<String, Rc<BrowserSpec>>,
    constructors: (&JSValue, &JSValue),
    diagnostics: &DiagnosticLog,
    checks: &crate::loader::KeyCheckers,
) -> Result<Vec<Rule>, EngineError> {
    let (regexp_ctor, function_ctor) = constructors;
    let (arr, count) = array;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let Some(item) = js_array_at(arr, i) else {
            continue;
        };
        let mut misplaced = false;
        for field in unknown_config_keys(&item, checks.rule.as_deref()) {
            if RULE_KEYS.contains(&field.as_str()) {
                continue;
            }
            if [
                "profile",
                "args",
                "incognito",
                "openInNewWindow",
                "openInBackground",
            ]
            .contains(&field.as_str())
            {
                misplaced = true;
                diagnostics.record_suppressing_warning(&format!(
                    "rules[{i}].{field} is ignored; move it inside the browser target; \
                     this rule is suppressed"
                ));
            } else {
                diagnostics
                    .record_config_warning(&format!("unknown rules[{i}].{field} is ignored"));
            }
        }
        let match_val = key(&item, "match");
        // `open` (Grinch) and `browser` (Finicky) are aliases.
        let open_val = key(&item, "open").or_else(|| key(&item, "browser"));
        let url_val = key(&item, "url");
        let matchers = compile_matchers(
            match_val.as_deref(),
            regexp_ctor,
            function_ctor,
            diagnostics,
        )?;
        if match_val
            .as_ref()
            .is_none_or(|value| is_undef_or_null(value))
        {
            diagnostics
                .record_config_warning(&format!("rules[{i}] has no match and will never fire"));
        }

        // Optional per-rule rewriter (combined entry).
        let rewriter = url_val
            .as_ref()
            .and_then(|uv| compile_rewriter(uv, function_ctor));

        // Target: `open: null` → suppress; fn → Fn; resolvable browser → Browser.
        // If `open`/`browser` is absent but a `url` rewrite IS present, that's
        // a pure rewrite-on-match (no routing change) — treat as default-target.
        let target = match open_val.as_ref() {
            Some(_) if misplaced => Target::Suppress,
            Some(ov) if unsafe { ov.isNull() } => Target::Suppress,
            Some(ov) if is_function(ov, function_ctor) => Target::Fn(UserFn::new(ov.clone())),
            Some(ov) => match resolve_browser(ov, browsers, true, diagnostics, checks) {
                // Empty bundle_id = explicit no-op browser (e.g. via
                // `appType: "none"`). Normalise to Target::Suppress so the
                // resolve path's URL handling matches `open: null` exactly,
                // including the "about:blank" Resolution.url.
                Some(b) if b.bundle_id.is_empty() => Target::Suppress,
                Some(b) => Target::Browser(b),
                None => {
                    diagnostics.record_config_warning(&format!(
                        "rules[{i}] has unresolvable `open` (not a string, object, or browser \
                         key) — entry ignored"
                    ));
                    continue;
                }
            },
            None => {
                if rewriter.is_some() {
                    diagnostics.record_config_warning(&format!(
                        "rules[{i}] has `url:` but no `open:` — move it to the top-level \
                         `rewrite:` array if it should apply globally"
                    ));
                } else {
                    diagnostics.record_config_warning(&format!(
                        "rules[{i}] has neither `open` nor `url` — entry ignored"
                    ));
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
    Ok(out)
}

/// Build a human-readable label for a rule's `match:` value at parse time.
/// String / array matchers turn into themselves; `domain()`/`from()`/`running()`
/// objects render as `kind:items`; fn matchers fall back to their source.
fn derive_match_label(v: Option<&JSValue>) -> String {
    const MAX: usize = 80;
    let Some(v) = v else {
        return "(no match)".to_string();
    };
    if is_undef_or_null(v) {
        return "(no match)".to_string();
    }
    if unsafe { v.isString() } {
        return js_to_string(v).unwrap_or_default();
    }
    if unsafe { v.isArray() } {
        let count = config_array_len(v).unwrap_or(0);
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
                let items =
                    js_array_to_strings(&arr, config_array_len(&arr).unwrap_or(0)).join(",");
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

pub(crate) fn parse_rewrite_array(
    arr: &JSValue,
    count: usize,
    regexp_ctor: &JSValue,
    function_ctor: &JSValue,
    diagnostics: &DiagnosticLog,
) -> Result<Vec<RewriteRule>, EngineError> {
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let Some(item) = js_array_at(arr, i) else {
            continue;
        };

        // Bare strip(...) marker (no match field) — treat as "always run".
        if is_marker(&item, "strip") {
            if let Some(r) = compile_strip(&item, diagnostics)? {
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
        let matchers = compile_matchers(
            match_val.as_deref(),
            regexp_ctor,
            function_ctor,
            diagnostics,
        )?;
        let Some(uv) = url_val else { continue };
        let Some(rewriter) = compile_rewriter(&uv, function_ctor) else {
            continue;
        };
        out.push(RewriteRule { matchers, rewriter });
    }
    Ok(out)
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
    diagnostics: &DiagnosticLog,
) -> Result<Vec<Matcher>, EngineError> {
    let Some(v) = v else { return Ok(vec![]) };
    if is_undef_or_null(v) {
        return Ok(vec![]);
    }
    if unsafe { v.isArray() } {
        let count = validate_config_array(v, "match")?;
        let mut ms = Vec::with_capacity(count);
        for i in 0..count {
            if let Some(item) = js_array_at(v, i)
                && let Some(m) = compile_matcher(&item, regexp_ctor, function_ctor, diagnostics)?
            {
                ms.push(m);
            }
        }
        return Ok(ms);
    }
    Ok(compile_matcher(v, regexp_ctor, function_ctor, diagnostics)?
        .map(|m| vec![m])
        .unwrap_or_default())
}

pub(crate) fn compile_matcher(
    v: &JSValue,
    regexp_ctor: &JSValue,
    function_ctor: &JSValue,
    diagnostics: &DiagnosticLog,
) -> Result<Option<Matcher>, EngineError> {
    // String → either a wildcard pattern (if it contains * or /) or a bare
    // hostname shorthand for a domain-and-subdomain match.
    if unsafe { v.isString() } {
        let Some(s) = js_to_string(v) else {
            return Ok(None);
        };
        if is_host_port_pattern(&s) && !s.contains(['*', '/', '?', '#']) {
            let pattern = format!(
                "^(?i-u:https?:|ftp:)?(?://)?{}(?:[/?#].*)?$",
                regex::escape(&s.to_ascii_lowercase())
            );
            return Ok(Regex::new(&pattern).ok().map(Matcher::Wildcard));
        }
        if s.contains('*') || s.contains('/') || is_host_port_pattern(&s) {
            return Ok(compile_wildcard(&s).map(Matcher::Wildcard));
        }
        // ASCII lowercase to match `quick_host`'s lowercasing of the URL's
        // host. URL hostnames are ASCII per the URL spec; using the
        // Unicode-aware to_lowercase() on either side could produce mismatches
        // on IDN inputs.
        return Ok(Some(Matcher::Domain(vec![s.to_ascii_lowercase()])));
    }
    if unsafe { v.isObject() } {
        if let Some(t) = key(v, "__type")
            && !unsafe { t.isUndefined() }
            && let Some(name) = js_to_string(&t)
        {
            match name.as_str() {
                "domain" => {
                    if let Some(arr) = key(v, "hosts") {
                        let count = validate_config_array(&arr, "domain hosts")?;
                        let hosts: Vec<String> = js_array_to_strings(&arr, count)
                            .into_iter()
                            .map(|s| s.to_ascii_lowercase())
                            .collect();
                        return Ok(Some(Matcher::Domain(hosts)));
                    }
                }
                "from" => {
                    if let Some(arr) = key(v, "apps") {
                        let count = validate_config_array(&arr, "from apps")?;
                        return Ok(Some(Matcher::From(js_array_to_strings(&arr, count))));
                    }
                }
                "running" => {
                    if let Some(arr) = key(v, "apps") {
                        let count = validate_config_array(&arr, "running apps")?;
                        return Ok(Some(Matcher::Running(js_array_to_strings(&arr, count))));
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
                Ok(re) => return Ok(Some(Matcher::Regex(re))),
                Err(e) => {
                    // The Rust `regex` crate doesn't speak JS-specific
                    // regex syntax (lookbehinds, `\b` in some contexts).
                    // Silently dropping the matcher meant rules whose
                    // only pattern was a regex would never fire with
                    // no diagnostic. Surface the failure at load time
                    // so users can port the pattern to a supported
                    // form (e.g. wildcards, fn matchers).
                    diagnostics.record_config_warning(&format!(
                        "regex /{pattern}/ failed to compile: {e}. This matcher will not fire; \
                         use a wildcard, domain(), or function matcher"
                    ));
                }
            }
        }
        if is_function(v, function_ctor) {
            return Ok(Some(Matcher::Fn(UserFn::new(v.retain()))));
        }
    }
    Ok(None)
}

fn compile_strip(
    v: &JSValue,
    diagnostics: &DiagnosticLog,
) -> Result<Option<Rewriter>, EngineError> {
    let Some(arr) = key(v, "params") else {
        return Ok(None);
    };
    let count = validate_config_array(&arr, "strip parameters")?;
    let params = js_array_to_strings(&arr, count);
    if params.is_empty() {
        diagnostics.record_config_warning(
            "strip() called with no arguments — rewriter will never strip anything",
        );
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
    Ok(Some(Rewriter::Strip { exact, prefixes }))
}

/// Port of Finicky's `matchWildcard`. Compiles a glob-style pattern to a
/// case-sensitive regex anchored at both ends; URL hostnames are normalized
/// separately. `*` is non-greedy `.*?`;
/// `\*` is a literal asterisk; patterns without a leading protocol/asterisk
/// get an optional `(?:https?:|...)?(?://)?` prefix so e.g. `"zoom.us/j/*"`
/// matches both bare and protocol-prefixed URLs.
pub(crate) fn compile_wildcard(pattern: &str) -> Option<Regex> {
    let primary = wildcard_expression(pattern, false);
    let anchored = if wildcard_scheme_prefix(pattern, true).is_some()
        && wildcard_scheme_prefix(pattern, false).is_none()
    {
        let scheme = wildcard_expression(pattern, true);
        format!("(?:{primary})|(?:{scheme})")
    } else {
        primary
    };
    RegexBuilder::new(&anchored).build().ok()
}

fn wildcard_expression(pattern: &str, allow_ambiguous_scheme: bool) -> String {
    // Private-use codepoint as the "this `*` was escaped" sentinel.
    // Previously U+0000 (NUL), which is a valid char in JS strings — a
    // pattern containing a literal NUL would have been misinterpreted as
    // a `\*`. Unicode private-use characters (U+E000..U+F8FF) are
    // guaranteed not to appear in real-world host patterns.
    const PLACEHOLDER: char = '\u{E000}';

    // Step 1: replace escaped asterisks with a sentinel.
    let mut work = pattern.replace("\\*", &PLACEHOLDER.to_string());
    let wildcard_scheme = wildcard_scheme_prefix(pattern, allow_ambiguous_scheme);
    let has_protocol = wildcard_scheme.is_some()
        || (pattern_has_protocol_prefix(pattern) && !is_host_port_pattern(pattern));
    let host_start = if let Some(index) = wildcard_scheme {
        work[index..].starts_with("://").then_some(index + 3)
    } else if has_protocol {
        work.find(':')
            .filter(|&index| work[index..].starts_with("://"))
            .map(|index| index + 3)
    } else if work.starts_with("*://") {
        Some(4)
    } else if !work.starts_with('*') {
        Some(0)
    } else {
        None
    };
    if let Some(start) = host_start {
        let end = work[start..]
            .find(['/', '?', '#'])
            .map_or(work.len(), |index| start + index);
        let host_start = start + work[start..end].rfind('@').map_or(0, |index| index + 1);
        let host_end = if work[host_start..end].starts_with('[') {
            work[host_start..end]
                .find(']')
                .map_or(end, |index| host_start + index + 1)
        } else {
            work[host_start..end]
                .find(':')
                .map_or(end, |index| host_start + index)
        };
        work[host_start..host_end].make_ascii_lowercase();
    }

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
    let starts_with_protocol = has_protocol;
    if !starts_with_protocol {
        if !pattern.starts_with('*') {
            work = format!("(?i-u:https?:|ftp:|mailto:|file:|tel:|sms:|data:)?(?://)?{work}");
        }
    } else {
        if let Some(index) = work.find(':') {
            let scheme = work[..index].replace('*', "[a-z0-9+._-]{0,}?");
            work = format!("(?i-u:{scheme}){}", &work[index..]);
        }
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
    wildcard_anchor(
        &work,
        !has_protocol && pattern.starts_with('*'),
        pattern.ends_with('*'),
    )
}

fn wildcard_anchor(work: &str, leading_wildcard: bool, trailing_wildcard: bool) -> String {
    if !leading_wildcard {
        return format!("^{work}$");
    }
    let end = [work.find('/'), work.find("\\?"), work.find('#')]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(work.len());
    if !work[..end].bytes().any(|byte| byte.is_ascii_uppercase()) {
        return format!("^{work}$");
    }
    // Preserve case-sensitive path matches while allowing a normalized host.
    let host = work[..end].to_ascii_lowercase().replace(".*?", "[^/?#@]*?");
    let remainder = &work[end..];
    let trailing = if end == work.len() && trailing_wildcard {
        "(?:[/?#].*)?"
    } else {
        ""
    };
    format!("^(?:{work}|(?i-u:[a-z][a-z0-9+.-]*://)?(?:[^/?#]*@)?{host}{remainder}{trailing})$")
}

fn wildcard_scheme_prefix(pattern: &str, allow_ambiguous: bool) -> Option<usize> {
    let index = pattern.find(':')?;
    let scheme = &pattern[..index];
    if !scheme.contains('*') || scheme.is_empty() {
        return None;
    }
    if !scheme
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"+.-_*".contains(&byte))
    {
        return None;
    }
    let tail = &pattern[index + 1..];
    if !allow_ambiguous
        && scheme.starts_with('*')
        && scheme.contains('.')
        && !tail.starts_with("//")
    {
        return None;
    }
    let first = tail.split(['/', '?', '#']).next().unwrap_or("");
    if !allow_ambiguous && !first.is_empty() && first.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(index)
}

fn is_host_port_pattern(pattern: &str) -> bool {
    let Some((host, rest)) = pattern.split_once(':') else {
        return false;
    };
    if !host.eq_ignore_ascii_case("localhost") && !host.contains('.') {
        return false;
    }
    let port = rest.split(['/', '?', '#']).next().unwrap_or("");
    !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit())
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
