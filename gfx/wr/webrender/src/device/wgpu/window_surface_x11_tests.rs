/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::wgpu::{
    wgt, Device, Options, SurfaceOptions,
    swapchain::Swapchain,
    tests::{validation_logging, ERRORS},
};
use std::os::raw::{c_char, c_int, c_long, c_uint, c_ulong, c_void};
use std::sync::atomic::Ordering;
use wgpu_hal::Adapter as _;

struct X11Window {
    library: libloading::Library,
    display: NonNull<c_void>,
    window: c_ulong,
    screen: c_int,
}

#[repr(C)]
#[derive(Default)]
struct XSetWindowAttributes {
    background_pixmap: c_ulong,
    background_pixel: c_ulong,
    border_pixmap: c_ulong,
    border_pixel: c_ulong,
    bit_gravity: c_int,
    win_gravity: c_int,
    backing_store: c_int,
    backing_planes: c_ulong,
    backing_pixel: c_ulong,
    save_under: c_int,
    event_mask: c_long,
    do_not_propagate_mask: c_long,
    override_redirect: c_int,
    colormap: c_ulong,
    cursor: c_ulong,
}

impl X11Window {
    fn resize(&self, size: [u32; 2]) {
        unsafe {
            self.library
                .get::<unsafe extern "C" fn(*mut c_void, c_ulong, c_uint, c_uint) -> c_int>(
                    b"XResizeWindow\0",
                )
                .unwrap()(self.display.as_ptr(), self.window, size[0], size[1]);
            self.library
                .get::<unsafe extern "C" fn(*mut c_void, c_int) -> c_int>(b"XSync\0")
                .unwrap()(self.display.as_ptr(), 0);
        }
    }

    unsafe fn new() -> Self {
        let library = libloading::Library::new("libX11.so.6").unwrap();
        let open = library
            .get::<unsafe extern "C" fn(*const c_char) -> *mut c_void>(b"XOpenDisplay\0")
            .unwrap();
        let display = NonNull::new(open(std::ptr::null())).expect("An X11 display is required");
        let screen = library
            .get::<unsafe extern "C" fn(*mut c_void) -> c_int>(b"XDefaultScreen\0")
            .unwrap()(display.as_ptr());
        let root = library
            .get::<unsafe extern "C" fn(*mut c_void, c_int) -> c_ulong>(b"XRootWindow\0")
            .unwrap()(display.as_ptr(), screen);
        let create = library
            .get::<unsafe extern "C" fn(
                *mut c_void,
                c_ulong,
                c_int,
                c_int,
                c_uint,
                c_uint,
                c_uint,
                c_ulong,
                c_ulong,
            ) -> c_ulong>(b"XCreateSimpleWindow\0")
            .unwrap();
        let window = create(display.as_ptr(), root, 0, 0, 64, 48, 0, 0, 0);
        assert_ne!(window, 0);
        // Mapping and resizing must not be deferred to a window manager.
        let mut attributes = XSetWindowAttributes {
            override_redirect: 1,
            ..Default::default()
        };
        const CW_OVERRIDE_REDIRECT: c_ulong = 1 << 9;
        library
            .get::<unsafe extern "C" fn(
                *mut c_void,
                c_ulong,
                c_ulong,
                *mut XSetWindowAttributes,
            ) -> c_int>(b"XChangeWindowAttributes\0")
            .unwrap()(
                display.as_ptr(),
                window,
                CW_OVERRIDE_REDIRECT,
                &mut attributes,
            );
        library
            .get::<unsafe extern "C" fn(*mut c_void, c_ulong) -> c_int>(b"XMapWindow\0")
            .unwrap()(display.as_ptr(), window);
        library
            .get::<unsafe extern "C" fn(*mut c_void, c_int) -> c_int>(b"XSync\0")
            .unwrap()(display.as_ptr(), 0);
        Self {
            library,
            display,
            window,
            screen,
        }
    }
}

impl HasDisplayHandle for X11Window {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Ok(unsafe {
            DisplayHandle::borrow_raw(Display::Xlib(XlibDisplayHandle::new(
                Some(self.display),
                self.screen,
            )))
        })
    }
}

impl HasWindowHandle for X11Window {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        Ok(unsafe { WindowHandle::borrow_raw(Window::Xlib(XlibWindowHandle::new(self.window))) })
    }
}

impl Drop for X11Window {
    fn drop(&mut self) {
        unsafe {
            self.library
                .get::<unsafe extern "C" fn(*mut c_void, c_ulong) -> c_int>(b"XDestroyWindow\0")
                .unwrap()(self.display.as_ptr(), self.window);
            self.library
                .get::<unsafe extern "C" fn(*mut c_void) -> c_int>(b"XCloseDisplay\0")
                .unwrap()(self.display.as_ptr());
        }
    }
}

#[test]
#[ignore = "Requires X11, Vulkan and the Khronos validation layer"]
fn window_surface_selects_present_adapter_and_retains_window() {
    validation_logging();
    for vsync in [true, false, true] {
        let window = Rc::new(unsafe { X11Window::new() });
        let weak = Rc::downgrade(&window);
        let options = Options {
            window: Some(window.clone()),
            validation: true,
            surface_options: SurfaceOptions {
                vsync,
                ..Default::default()
            },
            ..Default::default()
        };
        let device = Rc::new(Device::new(&options).unwrap());
        let surface = device.surface.take().unwrap();
        let caps = unsafe { device.adapter.surface_capabilities(surface.raw.as_ref()) }.unwrap();
        assert_eq!(surface.options.vsync, vsync);
        device.surface.set(Some(surface));
        let mut swapchain = Swapchain::new(&device).unwrap();
        assert!(Swapchain::new(&device).is_none());
        swapchain
            .configure([64, 48], options.surface_options)
            .unwrap();
        let config = swapchain.configuration().unwrap();
        let expected_mode = if !vsync && caps.present_modes.contains(&wgt::PresentMode::Immediate) {
            wgt::PresentMode::Immediate
        } else {
            wgt::PresentMode::Fifo
        };
        assert_eq!(config.present_mode, expected_mode);
        assert_eq!(config.usage, wgt::TextureUses::COLOR_TARGET);
        assert!(matches!(
            config.format,
            wgt::TextureFormat::Rgba8Unorm | wgt::TextureFormat::Bgra8Unorm
        ));
        assert!(Device::new(&Options {
            adapter_name: Some(" ".into()),
            ..options
        })
        .is_err());
        drop(window);
        assert!(weak.upgrade().is_some());
        drop(device);
        assert!(weak.upgrade().is_some());
        drop(swapchain);
        assert!(weak.upgrade().is_none());
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires X11, a presentation-capable Vulkan adapter and validation"]
fn swapchain_configuration_resizes_suspends_and_preserves_valid_state() {
    validation_logging();
    let window = Rc::new(unsafe { X11Window::new() });
    let device = Rc::new(
        Device::new(&Options {
            window: Some(window.clone()),
            validation: true,
            ..Default::default()
        })
        .unwrap(),
    );
    let mut swapchain = Swapchain::new(&device).unwrap();
    let options = SurfaceOptions::default();
    assert!(swapchain.configuration().is_none());
    for size in [
        [64, 48],
        [48, 32],
        [0, 48],
        [32, 24],
        [32, 0],
        [0, 0],
        [64, 48],
    ] {
        if !size.contains(&0) {
            window.resize(size);
        }
        swapchain.configure(size, options).unwrap();
        if size.contains(&0) {
            assert!(swapchain.configuration().is_none());
        } else {
            let config = swapchain.configuration().unwrap();
            assert_eq!(config.usage, wgt::TextureUses::COLOR_TARGET);
            assert_eq!([config.extent.width, config.extent.height], size);
        }
    }
    let before = swapchain.configuration().unwrap().clone();
    assert!(swapchain.configure([u32::MAX, 48], options).is_err());
    let after = swapchain.configuration().unwrap();
    assert_eq!(before.extent, after.extent);
    assert_eq!(before.format, after.format);
    assert!(!device.is_lost());
    device.lost.set(true);
    assert!(swapchain.configure([64, 48], options).is_err());
    assert_eq!(swapchain.configuration().unwrap().extent, before.extent);
    drop(swapchain);
    drop(device);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires X11 without DRI3, Vulkan validation, and an incompatible hardware adapter"]
fn window_surface_rejects_present_incompatible_adapter() {
    validation_logging();
    let window = Rc::new(unsafe { X11Window::new() });
    let weak = Rc::downgrade(&window);
    let options = Options {
        window: Some(window.clone()),
        validation: true,
        ..Default::default()
    };
    let error = Device::new(&options).err().unwrap();
    assert!(error.contains("No Vulkan adapters can present"), "{}", error);
    drop(options);
    drop(window);
    assert!(weak.upgrade().is_none());
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
