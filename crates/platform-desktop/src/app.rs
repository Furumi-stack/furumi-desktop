/// Applies the Furumi federation mark as the native application icon.
///
/// Slint forwards its window icon to Windows and X11. macOS deliberately
/// ignores that window-level API, so its Dock icon is installed through
/// `AppKit` from the same embedded SVG asset. On macOS this must be called
/// from the main event loop after `applicationDidFinishLaunching`.
pub fn set_application_icon() {
    if let Err(error) = set_native_application_icon() {
        eprintln!("failed to install the Furumi application icon: {error}");
    }
}

#[cfg(target_os = "macos")]
fn set_native_application_icon() -> Result<(), &'static str> {
    use std::ffi::c_void;

    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    let main_thread = MainThreadMarker::new().ok_or("not running on the macOS main thread")?;
    let svg = include_bytes!("../../ui/assets/federation.svg");
    // SAFETY: `dataWithBytes_length` copies `svg`; its pointer is valid for
    // the complete duration of the Objective-C call.
    let data = unsafe { NSData::dataWithBytes_length(svg.as_ptr().cast::<c_void>(), svg.len()) };
    let icon = NSImage::initWithData(main_thread.alloc(), &data)
        .ok_or("AppKit could not decode the embedded federation SVG")?;
    let application = NSApplication::sharedApplication(main_thread);
    // SAFETY: AppKit accepts a live NSImage here and retains it as the
    // application's Dock icon. This function is restricted to the main thread.
    unsafe { application.setApplicationIconImage(Some(&icon)) };
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn set_native_application_icon() -> Result<(), &'static str> {
    Ok(())
}
