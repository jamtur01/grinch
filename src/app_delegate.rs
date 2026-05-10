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
use objc2::runtime::AnyObject;
use objc2::{class, define_class, msg_send, sel, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationDelegate, NSMenu, NSMenuItem, NSSquareStatusItemLength,
    NSStatusBar, NSStatusItem, NSWorkspace,
};
use objc2_core_services::{AEEventClass, AEEventID};
use objc2_foundation::{
    MainThreadMarker, NSAppleEventDescriptor, NSAppleEventManager, NSNotification, NSObject,
    NSObjectProtocol, NSString, NSURL,
};

use crate::engine::{Engine, ModifierFlags};
use crate::loader::{find_config_path, load_config};
use crate::workspace::{
    current_modifier_flags, ensure_accessibility_permission, frontmost_opener, frontmost_opener_id,
    open_url, Opener,
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
            let opener = if !engine.needs_opener() {
                Opener::default()
            } else if engine.needs_opener_full() {
                frontmost_opener()
            } else {
                frontmost_opener_id()
            };
            let modifiers = if engine.needs_modifiers() { current_modifier_flags() } else { ModifierFlags::default() };
            for i in 0..count {
                let url = urls.objectAtIndex(i);
                let Some(raw) = url.absoluteString() else { continue };
                let raw = raw.to_string();
                let result = engine.resolve(&raw, &opener, modifiers);
                if result.browser.bundle_id.is_empty() {
                    continue;
                }
                open_url(&result.url, &result.browser, self.mtm());
            }
        }

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

            // Normal app-mode startup: load config, build the menu bar,
            // wire SIGHUP, install the running-apps cache observer, defeat
            // AppNap so first-click-after-idle stays fast, and ask for
            // Accessibility once.
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
            let opener = if !engine.needs_opener() {
                Opener::default()
            } else if engine.needs_opener_full() {
                frontmost_opener()
            } else {
                frontmost_opener_id()
            };
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

            let result = engine.resolve(&raw, &opener, modifiers);

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
        let Some(loaded) = load_config() else { return };
        match Engine::new(loaded) {
            Ok(e) => {
                *self.ivars().engine.borrow_mut() = Some(e);
            }
            Err(e) => eprintln!("grinch: engine init failed: {e}"),
        }
    }

    fn open_config(&self) {
        let path_ref = self.ivars().config_path.borrow();
        let Some(path) = path_ref.as_ref() else {
            eprintln!(
                "grinch: no config to open — create one at ~/.grinch.js, \
                 ~/.config/grinch.js, or ~/.config/grinch/grinch.js"
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
        let result = engine.resolve(raw, &opener, ModifierFlags::default());
        println!("URL:     {raw}");
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
        for _ in 0..(n / 10).min(1_000) {
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
    }
}

fn terminate(mtm: MainThreadMarker) {
    let app = NSApplication::sharedApplication(mtm);
    app.terminate(None);
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

fn install_sighup_handler(delegate: &Delegate) {
    let ptr: *const Delegate = delegate;
    let any_ptr: *mut AnyObject = ptr as *mut AnyObject;
    DELEGATE_PTR.store(any_ptr, Ordering::Relaxed);

    let mut fds: [libc::c_int; 2] = [-1; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        eprintln!("grinch: pipe() failed for SIGHUP self-pipe; reload disabled");
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
            if n <= 0 {
                // EOF or error — pipe is gone, nothing more to do.
                eprintln!("grinch: SIGHUP self-pipe closed; reload disabled");
                return;
            }
            // Off the signal handler now — libdispatch + Obj-C are safe.
            let queue = DispatchQueue::main();
            unsafe {
                queue.exec_async_f(std::ptr::null_mut(), reload_on_main);
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
