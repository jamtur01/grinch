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

mod chromium;
mod engine;
mod firefox;
mod helpers;
mod loader;
mod session_handler;
mod workspace;

use std::sync::Mutex;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::{AnyThread, DefinedClass, MainThreadMarker, define_class, msg_send};
use objc2_authentication_services::{
    ASWebAuthenticationSessionCallback, ASWebAuthenticationSessionErrorCode,
    ASWebAuthenticationSessionErrorDomain, ASWebAuthenticationSessionRequest,
    ASWebAuthenticationSessionWebBrowserSessionHandling,
    ASWebAuthenticationSessionWebBrowserSessionManager,
};
use objc2_foundation::{NSDate, NSError, NSObjectProtocol, NSRunLoop, NSString, NSURL, NSUUID};

static EVENTS: Mutex<Vec<(String, bool)>> = Mutex::new(Vec::new());

struct RequestIvars {
    url: Retained<NSURL>,
    uuid: Retained<NSUUID>,
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

        #[unsafe(method(completeWithCallbackURL:))]
        fn complete(&self, url: &NSURL) {
            record(format!("complete:{}", url.absoluteString().unwrap()));
        }

        #[unsafe(method(cancelWithError:))]
        fn cancel(&self, error: &NSError) {
            record(format!("cancel:{}:{}", error.domain(), error.code()));
        }
    }
);

impl Request {
    fn new(href: &str) -> Retained<Self> {
        let this = Self::alloc().set_ivars(RequestIvars {
            url: NSURL::URLWithString(&NSString::from_str(href)).unwrap(),
            uuid: NSUUID::new(),
        });
        unsafe { msg_send![super(this), init] }
    }
}

fn record(event: String) {
    EVENTS
        .lock()
        .unwrap()
        .push((event, MainThreadMarker::new().is_some()));
}

fn forwarded(url: &str, _mtm: MainThreadMarker) {
    record(format!("forward:{url}"));
}

fn deliver_from_worker(url: &'static str, cancel: bool) {
    std::thread::spawn(move || {
        assert!(MainThreadMarker::new().is_none());
        let request = Request::new(url);
        let manager =
            unsafe { ASWebAuthenticationSessionWebBrowserSessionManager::sharedManager() };
        let handler = unsafe { manager.sessionHandler() };
        unsafe {
            handler.beginHandlingWebAuthenticationSessionRequest(Some(&request));
            if cancel {
                handler.cancelWebAuthenticationSessionRequest(Some(&request));
            }
        }
        // Both local retains go away before main handles the request.
    })
    .join()
    .unwrap();
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

fn main() {
    let mtm = MainThreadMarker::new().expect("harness must own the main thread");
    session_handler::install(mtm, forwarded);

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
    assert_eq!(take_events(), ["cancel:com.grinch.AuthSession:1"]);
    assert!(!session_handler::try_complete_callback(
        "grinch-auth-test://done"
    ));

    // Nil framework requests must be harmless and leave no pending session.
    let manager = unsafe { ASWebAuthenticationSessionWebBrowserSessionManager::sharedManager() };
    let handler = unsafe { manager.sessionHandler() };
    unsafe {
        handler.beginHandlingWebAuthenticationSessionRequest(None);
        handler.cancelWebAuthenticationSessionRequest(None);
    }
    assert!(take_events().is_empty());
    println!(
        "Authentication request delivery, completion, cancellation, and invalid URL checks passed"
    );
}
