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
    ("com.google.Chrome",            "Google/Chrome"),
    ("com.google.Chrome.canary",     "Google/Chrome Canary"),
    ("com.google.Chrome.beta",       "Google/Chrome Beta"),
    ("com.google.Chrome.dev",        "Google/Chrome Dev"),
    ("com.brave.Browser",            "BraveSoftware/Brave-Browser"),
    ("com.brave.Browser.beta",       "BraveSoftware/Brave-Browser-Beta"),
    ("com.brave.Browser.nightly",    "BraveSoftware/Brave-Browser-Nightly"),
    ("com.microsoft.edgemac",        "Microsoft Edge"),
    ("com.microsoft.edgemac.Beta",   "Microsoft Edge Beta"),
    ("com.microsoft.edgemac.Dev",    "Microsoft Edge Dev"),
    ("com.vivaldi.Vivaldi",          "Vivaldi"),
    ("org.chromium.Chromium",        "Chromium"),
    ("company.thebrowser.Browser",   "Arc/User Data"), // Arc
    ("com.operasoftware.Opera",      "com.operasoftware.Opera"),
];

pub fn is_chromium(bundle_id: &str) -> bool {
    CHROMIUM_FAMILY.iter().any(|(b, _)| *b == bundle_id)
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
    let mut out = NameMap::new();
    let Some(path) = local_state_path(bundle_id) else {
        return out;
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return out;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) else {
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

    // Pass-through: caller already gave us a directory name.
    if map.values().any(|d| d == profile) {
        return profile.to_string();
    }

    // Display-name lookup.
    if let Some(dir) = map.get(profile) {
        return dir.clone();
    }

    profile.to_string()
}
