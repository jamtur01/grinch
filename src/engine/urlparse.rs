// Auto-split from the former monolithic engine.rs. Child of `engine`, so
// `use super::*;` pulls in the shared types, std imports, and the sibling
// modules' items that `engine` re-exports via `pub(crate) use`.
use super::*;

/// Return the part after an absolute URI's RFC 3986 scheme.
/// Relative inputs must never become browser command-line switches.
pub(crate) fn url_after_scheme(url: &str) -> Option<&str> {
    let (scheme, rest) = url.split_once(':')?;
    let (first, tail) = scheme.as_bytes().split_first()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    for byte in tail {
        if !byte.is_ascii_alphanumeric() && ![b'+', b'-', b'.'].contains(byte) {
            return None;
        }
    }
    Some(rest)
}

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
    // Opaque URIs can contain nested URLs; only this URI's own scheme
    // can introduce an authority. A later `://` belongs to its payload.
    let mut s = url_after_scheme(url)?.strip_prefix("//")?;
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
    let q = url.find(['?', '#'])?;
    if url.as_bytes()[q] != b'?' {
        return None;
    }
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
