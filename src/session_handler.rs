// ASWebAuthenticationSession handler.
//
// **What this fixes.** Apps that use `ASWebAuthenticationSession` for
// SSO/OAuth popups (Slack login, Claude Desktop login, many corporate
// auth flows, password-manager extensions) don't dispatch through the
// regular `http://` default-browser chain. macOS routes them to a
// special "trusted browser" via
// `ASWebAuthenticationSessionWebBrowserSessionManager`, and only apps
// that declare `ASWebAuthenticationSessionWebBrowserSupportCapabilities`
// in Info.plist with `IsSupported: true` are considered eligible.
// Without the declaration, macOS falls back to Safari regardless of the
// user's default-browser choice — which is the bug
// johnste/finicky#405 documents.
//
// **What we do.** Declare the capability (see Info.plist), register a
// `Delegate`-side handler that conforms to the
// `ASWebAuthenticationSessionWebBrowserSessionHandling` protocol, and
// when a session request lands forward its URL through the same
// `Engine::resolve` machinery that handles regular clicks. The user's
// chosen browser opens the URL as a normal tab; the auth flow proceeds
// in that browser; the callback URL eventually navigates back through
// the OS (custom scheme like `slack://oauth-callback?token=…`) to the
// originating app via its standard URL-handler registration.
//
// **What we don't do.** We never call `completeWithCallbackURL:` on the
// request — that requires the browser to intercept the callback
// navigation, which only a real WKWebView-hosting browser can do. The
// app's session API completion handler may sit waiting until the
// session times out, or may resolve via the app's registered URL
// handler if it has one (most do — Slack, Claude Desktop, Microsoft
// auth, GitHub, Google all register fallback URL handlers). The
// trade-off: SSO works in 95%+ of real flows; a session that strictly
// requires the session-API completion path (rare; ephemeral-only,
// no fallback registered) may need to be cancelled manually by the
// user. README documents the limitation.

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, MainThreadMarker, MainThreadOnly};
use objc2_authentication_services::{
    ASWebAuthenticationSessionRequest, ASWebAuthenticationSessionWebBrowserSessionHandling,
    ASWebAuthenticationSessionWebBrowserSessionManager,
};

use crate::engine::{Engine, ModifierFlags};
use crate::workspace::{open_url, Opener};

/// Installed lazily by `install` and called from the handler's protocol
/// methods. Holds a function pointer that takes the request URL string
/// and looks up the configured engine via the AppDelegate, then routes
/// the URL through `engine.resolve` and `open_url`. The indirection
/// avoids the handler holding a strong reference to the delegate
/// (and the engine inside it), which would otherwise create a retain
/// cycle (manager → handler → delegate → manager).
type Forwarder = fn(&str);

static mut FORWARDER: Option<Forwarder> = None;

/// One-shot installer. Sets the shared session manager's handler to a
/// new `Handler` instance and stashes the URL-forwarding callback so
/// the handler can reach back into engine.resolve / open_url. Safe to
/// call once per process; subsequent calls overwrite the manager's
/// handler (cheap, idempotent in practice).
pub fn install(mtm: MainThreadMarker, forwarder: Forwarder) {
    // SAFETY: only called from the main thread during AppDelegate
    // `applicationDidFinishLaunching:`. FORWARDER is a function pointer
    // read on the same thread by the handler's protocol methods.
    unsafe {
        FORWARDER = Some(forwarder);
    }
    let handler = Handler::new(mtm);
    let manager = unsafe { ASWebAuthenticationSessionWebBrowserSessionManager::sharedManager() };
    let proto: &ProtocolObject<dyn ASWebAuthenticationSessionWebBrowserSessionHandling> =
        ProtocolObject::from_ref(&*handler);
    unsafe { manager.setSessionHandler(proto) };
    // Leak the Rust handle — the manager holds its own retain via the
    // setter, and we never want to remove the handler.
    std::mem::forget(handler);
}

/// The most common URL-route shape: a synthetic opener (no real
/// frontmost-app context — auth sessions don't carry the caller pid
/// in a way we can query) and default modifier flags. Used by both
/// `install` callers and the protocol method bodies.
pub fn forward_through_engine(url: &str, engine: &Engine, mtm: MainThreadMarker) {
    let opener = Opener {
        bundle_id: "com.apple.AuthenticationServices".to_string(),
        name: "AuthenticationServices".to_string(),
        path: String::new(),
        pid: 0,
    };
    let result = engine.resolve(url, &opener, ModifierFlags::default());
    if result.browser.bundle_id.is_empty() {
        return; // suppressed (rule explicitly dropped)
    }
    open_url(&result.url, &result.browser, mtm);
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "GrinchAuthSessionHandler"]
    #[ivars = ()]
    pub struct Handler;

    unsafe impl NSObjectProtocol for Handler {}

    unsafe impl ASWebAuthenticationSessionWebBrowserSessionHandling for Handler {
        // Apple calls this on the main thread when an app starts a new
        // ASWebAuthenticationSession and Grinch is the registered
        // session handler. The request's `URL` is the auth provider's
        // start URL (e.g. `https://slack.com/oauth/v2/authorize?...`).
        // Forward it through engine.resolve so the user's preferred
        // browser opens the auth flow.
        #[unsafe(method(beginHandlingWebAuthenticationSessionRequest:))]
        fn begin_handling(&self, request: Option<&ASWebAuthenticationSessionRequest>) {
            let Some(request) = request else { return };
            let url = unsafe { request.URL() };
            let href = url
                .absoluteString()
                .map(|s| s.to_string())
                .unwrap_or_default();
            if href.is_empty() {
                return;
            }
            // SAFETY: FORWARDER set on main thread during AppDelegate
            // launch; this method also runs on main thread. Sequential
            // single-thread access; no race.
            let cb = unsafe { FORWARDER };
            if let Some(cb) = cb {
                cb(&href);
            }
        }

        // Called when the originating app cancels the session before we
        // complete it. Grinch never holds the session open (it's
        // forwarded immediately and forgotten), so cancellation is a
        // no-op for us — the browser tab the user opened stays open
        // for them to close.
        #[unsafe(method(cancelWebAuthenticationSessionRequest:))]
        fn cancel_handling(&self, _request: Option<&ASWebAuthenticationSessionRequest>) {
            // Intentionally empty.
        }
    }
);

impl Handler {
    pub fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}
