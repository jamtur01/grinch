// Firefox profile resolution.
//
// Firefox stores its profile inventory in `profiles.ini` (a flat
// INI file) under each app's per-app Application Support folder. The
// canonical command-line form is `firefox -P <Name>`, where `<Name>` is
// the user-visible profile name listed in profiles.ini under a
// `[Profile<n>]` section's `Name=` key.
//
// Unlike Chromium (where there's a separate display name vs on-disk
// directory), Firefox uses a single name end-to-end. So the only thing
// this module does is *validate* that the user-supplied profile name
// actually exists in profiles.ini and warn loudly when it doesn't —
// passing an unknown profile to `firefox -P` would silently open the
// profile-manager UI, which a user routing URLs would experience as
// "the link did nothing".

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::OnceLock;

/// Single source of truth for the Firefox-family bundle IDs Grinch knows
/// about, plus their per-app data directory under
/// `~/Library/Application Support/`. Mirrors `chromium::CHROMIUM_FAMILY`
/// in shape so the two modules stay easy to compare.
const FIREFOX_FAMILY: &[(&str, &str)] = &[
    ("org.mozilla.firefox", "Firefox"),
    ("org.mozilla.firefoxdeveloperedition", "Firefox"),
    ("org.mozilla.nightly", "Firefox"),
    // LibreWolf and Waterfox are Firefox forks that ship with the same
    // profiles.ini layout under their own per-app dirs.
    ("net.waterfox.waterfox", "Waterfox"),
    ("io.gitlab.librewolf-community", "LibreWolf"),
];

pub fn is_firefox(bundle_id: &str) -> bool {
    FIREFOX_FAMILY.iter().any(|(b, _)| *b == bundle_id)
}

fn data_dir(bundle_id: &str) -> Option<&'static str> {
    FIREFOX_FAMILY
        .iter()
        .find(|(b, _)| *b == bundle_id)
        .map(|(_, d)| *d)
}

fn profiles_ini_path(bundle_id: &str) -> Option<PathBuf> {
    let dir = data_dir(bundle_id)?;
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(format!(
        "{home}/Library/Application Support/{dir}/profiles.ini"
    )))
}

/// Per-process cache of `{bundle_id → set of profile names}`. profiles.ini
/// is small and resolution is rare (config-load only) but caching keeps
/// repeated lookups for the same browser cheap.
static CACHE: OnceLock<std::sync::Mutex<std::collections::HashMap<String, HashSet<String>>>> =
    OnceLock::new();

fn load_profile_names(bundle_id: &str) -> HashSet<String> {
    let Some(path) = profiles_ini_path(bundle_id) else {
        return HashSet::new();
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return HashSet::new();
    };
    parse_profile_names(&content)
}

/// Pure helper: extract profile names from a `profiles.ini` body. Walks
/// the file line by line, tracking whether we're inside a `[Profile<n>]`
/// section, and pushes any `Name=...` value found inside one.
///
/// We deliberately skip `[Install...]` and `[General]` sections — they
/// don't list profiles, and an `Install` section's `Default=Profiles/...`
/// value is a relative directory path, not a profile name.
fn parse_profile_names(content: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut in_profile_section = false;
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            // Section headers like [Profile0], [Profile1], [General], [InstallXXX].
            in_profile_section = line.starts_with("[Profile");
            continue;
        }
        if !in_profile_section {
            continue;
        }
        if let Some(value) = line
            .strip_prefix("Name=")
            .or_else(|| line.strip_prefix("Name = "))
        {
            out.insert(value.to_string());
        }
    }
    out
}

/// Resolve a user-supplied `profile` value to a Firefox profile name
/// suitable for `-P`.
///
/// - If the value matches a profile in profiles.ini, return as-is.
/// - If it doesn't (or profiles.ini can't be read), warn and return as-is
///   so Firefox can either find it (e.g. profile created since Grinch
///   loaded the config) or surface its own error.
pub fn resolve_profile_name(bundle_id: &str, profile: &str) -> String {
    let mutex = CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut cache = mutex.lock().unwrap_or_else(|e| e.into_inner());
    let names = cache
        .entry(bundle_id.to_string())
        .or_insert_with(|| load_profile_names(bundle_id));

    if names.is_empty() || names.contains(profile) {
        return profile.to_string();
    }

    let known: Vec<&str> = names.iter().map(String::as_str).collect();
    eprintln!(
        "grinch: Firefox profile {profile:?} not found in profiles.ini for {bundle_id} \
         (known profiles: {known:?}); passing through to Firefox unchanged"
    );
    profile.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Sample shaped after Firefox's actual profiles.ini layout. The keys
    // we care about are [Profile<n>] sections with a Name= line.
    const SAMPLE_PROFILES_INI: &str = r#"[Install4F96D1932A9F858E]
Default=Profiles/abcd1234.default-release
Locked=1

[Profile2]
Name=dev-edition-default
IsRelative=1
Path=Profiles/wxyz5678.dev-edition-default

[Profile1]
Name=Work
IsRelative=1
Path=Profiles/qrst9012.Work

[Profile0]
Name=default-release
IsRelative=1
Path=Profiles/abcd1234.default-release
Default=1

[General]
StartWithLastProfile=1
Version=2
"#;

    #[test]
    fn parse_profile_names_extracts_all_profile_sections() {
        let names = parse_profile_names(SAMPLE_PROFILES_INI);
        assert_eq!(names.len(), 3);
        assert!(names.contains("default-release"));
        assert!(names.contains("Work"));
        assert!(names.contains("dev-edition-default"));
    }

    #[test]
    fn parse_profile_names_ignores_install_and_general_sections() {
        // Install section has Default=Profiles/... which is NOT a Name=
        // line and shouldn't be picked up. General has Version=2 etc.
        let names = parse_profile_names(SAMPLE_PROFILES_INI);
        assert!(!names.contains("Profiles/abcd1234.default-release"));
        assert!(!names.contains("2"));
    }

    #[test]
    fn parse_profile_names_handles_empty_and_garbage() {
        assert!(parse_profile_names("").is_empty());
        assert!(parse_profile_names("not an ini file").is_empty());
        assert!(parse_profile_names("[General]\nVersion=2\n").is_empty());
    }

    #[test]
    fn parse_profile_names_skips_comments_and_blank_lines() {
        let content = r#"
# leading comment
[Profile0]
; semicolon comment
Name=My Profile
"#;
        let names = parse_profile_names(content);
        assert_eq!(names.len(), 1);
        assert!(names.contains("My Profile"));
    }

    #[test]
    fn is_firefox_recognises_known_browsers() {
        assert!(is_firefox("org.mozilla.firefox"));
        assert!(is_firefox("org.mozilla.firefoxdeveloperedition"));
        assert!(is_firefox("org.mozilla.nightly"));
        assert!(is_firefox("net.waterfox.waterfox"));
    }

    #[test]
    fn is_firefox_rejects_non_firefox() {
        assert!(!is_firefox("com.google.Chrome"));
        assert!(!is_firefox("com.apple.Safari"));
        assert!(!is_firefox(""));
    }
}
