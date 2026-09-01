// Auto-split from the former monolithic engine.rs. Child of `engine`, so
// `use super::*;` pulls in the shared types, std imports, and the sibling
// modules' items that `engine` re-exports via `pub(crate) use`.
use super::*;

/// Subcommand-style flag families: a per-browser-family flag that we wire
/// up on demand from a single boolean on the config side. Keeps the
/// flag-vs-family table out of `parse_browser_jsval` so adding a new
/// flag is a one-line change here.
enum FlagFamily {
    Incognito,
    NewWindow,
}

/// Map a family-aware boolean flag (`incognito: true`, `openInNewWindow:
/// true`) to the actual CLI arg the chosen browser expects, or `None` if
/// the browser family doesn't support it (Safari for either; both flags
/// rely on browsers honouring command-line flags, which Safari doesn't).
///
/// All of these flags need `creates_new_instance: true` for the same
/// reason `profile:` does — without it, LaunchServices routes the URL
/// into the existing window via Apple Events, where the flag is
/// silently ignored.
fn expand_flag_for_family(bundle_id: &str, flag: FlagFamily) -> Option<&'static str> {
    let chromium = crate::chromium::is_chromium(bundle_id);
    let firefox = crate::firefox::is_firefox(bundle_id);
    match (flag, chromium, firefox) {
        (FlagFamily::Incognito, true, _) => Some("--incognito"),
        (FlagFamily::Incognito, _, true) => Some("--private-window"),
        // Modern Firefox (>= 89) recognises `--new-window` alongside the
        // legacy `-new-window`; both create a new top-level window in
        // the same instance.
        (FlagFamily::NewWindow, true, _) | (FlagFamily::NewWindow, _, true) => Some("--new-window"),
        _ => None,
    }
}

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
    if let Some(rest) = s.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}/{rest}");
    }
    s.to_string()
}

pub(crate) fn parse_browser_jsval(v: &JSValue) -> BrowserSpec {
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
    if let Some(t) = key(v, "appType").and_then(|x| js_to_string(&x))
        && t == "none"
    {
        return BrowserSpec::empty();
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

    // `incognito: true` — append the browser-family-specific private-mode
    // flag. Same `creates_new_instance` reasoning as `profile:`: without
    // it, LaunchServices routes the URL into the existing window and the
    // flag is silently ignored on Chromium and Firefox.
    if let Some(incognito) = key(v, "incognito").map(|b| unsafe { b.toBool() })
        && incognito
    {
        if let Some(flag) = expand_flag_for_family(&bundle_id, FlagFamily::Incognito) {
            args.push(flag.to_string());
            creates_new_instance = true;
        } else {
            // Fires for Safari AND for any bundle ID outside Grinch's
            // Chromium/Firefox tables (path-form specs, less-common
            // forks, typos). Naming the bundle in the message makes
            // it actionable for the typo case.
            eprintln!(
                "grinch: ignoring `incognito: true` for {bundle_id} — no \
                     CLI private-mode flag known for this browser family \
                     (supported: Chromium, Firefox)"
            );
        }
    }

    // `openInNewWindow: true` — force a new top-level window in the
    // running browser instead of opening as a tab. Distinct from
    // `creates_new_instance` (which spawns a fresh application process,
    // used for profile routing). Same family-flag pattern as incognito.
    if let Some(new_window) = key(v, "openInNewWindow").map(|b| unsafe { b.toBool() })
        && new_window
    {
        if let Some(flag) = expand_flag_for_family(&bundle_id, FlagFamily::NewWindow) {
            args.push(flag.to_string());
            creates_new_instance = true;
        } else {
            eprintln!(
                "grinch: ignoring `openInNewWindow: true` for {bundle_id} — \
                     no CLI new-window flag known for this browser family \
                     (supported: Chromium, Firefox)"
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
pub(crate) fn resolve_browser(
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
