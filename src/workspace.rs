// Thin AppKit/Foundation glue: anything that touches NSWorkspace / NSEvent
// lives here so the engine stays framework-free.

use std::collections::HashSet;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};

use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSWorkspace, NSWorkspaceOpenConfiguration};
use objc2_application_services::{
    kAXTrustedCheckOptionPrompt, AXError, AXIsProcessTrusted, AXIsProcessTrustedWithOptions,
    AXUIElement,
};
use objc2_core_foundation::{kCFBooleanTrue, CFDictionary, CFRetained, CFString, CFType};
use objc2_foundation::{NSArray, NSBundle, NSString, NSURL};

// Raw FFI for CGEventSourceFlagsState. The objc2-core-graphics 0.3.2 crate
// declares a dependency on objc2-metal 0.3.2 which isn't published, so we
// can't use the crate-provided binding. CoreGraphics is already linked via
// objc2-application-services (which transitively pulls in core-graphics for
// AX types), so the symbol is available at link time.
const KCG_EVENT_SOURCE_STATE_COMBINED_SESSION_STATE: i32 = 0;
const KCG_EVENT_FLAG_MASK_SHIFT: u64 = 1 << 17;
const KCG_EVENT_FLAG_MASK_CONTROL: u64 = 1 << 18;
const KCG_EVENT_FLAG_MASK_ALTERNATE: u64 = 1 << 19; // Option
const KCG_EVENT_FLAG_MASK_COMMAND: u64 = 1 << 20;

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

pub fn frontmost_opener() -> Opener {
    let workspace = NSWorkspace::sharedWorkspace();
    let Some(app) = workspace.frontmostApplication() else {
        return Opener::default();
    };
    let bundle_id = app.bundleIdentifier().map(|s| s.to_string()).unwrap_or_default();
    let name = app.localizedName().map(|s| s.to_string()).unwrap_or_default();
    let path = app
        .executableURL()
        .and_then(|u| u.path())
        .map(|s| s.to_string())
        .unwrap_or_default();
    let pid = app.processIdentifier();
    Opener { bundle_id, name, path, pid }
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
    ModifierFlags {
        shift:   flags & KCG_EVENT_FLAG_MASK_SHIFT     != 0,
        option:  flags & KCG_EVENT_FLAG_MASK_ALTERNATE != 0,
        command: flags & KCG_EVENT_FLAG_MASK_COMMAND   != 0,
        control: flags & KCG_EVENT_FLAG_MASK_CONTROL   != 0,
    }
}

/// Resolve a user-provided browser identifier (which may be an app display
/// name like "Google Chrome" or a reverse-DNS bundle ID like "com.google.Chrome")
/// to a canonical bundle ID. Returns the input unchanged if no app is found —
/// the caller decides whether to warn or fall back.
///
/// Result is cached: LaunchServices calls (URLForApplicationWithBundleIdentifier
/// + fullPathForApplication) are ~50µs each and dominate the slow path when
/// dynamic `open` fns return browser identifiers per click. Cache lookup
/// is a single HashMap probe under a Mutex.
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
    if workspace.URLForApplicationWithBundleIdentifier(&name_ns).is_some() {
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

    let bundle_id_ns = NSString::from_str(&spec.bundle_id);
    let app_url = workspace.URLForApplicationWithBundleIdentifier(&bundle_id_ns);
    let Some(app_url) = app_url else {
        eprintln!("grinch: browser not found: {}", spec.bundle_id);
        let cfg = NSWorkspaceOpenConfiguration::configuration();
        workspace.openURL_configuration_completionHandler(&url_ns, &cfg, None);
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
