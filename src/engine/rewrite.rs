// Auto-split from the former monolithic engine.rs. Child of `engine`, so
// `use super::*;` pulls in the shared types, std imports, and the sibling
// modules' items that `engine` re-exports via `pub(crate) use`.
use super::*;

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
pub(crate) fn unwrap_safelink(url: &str) -> Option<String> {
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
pub(crate) fn unwrap_teams_launcher(url: &str) -> Option<String> {
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

    // Proofpoint URL Defense v3 lives at a different host
    // (`urldefense.com`, not `urldefense.proofpoint.com`) AND uses a
    // completely different URL shape — the encoded URL is in the path
    // between `__` markers, not in a query param. Dispatch it on its
    // own branch before the param-based unwrap below.
    //
    //   https://urldefense.com/v3/__<encoded>__;<base64-marker>!![tracker]$
    //
    // The `<encoded>` portion is the original URL with most non-ASCII
    // characters replaced by `*` placeholders (or `**X` run-length
    // markers); `<base64-marker>` is a URL-safe base64 stream of the
    // replacement characters in left-to-right order.
    // urldefense.com is the public Proofpoint v3 host; urldefense.us is the
    // FedRAMP / US-government tenant that uses the identical v3 format on a
    // different domain. Both dispatch to the same decoder.
    if host.as_ref() == "urldefense.com" || host.as_ref() == "urldefense.us" {
        return unwrap_proofpoint_v3(url);
    }

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

/// Decode a Proofpoint URL Defense v3 URL into the original target URL.
///
/// v3 wraps the original URL inside a path like:
///   `https://urldefense.com/v3/__<encoded>__;<b64-marker>!![tracker]$`
///
/// where `<encoded>` is the original URL with most special characters
/// replaced by `*` placeholders (or `**X` run-length markers for
/// repeated runs of 2–65 substitutions), and `<b64-marker>` is a
/// URL-safe base64 stream of the replacement characters in
/// left-to-right substitution order.
///
/// The single-`*` form takes one char from the replacement stream; the
/// `**X` form takes N chars, where X maps to a length via
/// `proofpoint_v3_run_length` (A=2, B=3, …, -=64, _=65). Returns None
/// for any structural failure (unparseable path, malformed base64,
/// exhausted replacement stream, unknown run-length char) so the
/// rewriter passes the URL through unchanged.
///
/// **ASCII-only scope.** The full Proofpoint decoder handles multi-byte
/// UTF-8 sequences with a `save_bytes` carry-over for replacement runs
/// that cross 65-byte segment boundaries; that's only relevant when the
/// original URL contains non-ASCII chars in the host or path (rare for
/// browser-router workloads, since URLs are typically ASCII). The
/// implementation here treats the replacement stream as Unicode chars
/// and pops one char per byte of run-length; it produces wrong output
/// for the rare non-ASCII case, which fails `looks_like_url` and falls
/// through as a pass-through — never the wrong-URL-routed-as-clean
/// outcome.
fn unwrap_proofpoint_v3(url: &str) -> Option<String> {
    // Extract `<encoded>` between `__` markers and `<b64-marker>` between
    // `__;` and the first `!`. The trailing `!![tracker]$` is opaque.
    let body_start = url.find("/v3/__")?;
    let after_open = &url[body_start + 6..];
    let body_end = after_open.find("__;")?;
    let encoded = &after_open[..body_end];
    let after_sep = &after_open[body_end + 3..];
    // Marker terminates at the first `!`. `!!` and `$` follow.
    let marker_end = after_sep.find('!')?;
    let marker_b64 = &after_sep[..marker_end];

    // Empty marker → the encoded URL has no `*` placeholders, it's the
    // real URL verbatim. Reject if the encoded part still contains `*`
    // (malformed Proofpoint output, or an unrelated v3-shaped attack
    // string with no marker stream): with no replacement chars to pop,
    // those `*`s would survive into the result and `looks_like_url`
    // doesn't validate that hosts are `*`-free.
    if marker_b64.is_empty() {
        if encoded.contains('*') {
            return None;
        }
        return if looks_like_url(encoded) {
            Some(encoded.to_string())
        } else {
            None
        };
    }
    let replacement_bytes = base64_url_decode(marker_b64)?;
    let replacement = String::from_utf8(replacement_bytes).ok()?;
    let mut chars = replacement.chars();

    let mut result = String::with_capacity(encoded.len() + replacement.len());
    let bytes = encoded.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'*' {
            // `**X` (run-length) vs single `*`. The `**` literal needs
            // at least two `*` bytes; if a third byte exists and is a
            // valid run-length marker, consume the run. Otherwise it's
            // a one-char substitution.
            if i + 2 < bytes.len() && bytes[i + 1] == b'*' {
                let n = proofpoint_v3_run_length(bytes[i + 2])?;
                for _ in 0..n {
                    result.push(chars.next()?);
                }
                i += 3;
            } else {
                result.push(chars.next()?);
                i += 1;
            }
        } else {
            // Non-ASCII bytes in the encoded path are unusual but
            // technically valid UTF-8. Push them verbatim — they're
            // already in their final form, no replacement needed.
            result.push(b as char);
            i += 1;
        }
    }
    if looks_like_url(&result) {
        Some(result)
    } else {
        None
    }
}

/// Map a Proofpoint v3 `**X` run-length marker character to the number
/// of replacement bytes it represents. Alphabet matches the canonical
/// decoder (cardi/proofpoint-url-decoder): A=2, B=3, …, Z=27, a=28, …,
/// z=53, 0=54, …, 9=63, -=64, _=65.
fn proofpoint_v3_run_length(b: u8) -> Option<usize> {
    match b {
        b'A'..=b'Z' => Some((b - b'A') as usize + 2),
        b'a'..=b'z' => Some((b - b'a') as usize + 28),
        b'0'..=b'9' => Some((b - b'0') as usize + 54),
        b'-' => Some(64),
        b'_' => Some(65),
        _ => None,
    }
}

/// Decode base64 into raw bytes. `accept_standard` widens the alphabet to
/// also accept the standard RFC 4648 §4 chars `+` and `/` alongside the
/// URL-safe §5 chars `-` and `_`; when false, only the URL-safe pair is
/// accepted (a `+`/`/` then fails as an invalid char). Padding (`=`) is
/// accepted but optional in both modes.
///
/// Strict on malformed tails: returns None on any invalid character, on a
/// trailing 6-bit leftover (input length ≡ 1 mod 4 after pad strip — a
/// single dangling char that encodes no bytes), or on non-zero padding
/// bits in the final char (an encoder smuggling data into the padding
/// region). This strictness is what stops a marker like `https://exa*.com`
/// from round-tripping through a permissive decoder.
pub(crate) fn base64_decode(s: &str, accept_standard: bool) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4 + 1);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for c in s.bytes() {
        let v: u32 = match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a') as u32 + 26,
            b'0'..=b'9' => (c - b'0') as u32 + 52,
            b'-' => 62,
            b'_' => 63,
            b'+' if accept_standard => 62,
            b'/' if accept_standard => 63,
            b'=' => continue,
            _ => return None,
        };
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1u32 << bits) - 1;
        }
    }
    // Valid base64 leaves 0, 2, or 4 leftover bits after byte emission
    // (input lengths 4n, 4n+3, 4n+2 chars respectively, padding stripped).
    // 6 leftover bits means input length 4n+1 — a single dangling char that
    // encodes no bytes; malformed, so fail rather than emit wrong output.
    if bits == 6 {
        return None;
    }
    // Leftover bits MUST be zero — they're the padding bits of the final
    // encoded char. A legitimate encoder always emits zero padding bits.
    if buf != 0 {
        return None;
    }
    Some(out)
}

/// URL-safe base64 decode (RFC 4648 §5: `-`/`_`, no `+`/`/`). Used for
/// Proofpoint v3 markers, which are always URL-safe. Thin wrapper over
/// [`base64_decode`] in strict URL-safe-only mode.
fn base64_url_decode(s: &str) -> Option<Vec<u8>> {
    base64_decode(s, false)
}

pub(crate) fn find_query_param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
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
