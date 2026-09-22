/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use raw_window_handle::{RawDisplayHandle as Display, RawWindowHandle as Window};

pub(super) fn validate(display: Display, window: Window) -> Result<&'static str, String> {
    match (display, window) {
        (Display::Xlib(display), Window::Xlib(window)) => {
            if display.display.is_none() {
                return Err("Xlib surface requires a display connection".into());
            }
            if window.window == 0 {
                return Err("Xlib surface requires a nonzero window ID".into());
            }
            Ok("xlib")
        }
        (Display::Xcb(display), Window::Xcb(_)) => {
            if display.connection.is_none() {
                return Err("Xcb surface requires a display connection".into());
            }
            Ok("xcb")
        }
        (Display::Wayland(_), Window::Wayland(_)) => Ok("wayland"),
        (Display::Drm(_), Window::Drm(_)) => Ok("drm"),
        _ => Err("Linux surface requires matching supported display/window handles".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use raw_window_handle::*;
    use std::{num::NonZeroU32, ptr::NonNull};

    #[test]
    fn linux_handle_pairs_reject_missing_connections_and_mismatches() {
        let pointer = NonNull::dangling();
        let displays = [
            RawDisplayHandle::Xlib(XlibDisplayHandle::new(Some(pointer), 0)),
            RawDisplayHandle::Xcb(XcbDisplayHandle::new(Some(pointer), 0)),
            RawDisplayHandle::Wayland(WaylandDisplayHandle::new(pointer)),
        ];
        let windows = [
            RawWindowHandle::Xlib(XlibWindowHandle::new(7)),
            RawWindowHandle::Xcb(XcbWindowHandle::new(NonZeroU32::new(7).unwrap())),
            RawWindowHandle::Wayland(WaylandWindowHandle::new(pointer)),
        ];
        for (d, display) in displays.iter().enumerate() {
            for (w, window) in windows.iter().enumerate() {
                assert_eq!(validate(*display, *window).is_ok(), d == w);
            }
        }
        assert!(validate(
            RawDisplayHandle::Xlib(XlibDisplayHandle::new(None, 0)),
            windows[0]
        )
        .is_err());
        assert!(validate(
            RawDisplayHandle::Xcb(XcbDisplayHandle::new(None, 0)),
            windows[1]
        )
        .is_err());
        assert!(validate(displays[0], RawWindowHandle::Xlib(XlibWindowHandle::new(0))).is_err());
    }
}
