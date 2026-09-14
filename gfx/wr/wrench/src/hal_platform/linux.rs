/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use winit::{
    raw_window_handle::{HasWindowHandle, RawWindowHandle},
    window::Window,
};

pub(super) fn identity(window: &Window) -> Result<(&'static str, u64), String> {
    Ok(
        match window
            .window_handle()
            .map_err(|error| error.to_string())?
            .as_raw()
        {
            RawWindowHandle::Xlib(handle) => ("xlib", handle.window as u64),
            RawWindowHandle::Xcb(handle) => ("xcb", handle.window.get() as u64),
            RawWindowHandle::Wayland(_) => ("wayland", 0),
            _ => return Err("Unsupported Linux window handle in Wrench".into()),
        },
    )
}
