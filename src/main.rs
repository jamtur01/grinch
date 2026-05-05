use objc2::runtime::ProtocolObject;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

mod app_delegate;
mod chromium;
mod engine;
mod helpers;
mod loader;
mod workspace;

use app_delegate::Delegate;

fn main() {
    let mtm = MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);

    let delegate = Delegate::new(mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));

    // No Dock icon; we live in the menu bar.
    app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);

    app.run();
}
