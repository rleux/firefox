/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{hal, Device, SurfaceOptions};
use raw_window_handle::{
    HasDisplayHandle, HasWindowHandle, RawDisplayHandle as Display, RawWindowHandle as Window,
};
use std::{
    rc::Rc,
    time::{Duration, Instant},
};

pub trait SurfaceWindow: HasDisplayHandle + HasWindowHandle {}
impl<T: HasDisplayHandle + HasWindowHandle> SurfaceWindow for T {}

pub(super) struct WindowSurface {
    pub raw: Box<dyn hal::DynSurface>,
    pub options: SurfaceOptions,
    pub owner: Rc<SurfaceOwner>,
}

pub(super) struct SurfaceOwner {
    lifetime: Option<(hal::renderer::SurfaceLifetime, unsafe extern "C" fn() -> !)>,
    _window: Rc<dyn SurfaceWindow>,
    #[cfg(test)]
    pub waiting: std::sync::Arc<std::sync::atomic::AtomicBool>,
    #[cfg(test)]
    pub timeout: Duration,
}

impl Drop for SurfaceOwner {
    fn drop(&mut self) {
        if let Some((lifetime, fatal_process)) = &self.lifetime {
            #[cfg(not(test))]
            let timeout = Duration::from_secs(10);
            #[cfg(test)]
            let timeout = self.timeout;
            let deadline = Instant::now() + timeout;
            #[cfg(test)]
            self.waiting
                .store(true, std::sync::atomic::Ordering::Release);
            while !lifetime.is_destroyed() {
                if Instant::now() >= deadline {
                    unsafe { fatal_process() };
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

impl WindowSurface {
    pub fn new(
        instance: &dyn hal::DynInstance,
        window: &Rc<dyn SurfaceWindow>,
        display: Display,
        options: SurfaceOptions,
        fatal_process: Option<unsafe extern "C" fn() -> !>,
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
        let lifetime = fatal_process
            .map(|fatal_process| {
                hal::renderer::surface_lifetime(raw.as_ref())
                    .map(|lifetime| (lifetime, fatal_process))
                    .map_err(|error| format!("Tracking Vulkan surface lifetime: {error}"))
            })
            .transpose()?;
        Ok(Self {
            raw,
            options,
            owner: Rc::new(SurfaceOwner {
                lifetime,
                _window: window.clone(),
                #[cfg(test)]
                waiting: Default::default(),
                #[cfg(test)]
                timeout: Duration::from_secs(10),
            }),
        })
    }
}

impl Device {
    pub(super) fn create_surface(
        &self,
        window: &Rc<dyn SurfaceWindow>,
        options: SurfaceOptions,
    ) -> Result<WindowSurface, String> {
        let display = window
            .display_handle()
            .map_err(|error| format!("Getting Vulkan display handle: {error}"))?;
        let owned_display = self
            .display_owner
            .as_ref()
            .ok_or("Vulkan window replacement requires an owned display")?
            .display_handle()
            .map_err(|error| format!("Getting owned Vulkan display: {error}"))?;
        if display.as_raw() != owned_display.as_raw() {
            return Err("Vulkan window replacement cannot change displays".into());
        }
        let surface = WindowSurface::new(
            self.instance.as_ref(),
            window,
            display.as_raw(),
            options,
            self.shared
                .as_ref()
                .map(|shared| shared.options.fatal_process),
        )?;
        if unsafe { self.adapter.surface_capabilities(surface.raw.as_ref()) }.is_none() {
            return Err("The Vulkan adapter cannot present to the replacement window".into());
        }
        Ok(surface)
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
