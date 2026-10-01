/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use raw_window_handle::*;
use std::cell::Cell;
use std::num::{NonZeroIsize, NonZeroU32};
use std::ptr::NonNull;

struct UnavailableWindow(Rc<Cell<bool>>);

impl Drop for UnavailableWindow {
    fn drop(&mut self) {
        self.0.set(true);
    }
}

impl HasDisplayHandle for UnavailableWindow {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Err(HandleError::Unavailable)
    }
}

impl HasWindowHandle for UnavailableWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        panic!("An unavailable display must be rejected before querying the window")
    }
}

#[test]
fn unavailable_window_is_rejected_before_vulkan_initialization() {
    let released = Rc::new(Cell::new(false));
    let options = super::super::Options {
        window: Some(Rc::new(UnavailableWindow(released.clone()))),
        ..Default::default()
    };
    let error = super::super::Device::new(&options).err().unwrap();
    assert!(error.contains("Getting Vulkan display handle"));
    drop(options);
    assert!(released.get());
}

#[test]
fn linux_handles_require_matching_protocols_and_connections() {
    let pointer = NonNull::dangling();
    let displays = [
        Display::Xlib(XlibDisplayHandle::new(Some(pointer), 0)),
        Display::Xcb(XcbDisplayHandle::new(Some(pointer), 0)),
        Display::Wayland(WaylandDisplayHandle::new(pointer)),
    ];
    let windows = [
        Window::Xlib(XlibWindowHandle::new(7)),
        Window::Xcb(XcbWindowHandle::new(NonZeroU32::new(7).unwrap())),
        Window::Wayland(WaylandWindowHandle::new(pointer)),
    ];
    for (d, display) in displays.iter().enumerate() {
        for (w, window) in windows.iter().enumerate() {
            assert_eq!(validate_linux(*display, *window).is_ok(), d == w);
        }
    }
    assert!(validate_linux(Display::Xlib(XlibDisplayHandle::new(None, 0)), windows[0]).is_err());
    assert!(validate_linux(Display::Xcb(XcbDisplayHandle::new(None, 0)), windows[1]).is_err());
    assert!(validate_linux(displays[0], Window::Xlib(XlibWindowHandle::new(0))).is_err());
}

#[test]
fn windows_handles_require_an_instance_and_matching_display() {
    let display = Display::Windows(WindowsDisplayHandle::new());
    let mut window = Win32WindowHandle::new(NonZeroIsize::new(1).unwrap());
    assert!(validate_windows(display, Window::Win32(window)).is_err());
    window.hinstance = NonZeroIsize::new(2);
    assert!(validate_windows(display, Window::Win32(window)).is_ok());
    assert!(validate_windows(
        Display::Android(AndroidDisplayHandle::new()),
        Window::Win32(window)
    )
    .is_err());
}

#[test]
fn android_handles_require_a_matching_display() {
    let window = Window::AndroidNdk(AndroidNdkWindowHandle::new(NonNull::dangling()));
    assert!(validate_android(Display::Android(AndroidDisplayHandle::new()), window).is_ok());
    assert!(validate_android(Display::Windows(WindowsDisplayHandle::new()), window).is_err());
}

#[cfg(all(target_os = "linux", feature = "debugger"))]
#[path = "window_surface_x11_tests.rs"]
pub(in crate::device::wgpu) mod x11;
