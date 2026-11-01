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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::engine::DiagnosticLog;

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
    // Zen Browser stores profiles in `~/Library/Application Support/zen/`
    // with the same profiles.ini layout as Firefox.
    ("app.zen-browser.zen", "zen"),
];

pub fn is_firefox(bundle_id: &str) -> bool {
    FIREFOX_FAMILY.iter().any(|(b, _)| *b == bundle_id)
}

/// Iterate the Firefox-family `(bundle_id, data_dir)` tuples. Used by
/// the JS bridge's `getRunningBrowsers` helper to intersect against the
/// running-apps snapshot in a stable order.
pub fn iter_family() -> impl Iterator<Item = &'static (&'static str, &'static str)> {
    FIREFOX_FAMILY.iter()
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

/// Cache of `{bundle_id → set of profile names}`, cleared on config reload. profiles.ini
/// is small and resolution is rare (config-load only) but caching keeps
/// repeated lookups for the same browser cheap.
/// Read failures are cached as empty sets too, including for dynamic targets.
/// ponytail: validation retries on Reload Config, not on each click after a transient failure.
static CACHE: OnceLock<std::sync::Mutex<std::collections::HashMap<String, HashSet<String>>>> =
    OnceLock::new();
#[derive(Default)]
struct GroupProfiles {
    profiles: HashMap<String, Vec<PathBuf>>,
    incomplete: bool,
}
static GROUP_CACHE: OnceLock<std::sync::Mutex<HashMap<String, GroupProfiles>>> = OnceLock::new();

/// Discard cached names so the next lookup rereads profile data.
pub fn clear_profile_cache() {
    if let Some(cache) = CACHE.get() {
        cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
    if let Some(cache) = GROUP_CACHE.get() {
        cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

fn store_ids(content: &str) -> Result<HashSet<String>, ()> {
    let mut ids = HashSet::new();
    let mut in_profile = false;
    for line in content.lines().map(str::trim) {
        if line.starts_with('[') && line.ends_with(']') {
            in_profile = line.starts_with("[Profile");
        } else if in_profile && let Some((key, value)) = line.split_once('=') {
            let value = value.trim();
            if key.trim() == "StoreID" && !value.is_empty() {
                if !value.bytes().all(|b| b.is_ascii_alphanumeric()) {
                    return Err(());
                }
                ids.insert(value.to_string());
            }
        }
    }
    Ok(ids)
}

fn read_group_store(path: &Path, root: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    std::fs::File::open(path)
        .map_err(|error| format!("{error}. {}", crate::engine::profile_access_advice(&error)))?;
    let output = std::process::Command::new("/usr/bin/sqlite3")
        .args([
            "-batch",
            "-readonly",
            "-json",
            "-init",
            "/dev/null",
            "-cmd",
            ".timeout 500",
        ])
        .arg(path)
        .arg("SELECT name, path FROM Profiles ORDER BY id;")
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    if output.stdout.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())?;
    let mut profiles = Vec::new();
    for row in rows {
        if let (Some(name), Some(path)) = (row["name"].as_str(), row["path"].as_str())
            && !name.is_empty()
            && !path.is_empty()
        {
            profiles.push((name.to_string(), root.join(path)));
        }
    }
    Ok(profiles)
}

fn load_group_profiles(bundle_id: &str, diagnostics: &DiagnosticLog) -> GroupProfiles {
    let Some(ini) = profiles_ini_path(bundle_id) else {
        return GroupProfiles::default();
    };
    let Some(root) = ini.parent() else {
        return GroupProfiles::default();
    };
    let directory = root.join("Profile Groups");
    let ids = match std::fs::read_to_string(&ini) {
        Ok(content) => match store_ids(&content) {
            Ok(ids) => ids,
            Err(()) => {
                diagnostics.record_profile_warning(
                    bundle_id,
                    Some(&ini),
                    "Invalid Firefox profile group ID; check profiles.ini and Reload Config",
                );
                return GroupProfiles {
                    incomplete: true,
                    ..Default::default()
                };
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return GroupProfiles::default();
        }
        Err(_) => {
            return GroupProfiles {
                incomplete: true,
                ..Default::default()
            };
        }
    };
    let mut profiles = GroupProfiles::default();
    let paths: Vec<PathBuf> = if ids.is_empty() {
        match std::fs::read_dir(&directory) {
            Ok(entries) => entries
                .filter_map(|entry| match entry {
                    Ok(entry) => Some(entry.path()),
                    Err(_) => {
                        profiles.incomplete = true;
                        None
                    }
                })
                .filter(|path| path.extension().is_some_and(|ext| ext == "sqlite"))
                .collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                profiles.incomplete = true;
                diagnostics.record_profile_warning(
                    bundle_id,
                    Some(&directory),
                    &format!(
                        "Cannot read Firefox profile groups: {error}. {}",
                        crate::engine::profile_access_advice(&error)
                    ),
                );
                Vec::new()
            }
        }
    } else {
        ids.iter()
            .map(|id| directory.join(format!("{id}.sqlite")))
            .collect()
    };
    for path in paths {
        match read_group_store(&path, root) {
            Ok(rows) => {
                for (name, path) in rows {
                    let entries = profiles.profiles.entry(name).or_default();
                    if !entries.contains(&path) {
                        entries.push(path);
                    }
                }
            }
            Err(error) => {
                profiles.incomplete = true;
                diagnostics.record_profile_warning(
                    bundle_id,
                    Some(&path),
                    &format!(
                        "Cannot read Firefox profile group: {error}; check access and Reload Config"
                    ),
                );
            }
        }
    }
    profiles
}

pub fn resolve_profile_args(
    bundle_id: &str,
    profile: &str,
    diagnostics: &DiagnosticLog,
) -> Result<Vec<String>, ()> {
    let expanded = if let Some(tail) = profile.strip_prefix("~/") {
        let Some(home) = std::env::var_os("HOME") else {
            diagnostics.record_profile_error(
                bundle_id,
                None,
                "Cannot expand Firefox profile path: HOME is not set",
            );
            return Err(());
        };
        Some(PathBuf::from(home).join(tail))
    } else {
        Path::new(profile)
            .is_absolute()
            .then(|| PathBuf::from(profile))
    };
    if let Some(path) = expanded {
        return profile_path_args(bundle_id, &path, diagnostics);
    }
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    let legacy = cache
        .entry(bundle_id.to_string())
        .or_insert_with(|| load_profile_names(bundle_id, diagnostics));
    if legacy.contains(profile) {
        return Ok(vec!["-P".to_string(), profile.to_string()]);
    }
    drop(cache);
    let groups = GROUP_CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut groups = groups.lock().unwrap_or_else(|e| e.into_inner());
    let group = groups
        .entry(bundle_id.to_string())
        .or_insert_with(|| load_group_profiles(bundle_id, diagnostics));
    if group.incomplete {
        diagnostics.record_profile_error(bundle_id, None, "Cannot safely select Firefox profile because profile groups could not be read; check access and Reload Config");
        return Err(());
    }
    let paths = group.profiles.get(profile).cloned();
    drop(groups);
    if let Some(paths) = paths {
        if paths.len() != 1 {
            diagnostics.record_profile_error(
                bundle_id,
                None,
                &format!(
                    "Firefox profile {profile:?} matches multiple profiles; use its full path"
                ),
            );
            return Err(());
        }
        return profile_path_args(bundle_id, &paths[0], diagnostics);
    }
    Ok(vec![
        "-P".to_string(),
        resolve_profile_name(bundle_id, profile, diagnostics),
    ])
}

fn profile_path_args(
    bundle_id: &str,
    path: &Path,
    diagnostics: &DiagnosticLog,
) -> Result<Vec<String>, ()> {
    if !path.is_dir() {
        diagnostics.record_profile_error(bundle_id, Some(path), "Firefox profile directory is missing or inaccessible; check the path and access, then Reload Config");
        return Err(());
    }
    Ok(vec![
        "--profile".to_string(),
        path.to_string_lossy().into_owned(),
    ])
}

fn load_profile_names(bundle_id: &str, diagnostics: &DiagnosticLog) -> HashSet<String> {
    let Some(path) = profiles_ini_path(bundle_id) else {
        diagnostics.record_profile_warning(
            bundle_id,
            None,
            "Cannot locate profiles.ini; check HOME.",
        );
        return HashSet::new();
    };
    match std::fs::read_to_string(&path) {
        Ok(content) => parse_profile_names(&content),
        Err(error) => {
            diagnostics.record_profile_warning(
                bundle_id,
                Some(&path),
                &format!(
                    "Cannot read {}: {error}; passing profile names through to Firefox. {}",
                    path.display(),
                    crate::engine::profile_access_advice(&error)
                ),
            );
            HashSet::new()
        }
    }
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
        // Lenient `Name = …` parsing: real profiles.ini files (written by
        // various Firefox forks and migration tools) use mixed spacing
        // around the `=` — `Name=x`, `Name = x`, `Name= x`, `Name =x` all
        // occur. Strict prefix-match on two fixed shapes silently dropped
        // valid entries, which downstream surfaced as the "profile
        // {profile:?} not found in profiles.ini" warning even though it
        // was right there. Split once on `=` and trim both sides.
        if let Some((key, value)) = line.split_once('=')
            && key.trim() == "Name"
        {
            out.insert(value.trim().to_string());
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
pub fn resolve_profile_name(bundle_id: &str, profile: &str, diagnostics: &DiagnosticLog) -> String {
    let mutex = CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut cache = mutex.lock().unwrap_or_else(|e| e.into_inner());
    let names = cache
        .entry(bundle_id.to_string())
        .or_insert_with(|| load_profile_names(bundle_id, diagnostics));

    if names.is_empty() || names.contains(profile) {
        return profile.to_string();
    }

    let known: Vec<&str> = names.iter().map(String::as_str).collect();
    diagnostics.record_profile_warning(
        bundle_id,
        profiles_ini_path(bundle_id).as_deref(),
        &format!(
            "Firefox profile {profile:?} not found in profiles.ini \
                  (known profiles: {known:?}); passing through to Firefox unchanged"
        ),
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
    fn parse_profile_names_accepts_mixed_spacing_around_equals() {
        // Real-world profiles.ini files (Firefox forks, migration tools)
        // use mixed spacing. Strict `Name=` and `Name = ` prefix matching
        // dropped valid entries. Verify `Name= x`, `Name =x`, tabs all work.
        let content = "[Profile0]\n\
                       Name= no-space-before\n\
                       [Profile1]\n\
                       Name =no-space-after\n\
                       [Profile2]\n\
                       Name\t=\ttab-padded\n";
        let names = parse_profile_names(content);
        assert!(names.contains("no-space-before"));
        assert!(names.contains("no-space-after"));
        assert!(names.contains("tab-padded"));
        assert_eq!(names.len(), 3);
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
        assert!(is_firefox("app.zen-browser.zen"));
    }

    #[test]
    fn is_firefox_rejects_non_firefox() {
        assert!(!is_firefox("com.google.Chrome"));
        assert!(!is_firefox("com.apple.Safari"));
        assert!(!is_firefox(""));
    }
}
