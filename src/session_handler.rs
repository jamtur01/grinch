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
// **Flow in two halves.** (1) On `beginHandlingWebAuthenticationSession
// Request:`, we stash the request keyed by its UUID and forward its
// URL through the same `Engine::resolve` machinery that handles
// regular clicks. The user's chosen browser opens the URL as a normal
// tab; the auth flow proceeds in that browser. (2) Eventually the
// browser navigates to the callback URL (a custom scheme like
// `slack://oauth-callback?token=…`); macOS routes the scheme to
// Grinch's regular `handle_url` / `application:openURLs:` /
// `application:continueUserActivity:` entrypoints; before each of
// those routes the URL through `engine.resolve` it calls
// `try_complete_callback` here. That walks the pending-request map,
// finds the session whose `callbackURLScheme` matches the incoming
// URL's scheme, and calls `completeWithCallbackURL:` — letting the
// originating app's session-API completion handler fire normally and
// dismissing the auth dialog cleanly. The callback URL is *not*
// routed onward as a click in that case.
//
// **The one limitation that remains.** A session that strictly
// requires the session-API completion path AND uses `http`/`https`
// as its `callbackURLScheme` (Apple's recommended modern shape via
// `ASWebAuthenticationSessionCallback.https(host:path:)`) won't be
// auto-completed, because we can't distinguish "user finished
// authenticating, this is the callback" from "user clicked an
// ordinary https:// link". Those flows still work but the originating
// app's session dialog may sit waiting until timeout. Custom-scheme
// callbacks (the common case) complete normally.

use std::cell::RefCell;
use std::collections::HashMap;

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, MainThreadMarker, MainThreadOnly, Message};
use objc2_authentication_services::{
    ASWebAuthenticationSessionRequest, ASWebAuthenticationSessionWebBrowserSessionHandling,
    ASWebAuthenticationSessionWebBrowserSessionManager,
};
// ASWebAuthenticationSessionCallback isn't used by name, but its
// feature flag in Cargo.toml is what unlocks `request.callback()`.
use objc2_foundation::{NSError, NSString, NSURL};

use crate::engine::{Engine, ModifierFlags};
use crate::workspace::{open_url, Opener};

// Pending session requests, keyed by UUID string. Lives on the main
// thread (the auth-services manager only dispatches on main, and the
// URL handlers that try-complete are also main-thread-only), so a
// `thread_local!` `RefCell` is sufficient — no `Mutex` ceremony, no
// `Send`/`Sync` worries for the held `Retained<…Request>`.
thread_local! {
    static PENDING: RefCell<HashMap<String, Retained<ASWebAuthenticationSessionRequest>>> =
        RefCell::new(HashMap::new());
}

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
        // Stash the request in the pending map so the eventual
        // callback can complete it, then forward the start URL through
        // engine.resolve so the user's preferred browser opens the
        // auth flow.
        #[unsafe(method(beginHandlingWebAuthenticationSessionRequest:))]
        fn begin_handling(&self, request: Option<&ASWebAuthenticationSessionRequest>) {
            let Some(request) = request else { return };
            let url = unsafe { request.URL() };
            let href = url
                .absoluteString()
                .map(|s| s.to_string())
                .unwrap_or_default();
            if href.is_empty() {
                // No URL → can't route, can't complete. Tell the
                // originating app immediately so its completion handler
                // fires with a real error rather than hanging.
                let domain = NSString::from_str("com.grinch.AuthSession");
                let err = unsafe { NSError::errorWithDomain_code_userInfo(&domain, 1, None) };
                unsafe { request.cancelWithError(&err) };
                return;
            }
            // Stash before forwarding so a (theoretical) immediate
            // callback round-trip can't beat us to the lookup. UUID
            // string is the dictionary key; the retain count of the
            // request is bumped here and decremented in
            // try_complete_callback / cancel_handling.
            let uuid = unsafe { request.UUID() }.UUIDString().to_string();
            PENDING.with(|p| {
                p.borrow_mut().insert(uuid, request.retain());
            });
            // SAFETY: FORWARDER set on main thread during AppDelegate
            // launch; this method also runs on main thread. Sequential
            // single-thread access; no race.
            let cb = unsafe { FORWARDER };
            if let Some(cb) = cb {
                cb(&href);
            }
        }

        // Called when the originating app cancels the session before
        // we get to deliver a callback. Drop the pending entry so the
        // retained request can be released.
        #[unsafe(method(cancelWebAuthenticationSessionRequest:))]
        fn cancel_handling(&self, request: Option<&ASWebAuthenticationSessionRequest>) {
            let Some(request) = request else { return };
            let uuid = unsafe { request.UUID() }.UUIDString().to_string();
            PENDING.with(|p| {
                p.borrow_mut().remove(&uuid);
            });
        }
    }
);

/// Try to complete a pending session by asking each pending request's
/// `ASWebAuthenticationSessionCallback` whether it matches the incoming
/// URL. Returns `true` when a session was completed (caller must NOT
/// route the URL onward — its job is done), `false` when no pending
/// session claims this URL (caller should route it normally via the
/// engine).
///
/// Uses the modern `callback` API (`matchesURL:`) instead of the
/// deprecated `callbackURLScheme` so the match covers both shapes
/// uniformly:
///
/// - Custom-scheme callbacks (`slack://oauth-callback?…`) — declared
///   via `callbackWithCustomScheme:`. The most common form.
/// - HTTPS callbacks (`https://auth.myapp.com/cb?…`) — declared via
///   `callbackWithHTTPSHost:path:`, the Universal-Links shape that
///   Apple recommends for modern flows. `callbackURLScheme` is nil
///   for these, so the deprecated API would silently miss them.
///
/// `matchesURL:` knows the rules for both — it rejects arbitrary
/// `http`/`https` URLs that don't match a registered host+path, so
/// passing every URL through this function is safe: a user's regular
/// web click won't be eaten as an auth callback.
pub fn try_complete_callback(url_str: &str) -> bool {
    let Some(url_ns) = NSURL::URLWithString(&NSString::from_str(url_str)) else {
        return false;
    };
    PENDING.with(|p| {
        // Snapshot UUIDs before the walk so we can mutate the map
        // while inspecting it. Obj-C enforces this via `allKeys`; Rust
        // via the borrow checker.
        let keys: Vec<String> = p.borrow().keys().cloned().collect();
        for key in keys {
            let request = match p.borrow().get(&key) {
                Some(r) => r.clone(),
                None => continue,
            };
            let Some(callback) = (unsafe { request.callback() }) else {
                continue;
            };
            if !unsafe { callback.matchesURL(&url_ns) } {
                continue;
            }
            unsafe { request.completeWithCallbackURL(&url_ns) };
            p.borrow_mut().remove(&key);
            return true;
        }
        false
    })
}

impl Handler {
    pub fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_authentication_services::ASWebAuthenticationSessionCallback;

    #[test]
    fn callback_matchesurl_recognises_custom_scheme_match() {
        // The framework's matchesURL: is what `try_complete_callback`
        // delegates to. Verify it behaves as the docs claim for a
        // custom-scheme callback registered via `callbackWithCustom
        // Scheme:` — exact-scheme match, case-insensitive.
        let cb = unsafe {
            ASWebAuthenticationSessionCallback::callbackWithCustomScheme(&NSString::from_str(
                "slack",
            ))
        };
        let url = NSURL::URLWithString(&NSString::from_str("slack://oauth?token=x")).unwrap();
        let bad = NSURL::URLWithString(&NSString::from_str("claude://oauth?token=x")).unwrap();
        let web = NSURL::URLWithString(&NSString::from_str("https://slack.com/oauth")).unwrap();
        assert!(unsafe { cb.matchesURL(&url) });
        assert!(!unsafe { cb.matchesURL(&bad) });
        // Most importantly: a regular https URL with the same hostname
        // as the scheme isn't a match. The framework knows the rules.
        assert!(!unsafe { cb.matchesURL(&web) });
    }

    #[test]
    fn try_complete_callback_returns_false_when_no_pending_requests() {
        // Sanity: empty pending map → never a match. The PENDING
        // thread-local is process-wide; this test is order-sensitive
        // with anything else that adds to it, but since we never
        // populate it from a unit test (would require building a real
        // ASWebAuthenticationSessionRequest, which the framework
        // doesn't let us construct directly), this is always-empty
        // in unit-test context.
        assert!(!try_complete_callback("slack://oauth?token=x"));
        assert!(!try_complete_callback("not-a-url"));
        assert!(!try_complete_callback(""));
    }
}
