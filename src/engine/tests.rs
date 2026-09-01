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
    let unrelated = concat!(
        "https://statics.teams.cdn.office.net/evergreen-assets/other/",
        "?url=https%3A%2F%2Fexample.com"
    );
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
    let wrapped = "https://x@urldefense.proofpoint.com/v2/url?u=https%3A%2F%2Fexample.com%2F&d=tag";
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
    let wrapped =
        "https://emea01.safelinks.protection.outlook.com/?secure&url=https%3A%2F%2Fexample.com%2F";
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
        unwrap_teams_launcher("https://teams.microsoft.com/l/meetup-join/19:meeting_abc").is_none()
    );
    // Unrelated host.
    assert!(
        unwrap_teams_launcher("https://example.com/dl/launcher/launcher.html?url=%2Fl%2Ffoo")
            .is_none()
    );
    // Right host, wrong path.
    assert!(
        unwrap_teams_launcher("https://teams.microsoft.com/other/path?url=%2Fl%2Ffoo").is_none()
    );
}

#[test]
fn teams_launcher_rejects_empty_or_malformed_inner_url() {
    // No `url` param → can't unwrap.
    assert!(
        unwrap_teams_launcher(
            "https://teams.microsoft.com/dl/launcher/launcher.html?type=meetup-join"
        )
        .is_none()
    );
    // Empty `url` param.
    assert!(
        unwrap_teams_launcher("https://teams.microsoft.com/dl/launcher/launcher.html?url=")
            .is_none()
    );
    // Malformed percent escape — decoder bails.
    assert!(
        unwrap_teams_launcher("https://teams.microsoft.com/dl/launcher/launcher.html?url=%ZZ")
            .is_none()
    );
}

#[test]
fn proofpoint_v3_unwraps_empty_marker() {
    // No special chars in the original → empty marker stream → the
    // encoded payload IS the original URL verbatim. The simplest v3
    // shape.
    let wrapped = "https://urldefense.com/v3/__https://example.com/path__;!!tracker-data$";
    assert_eq!(
        unwrap_safelink(wrapped).as_deref(),
        Some("https://example.com/path")
    );
}

#[test]
fn proofpoint_v3_unwraps_single_star_substitution() {
    // One special char (`?`) replaced with a single `*`. Marker
    // stream is base64-URL-encoded `?` (0x3F) = "Pw".
    let wrapped = "https://urldefense.com/v3/__https://example.com/*__;Pw!!tag$";
    assert_eq!(
        unwrap_safelink(wrapped).as_deref(),
        Some("https://example.com/?")
    );
}

#[test]
fn proofpoint_v3_unwraps_run_length_marker() {
    // Three special chars (`://`) collapsed into `**B` (B=3 in the
    // run-length alphabet). Marker stream "Oi8v" decodes to `://`.
    let wrapped = "https://urldefense.com/v3/__https**Bexample.com/__;Oi8v!!tag$";
    assert_eq!(
        unwrap_safelink(wrapped).as_deref(),
        Some("https://example.com/")
    );
}

#[test]
fn proofpoint_v3_unwraps_urldefense_us_government_host() {
    // FedRAMP tenant uses the same v3 format on `urldefense.us`.
    // Must dispatch through the same decoder.
    let wrapped = "https://urldefense.us/v3/__https://example.com/path__;!!tag$";
    assert_eq!(
        unwrap_safelink(wrapped).as_deref(),
        Some("https://example.com/path")
    );
}

#[test]
fn proofpoint_v3_empty_marker_rejects_literal_star() {
    // Malformed/adversarial: empty marker stream but the encoded URL
    // contains a literal `*`. With no replacement chars to pop, the
    // `*` would survive into the result; reject rather than route a
    // URL with `*` in the host or path.
    assert!(
        unwrap_safelink("https://urldefense.com/v3/__https://exa*mple.com/__;!!tag$").is_none()
    );
}

#[test]
fn proofpoint_v3_rejects_malformed_url() {
    // Missing the `__` delimiters around the encoded payload.
    assert!(unwrap_safelink("https://urldefense.com/v3/not-a-v3-shape").is_none());
    // Missing the marker terminator (`!`).
    assert!(unwrap_safelink("https://urldefense.com/v3/__https://example.com/*__;Pw").is_none());
    // Marker stream exhausted mid-decode (encoded has two `*`s, marker
    // only encodes one byte).
    assert!(
        unwrap_safelink("https://urldefense.com/v3/__https://example.com/**__;Pw!!tag$").is_none()
    );
    // Unknown run-length char (`@` isn't in the alphabet).
    assert!(
        unwrap_safelink("https://urldefense.com/v3/__https**@example.com/__;Oi8v!!tag$").is_none()
    );
    // Invalid base64 in marker.
    assert!(
        unwrap_safelink("https://urldefense.com/v3/__https://example.com/*__;P!@!!tag$").is_none()
    );
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

// -------- base64_decode --------

#[test]
fn base64_url_safe_alphabet_decodes_identically_in_both_modes() {
    // URL-safe chars `-`(62) and `_`(63) are accepted regardless of
    // `accept_standard`. "-_-_" = 111110 111111 111110 111111 →
    // 11111011 11111111 10111111 = [0xFB, 0xFF, 0xBF] in BOTH modes.
    let expected = Some(vec![0xFB, 0xFF, 0xBF]);
    assert_eq!(base64_decode("-_-_", false), expected);
    assert_eq!(base64_decode("-_-_", true), expected);
}

#[test]
fn base64_standard_alphabet_requires_accept_standard() {
    // Load-bearing distinction: `+`(62) and `/`(63) decode ONLY when
    // accept_standard=true. "+/+/" mirrors "-_-_" bit-for-bit, so with
    // the wide alphabet it yields [0xFB, 0xFF, 0xBF]; with the URL-safe
    // alphabet the `+` is an invalid char and the whole input fails.
    assert_eq!(base64_decode("+/+/", true), Some(vec![0xFB, 0xFF, 0xBF]));
    assert_eq!(base64_decode("+/+/", false), None);
    // Single standard chars also gated: `/` alone must not sneak through.
    assert_eq!(base64_decode("//8", false), None);
}

#[test]
fn base64_rejects_malformed_tails_and_accepts_optional_padding() {
    // Input length ≡ 1 mod 4 → a single dangling char encodes no bytes
    // (6 leftover bits); reject in both modes.
    assert_eq!(base64_decode("A", false), None);
    assert_eq!(base64_decode("A", true), None);
    // Non-zero padding bits in the final char: "AB" = 000000 000001 emits
    // 0x00 and leaves 0001 (non-zero) in the padding region → reject.
    assert_eq!(base64_decode("AB", false), None);
    // `=` padding is accepted but optional: "AA" and "AA==" both decode to
    // the same single zero byte.
    assert_eq!(base64_decode("AA", false), Some(vec![0x00]));
    assert_eq!(base64_decode("AA==", false), Some(vec![0x00]));
}

#[test]
fn base64_known_answer_standard_vs_url_safe_divergence() {
    // [0xFF, 0xFF] = 111111 111111 1111(00) → indices 63,63,60. Index 60
    // is the shared digit '8'; index 63 diverges: url-safe `_`, standard
    // `/`. So the two bytes encode to "__8" (url-safe) vs "//8" (standard).
    assert_eq!(base64_decode("__8", false), Some(vec![0xFF, 0xFF]));
    assert_eq!(base64_decode("//8", true), Some(vec![0xFF, 0xFF]));
    assert_eq!(base64_decode("//8", false), None);
}
