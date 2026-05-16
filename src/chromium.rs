// Chromium profile resolution.
//
// Chrome (and other Chromium-family browsers) store profiles in directories
// under their per-app Application Support folder. The directory names —
// "Default", "Profile 1", "Profile 2", etc. — are what `--profile-directory=`
// expects, but Chrome's UI shows profile *display names* like "Personal" or
// "Convergint". A user writing `profile: "Convergint"` in their grinch.js
// reasonably expects the latter.
//
// This module reads `Local State` (Chrome's session-scoped JSON file), looks
// up the display name in `profile.info_cache`, and returns the corresponding
// on-disk directory key. Values are resolved at config load and baked into
// `BrowserSpec`, so the hot path never touches JSON.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

/// Single source of truth for Chromium-family browsers Grinch knows about,
/// plus their per-app data directory under `~/Library/Application Support/`.
/// Both `is_chromium` (recognise the bundle) and `data_dir` (find Local State)
/// consult this table; adding a browser in one place picks it up in both.
const CHROMIUM_FAMILY: &[(&str, &str)] = &[
    ("com.google.Chrome", "Google/Chrome"),
    ("com.google.Chrome.canary", "Google/Chrome Canary"),
    ("com.google.Chrome.beta", "Google/Chrome Beta"),
    ("com.google.Chrome.dev", "Google/Chrome Dev"),
    ("com.brave.Browser", "BraveSoftware/Brave-Browser"),
    ("com.brave.Browser.beta", "BraveSoftware/Brave-Browser-Beta"),
    (
        "com.brave.Browser.nightly",
        "BraveSoftware/Brave-Browser-Nightly",
    ),
    ("com.microsoft.edgemac", "Microsoft Edge"),
    ("com.microsoft.edgemac.Beta", "Microsoft Edge Beta"),
    ("com.microsoft.edgemac.Dev", "Microsoft Edge Dev"),
    ("com.vivaldi.Vivaldi", "Vivaldi"),
    ("org.chromium.Chromium", "Chromium"),
    ("company.thebrowser.Browser", "Arc/User Data"), // Arc
    ("com.operasoftware.Opera", "com.operasoftware.Opera"),
    ("com.operasoftware.OperaGX", "com.operasoftware.OperaGX"),
    ("com.bookry.wavebox", "WaveboxApp"),
    ("net.imput.helium", "net.imput.helium"),
    ("ai.perplexity.comet", "Comet"),
    ("ru.yandex.desktop.yandex-browser", "Yandex/YandexBrowser"),
];

pub fn is_chromium(bundle_id: &str) -> bool {
    CHROMIUM_FAMILY.iter().any(|(b, _)| *b == bundle_id)
}

/// Iterate the Chromium-family `(bundle_id, data_dir)` tuples. Used by
/// the JS bridge's `getRunningBrowsers` helper to intersect against the
/// running-apps snapshot in a stable order.
pub fn iter_family() -> impl Iterator<Item = &'static (&'static str, &'static str)> {
    CHROMIUM_FAMILY.iter()
}

fn data_dir(bundle_id: &str) -> Option<&'static str> {
    CHROMIUM_FAMILY
        .iter()
        .find(|(b, _)| *b == bundle_id)
        .map(|(_, d)| *d)
}

fn local_state_path(bundle_id: &str) -> Option<PathBuf> {
    let dir = data_dir(bundle_id)?;
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(format!(
        "{home}/Library/Application Support/{dir}/Local State"
    )))
}

/// Per-process cache of {bundle_id → {display_name → directory_name}}. Local
/// State is small but parsing JSON has some cost; resolving is rare so a
/// OnceLock-guarded HashMap is enough.
static CACHE: OnceLock<std::sync::Mutex<HashMap<String, NameMap>>> = OnceLock::new();

type NameMap = HashMap<String, String>;

fn load_name_map(bundle_id: &str) -> NameMap {
    let Some(path) = local_state_path(bundle_id) else {
        return NameMap::new();
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return NameMap::new();
    };
    parse_name_map(&content)
}

/// Pure helper: extract {display_name → directory_name} from the JSON contents
/// of Chrome's `Local State`. Returns an empty map on any parse failure or
/// when the expected `profile.info_cache` shape is missing — the resolver
/// falls back to passing the user's value through unchanged.
fn parse_name_map(content: &str) -> NameMap {
    let mut out = NameMap::new();
    let Ok(json) = serde_json::from_str::<serde_json::Value>(content) else {
        return out;
    };
    let Some(info_cache) = json
        .get("profile")
        .and_then(|p| p.get("info_cache"))
        .and_then(|c| c.as_object())
    else {
        return out;
    };
    for (dir_name, info) in info_cache {
        if let Some(display) = info.get("name").and_then(|n| n.as_str()) {
            out.insert(display.to_string(), dir_name.clone());
        }
    }
    out
}

/// Pure helper: resolve a user-supplied `profile` value against a name map.
/// Pulled out of `resolve_profile_dir` so the lookup logic can be tested
/// without faking `HOME` to point at a Chrome data dir.
fn resolve_in_map(profile: &str, map: &NameMap) -> String {
    if map.values().any(|d| d == profile) {
        return profile.to_string();
    }
    if let Some(dir) = map.get(profile) {
        return dir.clone();
    }
    profile.to_string()
}

/// Resolve a user-supplied `profile` value to the on-disk directory name.
///
/// Lookup order:
///   1. If the value matches an entry in `info_cache` (i.e. is itself a
///      directory name like "Profile 10"), use as-is.
///   2. Otherwise, search `info_cache` for an entry whose display `name`
///      matches the value, and return its directory key.
///   3. If nothing matches (or Local State can't be read), return the value
///      unchanged so Chrome will create a fresh profile with that name —
///      the same behaviour as Finicky's fallback.
///
/// The Local State JSON is parsed once per bundle ID and cached. The lookup
/// runs while holding the mutex (no map clone) so dynamic `open` fns that
/// return profile-bearing browser specs don't pay map-copy cost per click.
pub fn resolve_profile_dir(bundle_id: &str, profile: &str) -> String {
    let mutex = CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    // unwrap_or_else recovers from a poisoned mutex; the data inside the
    // HashMap is just plain strings, so reading it after a panic is safe.
    let mut cache = mutex.lock().unwrap_or_else(|e| e.into_inner());
    let map = cache
        .entry(bundle_id.to_string())
        .or_insert_with(|| load_name_map(bundle_id));
    resolve_in_map(profile, map)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Sample shaped after Chrome's actual Local State format. The keys we
    // care about are profile.info_cache.<dir> = { name: <display>, ... }.
    const SAMPLE_LOCAL_STATE: &str = r#"{
        "profile": {
            "info_cache": {
                "Default":   { "name": "Personal", "user_name": "" },
                "Profile 1": { "name": "Work" },
                "Profile 7": { "name": "Convergint" }
            }
        },
        "other_unrelated": { "x": 1 }
    }"#;

    #[test]
    fn parse_name_map_extracts_display_to_dir() {
        let map = parse_name_map(SAMPLE_LOCAL_STATE);
        assert_eq!(map.get("Personal").map(String::as_str), Some("Default"));
        assert_eq!(map.get("Work").map(String::as_str), Some("Profile 1"));
        assert_eq!(map.get("Convergint").map(String::as_str), Some("Profile 7"));
        assert_eq!(map.len(), 3);
    }

    #[test]
    fn parse_name_map_returns_empty_on_garbage_json() {
        assert!(parse_name_map("not json").is_empty());
        assert!(parse_name_map("").is_empty());
        assert!(parse_name_map("{}").is_empty());
        // Right shape, wrong types.
        assert!(parse_name_map(r#"{"profile": {"info_cache": []}}"#).is_empty());
    }

    #[test]
    fn parse_name_map_skips_entries_without_name() {
        let map = parse_name_map(
            r#"{"profile":{"info_cache":{
                "Default": {"user_name": "no display"},
                "Profile 1": {"name": "Work"}
            }}}"#,
        );
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("Work").map(String::as_str), Some("Profile 1"));
    }

    #[test]
    fn resolve_in_map_passes_through_directory_keys() {
        // If the user already gave us a directory key (e.g. "Profile 1"),
        // we should return it unchanged even though no display name maps to it.
        let map = parse_name_map(SAMPLE_LOCAL_STATE);
        assert_eq!(resolve_in_map("Profile 1", &map), "Profile 1");
        assert_eq!(resolve_in_map("Default", &map), "Default");
    }

    #[test]
    fn resolve_in_map_translates_display_names() {
        let map = parse_name_map(SAMPLE_LOCAL_STATE);
        assert_eq!(resolve_in_map("Work", &map), "Profile 1");
        assert_eq!(resolve_in_map("Convergint", &map), "Profile 7");
    }

    #[test]
    fn resolve_in_map_falls_through_unknown() {
        // Unknown values pass through unchanged so Chrome creates a fresh
        // profile with the requested name (Finicky-compatible behaviour).
        let map = parse_name_map(SAMPLE_LOCAL_STATE);
        assert_eq!(resolve_in_map("NotARealProfile", &map), "NotARealProfile");
        assert_eq!(resolve_in_map("", &NameMap::new()), "");
    }

    #[test]
    fn is_chromium_recognises_known_browsers() {
        assert!(is_chromium("com.google.Chrome"));
        assert!(is_chromium("com.brave.Browser"));
        assert!(is_chromium("company.thebrowser.Browser")); // Arc
        assert!(is_chromium("org.chromium.Chromium"));
        // Less-common Chromium forks added so `profile:` shorthand
        // resolves their Local State instead of silently falling through.
        assert!(is_chromium("com.operasoftware.OperaGX"));
        assert!(is_chromium("com.bookry.wavebox"));
        assert!(is_chromium("net.imput.helium"));
        assert!(is_chromium("ai.perplexity.comet"));
        assert!(is_chromium("ru.yandex.desktop.yandex-browser"));
    }

    #[test]
    fn is_chromium_rejects_non_chromium() {
        assert!(!is_chromium("org.mozilla.firefox"));
        assert!(!is_chromium("com.apple.Safari"));
        assert!(!is_chromium(""));
        assert!(!is_chromium("com.google.Chrome.unknown")); // typo'd suffix
    }
}
