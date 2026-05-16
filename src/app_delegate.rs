use std::cell::RefCell;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::OnceLock;

/// GRINCH_DEBUG=1 enables per-resolve eprintln of opener / modifiers /
/// chosen browser. Read once at startup so we don't pay for `std::env::var`
/// on the click path. Override at launch via env var, no other knob.
fn debug_enabled() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| std::env::var("GRINCH_DEBUG").is_ok())
}

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool};
use objc2::{class, define_class, msg_send, sel, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationDelegate, NSMenu, NSMenuItem, NSSquareStatusItemLength,
    NSStatusBar, NSStatusItem, NSWorkspace,
};
use objc2_core_services::{AEEventClass, AEEventID};
use objc2_foundation::{
    MainThreadMarker, NSAppleEventDescriptor, NSAppleEventManager, NSNotification, NSObject,
    NSObjectProtocol, NSString, NSUserActivity, NSUserActivityTypeBrowsingWeb, NSURL,
};

use crate::engine::{Engine, ModifierFlags};
use crate::loader::{find_config_path, load_config};
use crate::workspace::{
    current_modifier_flags, ensure_accessibility_permission, frontmost_opener, frontmost_opener_id,
    list_http_browsers, open_url, opener_from_pid, Opener,
};

// SMAppService lives in ServiceManagement.framework; not transitively pulled
// in by any other dep so we link it explicitly. Empty extern is enough — we
// reach the Obj-C class via the runtime.
#[link(name = "ServiceManagement", kind = "framework")]
extern "C" {}

// SMAppServiceStatus enum (from ServiceManagement/SMAppService.h).
// 0 = NotRegistered (omitted; falls through the not-Enabled branch).
const SM_STATUS_ENABLED: isize = 1;
const SM_STATUS_REQUIRES_APPROVAL: isize = 2;
const SM_STATUS_NOT_FOUND: isize = 3;

// NSControlStateValue (NSCell.h).
const NS_CONTROL_STATE_VALUE_OFF: isize = 0;
const NS_CONTROL_STATE_VALUE_ON: isize = 1;

/// Apple Event four-char codes are u32s built from four ASCII bytes,
/// big-endian. e.g. `'GURL'` is `0x47 0x55 0x52 0x4c` = `0x4755_524c`.
const fn fourcc(s: &[u8; 4]) -> u32 {
    ((s[0] as u32) << 24) | ((s[1] as u32) << 16) | ((s[2] as u32) << 8) | (s[3] as u32)
}

// Internet Event class + Get URL event ID — both 'GURL'.
const K_INTERNET_EVENT_CLASS: AEEventClass = fourcc(b"GURL");
const K_AE_GET_URL: AEEventID = fourcc(b"GURL");
// Direct-object keyword '----' (the standard "main parameter" key).
const KEY_DIRECT_OBJECT: u32 = fourcc(b"----");
// keySenderPIDAttr ('spid'): the Apple-Event attribute carrying the pid of
// the process that sent the event. Set by LaunchServices when an app calls
// the standard openURL APIs, so it identifies the *real* opener even after
// macOS activates Grinch ahead of our open-URL callback. The frontmost-app
// snapshot can't do that — by the time we read it, Grinch is in front.
// Constant value from CarbonCore/AEDataModel.h; not exposed by objc2 yet.
const KEY_SENDER_PID_ATTR: u32 = fourcc(b"spid");

#[derive(Default)]
pub struct DelegateIvars {
    engine: RefCell<Option<Engine>>,
    status_item: RefCell<Option<Retained<NSStatusItem>>>,
    // Path the loader read (or would read) — kept around so "Open Config"
    // works even when the JS evaluation failed.
    config_path: RefCell<Option<PathBuf>>,
    // Held so `toggle_start_at_login` can flip the checkmark after a
    // successful (un)register.
    start_at_login_item: RefCell<Option<Retained<NSMenuItem>>>,
    // Last reload error message, or None on success. Drives the menu-bar
    // icon (🎄 vs ⚠️) and the disabled "Config error: …" item at the top of
    // the menu. Stderr is `/dev/null` for LaunchServices-launched apps, so
    // without this the user gets no signal that a reload failed.
    load_error: RefCell<Option<String>>,
    // Pre-built menu item that renders `load_error` — hidden when no error.
    error_menu_item: RefCell<Option<Retained<NSMenuItem>>>,
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "GrinchAppDelegate"]
    #[ivars = DelegateIvars]
    pub struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl NSApplicationDelegate for Delegate {
        // Modern unified URL-open hook (macOS 10.13+). Apple routes both
        // URL-scheme events (http://, https://, mailto:) and document-open
        // events (file:// from `open foo.html`) through this method when
        // implemented, replacing the older application:openFile: family.
        #[unsafe(method(application:openURLs:))]
        #[allow(non_snake_case)]
        fn application_openURLs(
            &self,
            _app: &NSApplication,
            urls: &objc2_foundation::NSArray<objc2_foundation::NSURL>,
        ) {
            let count = urls.count();
            if count == 0 {
                return;
            }
            let engine_ref = self.ivars().engine.borrow();
            let Some(engine) = engine_ref.as_ref() else { return };
            // Opener and modifiers are read once per batch, not per-URL.
            // openURLs: groups URLs from the same originating event (one
            // click, one drop, one open-with), so they share an opener and
            // modifier state by intent. Per-URL re-reads would also race
            // against the user releasing the key while the batch resolves.
            let sender_pid = NSAppleEventManager::sharedAppleEventManager()
                .currentAppleEvent()
                .and_then(|e| sender_pid_from_event(&e));
            let opener = resolve_opener(engine, sender_pid);
            let modifiers = if engine.needs_modifiers() { current_modifier_flags() } else { ModifierFlags::default() };
            for i in 0..count {
                let url = urls.objectAtIndex(i);
                let Some(raw) = url.absoluteString() else { continue };
                let raw = raw.to_string();
                let inner = unwrap_grinch_scheme(&raw);
                // If this URL is an auth-session callback that matches
                // a pending ASWebAuthenticationSession request, complete
                // the session and stop — don't also route the callback
                // URL through the rules. (A custom-scheme callback like
                // `slack://oauth-callback?token=…` would otherwise hit
                // the engine and either get dropped or open the wrong
                // browser.)
                if crate::session_handler::try_complete_callback(&inner) {
                    continue;
                }
                let result = engine.resolve(&inner, &opener, modifiers);
                if result.browser.bundle_id.is_empty() {
                    continue;
                }
                open_url(&result.url, &result.browser, self.mtm());
            }
        }

        // Handoff / Universal Links entrypoint. Apple delivers
        // ASWebAuthenticationSession callbacks declared with
        // `callbackWithHTTPSHost:path:` here rather than via the GURL
        // Apple Event path — the system uses the user-activity machinery
        // for https-callback shapes because it can ride the Universal-
        // Links infrastructure (same as web→app handoff for any
        // associated-domain-claimed URL).
        //
        // We check try_complete_callback first; if it returns true the
        // session has been completed by ASWebAuthenticationSession's
        // own completion handler and the URL should not be routed
        // onward. Otherwise we route the webpage URL through engine.
        // resolve like any other web click — preserving the user's
        // routing rules for non-auth Universal Links.
        //
        // The restoration_handler block parameter is required by the
        // protocol signature but we never have anything to restore.
        // Receiving as `*mut AnyObject` keeps us from having to spell
        // out the precise block signature; we just never invoke it.
        #[unsafe(method(application:continueUserActivity:restorationHandler:))]
        #[allow(non_snake_case)]
        fn application_continueUserActivity(
            &self,
            _app: &NSApplication,
            activity: &NSUserActivity,
            _restoration_handler: *mut AnyObject,
        ) -> Bool {
            // We only handle web-page activities (the Universal-Links
            // shape). Other activity types — Handoff for custom data,
            // SiriKit intents — pass through untouched.
            let activity_type = activity.activityType().to_string();
            // SAFETY: NSUserActivityTypeBrowsingWeb is an immortal
            // framework string constant. Deref + to_string is safe.
            let browsing_web = unsafe { NSUserActivityTypeBrowsingWeb }.to_string();
            if activity_type != browsing_web {
                return Bool::NO;
            }
            let Some(url) = activity.webpageURL() else {
                return Bool::NO;
            };
            let Some(href) = url.absoluteString() else {
                return Bool::NO;
            };
            let href = href.to_string();
            if crate::session_handler::try_complete_callback(&href) {
                return Bool::YES;
            }
            // Not an auth callback — route as a regular click.
            let engine_ref = self.ivars().engine.borrow();
            let Some(engine) = engine_ref.as_ref() else {
                return Bool::NO;
            };
            crate::session_handler::forward_through_engine(&href, engine, self.mtm());
            Bool::YES
        }

        // Pre-runloop init. Runs before any URL events can arrive (the
        // runloop hasn't started accepting them yet), which is the earliest
        // we can install handlers in the AppDelegate lifecycle. Two things
        // need that timing:
        //
        // 1. GURL Apple Event handler — legacy URL delivery path for plain
        //    `open https://x/` calls. Needs to be wired before LaunchServices
        //    fires the initial event for the URL that launched us.
        //
        // 2. ASWebAuthenticationSession session handler — macOS can deliver
        //    auth-session requests during launch itself (a third-party app
        //    calls into AS while Grinch is starting). Any window where
        //    `sharedManager.sessionHandler` is nil falls through to Safari,
        //    so installing late means the first SSO popup after launch
        //    silently misroutes. Matches Finicky PR #524's move of
        //    `InstallAuthenticationSessionHandler()` into pre-runloop init.
        //
        // The engine isn't loaded yet (that happens in
        // `did_finish_launching`), so an inbound request during this brief
        // sliver gets dropped silently by `forward_auth_session_url` — same
        // trade-off Finicky accepts. The alternative (load the engine here)
        // would push the menu-bar setup window even further back.
        #[unsafe(method(applicationWillFinishLaunching:))]
        fn will_finish_launching(&self, _notification: &NSNotification) {
            let manager = NSAppleEventManager::sharedAppleEventManager();
            let me: &AnyObject = self.as_ref();
            unsafe {
                manager.setEventHandler_andSelector_forEventClass_andEventID(
                    me,
                    sel!(handleURL:withReplyEvent:),
                    K_INTERNET_EVENT_CLASS,
                    K_AE_GET_URL,
                );
            }
            // DELEGATE_PTR is also set by `install_sighup_handler` later, but
            // the auth-session forwarder reads it earlier — point it at us
            // now so URLs delivered before SIGHUP install can still route.
            let ptr: *const Delegate = self;
            let any_ptr: *mut AnyObject = ptr as *mut AnyObject;
            DELEGATE_PTR.store(any_ptr, Ordering::Relaxed);
            crate::session_handler::install(self.mtm(), forward_auth_session_url);
        }

        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            // CLI short-circuit: --test / --bench load the engine, run the
            // requested command, and terminate. No menu bar, no SIGHUP
            // handler, no Accessibility prompt — none of those are useful
            // for a one-shot CLI invocation, and skipping them avoids the
            // SMAppService / NSStatusBar work (which can stall in headless
            // sandboxes that have no UI session).
            let args: Vec<String> = std::env::args().collect();
            let cli_test = args.iter().position(|a| a == "--test");
            let cli_bench = args.iter().position(|a| a == "--bench");
            let cli_list_rules = args.iter().any(|a| a == "--list-rules");
            let cli_list_browsers = args.iter().any(|a| a == "--list-browsers");
            let cli_validate = args.iter().any(|a| a == "--validate");

            if let Some(idx) = cli_test {
                let Some(url) = args.get(idx + 1) else {
                    // Don't fall through to terminate() with no diagnostic — the
                    // resident default-browser instance would silently exit on a
                    // typo, leaving the user wondering where Grinch went.
                    eprintln!("usage: Grinch --test <url>");
                    std::process::exit(2);
                };
                self.reload_engine();
                self.test_url(url);
                terminate(self.mtm());
                return;
            }
            if let Some(idx) = cli_bench {
                let (Some(n), Some(url)) = (args.get(idx + 1), args.get(idx + 2)) else {
                    eprintln!("usage: Grinch --bench <iterations> <url>");
                    std::process::exit(2);
                };
                let n: usize = n.parse().unwrap_or(10_000);
                self.reload_engine();
                self.bench(n, url);
                terminate(self.mtm());
                return;
            }
            if cli_list_rules {
                self.reload_engine();
                self.list_rules();
                terminate(self.mtm());
                return;
            }
            if cli_list_browsers {
                self.list_browsers();
                terminate(self.mtm());
                return;
            }
            if cli_validate {
                self.reload_engine();
                let code = self.report_validation();
                std::process::exit(code);
            }

            // Normal app-mode startup: kill any stale duplicate instances
            // first so we don't pile up menu bar icons (see Finicky #515),
            // then load config, build the menu bar, wire SIGHUP, install the
            // running-apps cache observer, defeat AppNap so first-click-
            // after-idle stays fast, and ask for Accessibility once. The
            // ASWebAuthenticationSession handler is already installed by
            // `will_finish_launching` so SSO/OAuth popups route from the
            // earliest possible point in the launch sequence.
            crate::workspace::terminate_duplicate_instances();
            self.reload_engine();
            self.setup_menu_bar();
            install_sighup_handler(self);
            crate::workspace::install_running_apps_observer();
            crate::workspace::defeat_app_nap();

            if !ensure_accessibility_permission() {
                eprintln!(
                    "grinch: Accessibility permission not granted yet. \
                     Rules that read opener.windowTitle (e.g. routing by Slack \
                     workspace) will silently no-op until you allow Grinch.app \
                     in System Settings → Privacy & Security → Accessibility."
                );
            }
        }
    }

    impl Delegate {
        // Apple Event GURL handler. Selector: handleURL:withReplyEvent:.
        #[unsafe(method(handleURL:withReplyEvent:))]
        fn handle_url(&self, event: &NSAppleEventDescriptor, _reply: &NSAppleEventDescriptor) {
            let raw = event
                .paramDescriptorForKeyword(KEY_DIRECT_OBJECT)
                .and_then(|d| d.stringValue())
                .map(|s| s.to_string());
            let Some(raw) = raw else { return };

            let engine_ref = self.ivars().engine.borrow();
            let Some(engine) = engine_ref.as_ref() else { return };
            let sender_pid = sender_pid_from_event(event);
            let opener = resolve_opener(engine, sender_pid);
            let modifiers = if engine.needs_modifiers() { current_modifier_flags() } else { ModifierFlags::default() };

            // Diagnostic — gated by GRINCH_DEBUG=1 in env. Prints opener and
            // modifier state for each resolved URL so the user can verify
            // CGEventSourceFlagsState is actually picking up held keys.
            if debug_enabled() {
                eprintln!(
                    "grinch: resolve url={raw:?} opener=(bundle={}, name={}) modifiers={{shift:{}, option:{}, command:{}, control:{}}}",
                    opener.bundle_id, opener.name,
                    modifiers.shift, modifiers.option, modifiers.command, modifiers.control,
                );
            }

            let inner = unwrap_grinch_scheme(&raw);
            // Same pending-session check the openURLs: path does — see
            // there for the rationale. The two URL ingress points need
            // it independently because macOS dispatches GURL events
            // to handle_url:withReplyEvent: directly without going
            // through application:openURLs: for legacy URL handlers.
            if crate::session_handler::try_complete_callback(&inner) {
                return;
            }
            let result = engine.resolve(&inner, &opener, modifiers);

            if debug_enabled() {
                eprintln!(
                    "grinch: → browser={} args={:?} url={:?}",
                    result.browser.bundle_id, result.browser.args, result.url,
                );
            }

            if result.browser.bundle_id.is_empty() {
                return; // suppressed (open: null)
            }
            open_url(&result.url, &result.browser, self.mtm());
        }

        // Menu bar action: Reload Config.
        #[unsafe(method(reloadConfig:))]
        fn menu_reload_config(&self, _sender: Option<&AnyObject>) {
            self.reload_engine();
        }

        // Menu bar action: Open Config in the user's default editor.
        #[unsafe(method(openConfig:))]
        fn menu_open_config(&self, _sender: Option<&AnyObject>) {
            self.open_config();
        }

        // Menu bar action: toggle Start at Login (SMAppService).
        #[unsafe(method(toggleStartAtLogin:))]
        fn menu_toggle_start_at_login(&self, _sender: Option<&AnyObject>) {
            self.toggle_start_at_login();
        }
    }
);

impl Delegate {
    pub fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(DelegateIvars::default());
        unsafe { msg_send![super(this), init] }
    }

    pub fn reload_engine(&self) {
        // Refresh the path even if loading fails — keeps "Open Config"
        // pointed at the actual file the user wants to fix.
        *self.ivars().config_path.borrow_mut() = find_config_path();
        let result = match load_config() {
            Ok(loaded) => Engine::new(loaded)
                .map(|e| {
                    *self.ivars().engine.borrow_mut() = Some(e);
                })
                .map_err(|e| {
                    let msg = format!("engine init failed: {e}");
                    eprintln!("grinch: {msg}");
                    msg
                }),
            Err(msg) => Err(msg),
        };
        self.set_load_error(result.err());
    }

    fn set_load_error(&self, err: Option<String>) {
        *self.ivars().load_error.borrow_mut() = err;
        // Skip the AppKit calls if the menu bar hasn't been built yet —
        // setup_menu_bar() re-applies the current state at the end.
        if self.ivars().status_item.borrow().is_none() {
            return;
        }
        self.refresh_status_item();
        self.refresh_error_menu_item();
    }

    fn refresh_status_item(&self) {
        let item_ref = self.ivars().status_item.borrow();
        let Some(item) = item_ref.as_ref() else {
            return;
        };
        let Some(button) = item.button(self.mtm()) else {
            return;
        };
        let title = if self.ivars().load_error.borrow().is_some() {
            "⚠️"
        } else {
            "🎄"
        };
        button.setTitle(&NSString::from_str(title));
    }

    fn refresh_error_menu_item(&self) {
        let item_ref = self.ivars().error_menu_item.borrow();
        let Some(item) = item_ref.as_ref() else {
            return;
        };
        let err_ref = self.ivars().load_error.borrow();
        match err_ref.as_ref() {
            Some(msg) => {
                // Menu titles wrap awkwardly past ~80 chars in the macOS
                // status bar; the full message is still on stderr / in
                // `Console.app` if the user wants the whole thing.
                let truncated = truncate_for_menu(msg, 80);
                item.setTitle(&NSString::from_str(&format!("⚠ Config error: {truncated}")));
                item.setHidden(false);
            }
            None => item.setHidden(true),
        }
    }

    fn open_config(&self) {
        let path_ref = self.ivars().config_path.borrow();
        let Some(path) = path_ref.as_ref() else {
            eprintln!(
                "grinch: no config to open — create one at ~/.grinch.js, \
                 ~/.config/grinch.js, ~/.config/grinch/grinch.js, or \
                 /Library/Application Support/Grinch/grinch.js (system-wide)"
            );
            return;
        };
        let path_ns = NSString::from_str(&path.to_string_lossy());
        let url = NSURL::fileURLWithPath(&path_ns);
        let workspace = NSWorkspace::sharedWorkspace();
        // openURL hands the file to the user's default app for `.js`
        // (typically a text editor); Apple has not deprecated the basic
        // single-URL form, only the application-specific variants.
        workspace.openURL(&url);
    }

    fn toggle_start_at_login(&self) {
        let status = sm_status();
        let ok = if status == SM_STATUS_ENABLED {
            sm_unregister()
        } else {
            sm_register()
        };
        if !ok {
            return;
        }
        let new_status = sm_status();
        // RequiresApproval = the user has Login Items toggled off for
        // Grinch in System Settings; nudge them there so the toggle has
        // a chance to take effect.
        if new_status == SM_STATUS_REQUIRES_APPROVAL {
            sm_open_login_items_settings();
        }
        self.refresh_start_at_login_check(new_status);
    }

    fn refresh_start_at_login_check(&self, status: isize) {
        let item_ref = self.ivars().start_at_login_item.borrow();
        let Some(item) = item_ref.as_ref() else {
            return;
        };
        let state = if status == SM_STATUS_ENABLED {
            NS_CONTROL_STATE_VALUE_ON
        } else {
            NS_CONTROL_STATE_VALUE_OFF
        };
        item.setState(state);
    }

    fn list_rules(&self) {
        let engine_ref = self.ivars().engine.borrow();
        let Some(engine) = engine_ref.as_ref() else {
            println!("grinch: no config loaded");
            return;
        };
        let lines = engine.rule_listing();
        if lines.is_empty() {
            println!("grinch: no rules in config (everything falls through to default)");
            return;
        }
        for line in lines {
            println!("{line}");
        }
    }

    /// `--validate` body: returns the process exit code. 0 = config loaded
    /// cleanly, 1 = load error (already printed to stderr by the loader
    /// AND captured into the menu-bar error string via reload_engine).
    fn report_validation(&self) -> i32 {
        let path = self
            .ivars()
            .config_path
            .borrow()
            .as_ref()
            .map(|p| p.display().to_string());
        if let Some(err) = self.ivars().load_error.borrow().as_ref() {
            println!("grinch: config invalid — {err}");
            if let Some(path) = path {
                println!("path:   {path}");
            }
            return 1;
        }
        match path {
            Some(path) => println!("grinch: config OK — {path}"),
            None => println!("grinch: config OK (no path resolved)"),
        }
        let engine_ref = self.ivars().engine.borrow();
        if let Some(engine) = engine_ref.as_ref() {
            let rules = engine.rule_listing().len();
            println!("rules:  {rules}");
        }
        0
    }

    fn list_browsers(&self) {
        let browsers = list_http_browsers();
        if browsers.is_empty() {
            println!("grinch: no http handlers registered with LaunchServices");
            return;
        }
        // Two-column layout: bundle ID (the value you write in a config)
        // and the display name (what System Settings shows). Width derived
        // from the longest bundle id so all names line up.
        let id_width = browsers
            .iter()
            .map(|b| b.bundle_id.chars().count())
            .max()
            .unwrap_or(0);
        for b in &browsers {
            println!("{:<width$}  {}", b.bundle_id, b.name, width = id_width);
        }
    }

    fn test_url(&self, raw: &str) {
        let engine_ref = self.ivars().engine.borrow();
        let Some(engine) = engine_ref.as_ref() else {
            println!("grinch: no config loaded");
            return;
        };
        let opener = Opener {
            bundle_id: "test.app".to_string(),
            name: "test".to_string(),
            path: String::new(),
            pid: 0,
        };
        // Mirror the real URL-handler path: `grinch:<inner>` strips to <inner>
        // before resolve, so `--test grinch:https://x/` exercises the same
        // routing the user would get from `open grinch:https://x/`.
        let inner = unwrap_grinch_scheme(raw);
        let result = engine.resolve(&inner, &opener, ModifierFlags::default());
        println!("URL:     {raw}");
        if inner.as_ref() != raw {
            println!("Routed:  {inner}");
        }
        println!("Final:   {}", result.url);
        println!("Browser: {}", result.browser.bundle_id);
        if !result.browser.args.is_empty() {
            println!("Args:    {}", result.browser.args.join(" "));
        }
    }

    fn bench(&self, n: usize, url: &str) {
        let engine_ref = self.ivars().engine.borrow();
        let Some(engine) = engine_ref.as_ref() else {
            println!("grinch: no config loaded");
            return;
        };
        let opener = Opener {
            bundle_id: "com.tinyspeck.slackmacgap".to_string(),
            name: "Slack".to_string(),
            path: String::new(),
            pid: 0,
        };
        // Warmup: min(n / 10, 1000) iterations to JIT-warm the JS bridge
        // and populate per-resolve caches before timing. Capped at 1000 so
        // a 1M-iter run doesn't spend 100k iters warming.
        let warmup = (n / 10).min(1_000);
        for _ in 0..warmup {
            let _ = engine.resolve(url, &opener, ModifierFlags::default());
        }
        let start = std::time::Instant::now();
        for _ in 0..n {
            let _ = engine.resolve(url, &opener, ModifierFlags::default());
        }
        let elapsed = start.elapsed();
        let ns_per_op = elapsed.as_nanos() / (n as u128).max(1);
        let us_per_op = elapsed.as_secs_f64() * 1_000_000.0 / n.max(1) as f64;
        println!("Benchmark: {n} iterations");
        println!("Total:     {}ms", elapsed.as_millis());
        println!("Per-op:    {ns_per_op}ns  ({us_per_op:.2}µs)");
        let r = engine.resolve(url, &opener, ModifierFlags::default());
        println!("URL:       {url}");
        println!("Browser:   {}", r.browser.bundle_id);
        // Bench measures resolve() in isolation — the synthetic Opener
        // (pid=0) skips the real LaunchServices IPC, and ctx.opener.windowTitle
        // short-circuits to "". Real-click latency adds frontmost_opener()
        // (~100–500 µs of LS IPC) and current_modifier_flags() (~100 ns)
        // on top, when the engine's needs_opener / needs_modifiers flags
        // demand them.
        println!("Note:      synthetic Opener (pid=0); does not include LaunchServices IPC");
    }

    fn setup_menu_bar(&self) {
        // options.hideIcon — Finicky-compat. Skip status-item creation
        // entirely when the user opts out. Read once at launch; reloads
        // don't add/remove the icon mid-session (kill -HUP $(pgrep …) +
        // change won't toggle visibility — restart the app to take
        // effect).
        let hide_icon = self
            .ivars()
            .engine
            .borrow()
            .as_ref()
            .map(|e| e.hide_icon())
            .unwrap_or(false);
        if hide_icon {
            return;
        }

        let mtm = self.mtm();
        let bar = NSStatusBar::systemStatusBar();
        let item = bar.statusItemWithLength(NSSquareStatusItemLength);
        if let Some(button) = item.button(mtm) {
            button.setTitle(&NSString::from_str("🎄"));
        } else {
            // Shouldn't happen on a healthy system — NSStatusBar.statusItemWithLength
            // returns an item with a button on macOS 10.10+. Log so a missing
            // menu-bar icon isn't completely silent.
            eprintln!("grinch: status item has no button — menu bar icon will be invisible");
        }

        let menu = NSMenu::new(mtm);
        let me: &AnyObject = self.as_ref();

        // Pre-built "Config error: …" item at the top of the menu. Hidden
        // by default; flipped on by refresh_error_menu_item() when a reload
        // captures an error. Disabled (no action) so it reads as status,
        // not a button. The separator after it is hidden too so the menu
        // is visually unchanged in the healthy case.
        let error_item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(""),
                None,
                &NSString::from_str(""),
            )
        };
        error_item.setHidden(true);
        menu.addItem(&error_item);
        *self.ivars().error_menu_item.borrow_mut() = Some(error_item);

        // Version label: disabled menu item ("nil action" → grey, non-
        // clickable). macOS convention is to put app-identity info at
        // the top of a status menu so users can quickly check what
        // version they're on without opening About. The value is the
        // crate version stamped into the binary at compile time —
        // matches what `Grinch --version` prints.
        let version = format!("Grinch {}", env!("CARGO_PKG_VERSION"));
        let version_item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(&version),
                None,
                &NSString::from_str(""),
            )
        };
        menu.addItem(&version_item);
        menu.addItem(&NSMenuItem::separatorItem(mtm));

        let open_config = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Open Config"),
                Some(sel!(openConfig:)),
                &NSString::from_str("o"),
            )
        };
        unsafe { open_config.setTarget(Some(me)) };
        menu.addItem(&open_config);

        let reload = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Reload Config"),
                Some(sel!(reloadConfig:)),
                &NSString::from_str("r"),
            )
        };
        unsafe { reload.setTarget(Some(me)) };
        menu.addItem(&reload);

        menu.addItem(&NSMenuItem::separatorItem(mtm));

        let start_at_login = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Start at Login"),
                Some(sel!(toggleStartAtLogin:)),
                &NSString::from_str(""),
            )
        };
        unsafe { start_at_login.setTarget(Some(me)) };
        let initial_state = if sm_status() == SM_STATUS_ENABLED {
            NS_CONTROL_STATE_VALUE_ON
        } else {
            NS_CONTROL_STATE_VALUE_OFF
        };
        start_at_login.setState(initial_state);
        menu.addItem(&start_at_login);
        *self.ivars().start_at_login_item.borrow_mut() = Some(start_at_login);

        menu.addItem(&NSMenuItem::separatorItem(mtm));

        let quit = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Quit Grinch"),
                Some(sel!(terminate:)),
                &NSString::from_str("q"),
            )
        };
        menu.addItem(&quit);

        item.setMenu(Some(&menu));
        *self.ivars().status_item.borrow_mut() = Some(item);

        // The first reload_engine() runs *before* the menu bar exists, so
        // any startup load error is already in ivars without UI. Apply it
        // now that the icon and error item are live.
        self.refresh_status_item();
        self.refresh_error_menu_item();
    }
}

fn terminate(mtm: MainThreadMarker) {
    let app = NSApplication::sharedApplication(mtm);
    app.terminate(None);
}

/// Bridge from `session_handler::install`'s function-pointer
/// interface to the engine + main-thread context. Called on the main
/// thread by the auth-session handler when a session URL needs
/// forwarding. Looks up the delegate via the same DELEGATE_PTR the
/// SIGHUP reload path uses; reads the engine through it and dispatches
/// via session_handler::forward_through_engine.
fn forward_auth_session_url(url: &str) {
    // SAFETY: only called on the main thread (the auth-session manager
    // dispatches its handler methods on the main run loop). MainThreadMarker
    // is therefore valid; the unwrap can't fire in practice.
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let p = DELEGATE_PTR.load(Ordering::Relaxed);
    if p.is_null() {
        return;
    }
    // The pointer was stored from a `Retained<Delegate>` that lives for
    // the lifetime of the process (NSApplication holds the strong ref).
    // Reading through it is safe.
    let delegate: &Delegate = unsafe { &*(p as *const Delegate) };
    let engine_ref = delegate.ivars().engine.borrow();
    let Some(engine) = engine_ref.as_ref() else {
        return;
    };
    crate::session_handler::forward_through_engine(url, engine, mtm);
}

/// If `url` uses the `grinch:` scheme (an opt-in routing hook for scripts:
/// `open grinch:https://example.com/`), strip the `grinch:` prefix so the
/// engine resolves the inner URL through the user's rules as if it had
/// arrived normally. Otherwise return the input unchanged.
///
/// Accepts three shapes:
///   - `grinch:<inner>` — RFC 3986 opaque form
///   - `grinch://<inner>` — the form `open(1)` synthesises when invoked
///     with `--background`
///   - `grinch://open/<base64>` — the envelope shape Finicky's published
///     Chrome and Firefox addons emit (PRs johnste/finicky#423, #418).
///     The payload is `btoa(url)` — standard base64 — but the decoder
///     also accepts the URL-safe alphabet (`-`/`_`) and missing padding.
///
/// Empty `grinch:` payloads route as `""`, which falls through to the
/// default browser — same as any other no-op URL would. A `grinch://open/…`
/// shape whose payload doesn't decode to valid base64 is passed through
/// unchanged so the engine sees the malformed input rather than silently
/// dropping it.
fn unwrap_grinch_scheme(url: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    if let Some(b64) = url.strip_prefix("grinch://open/") {
        return match decode_envelope_b64(b64) {
            Some(decoded) => Cow::Owned(decoded),
            None => Cow::Borrowed(url),
        };
    }
    if let Some(rest) = url.strip_prefix("grinch://") {
        return Cow::Borrowed(rest);
    }
    if let Some(rest) = url.strip_prefix("grinch:") {
        return Cow::Borrowed(rest);
    }
    Cow::Borrowed(url)
}

/// Decode a `grinch://open/<…>` envelope payload. Accepts both standard
/// base64 (what JavaScript's `btoa` emits — `+` / `/`) and URL-safe
/// base64 (`-` / `_`), with optional `=` padding. Returns None on any
/// invalid character, a trailing 6-bit leftover (single dangling char
/// that encodes no bytes — malformed base64), or if the decoded bytes
/// aren't valid UTF-8. The caller falls back to passing the original
/// URL through unchanged.
fn decode_envelope_b64(s: &str) -> Option<String> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4 + 1);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for c in s.bytes() {
        let v: u32 = match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a') as u32 + 26,
            b'0'..=b'9' => (c - b'0') as u32 + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
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
    if bits == 6 {
        return None;
    }
    // Any leftover bits MUST themselves be zero — they're padding bits of
    // the final encoded char. A legitimate `btoa()` always emits zero
    // padding bits; non-zero leftover means malformed or adversarial
    // input. Strict-mode RFC 4648 rejects it; matching the engine's
    // `base64_url_decode` so both envelope shapes agree.
    if buf != 0 {
        return None;
    }
    String::from_utf8(out).ok()
}

/// Read the sender pid attribute (`'spid'`) off a GURL Apple Event. Returns
/// None when the attribute is absent or zero — the caller falls back to the
/// frontmost-app heuristic, which is wrong but better than nothing.
fn sender_pid_from_event(event: &NSAppleEventDescriptor) -> Option<i32> {
    let pid = event
        .attributeDescriptorForKeyword(KEY_SENDER_PID_ATTR)?
        .int32Value();
    (pid > 0).then_some(pid)
}

/// Pick the opener for this resolve given the engine's runtime needs and
/// the sender pid from the Apple Event (if any). Sender pid is the canonical
/// signal — it survives LaunchServices activating Grinch ahead of our
/// callback. Fall back to `frontmost_opener()` only when the attribute is
/// absent or the pid no longer maps to a running app (process exited).
fn resolve_opener(engine: &Engine, sender_pid: Option<i32>) -> Opener {
    if !engine.needs_opener() {
        return Opener::default();
    }
    if engine.needs_opener_full() {
        if let Some(opener) = sender_pid.and_then(opener_from_pid) {
            return opener;
        }
        return frontmost_opener();
    }
    if let Some(opener) = sender_pid.and_then(opener_from_pid) {
        return Opener {
            bundle_id: opener.bundle_id,
            ..Opener::default()
        };
    }
    frontmost_opener_id()
}

/// Trim a multi-line error message to a single line capped at `max_chars`
/// characters. The macOS menu bar renders titles single-line and clips wide
/// items; this gives users the first sentence of the error inline with the
/// status item without truncating mid-codepoint.
fn truncate_for_menu(msg: &str, max_chars: usize) -> String {
    let single_line = msg.split('\n').next().unwrap_or(msg).trim();
    if single_line.chars().count() <= max_chars {
        return single_line.to_string();
    }
    let prefix: String = single_line.chars().take(max_chars).collect();
    format!("{prefix}…")
}

// MARK: - SIGHUP handling
//
// Grinch reloads its config on SIGHUP via the textbook self-pipe trick:
//
//   1. Open a pipe at startup; stash the write end in an atomic.
//   2. Signal handler does only one thing — write a single byte to the
//      pipe. `write(2)` is on POSIX's async-signal-safe list (libdispatch
//      and Obj-C runtime calls are not — the previous direct-dispatch_async
//      handler was a latent deadlock waiting for unlucky timing).
//   3. A background thread blocks reading the other end; on byte arrival
//      it dispatches the reload to the main queue from normal context,
//      where `DispatchQueue::main()` and `msg_send!` are safe.

static DELEGATE_PTR: AtomicPtr<AnyObject> = AtomicPtr::new(std::ptr::null_mut());
static SIGHUP_PIPE_WRITE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

extern "C" fn sighup_trampoline(_sig: libc::c_int) {
    let fd = SIGHUP_PIPE_WRITE.load(Ordering::Relaxed);
    if fd < 0 {
        return;
    }
    // SAFETY: write(2) is async-signal-safe per POSIX; a single-byte write
    // ≤ PIPE_BUF is atomic. Errors ignored — if the pipe is full, a reload
    // is already pending in the reader thread's queue.
    let b: u8 = b'r';
    unsafe {
        libc::write(fd, &b as *const u8 as *const libc::c_void, 1);
    }
}

extern "C" fn reload_on_main(_ctx: *mut c_void) {
    let p = DELEGATE_PTR.load(Ordering::Relaxed);
    if p.is_null() {
        return;
    }
    unsafe {
        let _: () = msg_send![&*p, reloadConfig: std::ptr::null::<AnyObject>()];
    }
}

/// Outcome of a `read(2)` on the SIGHUP self-pipe. Pulled out of the
/// reader thread so the EOF / EINTR / fatal-error classification is
/// testable without spawning a real thread or invoking syscalls.
enum PipeReadOutcome {
    Data,
    Interrupted,
    Eof,
    Fatal(std::io::Error),
}

fn classify_pipe_read(n: isize, err: std::io::Error) -> PipeReadOutcome {
    if n > 0 {
        return PipeReadOutcome::Data;
    }
    if n == 0 {
        return PipeReadOutcome::Eof;
    }
    if err.kind() == std::io::ErrorKind::Interrupted {
        return PipeReadOutcome::Interrupted;
    }
    PipeReadOutcome::Fatal(err)
}

fn install_sighup_handler(_delegate: &Delegate) {
    // Idempotency: if called twice (e.g., a future refactor that reloads
    // the delegate without restarting the process), don't open a second
    // pipe + reader thread, which would leak fds and double-handle SIGHUP.
    //
    // `DELEGATE_PTR` is set in `applicationWillFinishLaunching:` before this
    // runs, so we don't store again here — there's only ever one Delegate
    // instance per process and writing the same pointer a second time would
    // be redundant (and would lie about the lifecycle if someone reads the
    // store as "this is where the pointer becomes live").
    static INSTALLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if INSTALLED.load(Ordering::SeqCst) {
        return;
    }

    let mut fds: [libc::c_int; 2] = [-1; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        // INSTALLED is intentionally NOT set on this failure path — a
        // later retry (e.g. after the process recovers from FD
        // exhaustion) can re-attempt. Previously the swap-before-pipe
        // ordering set INSTALLED=true first, so a single pipe() failure
        // permanently disabled SIGHUP reload for the rest of the
        // process's life with no way back.
        eprintln!("grinch: pipe() failed for SIGHUP self-pipe; reload disabled");
        return;
    }
    // Past the fallible setup — mark installed before spawning the
    // reader thread + registering the signal handler so a concurrent
    // call (unlikely on the main thread, but cheap to guard) doesn't
    // double-install.
    if INSTALLED.swap(true, Ordering::SeqCst) {
        // Lost the race; another call beat us. Close the redundant
        // fds and bail.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
        return;
    }
    SIGHUP_PIPE_WRITE.store(fds[1], Ordering::Relaxed);

    // Reader thread: drains bytes from the read end and posts a reload to
    // the main queue per byte arrival. Lives for the process lifetime.
    let read_fd = fds[0];
    std::thread::spawn(move || {
        let mut buf = [0u8; 64];
        loop {
            let n =
                unsafe { libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            match classify_pipe_read(n, std::io::Error::last_os_error()) {
                PipeReadOutcome::Data => {
                    // Off the signal handler now — libdispatch + Obj-C are safe.
                    let queue = DispatchQueue::main();
                    unsafe {
                        queue.exec_async_f(std::ptr::null_mut(), reload_on_main);
                    }
                }
                PipeReadOutcome::Interrupted => continue,
                PipeReadOutcome::Eof => {
                    eprintln!("grinch: SIGHUP self-pipe closed; reload disabled");
                    return;
                }
                PipeReadOutcome::Fatal(err) => {
                    eprintln!("grinch: SIGHUP self-pipe read error: {err}; reload disabled");
                    return;
                }
            }
        }
    });

    let handler = sighup_trampoline as *const () as libc::sighandler_t;
    unsafe { libc::signal(libc::SIGHUP, handler) };
}

// MARK: - SMAppService (Start at Login)
//
// Thin wrapper over SMAppService.mainApp. Methods are reached via the Obj-C
// runtime (no objc2-service-management crate dep). The framework is linked
// at the top of this file.

// Apple's ServiceManagement/SMAppService.h declares the property as
//   @property (class, readonly) SMAppService *mainAppService NS_SWIFT_NAME(mainApp);
// so the Swift name `mainApp` only applies in Swift; the Obj-C runtime
// selector is `mainAppService`.
fn sm_status() -> isize {
    unsafe {
        let cls = class!(SMAppService);
        let service: *mut AnyObject = msg_send![cls, mainAppService];
        if service.is_null() {
            return SM_STATUS_NOT_FOUND;
        }
        msg_send![&*service, status]
    }
}

fn sm_register() -> bool {
    sm_register_call(false)
}

fn sm_unregister() -> bool {
    sm_register_call(true)
}

// `unregister` and `registerAndReturnError:` share their out-error shape, so
// dispatch through one path to keep the unsafe surface minimal.
fn sm_register_call(unregister: bool) -> bool {
    unsafe {
        let cls = class!(SMAppService);
        let service: *mut AnyObject = msg_send![cls, mainAppService];
        if service.is_null() {
            eprintln!("grinch: SMAppService.mainAppService returned nil");
            return false;
        }
        let mut error: *mut AnyObject = std::ptr::null_mut();
        let ok: bool = if unregister {
            msg_send![&*service, unregisterAndReturnError: &mut error]
        } else {
            msg_send![&*service, registerAndReturnError: &mut error]
        };
        if !ok {
            let op = if unregister { "unregister" } else { "register" };
            if error.is_null() {
                eprintln!("grinch: SMAppService {op} failed");
            } else {
                let desc: *mut NSString = msg_send![&*error, localizedDescription];
                let msg = if desc.is_null() {
                    String::from("(no description)")
                } else {
                    (*desc).to_string()
                };
                eprintln!("grinch: SMAppService {op} failed: {msg}");
            }
        }
        ok
    }
}

fn sm_open_login_items_settings() {
    unsafe {
        let cls = class!(SMAppService);
        let _: () = msg_send![cls, openSystemSettingsLoginItems];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwrap_grinch_scheme_strips_opaque_prefix() {
        assert_eq!(
            unwrap_grinch_scheme("grinch:https://example.com/"),
            "https://example.com/"
        );
        assert_eq!(
            unwrap_grinch_scheme("grinch:slack://team/channel"),
            "slack://team/channel"
        );
    }

    #[test]
    fn unwrap_grinch_scheme_strips_authority_form() {
        // `open grinch://...` on macOS sometimes synthesises the authority
        // form; we accept both shapes since the URL normaliser may collapse
        // `grinch:` against `grinch://`.
        assert_eq!(
            unwrap_grinch_scheme("grinch://https://example.com/"),
            "https://example.com/"
        );
    }

    #[test]
    fn unwrap_grinch_scheme_passes_through_unrelated_urls() {
        assert_eq!(
            unwrap_grinch_scheme("https://example.com/"),
            "https://example.com/"
        );
        // Scheme suffix only — must not partial-match.
        assert_eq!(unwrap_grinch_scheme("notgrinch:foo"), "notgrinch:foo");
    }

    #[test]
    fn unwrap_grinch_scheme_handles_empty_payload() {
        // `grinch:` with nothing after is harmless: the engine resolves
        // the empty string and falls through to default.
        assert_eq!(unwrap_grinch_scheme("grinch:"), "");
        assert_eq!(unwrap_grinch_scheme("grinch://"), "");
    }

    #[test]
    fn unwrap_grinch_scheme_decodes_envelope_standard_b64() {
        // `btoa("https://example.com/")` = "aHR0cHM6Ly9leGFtcGxlLmNvbS8="
        // — what Finicky's published browser addons emit verbatim.
        assert_eq!(
            unwrap_grinch_scheme("grinch://open/aHR0cHM6Ly9leGFtcGxlLmNvbS8="),
            "https://example.com/"
        );
        // Same payload, padding stripped — the URL parser may have eaten
        // the trailing `=` since it's reserved.
        assert_eq!(
            unwrap_grinch_scheme("grinch://open/aHR0cHM6Ly9leGFtcGxlLmNvbS8"),
            "https://example.com/"
        );
    }

    #[test]
    fn unwrap_grinch_scheme_decodes_envelope_url_safe_b64() {
        // URL-safe alphabet (`-`/`_` instead of `+`/`/`). Some addons
        // pre-encode the payload to keep it path-safe; we accept both.
        // `btoa("https://example.com/?q=a+b")` uses `+`; the URL-safe
        // variant replaces it with `-`. Verify the decoder accepts the
        // safe form.
        let standard = "aHR0cHM6Ly9leGFtcGxlLmNvbS8/cT1hK2I=";
        let safe = standard
            .replace('+', "-")
            .replace('/', "_")
            .replace('=', "");
        let safe_url = format!("grinch://open/{safe}");
        assert_eq!(
            unwrap_grinch_scheme(&safe_url),
            "https://example.com/?q=a+b"
        );
    }

    #[test]
    fn unwrap_grinch_scheme_passes_through_malformed_envelope() {
        // Garbage payload after the envelope prefix → pass through the
        // whole URL unchanged so the engine sees the malformed input
        // (and either rejects it or falls to default) rather than us
        // silently swallowing the click.
        let url = "grinch://open/!!!not-base64!!!";
        assert_eq!(unwrap_grinch_scheme(url), url);
    }

    #[test]
    fn unwrap_grinch_scheme_rejects_non_zero_padding_bits() {
        // Strict-mode base64: the leftover bits at end-of-input must be
        // zero (they're padding bits of the final encoded char). `btoa`
        // always emits zero padding; non-zero leftover is malformed or
        // adversarial. "aR" decodes to one byte `i` with 4 padding bits
        // = 0b0001 (nonzero) — strict decoders reject this.
        let url = "grinch://open/aR";
        // Pass-through, not silent decode to a one-byte non-URL string.
        assert_eq!(unwrap_grinch_scheme(url), url);
    }

    #[test]
    fn truncate_for_menu_returns_short_strings_unchanged() {
        assert_eq!(truncate_for_menu("hello", 80), "hello");
    }

    #[test]
    fn truncate_for_menu_trims_to_first_line() {
        assert_eq!(
            truncate_for_menu("first line\nsecond line\nthird line", 80),
            "first line"
        );
    }

    #[test]
    fn truncate_for_menu_caps_long_lines_with_ellipsis() {
        let long = "x".repeat(200);
        let got = truncate_for_menu(&long, 10);
        assert_eq!(got.chars().count(), 11); // 10 chars + '…'
        assert!(got.ends_with('…'), "got: {got}");
    }

    #[test]
    fn truncate_for_menu_counts_chars_not_bytes() {
        // Multi-byte chars: each counts as one toward the cap.
        let s = "äöü".repeat(20); // 60 chars, 120 bytes
        let got = truncate_for_menu(&s, 5);
        assert_eq!(got.chars().count(), 6); // 5 chars + '…'
    }

    #[test]
    fn classify_pipe_read_distinguishes_data_eof_eintr_fatal() {
        // Positive byte count → data ready, post a reload.
        assert!(matches!(
            classify_pipe_read(64, std::io::Error::from_raw_os_error(0)),
            PipeReadOutcome::Data
        ));
        // 0 bytes → true EOF, write end closed. Don't reload, exit thread.
        assert!(matches!(
            classify_pipe_read(0, std::io::Error::from_raw_os_error(0)),
            PipeReadOutcome::Eof
        ));
        // -1 with EINTR → keep looping; the syscall was interrupted by
        // an unrelated signal, the pipe is still open. Regression
        // against the pre-fix behaviour that treated this as fatal and
        // silently disabled SIGHUP reload for the rest of the process.
        let eintr = std::io::Error::from_raw_os_error(libc::EINTR);
        assert!(matches!(
            classify_pipe_read(-1, eintr),
            PipeReadOutcome::Interrupted
        ));
        // -1 with anything else (EBADF, EIO, etc.) → fatal, exit thread.
        let ebadf = std::io::Error::from_raw_os_error(libc::EBADF);
        assert!(matches!(
            classify_pipe_read(-1, ebadf),
            PipeReadOutcome::Fatal(_)
        ));
    }
}
