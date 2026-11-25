//! Window chrome helpers shared with paper-rs: traffic lights inset into the
//! unified toolbar of a transparent-titlebar window.

use objc2_app_kit::{NSView, NSWindow, NSWindowButton};
use objc2_foundation::NSPoint;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

fn ns_window(window: &Window) -> Option<objc2::rc::Retained<NSWindow>> {
    let RawWindowHandle::AppKit(h) = window.window_handle().ok()?.as_raw() else {
        return None;
    };
    // SAFETY: winit hands out a valid NSView pointer for the lifetime of the window, and we
    // are on the main thread (all winit window callbacks are).
    let view: &NSView = unsafe { h.ns_view.cast().as_ref() };
    view.window()
}

/// Whether none of the window can be seen (covered, minimized, on another
/// Space, or the screen locked): drawing it would be wasted.
pub fn is_occluded(window: &Window) -> bool {
    ns_window(window).is_some_and(|w| !w.occlusionState().contains(objc2_app_kit::NSWindowOcclusionState::Visible))
}

/// Tag the window's Metal layers sRGB, so the window server color-manages
/// what we draw (sRGB values) for the display instead of showing them as
/// display-native values.
pub fn tag_layers_srgb(window: &Window) {
    use objc2::runtime::AnyObject;
    use objc2::{msg_send, ClassType};
    let RawWindowHandle::AppKit(h) = window.window_handle().map(|h| h.as_raw()).unwrap_or(RawWindowHandle::Web(winit::raw_window_handle::WebWindowHandle::new(0))) else {
        return;
    };
    // SAFETY: valid NSView on the main thread (see `ns_window`).
    let view: &NSView = unsafe { h.ns_view.cast().as_ref() };
    let Some(cs) = objc2_core_graphics::CGColorSpace::with_name(Some(unsafe { objc2_core_graphics::kCGColorSpaceSRGB })) else { return };
    unsafe {
        let Some(root) = view.layer() else { return };
        let mut layers = vec![root.clone()];
        if let Some(subs) = root.sublayers() {
            layers.extend(subs.iter());
        }
        for l in layers {
            let any: &AnyObject = &l;
            if any.class().name().to_str().unwrap_or("") == objc2_quartz_core::CAMetalLayer::class().name().to_str().unwrap_or("-") {
                let _: () = msg_send![any, setColorspace: &*cs];
            }
        }
    }
}

/// Move the close/minimise/zoom buttons so they start `x` points from the left and are
/// vertically centred in a titlebar area `bar_h` points tall (our toolbar). AppKit lays the
/// titlebar out again on show, resize and fullscreen changes, so this is re-applied from
/// redraw; it returns early when the buttons are already in place.
pub fn inset_traffic_lights(window: &Window, x: f64, bar_h: f64) {
    let Some(win) = ns_window(window) else { return };
    let (Some(close), Some(mini), Some(zoom)) = (
        win.standardWindowButton(NSWindowButton::CloseButton),
        win.standardWindowButton(NSWindowButton::MiniaturizeButton),
        win.standardWindowButton(NSWindowButton::ZoomButton),
    ) else {
        return;
    };
    // SAFETY: plain view-hierarchy getters on main-thread AppKit objects.
    let Some(container) = (unsafe { close.superview().and_then(|v| v.superview()) }) else {
        return;
    };
    let close_rect = close.frame();
    let btn_y = ((bar_h - close_rect.size.height) / 2.0).round();
    if (close_rect.origin.x - x).abs() < 0.5
        && (close_rect.origin.y - btn_y).abs() < 0.5
        && (container.frame().size.height - bar_h).abs() < 0.5
    {
        return;
    }
    let mut bar = container.frame();
    bar.size.height = bar_h;
    bar.origin.y = win.frame().size.height - bar_h;
    container.setFrame(bar);
    // The buttons' own superview (NSTitlebarView) must grow with the container.
    // SAFETY: as above.
    if let Some(tv) = unsafe { close.superview() } {
        let mut f = tv.frame();
        f.origin.y = 0.0;
        f.size.height = bar_h;
        tv.setFrame(f);
    }
    let gap = mini.frame().origin.x - close_rect.origin.x;
    for (i, b) in [close, mini, zoom].iter().enumerate() {
        b.setFrameOrigin(NSPoint::new(x + gap * i as f64, btn_y));
    }
}


/// Bring the app (an accessory app without a Dock icon) to the front.
pub fn activate() {
    if let Some(mtm) = objc2::MainThreadMarker::new() {
        objc2_app_kit::NSApplication::sharedApplication(mtm).activate();
    }
}

/// Reveal a file in Finder.
pub fn reveal(path: &std::path::Path) {
    use objc2_foundation::{NSArray, NSURL};
    // With the recording's separate screen and camera files, if any.
    let urls: Vec<_> =
        crate::separate::recording_files(path).iter().map(|p| NSURL::fileURLWithPath(&crate::util::ns(&p.to_string_lossy()))).collect();
    objc2_app_kit::NSWorkspace::sharedWorkspace().activateFileViewerSelectingURLs(&NSArray::from_retained_slice(&urls));
}

/// Open a folder in Finder.
pub fn open_folder(dir: &std::path::Path) {
    let _ = std::fs::create_dir_all(dir);
    let url = objc2_foundation::NSURL::fileURLWithPath(&crate::util::ns(&dir.to_string_lossy()));
    objc2_app_kit::NSWorkspace::sharedWorkspace().openURL(&url);
}

/// Dock icon for the running app (the bundle's AppIcon.icns covers Finder;
/// this also covers `rec-rs gui` outside the bundle and a stale Dock cache).
pub fn set_dock_icon() {
    static ICON: &[u8] = include_bytes!("../../support/icon.png");
    let Some(mtm) = objc2::MainThreadMarker::new() else { return };
    let data = objc2_foundation::NSData::with_bytes(ICON);
    if let Some(img) = { use objc2::AnyThread; objc2_app_kit::NSImage::initWithData(objc2_app_kit::NSImage::alloc(), &data) } {
        unsafe { objc2_app_kit::NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&img)) };
    }
}
