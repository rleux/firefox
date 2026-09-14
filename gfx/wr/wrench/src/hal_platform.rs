/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use winit::window::Window;

#[cfg(target_os = "linux")]
mod linux;

pub(crate) fn identity(window: &Window) -> Result<(&'static str, u64), String> {
    #[cfg(target_os = "linux")]
    {
        linux::identity(window)
    }
    #[cfg(target_os = "android")]
    {
        use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        match window.window_handle().map_err(|error| error.to_string())?.as_raw() {
            RawWindowHandle::AndroidNdk(handle) => Ok(("android", handle.a_native_window.as_ptr() as u64)),
            _ => Err("Expected an Android native window".into()),
        }
    }
    #[cfg(target_os = "windows")]
    {
        use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        match window.window_handle().map_err(|error| error.to_string())?.as_raw() {
            RawWindowHandle::Win32(handle) => Ok(("win32", handle.hwnd.get() as u64)),
            _ => Err("Expected a Win32 native window".into()),
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "windows")))]
    {
        let _ = window;
        Ok(("native", 0))
    }
}

pub(crate) fn event_loop<T: 'static>() -> Result<winit::event_loop::EventLoop<T>, String> {
    #[allow(unused_mut)]
    let mut builder = winit::event_loop::EventLoop::<T>::with_user_event();
    #[cfg(target_os = "android")]
    {
        use winit::platform::android::EventLoopBuilderExtAndroid;
        builder.with_android_app(crate::ANDROID_APP.get()
            .ok_or("HAL Android entry requires AndroidApp")?.clone());
    }
    builder.build().map_err(|error| error.to_string())
}
