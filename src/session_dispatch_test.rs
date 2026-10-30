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

use std::cell::RefCell;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::{Bool, ProtocolObject, Sel};
use objc2::{
    AnyThread, ClassType, DefinedClass, MainThreadMarker, Message, define_class, msg_send, sel,
};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSButton, NSTextField, NSView};
use objc2_authentication_services::{
    ASWebAuthenticationSessionCallback, ASWebAuthenticationSessionErrorCode,
    ASWebAuthenticationSessionErrorDomain, ASWebAuthenticationSessionRequest,
    ASWebAuthenticationSessionRequestDelegate, ASWebAuthenticationSessionWebBrowserSessionHandling,
    ASWebAuthenticationSessionWebBrowserSessionManager,
};
use objc2_foundation::{
    NSDate, NSError, NSObject, NSObjectProtocol, NSRunLoop, NSString, NSURL, NSUUID,
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
    for _ in 0..2 {
        assert!(session_handler::try_complete_callback(
            "grinch-auth-test://done"
        ));
    }
    assert!(!session_handler::try_complete_callback(
        "grinch-auth-test://done"
    ));
    assert_eq!(
        take_events(),
        [
            "complete:grinch-auth-test://done",
            "complete:grinch-auth-test://done"
        ]
    );
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
    let window = windows.objectAtIndex(0);
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
    std::fs::write(&config, "module.exports = { default: null };").unwrap();
    let _: () = unsafe { msg_send![&*reload, performClick: None::<&objc2::runtime::AnyObject>] };
    let value: Retained<NSString> = unsafe { msg_send![&*status, stringValue] };
    assert_eq!(value.to_string(), "Config loaded");
    window.close();
    assert_eq!(
        app.activationPolicy(),
        NSApplicationActivationPolicy::Prohibited
    );
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

fn main() {
    let mtm = MainThreadMarker::new().expect("harness must own the main thread");
    verify_actions_window(mtm);
    session_handler::install(mtm, forwarded);
    verify_startup_queue(mtm);

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
