/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::super::*;
use raw_window_handle::{DisplayHandle, HasDisplayHandle, HasWindowHandle, RawDisplayHandle};
use std::rc::Rc;

#[cfg(target_os = "linux")]
mod linux;

pub trait SurfaceWindow: HasDisplayHandle + HasWindowHandle {}
impl<T: HasDisplayHandle + HasWindowHandle> SurfaceWindow for T {}

pub(crate) struct WindowOwner {
    window: Rc<dyn SurfaceWindow>,
    display: RawDisplayHandle,
}

impl WindowOwner {
    pub fn new(window: Rc<dyn SurfaceWindow>) -> Result<Self> {
        let display = window
            .display_handle()
            .map_err(|error| format!("Getting display handle: {error}"))?
            .as_raw();
        #[cfg(target_os = "linux")]
        {
            let handle = window
                .window_handle()
                .map_err(|error| format!("Getting window handle: {error}"))?;
            let protocol = linux::validate(display, handle.as_raw())?;
            println!("HAL Linux surface: {protocol}");
        }
        #[cfg(target_os = "android")]
        validate_android(display, window.window_handle()
            .map_err(|error| format!("Getting Android native window: {error}"))?.as_raw())?;
        Ok(Self { window, display })
    }

    pub fn display_handle(&self) -> Result<DisplayHandle<'_>> {
        let display = self
            .window
            .display_handle()
            .map_err(|error| format!("Getting display handle: {error}"))?;
        if display.as_raw() != self.display {
            return Err(
                "Surface display changed; recreate the renderer for the new display".into(),
            );
        }
        Ok(display)
    }

    pub fn create_surface<A: hal::Api>(&self, instance: &A::Instance) -> Result<A::Surface> {
        let display = self.display_handle()?;
        let window = self
            .window
            .window_handle()
            .map_err(|error| format!("Getting window handle: {error}"))?;
        #[cfg(target_os = "linux")]
        linux::validate(display.as_raw(), window.as_raw())?;
        #[cfg(target_os = "android")]
        validate_android(display.as_raw(), window.as_raw())?;
        unsafe { instance.create_surface(display.as_raw(), window.as_raw()) }
            .map_err(|error| format!("Creating native surface: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use raw_window_handle::{HandleError, WindowHandle};
    use std::cell::Cell;

    struct UnavailableWindow(Rc<Cell<bool>>);
    impl Drop for UnavailableWindow {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }
    impl HasDisplayHandle for UnavailableWindow {
        fn display_handle(&self) -> std::result::Result<DisplayHandle<'_>, HandleError> {
            Err(HandleError::Unavailable)
        }
    }
    impl HasWindowHandle for UnavailableWindow {
        fn window_handle(&self) -> std::result::Result<WindowHandle<'_>, HandleError> {
            panic!("Display rejection must precede the window query")
        }
    }

    #[test]
    fn unavailable_display_releases_owner_before_native_initialization() {
        let released = Rc::new(Cell::new(false));
        let result = WindowOwner::new(Rc::new(UnavailableWindow(released.clone())));
        assert!(matches!(result, Err(error) if error.contains("Getting display handle")));
        assert!(released.get());
    }
}

#[cfg(target_os = "android")]
fn validate_android(display: RawDisplayHandle, window: raw_window_handle::RawWindowHandle) -> Result<()> {
    if !matches!((display, window), (RawDisplayHandle::Android(_), raw_window_handle::RawWindowHandle::AndroidNdk(_))) {
        return Err("Android Vulkan requires a live activity native window".into());
    }
    Ok(())
}
