// macOS: the client is in the Dock (and the app switcher) only while it has a window to show.
//
// A session with no window open - its last application closed, or an empty desktop just
// attached - still left a Dock icon that did nothing when clicked: there was nothing to bring
// forward, and winit offers no hook for the click (`applicationShouldHandleReopen`). The session
// is still attached, and the next window the server maps brings the icon back.
//
// This is AppKit's activation policy - `Regular` with an icon, `Accessory` without - switched at
// run time, which winit only lets us choose once, before the event loop starts. The AppKit
// bindings are the ones winit already links, so nothing new is compiled in.

#[cfg(target_os = "macos")]
pub fn set_visible(visible: bool) {
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    use objc2_foundation::MainThreadMarker;

    // only ever called from the event loop's callbacks, which run on the main thread
    let Some(mtm) = MainThreadMarker::new() else { return };
    let app = NSApplication::sharedApplication(mtm);
    let policy = if visible { NSApplicationActivationPolicy::Regular } else { NSApplicationActivationPolicy::Accessory };
    app.setActivationPolicy(policy);
    if visible {
        // an Accessory app's window opens behind whatever is active: the user just asked for it
        // (launched an application), so bring it forward
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }
}

#[cfg(not(target_os = "macos"))]
pub fn set_visible(_visible: bool) {}
