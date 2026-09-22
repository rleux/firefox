/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use raw_window_handle::{RawDisplayHandle, RawWindowHandle};

pub(super) fn validate(display: RawDisplayHandle, window: RawWindowHandle) -> Result<(), String> {
    match (display, window) {
        (RawDisplayHandle::Windows(_), RawWindowHandle::Win32(handle))
            if handle.hinstance.is_some() =>
        {
            Ok(())
        }
        (RawDisplayHandle::Windows(_), RawWindowHandle::Win32(_)) => {
            Err("Vulkan Win32 surface requires the window's HINSTANCE".into())
        }
        _ => Err("Vulkan Windows surface requires matching Windows/Win32 handles".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use raw_window_handle::{Win32WindowHandle, WindowsDisplayHandle, XlibDisplayHandle};
    use std::num::NonZeroIsize;

    #[test]
    fn win32_rejects_missing_instance_and_mismatched_display() {
        let display = RawDisplayHandle::Windows(WindowsDisplayHandle::new());
        let mut window = Win32WindowHandle::new(NonZeroIsize::new(1).unwrap());
        assert!(validate(display, RawWindowHandle::Win32(window)).is_err());
        window.hinstance = NonZeroIsize::new(2);
        assert!(validate(display, RawWindowHandle::Win32(window)).is_ok());
        assert!(validate(
            RawDisplayHandle::Xlib(XlibDisplayHandle::new(None, 0)),
            RawWindowHandle::Win32(window)
        )
        .is_err());
    }
}
