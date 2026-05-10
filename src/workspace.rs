// Thin AppKit/Foundation glue: anything that touches NSWorkspace / NSEvent
// lives here so the engine stays framework-free.

use std::collections::{HashMap, HashSet};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send, MainThreadMarker};
use objc2_app_kit::{
    NSWorkspace, NSWorkspaceDidLaunchApplicationNotification,
    NSWorkspaceDidTerminateApplicationNotification, NSWorkspaceOpenConfiguration,
};
use objc2_application_services::{
    kAXTrustedCheckOptionPrompt, AXError, AXIsProcessTrusted, AXIsProcessTrustedWithOptions,
    AXUIElement,
};
use objc2_core_foundation::{kCFBooleanTrue, CFDictionary, CFRetained, CFString, CFType};
use objc2_foundation::{
    NSActivityOptions, NSArray, NSBundle, NSNotification, NSProcessInfo, NSString, NSURL,
};

// Raw FFI for CGEventSourceFlagsState. The objc2-core-graphics 0.3.2 crate
// declares a dependency on objc2-metal 0.3.2 which isn't published, so we
// can't use the crate-provided binding. CoreGraphics is already linked via
// objc2-application-services (which transitively pulls in core-graphics for
// AX types), so the symbol is available at link time.
const KCG_EVENT_SOURCE_STATE_COMBINED_SESSION_STATE: i32 = 0;
// Bit positions match Apple's CGEventTypes.h kCGEventFlagMask* constants.
const KCG_EVENT_FLAG_MASK_ALPHA_SHIFT: u64 = 1 << 16; // Caps Lock
const KCG_EVENT_FLAG_MASK_SHIFT: u64 = 1 << 17;
const KCG_EVENT_FLAG_MASK_CONTROL: u64 = 1 << 18;
const KCG_EVENT_FLAG_MASK_ALTERNATE: u64 = 1 << 19; // Option
const KCG_EVENT_FLAG_MASK_COMMAND: u64 = 1 << 20;
const KCG_EVENT_FLAG_MASK_SECONDARY_FN: u64 = 1 << 23; // Fn / Globe

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventSourceFlagsState(state_id: i32) -> u64;
}

use crate::engine::{BrowserSpec, ModifierFlags};

#[derive(Clone, Debug, Default)]
pub struct Opener {
    pub bundle_id: String,
    pub name: String,
    pub path: String,
    pub pid: i32,
}

pub fn running_app_bundle_ids() -> HashSet<String> {
    let workspace = NSWorkspace::sharedWorkspace();
    let apps = workspace.runningApplications();
    let count = apps.count();
    let mut out = HashSet::with_capacity(count);
    for i in 0..count {
        let app = apps.objectAtIndex(i);
        if let Some(id) = app.bundleIdentifier() {
            out.insert(id.to_string());
        }
    }
    out
}

/// Process-wide cached snapshot of running app bundle IDs. The snapshot
/// stays valid until the next NSWorkspace launch/terminate notification
/// invalidates it (see `install_running_apps_observer`). Without the
/// observer (e.g., in tests, before app start), every call rebuilds.
///
/// Returned as `Arc` so the per-resolve cache can hold a reference that
/// outlives any concurrent invalidation without copying the underlying
/// `HashSet` (typically 50–200 entries).
static RUNNING_APPS_CACHE: Mutex<Option<Arc<HashSet<String>>>> = Mutex::new(None);

/// Process-wide cached `bundle_id` → `app bundle path` lookups. Each miss
/// hits LaunchServices via `URLForApplicationWithBundleIdentifier` (an
/// XPC round-trip to `lsd`, ~50–500 µs depending on warmth); the cache
/// turns subsequent clicks at the same browser into a HashMap probe.
///
/// Stored as `Option<String>` so a negative result ("browser not found")
/// is also remembered — re-querying for a missing bundle ID on every
/// click would otherwise burn the same XPC roundtrip indefinitely.
///
/// Invalidated alongside `RUNNING_APPS_CACHE` by the NSWorkspace
/// launch/terminate observer. Strictly speaking the bundle URL only
/// changes when an app is moved/installed/uninstalled, but the
/// launch/terminate signal catches "newly installed and launched"
/// without extra observers — the rarer move/uninstall cases will fix
/// themselves on the next NSWorkspace event.
static BUNDLE_URL_CACHE: Mutex<Option<HashMap<String, Option<String>>>> = Mutex::new(None);

pub fn running_apps_cached() -> Arc<HashSet<String>> {
    if let Some(c) = RUNNING_APPS_CACHE.lock().unwrap().as_ref() {
        return c.clone();
    }
    // Fetch outside the lock — runningApplications() can stall briefly under
    // memory pressure and we don't want to serialise other readers behind it.
    let fresh = Arc::new(running_app_bundle_ids());
    let mut g = RUNNING_APPS_CACHE.lock().unwrap();
    if let Some(c) = g.as_ref() {
        return c.clone();
    }
    *g = Some(fresh.clone());
    fresh
}

/// Look up the on-disk URL of an app bundle by ID, hitting LaunchServices
/// only on a cache miss. Returns `None` when no app with that bundle ID
/// is installed (cached as well, so we don't re-query for missing apps).
fn resolved_app_url(bundle_id: &str) -> Option<Retained<NSURL>> {
    {
        let cache = BUNDLE_URL_CACHE.lock().unwrap();
        if let Some(map) = cache.as_ref() {
            if let Some(hit) = map.get(bundle_id) {
                return hit
                    .as_ref()
                    .map(|p| NSURL::fileURLWithPath(&NSString::from_str(p)));
            }
        }
    }
    let workspace = NSWorkspace::sharedWorkspace();
    let bundle_ns = NSString::from_str(bundle_id);
    let url = workspace.URLForApplicationWithBundleIdentifier(&bundle_ns);
    let path = url.as_ref().and_then(|u| u.path()).map(|s| s.to_string());
    {
        let mut cache = BUNDLE_URL_CACHE.lock().unwrap();
        cache
            .get_or_insert_with(HashMap::new)
            .insert(bundle_id.to_string(), path);
    }
    url
}

fn invalidate_caches() {
    *RUNNING_APPS_CACHE.lock().unwrap() = None;
    *BUNDLE_URL_CACHE.lock().unwrap() = None;
}

/// Register a process-lifetime "user-initiated" activity with
/// NSProcessInfo so AppNap doesn't suspend Grinch when it's been idle.
/// LSUIElement apps (no Dock tile, no key window) are AppNap's prime
/// target; without this, a long-idle Grinch can pay an extra 20–100 ms
/// of resume latency on the first click after suspension.
///
/// Uses `UserInitiatedAllowingIdleSystemSleep` — keeps the process
/// schedulable, but doesn't prevent the user's display/system from
/// sleeping. The returned activity token must outlive the process; we
/// leak it intentionally.
///
/// Idempotent: repeated calls are no-ops.
pub fn defeat_app_nap() {
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let info = NSProcessInfo::processInfo();
    let reason = NSString::from_str("Grinch routes URLs on demand and must respond to clicks");
    let token = info.beginActivityWithOptions_reason(
        NSActivityOptions::UserInitiatedAllowingIdleSystemSleep,
        &reason,
    );
    std::mem::forget(token);
}

/// Install NSWorkspace launch/terminate observers that invalidate the
/// `running_apps_cached` snapshot. Idempotent — repeated calls are no-ops.
/// The observer tokens and block are leaked intentionally; the observers
/// must live for the duration of the process.
pub fn install_running_apps_observer() {
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let workspace = NSWorkspace::sharedWorkspace();
    let nc = workspace.notificationCenter();
    let block = RcBlock::new(|_n: NonNull<NSNotification>| {
        invalidate_caches();
    });
    unsafe {
        let t1 = nc.addObserverForName_object_queue_usingBlock(
            Some(NSWorkspaceDidLaunchApplicationNotification),
            None,
            None,
            &block,
        );
        let t2 = nc.addObserverForName_object_queue_usingBlock(
            Some(NSWorkspaceDidTerminateApplicationNotification),
            None,
            None,
            &block,
        );
        std::mem::forget(t1);
        std::mem::forget(t2);
        std::mem::forget(block);
    }
}

/// True if any running app's bundle identifier OR localized name matches
/// `id`. Matches Finicky's `finicky.isAppRunning` semantics where
/// `isAppRunning("Slack")` works as well as
/// `isAppRunning("com.tinyspeck.slackmacgap")`.
///
/// The bundle-ID + display-name dual-check makes this slightly more
/// expensive than the bundle-only `running_app_bundle_ids` walk used by
/// the declarative `running()` matcher — we read both fields per app
/// rather than collecting one and intersecting. Worth it: most users
/// reach for the friendlier display-name form.
pub fn is_app_running(id: &str) -> bool {
    let workspace = NSWorkspace::sharedWorkspace();
    let apps = workspace.runningApplications();
    let count = apps.count();
    for i in 0..count {
        let app = apps.objectAtIndex(i);
        if let Some(bundle) = app.bundleIdentifier() {
            if bundle.to_string() == id {
                return true;
            }
        }
        if let Some(name) = app.localizedName() {
            if name.to_string() == id {
                return true;
            }
        }
    }
    false
}

pub fn frontmost_opener() -> Opener {
    let workspace = NSWorkspace::sharedWorkspace();
    let Some(app) = workspace.frontmostApplication() else {
        return Opener::default();
    };
    let bundle_id = app
        .bundleIdentifier()
        .map(|s| s.to_string())
        .unwrap_or_default();
    let name = app
        .localizedName()
        .map(|s| s.to_string())
        .unwrap_or_default();
    let path = app
        .executableURL()
        .and_then(|u| u.path())
        .map(|s| s.to_string())
        .unwrap_or_default();
    let pid = app.processIdentifier();
    Opener {
        bundle_id,
        name,
        path,
        pid,
    }
}

/// Bundle-id-only variant of [`frontmost_opener`]. Used when the config
/// has `from()` matchers but no fn matchers/rewrites/targets that read
/// `ctx.opener` — skips `localizedName` and `executableURL` IPC entirely
/// (each is a LaunchServices round-trip). pid stays 0 since the AX-based
/// `windowTitle` block can't fire from a config that doesn't read ctx.
pub fn frontmost_opener_id() -> Opener {
    let workspace = NSWorkspace::sharedWorkspace();
    let Some(app) = workspace.frontmostApplication() else {
        return Opener::default();
    };
    let bundle_id = app
        .bundleIdentifier()
        .map(|s| s.to_string())
        .unwrap_or_default();
    Opener {
        bundle_id,
        ..Opener::default()
    }
}

/// Check whether Grinch.app has been granted Accessibility permission, and
/// if not, ask the system to show its standard prompt to the user. Returns
/// the trusted state at the time of the call (which is unaffected by the
/// prompt — granting takes effect only after the user enables it in System
/// Settings, and Grinch sees the change only on its next AX call).
///
/// Safe to call at every launch. The OS only shows the prompt the first
/// time per app per session; subsequent calls with the prompt option are
/// no-ops if the user has already been asked. Granting is persistent.
pub fn ensure_accessibility_permission() -> bool {
    if unsafe { AXIsProcessTrusted() } {
        return true;
    }
    // Build options = { "AXTrustedCheckOptionPrompt": kCFBooleanTrue }.
    let key: &CFString = unsafe { kAXTrustedCheckOptionPrompt };
    let value = unsafe { kCFBooleanTrue }.expect("kCFBooleanTrue");
    let mut keys: [*const std::ffi::c_void; 1] = [key as *const _ as *const _];
    let mut values: [*const std::ffi::c_void; 1] = [value as *const _ as *const _];
    let dict = unsafe {
        CFDictionary::new(
            None,
            keys.as_mut_ptr(),
            values.as_mut_ptr(),
            1,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    let dict = dict.expect("CFDictionary::new returned NULL");
    unsafe { AXIsProcessTrustedWithOptions(Some(&dict)) }
}

/// Fetch the focused window title for a given pid via the Accessibility API.
/// Returns an empty string on any failure (no permission, app dead, no window).
/// First failure with permission-denied semantics emits a one-time stderr hint.
pub fn frontmost_window_title(pid: i32) -> String {
    static WARNED: AtomicBool = AtomicBool::new(false);

    if pid == 0 {
        return String::new();
    }

    // SAFETY: AXUIElement::new_application is documented to handle invalid
    // pids by returning a valid element that fails subsequent calls.
    let app = unsafe { AXUIElement::new_application(pid) };

    let focused_attr = CFString::from_str("AXFocusedWindow");
    let title_attr = CFString::from_str("AXTitle");

    let window = match copy_attribute(&app, &focused_attr) {
        Ok(v) => v,
        Err(err) => {
            if (err == AXError::APIDisabled || err == AXError::NotImplemented)
                && !WARNED.swap(true, Ordering::Relaxed)
            {
                eprintln!(
                    "grinch: opener.windowTitle requires Accessibility permission. \
                     Grant it in System Settings → Privacy & Security → Accessibility, \
                     then add Grinch.app to the list."
                );
            }
            return String::new();
        }
    };

    // window is a CFType holding a window AXUIElement.
    let window_ax: &AXUIElement = unsafe { &*(&*window as *const CFType as *const AXUIElement) };

    let title = match copy_attribute(window_ax, &title_attr) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };

    // title is a CFString-typed CFType.
    let cf_str: &CFString = unsafe { &*(&*title as *const CFType as *const CFString) };
    cf_str.to_string()
}

/// Wrapper around AXUIElementCopyAttributeValue that returns Result.
fn copy_attribute(
    element: &AXUIElement,
    attribute: &CFString,
) -> Result<CFRetained<CFType>, AXError> {
    let mut out: *const CFType = std::ptr::null();
    let err = unsafe {
        element.copy_attribute_value(
            attribute,
            NonNull::new_unchecked(&mut out as *mut *const CFType),
        )
    };
    if err != AXError::Success {
        return Err(err);
    }
    let Some(non_null) = NonNull::new(out as *mut CFType) else {
        return Err(AXError::Failure);
    };
    Ok(unsafe { CFRetained::from_raw(non_null) })
}

/// Read the current modifier-key state. We use CGEventSourceFlagsState rather
/// than [NSEvent modifierFlags] because Grinch is `LSUIElement` (background,
/// no Dock icon, never activated) — its NSEvent queue is never populated, so
/// `[NSEvent modifierFlags]` reliably returns 0 regardless of what the user
/// is holding. CGEventSourceFlagsState reads global session-wide state, which
/// works in any process.
pub fn current_modifier_flags() -> ModifierFlags {
    let flags = unsafe { CGEventSourceFlagsState(KCG_EVENT_SOURCE_STATE_COMBINED_SESSION_STATE) };
    flags_from_mask(flags)
}

/// Pure decoder for CG event flag bitmasks. Pulled out of
/// `current_modifier_flags` so the bit-position mapping is testable —
/// transposing Option ↔ Alternate is the kind of mistake that's invisible
/// in code review but routes URLs to the wrong browser when held.
fn flags_from_mask(flags: u64) -> ModifierFlags {
    ModifierFlags {
        shift: flags & KCG_EVENT_FLAG_MASK_SHIFT != 0,
        option: flags & KCG_EVENT_FLAG_MASK_ALTERNATE != 0,
        command: flags & KCG_EVENT_FLAG_MASK_COMMAND != 0,
        control: flags & KCG_EVENT_FLAG_MASK_CONTROL != 0,
        caps_lock: flags & KCG_EVENT_FLAG_MASK_ALPHA_SHIFT != 0,
        function: flags & KCG_EVENT_FLAG_MASK_SECONDARY_FN != 0,
    }
}

/// Resolve a user-provided browser identifier (which may be an app display
/// name like "Google Chrome" or a reverse-DNS bundle ID like "com.google.Chrome")
/// to a canonical bundle ID. Returns the input unchanged if no app is found —
/// the caller decides whether to warn or fall back.
///
/// Result is cached: LaunchServices calls
/// (URLForApplicationWithBundleIdentifier + fullPathForApplication) are
/// ~50µs each and dominate the slow path when dynamic `open` fns return
/// browser identifiers per click. Cache lookup is a single HashMap probe
/// under a Mutex.
pub fn resolve_browser_identifier(name: &str) -> String {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, String>>> =
        std::sync::OnceLock::new();
    let mutex = CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    {
        let cache = mutex.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(hit) = cache.get(name) {
            return hit.clone();
        }
    }

    let resolved = resolve_browser_identifier_uncached(name);
    mutex
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(name.to_string(), resolved.clone());
    resolved
}

fn resolve_browser_identifier_uncached(name: &str) -> String {
    let workspace = NSWorkspace::sharedWorkspace();
    let name_ns = NSString::from_str(name);

    // Already a bundle ID? URLForApplicationWithBundleIdentifier returns Some
    // when an app with that ID is installed.
    if workspace
        .URLForApplicationWithBundleIdentifier(&name_ns)
        .is_some()
    {
        return name.to_string();
    }

    // Try as an app display name. fullPathForApplication is deprecated since
    // 10.15 but is the only API that resolves a name without LaunchServices
    // gymnastics; it still works on macOS 13+.
    #[allow(deprecated)]
    if let Some(path) = workspace.fullPathForApplication(&name_ns) {
        let url = NSURL::fileURLWithPath(&path);
        if let Some(bundle) = NSBundle::bundleWithURL(&url) {
            if let Some(id) = bundle.bundleIdentifier() {
                return id.to_string();
            }
        }
    }

    // Couldn't resolve — fall back to the input string. NSWorkspace.open will
    // emit a "browser not found" warning if it really doesn't exist.
    name.to_string()
}

/// Resolve a filesystem path to an `.app` bundle ID. Used by browser specs
/// that declare `appType: "path"` (e.g. `name: "/Applications/MyBrowser.app"`)
/// — useful for browsers that aren't installed in `/Applications` or aren't
/// registered with LaunchServices yet. Returns the input unchanged if the
/// bundle can't be opened or has no `CFBundleIdentifier`, so the eventual
/// open call gets something to work with (probably failing visibly rather
/// than silently).
/// Read NSHost's user-friendly + canonical machine identity.
/// Returns `(localized_name, name)` — equivalent to
/// `[[NSHost currentHost] localizedName]` and `[currentHost name]`.
///
/// The two values differ when the user has set a "Computer Name" in
/// System Settings → General → About: localizedName follows that
/// (e.g. "James's MacBook Pro"), while `name` is the canonical
/// hostname (e.g. "jamtur01-mbp"). On a fresh install both are the
/// same. Empty strings if NSHost yields nil for either field.
pub fn host_info() -> (String, String) {
    unsafe {
        let cls = class!(NSHost);
        let host: *mut AnyObject = msg_send![cls, currentHost];
        if host.is_null() {
            return (String::new(), String::new());
        }
        let localized: *mut NSString = msg_send![&*host, localizedName];
        let canonical: *mut NSString = msg_send![&*host, name];
        let l = if localized.is_null() {
            String::new()
        } else {
            (*localized).to_string()
        };
        let c = if canonical.is_null() {
            String::new()
        } else {
            (*canonical).to_string()
        };
        (l, c)
    }
}

pub fn resolve_browser_path(path: &str) -> String {
    let path_ns = NSString::from_str(path);
    let url = NSURL::fileURLWithPath(&path_ns);
    if let Some(bundle) = NSBundle::bundleWithURL(&url) {
        if let Some(id) = bundle.bundleIdentifier() {
            return id.to_string();
        }
    }
    eprintln!("grinch: couldn't load bundle at path {path}");
    path.to_string()
}

/// Open `url` in the given browser app. If the bundle ID is empty (suppress),
/// do nothing. If the bundle ID is unknown, fall back to the user's actual
/// default browser via NSWorkspace.
pub fn open_url(url: &str, spec: &BrowserSpec, mtm: MainThreadMarker) {
    let _ = mtm;
    if spec.bundle_id.is_empty() {
        return;
    }
    let workspace = NSWorkspace::sharedWorkspace();
    let url_ns = NSURL::URLWithString(&NSString::from_str(url));
    let Some(url_ns) = url_ns else {
        eprintln!("grinch: invalid URL: {url}");
        return;
    };

    let Some(app_url) = resolved_app_url(&spec.bundle_id) else {
        // Suppress instead of falling back to NSWorkspace.openURL with no
        // app: when Grinch is the system default browser (the expected
        // setup), that call dispatches the URL right back to Grinch via
        // Apple Events, the same routing fires, and we loop indefinitely
        // through the OS until something kills the process. A clear
        // error + dropped URL is strictly safer than a runaway loop —
        // the user can fix their config and click again.
        static WARNED: AtomicBool = AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::Relaxed) {
            eprintln!(
                "grinch: browser not found ({}); URL dropped. Edit your config \
                 to reference an installed browser (bundle ID or app name).",
                spec.bundle_id
            );
        } else {
            eprintln!("grinch: browser not found ({}); URL dropped", spec.bundle_id);
        }
        return;
    };

    let cfg = NSWorkspaceOpenConfiguration::configuration();
    if spec.open_in_background {
        cfg.setActivates(false);
    }
    if spec.creates_new_instance {
        cfg.setCreatesNewApplicationInstance(true);
    }

    // Two-API split, matching Finicky's launcher_native.m:
    //
    // - When custom args are present (profile flags, --incognito, etc.) we
    //   call openApplicationAtURL:configuration: and pass the URL as the
    //   LAST element of configuration.arguments. This is a *launch* call:
    //   the args reach Chrome's command-line parser, so --profile-directory
    //   actually takes effect.
    //
    // - When there are no args, openURLs:withApplicationAtURL:configuration:
    //   is fine — LaunchServices routes the URL via the running instance,
    //   which is what we want for the simple case.
    //
    // Using openURLs: with profile args was the bug behind held-shift not
    // routing to the Convergint profile: Chrome receives the URL through
    // its URL-handling path (Apple Event GURL), where --profile-directory
    // is silently ignored.
    if !spec.args.is_empty() {
        // Per-click NSString allocs (one per static arg + the URL). Pre-caching
        // the static-arg NSStrings on BrowserSpec was considered but rejected:
        // wall-clock here is dominated by the openApplicationAtURL IPC (low
        // milliseconds), and humans click links on the order of 1–100/day, so
        // the saving (a few hundred ns × N args) doesn't show up against
        // millisecond IPC. Not worth the duplicated args/args_ns state.
        let mut all_args: Vec<&str> = spec.args.iter().map(|s| s.as_str()).collect();
        all_args.push(url);
        let args_ns: Vec<Retained<NSString>> =
            all_args.iter().map(|s| NSString::from_str(s)).collect();
        let arr = NSArray::from_retained_slice(&args_ns);
        cfg.setArguments(&arr);
        workspace.openApplicationAtURL_configuration_completionHandler(&app_url, &cfg, None);
    } else {
        let urls = NSArray::from_retained_slice(&[url_ns]);
        workspace.openURLs_withApplicationAtURL_configuration_completionHandler(
            &urls, &app_url, &cfg, None,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_from_mask_zero_means_no_modifiers() {
        let m = flags_from_mask(0);
        assert!(!m.shift && !m.option && !m.command && !m.control);
        assert!(!m.caps_lock && !m.function);
    }

    #[test]
    fn flags_from_mask_decodes_each_bit_individually() {
        let m = flags_from_mask(KCG_EVENT_FLAG_MASK_SHIFT);
        assert!(m.shift && !m.option && !m.command && !m.control);

        let m = flags_from_mask(KCG_EVENT_FLAG_MASK_ALTERNATE);
        assert!(!m.shift && m.option && !m.command && !m.control);

        let m = flags_from_mask(KCG_EVENT_FLAG_MASK_COMMAND);
        assert!(!m.shift && !m.option && m.command && !m.control);

        let m = flags_from_mask(KCG_EVENT_FLAG_MASK_CONTROL);
        assert!(!m.shift && !m.option && !m.command && m.control);

        let m = flags_from_mask(KCG_EVENT_FLAG_MASK_ALPHA_SHIFT);
        assert!(m.caps_lock && !m.shift && !m.function);

        let m = flags_from_mask(KCG_EVENT_FLAG_MASK_SECONDARY_FN);
        assert!(m.function && !m.caps_lock && !m.shift);
    }

    #[test]
    fn flags_from_mask_decodes_combinations() {
        let m = flags_from_mask(KCG_EVENT_FLAG_MASK_SHIFT | KCG_EVENT_FLAG_MASK_COMMAND);
        assert!(m.shift && m.command);
        assert!(!m.option && !m.control);
    }

    #[test]
    fn flags_from_mask_ignores_irrelevant_high_bits() {
        // CG events carry other flag bits we don't surface — bit 21 is the
        // help-key mask, bit 24+ are device-specific. They shouldn't flip
        // our fields.
        let unrelated = (1u64 << 21) | (1u64 << 24);
        let m = flags_from_mask(unrelated);
        assert!(!m.shift && !m.option && !m.command && !m.control);
        assert!(!m.caps_lock && !m.function);
    }

    #[test]
    fn flags_from_mask_alternate_is_option_not_some_other_thing() {
        // Regression guard against transposing Option (kCGEventFlagMaskAlternate,
        // bit 19) with Control (bit 18) or Command (bit 20).
        assert_eq!(KCG_EVENT_FLAG_MASK_ALTERNATE, 1u64 << 19);
        assert_eq!(KCG_EVENT_FLAG_MASK_CONTROL, 1u64 << 18);
        assert_eq!(KCG_EVENT_FLAG_MASK_COMMAND, 1u64 << 20);
        assert_eq!(KCG_EVENT_FLAG_MASK_SHIFT, 1u64 << 17);
    }

    #[test]
    fn running_apps_cached_returns_same_arc_until_invalidated() {
        // Reset cache state so we don't depend on test ordering. Other
        // tests may have populated it via direct calls or via engine
        // resolves that touched a `running()` matcher.
        invalidate_caches();
        let a = running_apps_cached();
        let b = running_apps_cached();
        // Same Arc means subsequent reads avoid re-fetching the
        // NSWorkspace snapshot — the win this cache exists to give us.
        assert!(Arc::ptr_eq(&a, &b));
        invalidate_caches();
        let c = running_apps_cached();
        assert!(!Arc::ptr_eq(&a, &c));
    }
}
