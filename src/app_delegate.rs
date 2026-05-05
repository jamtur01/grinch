use std::cell::RefCell;
use std::ffi::c_void;
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
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationDelegate, NSMenu, NSMenuItem, NSSquareStatusItemLength,
    NSStatusBar, NSStatusItem,
};
use objc2_core_services::{AEEventClass, AEEventID};
use objc2_foundation::{
    MainThreadMarker, NSAppleEventDescriptor, NSAppleEventManager, NSNotification, NSObject,
    NSObjectProtocol, NSString,
};

use crate::engine::{Engine, ModifierFlags};
use crate::loader::load_config;
use crate::workspace::{
    current_modifier_flags, ensure_accessibility_permission, frontmost_opener, open_url, Opener,
};

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
            let opener = if engine.needs_opener() { frontmost_opener() } else { Opener::default() };
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
            self.reload_engine();
            self.setup_menu_bar();
            install_sighup_handler(self);

            // Skip the Accessibility prompt in CLI modes — they don't need
            // opener.windowTitle resolution and the dialog is jarring during
            // a one-shot --test or --bench invocation.
            let args: Vec<String> = std::env::args().collect();
            let in_cli_mode = args.iter().any(|a| a == "--test" || a == "--bench");
            if !in_cli_mode && !ensure_accessibility_permission() {
                eprintln!(
                    "grinch: Accessibility permission not granted yet. \
                     Rules that read opener.windowTitle (e.g. routing by Slack \
                     workspace) will silently no-op until you allow Grinch.app \
                     in System Settings → Privacy & Security → Accessibility."
                );
            }

            // CLI modes: --test <url>, --bench <n> <url>
            if let Some(idx) = args.iter().position(|a| a == "--test") {
                if let Some(url) = args.get(idx + 1) {
                    self.test_url(url);
                }
                terminate(self.mtm());
                return;
            }
            if let Some(idx) = args.iter().position(|a| a == "--bench") {
                if let (Some(n), Some(url)) = (args.get(idx + 1), args.get(idx + 2)) {
                    let n: usize = n.parse().unwrap_or(10_000);
                    self.bench(n, url);
                }
                terminate(self.mtm());
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
            let opener = if engine.needs_opener() { frontmost_opener() } else { Opener::default() };
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
    }
);

impl Delegate {
    pub fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(DelegateIvars::default());
        unsafe { msg_send![super(this), init] }
    }

    pub fn reload_engine(&self) {
        let Some(loaded) = load_config() else { return };
        match Engine::new(loaded) {
            Ok(e) => {
                *self.ivars().engine.borrow_mut() = Some(e);
            }
            Err(e) => eprintln!("grinch: engine init failed: {e:?}"),
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
        println!(
            "Note:      synthetic Opener (pid=0); does not include LaunchServices IPC"
        );
    }

    fn setup_menu_bar(&self) {
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
// Grinch reloads its config on SIGHUP. We install a libc signal handler that
// schedules a callback on the main dispatch queue (which the run loop drains),
// from which we can safely poke the delegate's reloadConfig: action.

static DELEGATE_PTR: AtomicPtr<AnyObject> = AtomicPtr::new(std::ptr::null_mut());

extern "C" fn sighup_trampoline(_sig: libc::c_int) {
    let queue = DispatchQueue::main();
    unsafe { queue.exec_async_f(std::ptr::null_mut(), reload_on_main) };
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
    let handler = sighup_trampoline as *const () as libc::sighandler_t;
    unsafe { libc::signal(libc::SIGHUP, handler) };
}

