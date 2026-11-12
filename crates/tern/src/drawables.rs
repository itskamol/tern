//! Caps the window's Metal layer at two drawables instead of GPUI's three, so the window holds
//! at most two window-sized IOSurfaces (one fewer: 18.6 MB at 1320x880).
//!
//! GPUI sets the count once, when it makes the layer (`MetalRenderer::configure_layer`), and
//! never replaces the layer or resets the count on resize, full screen or a display change, so
//! one call after the window opens holds for its life. `apply` is idempotent and cheap, and the
//! shell repeats it on bounds changes anyway so a GPUI update that re-creates the layer cannot
//! silently undo the cap.
//!
//! The only `unsafe` is turning the window handle's `NSView` pointer into a reference; everything
//! after that is objc2's safe bindings.
#![allow(unsafe_code)]

/// Drawables the window keeps: one being shown, one being drawn.
pub const WANTED: usize = 2;

/// Whether the layer needs the call; setting an unchanged count is skipped.
const fn needs_cap(current: usize) -> bool {
    current != WANTED
}

#[cfg(target_os = "macos")]
use objc2_quartz_core::CAMetalLayer;

/// Sets the cap on `layer` and returns the count read back.
#[cfg(target_os = "macos")]
fn cap_layer(layer: &CAMetalLayer) -> usize {
    if needs_cap(layer.maximumDrawableCount()) {
        layer.setMaximumDrawableCount(WANTED);
    }
    layer.maximumDrawableCount()
}

/// Caps `window`'s Metal layer at [`WANTED`] drawables and returns the count read back, or
/// `None` when the window has no Metal layer (not macOS, or not yet attached).
#[cfg(target_os = "macos")]
pub fn apply(window: &gpui::Window) -> Option<usize> {
    use objc2_app_kit::NSView;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let RawWindowHandle::AppKit(handle) = HasWindowHandle::window_handle(window).ok()?.as_raw()
    else {
        return None;
    };
    // SAFETY: GPUI's `ns_view` is the window's live content `NSView`, and `window` is borrowed
    // on the main thread for the call, so the view outlives this reference; it is only used to
    // read `layer`, which retains what it returns.
    let view: &NSView = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
    let layer = view.layer()?.downcast::<CAMetalLayer>().ok()?;
    let count = cap_layer(&layer);
    tracing::debug!(count, "window_drawables_capped");
    Some(count)
}

#[cfg(not(target_os = "macos"))]
pub fn apply(_window: &gpui::Window) -> Option<usize> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_layer_off_the_cap_needs_the_call() {
        assert!(needs_cap(3));
        assert!(needs_cap(1));
        assert!(!needs_cap(WANTED));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_layer_gpui_made_with_three_is_read_back_as_two() {
        let layer = CAMetalLayer::new();
        layer.setMaximumDrawableCount(3);
        assert_eq!(layer.maximumDrawableCount(), 3);
        assert_eq!(cap_layer(&layer), 2);
        assert_eq!(layer.maximumDrawableCount(), 2);
        assert_eq!(cap_layer(&layer), 2);
    }
}
