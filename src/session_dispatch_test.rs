//! Native run-loop regression check: libtest runs tests on worker threads,
//! so this target owns main and pumps Foundation's run loop itself.
#![expect(
    dead_code,
    reason = "the harness imports app modules but only exercises session delivery"
)]
#![expect(
    unused_imports,
    reason = "libtest functions in imported app modules are omitted by this custom harness"
)]

mod app_delegate;
mod chromium;
mod engine;
mod firefox;
mod helpers;
mod loader;
mod session_handler;
mod updater;
mod workspace;

use std::cell::{Cell, RefCell};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::{Bool, ProtocolObject, Sel};
use objc2::{
    AnyThread, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class,
    msg_send, sel,
};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSButton,
    NSRunningApplication, NSTextField, NSView, NSWindow,
};
use objc2_authentication_services::{
    ASWebAuthenticationSessionCallback, ASWebAuthenticationSessionErrorCode,
    ASWebAuthenticationSessionErrorDomain, ASWebAuthenticationSessionRequest,
    ASWebAuthenticationSessionRequestDelegate, ASWebAuthenticationSessionWebBrowserSessionHandling,
    ASWebAuthenticationSessionWebBrowserSessionManager,
};
use objc2_foundation::{
    NSArray, NSDate, NSError, NSNotification, NSObject, NSObjectProtocol, NSRunLoop, NSString,
    NSURL, NSUUID,
};

static EVENTS: Mutex<Vec<(String, bool)>> = Mutex::new(Vec::new());
static CANCELLATIONS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

enum Forwarding {
    Record,
    Route(Box<engine::Engine>),
    Unavailable,
}

thread_local! {
    static FORWARDING: RefCell<Forwarding> = const { RefCell::new(Forwarding::Record) };
}

struct RequestIvars {
    url: Retained<NSURL>,
    uuid: Retained<NSUUID>,
    observer: Option<Retained<Observer>>,
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "GrinchTestReopenDelegate"]
    #[ivars = Retained<app_delegate::Delegate>]
    struct ReopenDelegate;

    unsafe impl NSObjectProtocol for ReopenDelegate {}

    unsafe impl NSApplicationDelegate for ReopenDelegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish(&self, _notification: &NSNotification) {
            probe_record("started");
        }

        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn reopen(&self, app: &NSApplication, visible: bool) -> Bool {
            let handled: Bool = unsafe {
                msg_send![&**self.ivars(), applicationShouldHandleReopen: app,
                    hasVisibleWindows: visible]
            };
            let windows = app.windows();
            let visible_count = (0..windows.count())
                .filter(|&i| windows.objectAtIndex(i).isVisible())
                .count();
            probe_record(&format!("reopen:{visible_count}"));
            handled
        }
    }
);

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "GrinchTestDuplicateDelegate"]
    struct DuplicateDelegate;

    unsafe impl NSObjectProtocol for DuplicateDelegate {}

    unsafe impl NSApplicationDelegate for DuplicateDelegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish(&self, _notification: &NSNotification) {
            let directory = PathBuf::from(std::env::var_os("GRINCH_DUPLICATE_TEST_DIR").unwrap());
            let pid = std::process::id();
            std::fs::write(directory.join(format!("ready-{pid}")), "ready").unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !directory.join("start").exists() {
                if Instant::now() >= deadline {
                    std::process::exit(2);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            workspace::terminate_duplicate_instances();
            std::fs::write(directory.join(format!("passed-{pid}")), "passed").unwrap();
        }
    }
);

fn run_duplicate_probe(mtm: MainThreadMarker) {
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    let delegate: Retained<DuplicateDelegate> =
        unsafe { msg_send![DuplicateDelegate::alloc(mtm), init] };
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    app.run();
}

define_class!(
    #[unsafe(super = NSObject)]
    #[name = "GrinchTestUpdater"]
    #[ivars = Cell<bool>]
    struct TestUpdater;

    unsafe impl NSObjectProtocol for TestUpdater {}

    impl TestUpdater {
        #[unsafe(method_id(updater))]
        fn updater(&self) -> Retained<Self> {
            self.retain()
        }

        #[unsafe(method(canCheckForUpdates))]
        fn can_check(&self) -> bool {
            self.ivars().get()
        }

        #[unsafe(method(setCanCheckForUpdates:))]
        fn set_can_check(&self, enabled: bool) {
            self.ivars().set(enabled);
        }
    }
);

define_class!(
    #[unsafe(super = ASWebAuthenticationSessionRequest)]
    #[name = "GrinchTestAuthRequest"]
    #[ivars = RequestIvars]
    struct Request;

    unsafe impl NSObjectProtocol for Request {}

    impl Request {
        #[unsafe(method_id(URL))]
        fn url(&self) -> Retained<NSURL> {
            self.ivars().url.clone()
        }

        #[unsafe(method_id(UUID))]
        fn uuid(&self) -> Retained<NSUUID> {
            self.ivars().uuid.clone()
        }

        #[unsafe(method_id(callback))]
        fn callback(&self) -> Retained<ASWebAuthenticationSessionCallback> {
            unsafe {
                ASWebAuthenticationSessionCallback::callbackWithCustomScheme(
                    &NSString::from_str("grinch-auth-test"),
                )
            }
        }
    }
);

define_class!(
    #[unsafe(super = NSObject)]
    #[name = "GrinchTestAuthObserver"]
    #[ivars = ()]
    struct Observer;

    unsafe impl NSObjectProtocol for Observer {}

    unsafe impl ASWebAuthenticationSessionRequestDelegate for Observer {
        #[unsafe(method(authenticationSessionRequest:didCompleteWithCallbackURL:))]
        fn complete(&self, _request: &ASWebAuthenticationSessionRequest, url: &NSURL) {
            record(format!("complete:{}", url.absoluteString().unwrap()));
        }

        #[unsafe(method(authenticationSessionRequest:didCancelWithError:))]
        fn cancel(&self, request: &ASWebAuthenticationSessionRequest, error: &NSError) {
            record(format!("cancel:{}:{}", error.domain(), error.code()));
            CANCELLATIONS.lock().unwrap().push((
                unsafe { request.UUID() }.UUIDString().to_string(),
                error.localizedDescription().to_string(),
            ));
        }
    }
);

impl Request {
    fn new(href: &str, canceling_uuid: Option<&NSUUID>) -> Retained<Self> {
        // Only the original request has the framework's completion delegate.
        // Cancellation delivers a separate request object with the same UUID.
        let observer = canceling_uuid.is_none().then(|| {
            let this = Observer::alloc().set_ivars(());
            unsafe { msg_send![super(this), init] }
        });
        let this = Self::alloc().set_ivars(RequestIvars {
            url: NSURL::URLWithString(&NSString::from_str(href)).unwrap(),
            uuid: canceling_uuid.map_or_else(NSUUID::new, |uuid| uuid.retain()),
            observer,
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };
        let delegate = this
            .ivars()
            .observer
            .as_deref()
            .map(ProtocolObject::from_ref);
        unsafe { this.setDelegate(delegate) };
        this
    }
}

fn record(event: String) {
    EVENTS
        .lock()
        .unwrap()
        .push((event, MainThreadMarker::new().is_some()));
}

fn forwarded(uuid: &str, url: &str, mtm: MainThreadMarker) -> Result<(), &'static str> {
    FORWARDING.with(|forwarding| {
        match &*forwarding.borrow() {
            Forwarding::Record => record(format!("forward:{url}")),
            Forwarding::Route(engine) => {
                session_handler::forward_through_engine(url, engine, Some(uuid), mtm);
            }
            Forwarding::Unavailable => return Err("Configuration unavailable; fix it and reload"),
        }
        Ok(())
    })
}

fn deliver_from_worker(url: &'static str, cancel: bool) -> String {
    std::thread::spawn(move || {
        assert!(MainThreadMarker::new().is_none());
        let request = Request::new(url, None);
        let manager =
            unsafe { ASWebAuthenticationSessionWebBrowserSessionManager::sharedManager() };
        let handler = unsafe { manager.sessionHandler() };
        unsafe {
            handler.beginHandlingWebAuthenticationSessionRequest(Some(&request));
            if cancel {
                let notification = Request::new(url, Some(&request.ivars().uuid));
                handler.cancelWebAuthenticationSessionRequest(Some(&notification));
            }
        }
        // Both local retains go away before main handles the request.
        request.ivars().uuid.UUIDString().to_string()
    })
    .join()
    .unwrap()
}

fn wait_for_events(count: usize) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while EVENTS.lock().unwrap().len() < count && Instant::now() < deadline {
        NSRunLoop::mainRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.01));
    }
    assert_eq!(
        EVENTS.lock().unwrap().len(),
        count,
        "authentication callback was dropped"
    );
}

fn take_events() -> Vec<String> {
    let events = std::mem::take(&mut *EVENTS.lock().unwrap());
    assert!(
        events.iter().all(|(_, main)| *main),
        "callback ran off main"
    );
    events.into_iter().map(|(event, _)| event).collect()
}

fn verify_startup_queue(mtm: MainThreadMarker) {
    deliver_from_worker("https://example.invalid/queued-first", false);
    deliver_from_worker("https://example.invalid/queued-second", false);
    deliver_from_worker("https://example.invalid/queued-canceled", true);
    wait_for_events(1);
    assert_eq!(
        take_events(),
        [format!(
            "cancel:{}:{}",
            unsafe { ASWebAuthenticationSessionErrorDomain },
            ASWebAuthenticationSessionErrorCode::CanceledLogin.0
        )]
    );
    session_handler::finish_startup(mtm);
    assert_eq!(
        take_events(),
        [
            "forward:https://example.invalid/queued-first",
            "forward:https://example.invalid/queued-second",
        ]
    );
    session_handler::finish_startup(mtm);
    assert!(
        take_events().is_empty(),
        "startup requests must only drain once"
    );
    assert!(session_handler::try_complete_callback(
        "grinch-auth-test://done"
    ));
    assert!(!session_handler::try_complete_callback(
        "grinch-auth-test://done"
    ));
    assert_eq!(take_events(), [canceled_event(), canceled_event()]);
    CANCELLATIONS.lock().unwrap().clear();
}

fn canceled_event() -> String {
    format!(
        "cancel:{}:{}",
        unsafe { ASWebAuthenticationSessionErrorDomain },
        ASWebAuthenticationSessionErrorCode::CanceledLogin.0
    )
}

fn verify_cancellation(uuid: &str, reason: &str) {
    wait_for_events(1);
    assert_eq!(take_events(), [canceled_event()]);
    let errors = std::mem::take(&mut *CANCELLATIONS.lock().unwrap());
    assert_eq!(
        errors.len(),
        1,
        "each failed request must finish exactly once"
    );
    assert_eq!(errors[0].0, uuid);
    assert!(
        !errors[0].1.is_empty(),
        "failed requests need an error description"
    );
    assert!(errors[0].1.contains(reason), "unexpected error: {errors:?}");
    assert!(!session_handler::try_complete_callback(
        "grinch-auth-test://done"
    ));
}

fn verify_failed_routes() {
    CANCELLATIONS.lock().unwrap().clear();
    FORWARDING.with(|forwarding| *forwarding.borrow_mut() = Forwarding::Unavailable);
    let uuid = deliver_from_worker("https://example.invalid/no-config", false);
    verify_cancellation(&uuid, "Configuration unavailable");

    for (config, reason) in [
        ("{ default: { appType: 'none' } }", "URL suppressed"),
        (
            "{ default: 'com.grinch.test.missing-browser' }",
            "Browser not found",
        ),
        (
            "{ default: 'com.grinch.test.missing-browser',
               rewrite: [{ match: () => true, url: '--incognito' }] }",
            "Invalid URL",
        ),
        (
            "{ default: 'com.grinch.test.missing-browser',
               rewrite: [{ match: () => true, url: 'https://[invalid' }] }",
            "Invalid URL",
        ),
    ] {
        let engine =
            engine::integration_tests::build_engine(&format!("module.exports = {config};"));
        FORWARDING
            .with(|forwarding| *forwarding.borrow_mut() = Forwarding::Route(Box::new(engine)));
        let uuid = deliver_from_worker("https://example.invalid/failed", false);
        verify_cancellation(&uuid, reason);
    }
}

fn verify_native_launch_failures() {
    // Parse a real bundle identity, then remove the fixture before launch.
    // Both native launch strategies must fail without opening any application.
    for args in ["", "args: ['--test'],"] {
        let path = std::env::temp_dir().join(format!("grinch-{}.app", NSUUID::new().UUIDString()));
        let contents = path.join("Contents");
        std::fs::create_dir_all(&contents).unwrap();
        std::fs::write(
            contents.join("Info.plist"),
            r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict>
                <key>CFBundleIdentifier</key><string>com.grinch.test.unlaunchable</string>
                <key>CFBundleExecutable</key><string>missing</string>
                <key>CFBundlePackageType</key><string>APPL</string>
            </dict></plist>"#,
        )
        .unwrap();
        let config = format!(
            "module.exports = {{ default: {{ name: {}, appType: 'path', {args} }} }};",
            serde_json::to_string(&path).unwrap(),
        );
        let engine = engine::integration_tests::build_engine(&config);
        let log_path = engine.diagnostics().ensure_file().unwrap();
        let resolved = engine.resolve(
            "https://example.invalid/auth",
            &workspace::Opener::default(),
            engine::ModifierFlags::default(),
        );
        assert_eq!(resolved.browser.bundle_id, "com.grinch.test.unlaunchable");
        std::fs::remove_dir_all(&path).unwrap();
        FORWARDING
            .with(|forwarding| *forwarding.borrow_mut() = Forwarding::Route(Box::new(engine)));
        let uuid = deliver_from_worker("https://example.invalid/native-failure", false);
        verify_cancellation(&uuid, "");
        let log = std::fs::read_to_string(log_path).unwrap();
        let event: serde_json::Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
        let strategy = if args.is_empty() {
            "open_urls"
        } else {
            "launch_new_instance"
        };
        assert_eq!(event["strategy"], strategy);
        assert!(
            event["errorDomain"].is_string(),
            "expected a native launch failure"
        );
    }
}

fn verify_late_failures_do_not_finish_twice() {
    let engine = engine::integration_tests::build_engine(
        "module.exports = { default: 'com.grinch.test.missing-browser' };",
    );
    FORWARDING.with(|forwarding| *forwarding.borrow_mut() = Forwarding::Route(Box::new(engine)));
    let manager = unsafe { ASWebAuthenticationSessionWebBrowserSessionManager::sharedManager() };
    let handler = unsafe { manager.sessionHandler() };
    for cancel in [false, true] {
        let request = Request::new("https://example.invalid/late-failure", None);
        unsafe { handler.beginHandlingWebAuthenticationSessionRequest(Some(&request)) };
        if cancel {
            let notification = Request::new(
                "https://example.invalid/late-failure",
                Some(&request.ivars().uuid),
            );
            unsafe { handler.cancelWebAuthenticationSessionRequest(Some(&notification)) };
        } else {
            assert!(session_handler::try_complete_callback(
                "grinch-auth-test://done"
            ));
        }
        dispatch2::DispatchQueue::main().exec_async(|| record("drained".to_string()));
        wait_for_events(2);
        let expected = if cancel {
            canceled_event()
        } else {
            "complete:grinch-auth-test://done".to_string()
        };
        assert_eq!(take_events(), [expected, "drained".to_string()]);
        assert!(!session_handler::try_complete_callback(
            "grinch-auth-test://done"
        ));
        assert_eq!(CANCELLATIONS.lock().unwrap().len(), usize::from(cancel));
        CANCELLATIONS.lock().unwrap().clear();
    }
}

fn verify_ambiguous_callback() {
    let first = deliver_from_worker("https://example.invalid/first", false);
    let second = deliver_from_worker("https://example.invalid/second", false);
    wait_for_events(2);
    take_events();
    assert!(session_handler::try_complete_callback(
        "grinch-auth-test://done"
    ));
    let events = take_events();
    assert_eq!(events, [canceled_event(), canceled_event()]);
    let cancellations = CANCELLATIONS.lock().unwrap();
    assert!(cancellations.iter().any(|(uuid, _)| uuid == &first));
    assert!(cancellations.iter().any(|(uuid, _)| uuid == &second));
    drop(cancellations);
    CANCELLATIONS.lock().unwrap().clear();
    assert!(!session_handler::try_complete_callback(
        "grinch-auth-test://done"
    ));
}

fn verify_actions_window(mtm: MainThreadMarker) {
    let old_home = std::env::var_os("HOME");
    let home = std::env::temp_dir().join(format!("grinch-actions-{}", NSUUID::new().UUIDString()));
    std::fs::create_dir(&home).unwrap();
    let config = home.join(".grinch.js");
    std::fs::write(&config, "module.exports = {};").unwrap();
    unsafe { std::env::set_var("HOME", &home) };
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    let delegate = app_delegate::Delegate::new(mtm);
    let handled: Bool = unsafe {
        msg_send![&*delegate, applicationShouldHandleReopen: &*app, hasVisibleWindows: false]
    };
    assert!(!handled.as_bool());
    assert_eq!(
        app.activationPolicy(),
        NSApplicationActivationPolicy::Accessory
    );
    let windows = app.windows();
    let window = (0..windows.count())
        .map(|i| windows.objectAtIndex(i))
        .find(|window| window.isVisible())
        .unwrap();
    assert!(window.isVisible());
    let content = window.contentView().unwrap();
    let views = content.subviews();
    let mut actions: Vec<Sel> = Vec::new();
    let mut reload: Option<Retained<NSView>> = None;
    let mut status: Option<Retained<NSView>> = None;
    for index in 0..views.count() {
        let view = views.objectAtIndex(index);
        let is_button: bool = unsafe { msg_send![&*view, isKindOfClass: NSButton::class()] };
        if is_button {
            let action: Option<Sel> = unsafe { msg_send![&*view, action] };
            let action = action.unwrap();
            let target: *mut objc2::runtime::AnyObject = unsafe { msg_send![&*view, target] };
            assert_eq!(
                target.cast_const(),
                (&*delegate as *const app_delegate::Delegate).cast()
            );
            if action == sel!(reloadConfig:) {
                reload = Some(view.clone());
            }
            if action == sel!(checkForUpdates:) {
                let enabled: bool = unsafe { msg_send![&*view, isEnabled] };
                assert!(!enabled, "updates must be disabled without Sparkle");
            }
            actions.push(action);
        } else {
            let is_label: bool = unsafe { msg_send![&*view, isKindOfClass: NSTextField::class()] };
            if is_label {
                status = Some(view);
            }
        }
    }
    for action in [
        sel!(openConfig:),
        sel!(reloadConfig:),
        sel!(openDiagnosticLog:),
        sel!(checkForUpdates:),
    ] {
        assert!(actions.contains(&action), "missing action {action:?}");
    }
    let reload = reload.unwrap();
    let status = status.unwrap();
    let _: () = unsafe { msg_send![&*reload, performClick: None::<&objc2::runtime::AnyObject>] };
    let value: Retained<NSString> = unsafe { msg_send![&*status, stringValue] };
    assert!(value.to_string().contains("Config error:"));
    std::fs::write(&config, "module.exports = { default: null, rules: [{ match: 'example.com', browser: 'com.google.Chrome', profile: 'Work' }] };").unwrap();
    let _: () = unsafe { msg_send![&*reload, performClick: None::<&objc2::runtime::AnyObject>] };
    let value: Retained<NSString> = unsafe { msg_send![&*status, stringValue] };
    assert!(value.to_string().contains("Config warning:"));
    std::fs::write(&config, "module.exports = { default: null };").unwrap();
    let _: () = unsafe { msg_send![&*reload, performClick: None::<&objc2::runtime::AnyObject>] };
    let value: Retained<NSString> = unsafe { msg_send![&*status, stringValue] };
    assert_eq!(value.to_string(), "Config loaded");
    verify_window_overlap(mtm, &app, &delegate, &window);
    let _: Bool = unsafe {
        msg_send![&*delegate, applicationShouldHandleReopen: &*app, hasVisibleWindows: false]
    };
    assert!(window.isVisible());
    window.close();
    assert_eq!(
        app.activationPolicy(),
        NSApplicationActivationPolicy::Prohibited
    );
    unsafe {
        if let Some(old_home) = old_home {
            std::env::set_var("HOME", old_home);
        } else {
            std::env::remove_var("HOME");
        }
    }
    std::fs::remove_dir_all(home).unwrap();
}

fn verify_window_overlap(
    mtm: MainThreadMarker,
    app: &NSApplication,
    delegate: &app_delegate::Delegate,
    window: &NSWindow,
) {
    let driver = updater::UserDriverDelegate::new(mtm);
    let _: () = unsafe {
        msg_send![&*driver, standardUserDriverWillHandleShowingUpdate: true,
            forUpdate: &*driver, state: &*driver]
    };
    window.close();
    assert_eq!(
        app.activationPolicy(),
        NSApplicationActivationPolicy::Accessory
    );
    let _: Bool = unsafe {
        msg_send![delegate, applicationShouldHandleReopen: app, hasVisibleWindows: false]
    };
    let _: () = unsafe { msg_send![&*driver, standardUserDriverWillFinishUpdateSession] };
    assert!(window.isVisible());
    assert_eq!(
        app.activationPolicy(),
        NSApplicationActivationPolicy::Accessory
    );
    window.close();
    assert_eq!(
        app.activationPolicy(),
        NSApplicationActivationPolicy::Prohibited
    );
}

fn verify_update_button_binding(mtm: MainThreadMarker) {
    let updater = TestUpdater::alloc().set_ivars(Cell::new(false));
    let updater: Retained<TestUpdater> = unsafe { msg_send![super(updater), init] };
    let app = NSApplication::sharedApplication(mtm);
    let delegate = app_delegate::Delegate::new(mtm);
    *delegate.ivars().updater.borrow_mut() = Some(updater::Updater {
        controller: updater.clone().into_super().into(),
        _user_driver_delegate: updater::UserDriverDelegate::new(mtm),
    });
    let _: Bool = unsafe {
        msg_send![&*delegate, applicationShouldHandleReopen: &*app, hasVisibleWindows: false]
    };
    let windows = app.windows();
    let window = (0..windows.count())
        .map(|i| windows.objectAtIndex(i))
        .find(|window| window.isVisible())
        .unwrap();
    let views = window.contentView().unwrap().subviews();
    let button = (0..views.count())
        .map(|i| views.objectAtIndex(i))
        .find(|view| {
            let is_button: bool = unsafe { msg_send![&**view, isKindOfClass: NSButton::class()] };
            if !is_button {
                return false;
            }
            let action: Option<Sel> = unsafe { msg_send![&**view, action] };
            action == Some(sel!(checkForUpdates:))
        })
        .unwrap();
    let enabled: bool = unsafe { msg_send![&*button, isEnabled] };
    assert!(!enabled);
    for enabled in [true, false, true] {
        let _: () = unsafe { msg_send![&*updater, setCanCheckForUpdates: enabled] };
        let actual: bool = unsafe { msg_send![&*button, isEnabled] };
        assert_eq!(actual, enabled);
    }
    window.close();
}

fn probe_record(event: &str) {
    let path = std::env::var_os("GRINCH_REOPEN_PROBE").unwrap();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    writeln!(file, "{event}").unwrap();
}

fn run_reopen_probe(mtm: MainThreadMarker) {
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    let delegate = ReopenDelegate::alloc(mtm).set_ivars(app_delegate::Delegate::new(mtm));
    let delegate: Retained<ReopenDelegate> = unsafe { msg_send![super(delegate), init] };
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    app.run();
}

struct ReopenProbe {
    child: Child,
    directory: PathBuf,
}

impl Drop for ReopenProbe {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn wait_for_probe_events(path: &Path, count: usize) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let contents = std::fs::read_to_string(path).unwrap_or_default();
        let events: Vec<String> = contents.lines().map(str::to_owned).collect();
        if events.len() >= count {
            return events;
        }
        assert!(
            Instant::now() < deadline,
            "reopen events timed out: {events:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn verify_real_reopen() {
    let id = NSUUID::new().UUIDString().to_string();
    let directory = std::env::temp_dir().join(format!("grinch-reopen-{id}"));
    let bundle = directory.join("ReopenProbe.app");
    let contents = bundle.join("Contents");
    let executable = contents.join("MacOS/probe");
    std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
    std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
    let plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
        <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
        \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\
        <plist version=\"1.0\"><dict><key>CFBundleIdentifier</key>\
        <string>org.grinch.test.reopen-{id}</string><key>CFBundleExecutable</key>\
        <string>probe</string><key>CFBundlePackageType</key><string>APPL</string>\
        <key>LSUIElement</key><true/></dict></plist>"
    );
    std::fs::write(contents.join("Info.plist"), plist).unwrap();
    let events = directory.join("events");
    let child = Command::new(&executable)
        .arg("--reopen-probe")
        .env("HOME", &directory)
        .env("GRINCH_REOPEN_PROBE", &events)
        .spawn()
        .unwrap();
    let _probe = ReopenProbe { child, directory };
    assert_eq!(wait_for_probe_events(&events, 1), ["started"]);
    for count in 2..=3 {
        let status = Command::new("/usr/bin/open").arg(&bundle).status().unwrap();
        assert!(status.success(), "open failed: {status}");
        let recorded = wait_for_probe_events(&events, count);
        assert_eq!(recorded.last().unwrap(), "reopen:1");
    }
}

fn verify_ordinary_startup_delivery(mtm: MainThreadMarker) {
    let directory =
        std::env::temp_dir().join(format!("grinch-url-startup-{}", NSUUID::new().UUIDString()));
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join(".grinch.js"),
        "module.exports = { default: null, options: { logRequests: true } };",
    )
    .unwrap();
    let previous = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", &directory) };
    let app = NSApplication::sharedApplication(mtm);
    let delegate = app_delegate::Delegate::new(mtm);
    let first = NSURL::URLWithString(&NSString::from_str("https://first.example/")).unwrap();
    let second = NSURL::URLWithString(&NSString::from_str("https://second.example/")).unwrap();
    let urls = NSArray::from_retained_slice(&[first, second]);
    let _: () = unsafe { msg_send![&*delegate, application: &*app, openURLs: &*urls] };
    assert!(!directory.join("Library/Logs/Grinch").exists());
    delegate.reload_engine();
    delegate.finish_url_startup();
    delegate.finish_url_startup();
    let logs = directory.join("Library/Logs/Grinch");
    let entries: Vec<_> = std::fs::read_dir(logs)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(entries.len(), 1);
    let body = std::fs::read_to_string(&entries[0]).unwrap();
    assert_eq!(body.lines().count(), 2, "{body}");
    assert!(body.contains("https://first.example/") && body.contains("https://second.example/"));
    unsafe {
        if let Some(home) = previous {
            std::env::set_var("HOME", home);
        } else {
            std::env::remove_var("HOME");
        }
    }
    std::fs::remove_dir_all(directory).unwrap();
}

struct DuplicateProbes {
    children: Vec<Child>,
    directory: PathBuf,
}

impl Drop for DuplicateProbes {
    fn drop(&mut self) {
        for child in &mut self.children {
            if child.try_wait().is_ok_and(|status| status.is_none()) {
                let _ = child.kill();
            }
        }
        for child in &mut self.children {
            let _ = child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn verify_simultaneous_duplicate_startup() {
    let id = NSUUID::new().UUIDString().to_string();
    let bundle_id = format!("org.grinch.test.duplicate-{id}");
    let directory = std::env::temp_dir().join(format!("grinch-duplicate-{id}"));
    let contents = directory.join("Probe.app/Contents");
    let executable = contents.join("MacOS/probe");
    std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
    std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
    let plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict>\
        <key>CFBundleIdentifier</key><string>{bundle_id}</string>\
        <key>CFBundleExecutable</key><string>probe</string>\
        <key>CFBundlePackageType</key><string>APPL</string>\
        <key>LSUIElement</key><true/></dict></plist>"
    );
    std::fs::write(contents.join("Info.plist"), plist).unwrap();
    let mut probes = DuplicateProbes {
        children: Vec::new(),
        directory,
    };
    for index in 0..2 {
        let home = probes.directory.join(format!("home-{index}"));
        std::fs::create_dir_all(&home).unwrap();
        probes.children.push(
            Command::new(&executable)
                .arg("--duplicate-probe")
                .env("HOME", home)
                .env("GRINCH_DUPLICATE_TEST_DIR", &probes.directory)
                .spawn()
                .unwrap(),
        );
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let ready = probes.children.iter().all(|child| {
            probes
                .directory
                .join(format!("ready-{}", child.id()))
                .exists()
        });
        let registered = NSRunningApplication::runningApplicationsWithBundleIdentifier(
            &NSString::from_str(&bundle_id),
        )
        .count()
            >= 2;
        if ready && registered {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "duplicate probes did not register"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    std::fs::write(probes.directory.join("start"), "start").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut alive = Vec::new();
        for child in &mut probes.children {
            if child.try_wait().unwrap().is_none() {
                alive.push(child.id());
            }
        }
        if alive.len() == 1
            && probes
                .directory
                .join(format!("passed-{}", alive[0]))
                .exists()
        {
            break;
        }
        assert!(!alive.is_empty(), "both duplicate instances exited");
        assert!(
            Instant::now() < deadline,
            "duplicate cleanup did not leave one running instance"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn main() {
    let mtm = MainThreadMarker::new().expect("harness must own the main thread");
    if std::env::args().any(|arg| arg == "--reopen-probe") {
        run_reopen_probe(mtm);
        return;
    }
    if std::env::args().any(|arg| arg == "--duplicate-probe") {
        run_duplicate_probe(mtm);
        return;
    }
    verify_real_reopen();
    verify_ordinary_startup_delivery(mtm);
    verify_simultaneous_duplicate_startup();
    verify_update_button_binding(mtm);
    verify_actions_window(mtm);
    session_handler::install(mtm, forwarded);
    verify_startup_queue(mtm);
    verify_ambiguous_callback();

    deliver_from_worker("https://example.invalid/auth", false);
    assert!(
        EVENTS.lock().unwrap().is_empty(),
        "request must wait for main"
    );
    wait_for_events(1);
    assert!(session_handler::try_complete_callback(
        "grinch-auth-test://done"
    ));
    assert!(!session_handler::try_complete_callback(
        "grinch-auth-test://done"
    ));
    assert_eq!(
        take_events(),
        [
            "forward:https://example.invalid/auth",
            "complete:grinch-auth-test://done",
        ]
    );

    deliver_from_worker("https://example.invalid/canceled", true);
    wait_for_events(2);
    assert!(!session_handler::try_complete_callback(
        "grinch-auth-test://done"
    ));
    assert_eq!(
        take_events(),
        [
            "forward:https://example.invalid/canceled".to_string(),
            format!(
                "cancel:{}:{}",
                unsafe { ASWebAuthenticationSessionErrorDomain },
                ASWebAuthenticationSessionErrorCode::CanceledLogin.0
            ),
        ]
    );

    deliver_from_worker("", false);
    wait_for_events(1);
    assert_eq!(take_events(), [canceled_event()]);
    assert!(!session_handler::try_complete_callback(
        "grinch-auth-test://done"
    ));

    // Missing and already-removed requests must not produce another completion.
    let manager = unsafe { ASWebAuthenticationSessionWebBrowserSessionManager::sharedManager() };
    let handler = unsafe { manager.sessionHandler() };
    let unknown = Request::new("https://example.invalid/unknown", None);
    unsafe {
        handler.beginHandlingWebAuthenticationSessionRequest(None);
        handler.cancelWebAuthenticationSessionRequest(None);
        handler.cancelWebAuthenticationSessionRequest(Some(&unknown));
    }
    assert!(take_events().is_empty());
    verify_failed_routes();
    verify_native_launch_failures();
    verify_late_failures_do_not_finish_twice();
    println!("Authentication startup, completion, cancellation, and launch failure checks passed");
}
