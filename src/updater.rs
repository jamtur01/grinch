// Sparkle auto-updates. The Makefile embeds Sparkle.framework in
// Grinch.app/Contents/Frameworks and this module loads it at runtime, so
// `cargo build`, `cargo test`, and the CLI modes never need the framework.
// Ad-hoc builds ship without it (scripts/sign-app.sh), which leaves the
// updater, and its menu items, absent.
//
// Grinch runs with the Prohibited activation policy, which can't show
// windows. Sparkle's windows need Accessory, so the policy switches to
// Accessory while Sparkle has UI up and back when its update session ends.

use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, NSObject};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_foundation::{NSBundle, NSString};

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "GrinchUpdaterUserDriverDelegate"]
    struct UserDriverDelegate;

    impl UserDriverDelegate {
        // Sparkle logs a warning for menu bar apps that schedule update
        // checks without opting in to gentle reminders. Returning YES keeps
        // Sparkle's default presentation.
        #[unsafe(method(supportsGentleScheduledUpdateReminders))]
        fn supports_gentle_scheduled_update_reminders(&self) -> bool {
            true
        }

        // A scheduled check found an update; Sparkle is about to show it.
        #[unsafe(method(standardUserDriverWillHandleShowingUpdate:forUpdate:state:))]
        fn will_handle_showing_update(
            &self,
            _handle_showing_update: bool,
            _update: &AnyObject,
            _state: &AnyObject,
        ) {
            allow_windows(self.mtm(), true);
        }

        #[unsafe(method(standardUserDriverWillFinishUpdateSession))]
        fn will_finish_update_session(&self) {
            allow_windows(self.mtm(), false);
        }
    }
);

impl UserDriverDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

pub struct Updater {
    controller: Retained<AnyObject>,
    // Sparkle references its delegates weakly; the updater owns this one.
    _user_driver_delegate: Retained<UserDriverDelegate>,
}

impl Updater {
    /// Load the embedded Sparkle.framework and create an updater that has not
    /// started yet, so callers can apply `options.checkForUpdates` before the
    /// first scheduled check.
    ///
    /// Returns `None` when the app bundle has no Sparkle.framework or it fails
    /// to load.
    pub fn load(mtm: MainThreadMarker) -> Option<Self> {
        let frameworks = NSBundle::mainBundle().privateFrameworksPath()?;
        let path =
            frameworks.stringByAppendingPathComponent(&NSString::from_str("Sparkle.framework"));
        let bundle = NSBundle::bundleWithPath(&path)?;
        // Loading runs the framework's initializers. Sparkle.framework is
        // part of Grinch's own signed bundle, verified with it by codesign.
        if let Err(error) = unsafe { bundle.loadAndReturnError() } {
            eprintln!(
                "grinch: couldn't load {path} — updates are disabled; reinstall Grinch from the \
                 latest release: {}",
                error.localizedDescription()
            );
            return None;
        }
        let Some(class) = AnyClass::get(c"SPUStandardUpdaterController") else {
            eprintln!("grinch: {path} has no SPUStandardUpdaterController — updates are disabled");
            return None;
        };
        let user_driver_delegate = UserDriverDelegate::new(mtm);
        let controller: Option<Retained<AnyObject>> = unsafe {
            let allocated: Allocated<AnyObject> = msg_send![class, alloc];
            msg_send![
                allocated,
                initWithStartingUpdater: false,
                updaterDelegate: None::<&AnyObject>,
                userDriverDelegate: Some(&*user_driver_delegate),
            ]
        };
        let Some(controller) = controller else {
            eprintln!(
                "grinch: SPUStandardUpdaterController init returned nil — updates are disabled"
            );
            return None;
        };
        Some(Self {
            controller,
            _user_driver_delegate: user_driver_delegate,
        })
    }

    /// Start Sparkle's update cycle. Sparkle shows its own alert if the app
    /// is misconfigured (for example, a missing feed URL or public key).
    pub fn start(&self) {
        unsafe { msg_send![&self.controller, startUpdater] }
    }

    /// User-initiated check. Sparkle shows a progress window straight away.
    pub fn check_now(&self, mtm: MainThreadMarker) {
        allow_windows(mtm, true);
        unsafe { msg_send![&self.controller, checkForUpdates: None::<&AnyObject>] }
    }

    /// False while an update session is already running.
    pub fn can_check_now(&self) -> bool {
        unsafe {
            let updater: Retained<AnyObject> = msg_send![&self.controller, updater];
            msg_send![&updater, canCheckForUpdates]
        }
    }

    pub fn automatically_checks(&self) -> bool {
        unsafe {
            let updater: Retained<AnyObject> = msg_send![&self.controller, updater];
            msg_send![&updater, automaticallyChecksForUpdates]
        }
    }

    /// Sparkle persists this in Grinch's user defaults.
    pub fn set_automatically_checks(&self, enabled: bool) {
        unsafe {
            let updater: Retained<AnyObject> = msg_send![&self.controller, updater];
            let _: () = msg_send![&updater, setAutomaticallyChecksForUpdates: enabled];
        }
    }
}

/// Switch between the Prohibited policy Grinch normally runs with and the
/// Accessory policy Sparkle's windows need. Neither shows a Dock icon.
fn allow_windows(mtm: MainThreadMarker, allow: bool) {
    let policy = if allow {
        NSApplicationActivationPolicy::Accessory
    } else {
        NSApplicationActivationPolicy::Prohibited
    };
    NSApplication::sharedApplication(mtm).setActivationPolicy(policy);
}
