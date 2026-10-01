/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{hal, SurfaceOptions};
use raw_window_handle::{
    HasDisplayHandle, HasWindowHandle, RawDisplayHandle as Display, RawWindowHandle as Window,
};
use std::rc::Rc;

pub trait SurfaceWindow: HasDisplayHandle + HasWindowHandle {}
impl<T: HasDisplayHandle + HasWindowHandle> SurfaceWindow for T {}

pub(super) struct WindowSurface {
    pub raw: Box<dyn hal::DynSurface>,
    pub options: SurfaceOptions,
    // The native surface must be destroyed before its window.
    _window: Rc<dyn SurfaceWindow>,
}

impl WindowSurface {
    pub fn new(
        instance: &dyn hal::DynInstance,
        window: &Rc<dyn SurfaceWindow>,
        display: Display,
        options: SurfaceOptions,
    ) -> Result<Self, String> {
        let handle = window
            .window_handle()
            .map_err(|error| format!("Getting Vulkan window handle: {error}"))?;
        #[cfg(target_os = "linux")]
        validate_linux(display, handle.as_raw())?;
        #[cfg(target_os = "windows")]
        validate_windows(display, handle.as_raw())?;
        #[cfg(target_os = "android")]
        validate_android(display, handle.as_raw())?;
        let raw = unsafe { instance.create_surface(display, handle.as_raw()) }
            .map_err(|error| format!("Creating Vulkan window surface: {error}"))?;
        Ok(Self {
            raw,
            options,
            _window: window.clone(),
        })
    }
}

#[cfg(any(target_os = "linux", test))]
fn validate_linux(display: Display, window: Window) -> Result<(), String> {
    match (display, window) {
        (Display::Xlib(display), Window::Xlib(window))
            if display.display.is_some() && window.window != 0 =>
        {
            Ok(())
        }
        (Display::Xcb(display), Window::Xcb(_)) if display.connection.is_some() => Ok(()),
        (Display::Wayland(_), Window::Wayland(_)) => Ok(()),
        _ => Err("Vulkan Linux surface requires matching live Xlib, Xcb or Wayland handles".into()),
    }
}

#[cfg(any(target_os = "windows", test))]
fn validate_windows(display: Display, window: Window) -> Result<(), String> {
    match (display, window) {
        (Display::Windows(_), Window::Win32(window)) if window.hinstance.is_some() => Ok(()),
        _ => Err("Vulkan Windows surface requires Windows/Win32 handles with HINSTANCE".into()),
    }
}

#[cfg(any(target_os = "android", test))]
fn validate_android(display: Display, window: Window) -> Result<(), String> {
    match (display, window) {
        (Display::Android(_), Window::AndroidNdk(_)) => Ok(()),
        _ => Err("Vulkan Android surface requires Android/AndroidNdk handles".into()),
    }
}

#[cfg(test)]
#[path = "window_surface_tests.rs"]
pub(super) mod tests;
