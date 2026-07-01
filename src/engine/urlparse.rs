// Auto-split from the former monolithic engine.rs. Child of `engine`, so
// `use super::*;` pulls in the shared types, std imports, and the sibling
// modules' items that `engine` re-exports via `pub(crate) use`.
use super::*;

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
