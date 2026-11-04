/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use raw_window_handle::{RawDisplayHandle as Display, RawWindowHandle as Window};
use std::{
    os::raw::{c_char, c_int, c_long, c_uint, c_ulong, c_void},
    ptr::NonNull,
};
pub(crate) struct X11Display {
    library: libloading::Library,
    // Xext callbacks registered by Vulkan must stay loaded through XCloseDisplay.
    _xext: libloading::Library,
    display: NonNull<c_void>,
    screen: c_int,
}

pub(crate) struct X11Window {
    connection: Rc<X11Display>,
    window: c_ulong,
}

// Immutable native handles are destroyed only by these owners' Drop implementations.
unsafe impl SurfaceWindow for X11Window {}

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
    pub(crate) fn pixels(&self, size: [u32; 2]) -> Vec<c_ulong> {
        unsafe {
            let get_image = self
                .connection
                .library
                .get::<unsafe extern "C" fn(
                    *mut c_void,
                    c_ulong,
                    c_int,
                    c_int,
                    c_uint,
                    c_uint,
                    c_ulong,
                    c_int,
                ) -> *mut c_void>(b"XGetImage\0")
                .unwrap();
            let get_pixel = self
                .connection
                .library
                .get::<unsafe extern "C" fn(*mut c_void, c_int, c_int) -> c_ulong>(b"XGetPixel\0")
                .unwrap();
            let destroy = self
                .connection
                .library
                .get::<unsafe extern "C" fn(*mut c_void) -> c_int>(b"XDestroyImage\0")
                .unwrap();
            let image = get_image(
                self.connection.display.as_ptr(),
                self.window,
                0,
                0,
                size[0],
                size[1],
                !0,
                2,
            );
            assert!(!image.is_null());
            let mut pixels = Vec::new();
            for y in 0..size[1] {
                for x in 0..size[0] {
                    pixels.push(get_pixel(image, x as c_int, y as c_int) & 0x00ff_ffff);
                }
            }
            destroy(image);
            pixels
        }
    }

    pub(crate) fn resize(&self, size: [u32; 2]) {
        unsafe {
            self.connection
                .library
                .get::<unsafe extern "C" fn(*mut c_void, c_ulong, c_uint, c_uint) -> c_int>(
                    b"XResizeWindow\0",
                )
                .unwrap()(
                self.connection.display.as_ptr(),
                self.window,
                size[0],
                size[1],
            );
            self.connection
                .library
                .get::<unsafe extern "C" fn(*mut c_void, c_int) -> c_int>(b"XSync\0")
                .unwrap()(self.connection.display.as_ptr(), 0);
        }
    }

    pub(crate) unsafe fn new() -> Self {
        Self::with_display(Rc::new(X11Display::new()))
    }

    pub(crate) unsafe fn with_display(connection: Rc<X11Display>) -> Self {
        let library = &connection.library;
        let display = connection.display;
        let screen = connection.screen;
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
        Self { connection, window }
    }
}

impl X11Display {
    pub(crate) unsafe fn new() -> Self {
        let library = libloading::Library::new("libX11.so.6").unwrap();
        let xext = libloading::Library::new("libXext.so.6").unwrap();
        let open = library
            .get::<unsafe extern "C" fn(*const c_char) -> *mut c_void>(b"XOpenDisplay\0")
            .unwrap();
        let display = NonNull::new(open(std::ptr::null())).expect("An X11 display is required");
        let screen = library
            .get::<unsafe extern "C" fn(*mut c_void) -> c_int>(b"XDefaultScreen\0")
            .unwrap()(display.as_ptr());
        Self {
            library,
            _xext: xext,
            display,
            screen,
        }
    }
}

impl HasDisplayHandle for X11Display {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Ok(unsafe {
            DisplayHandle::borrow_raw(Display::Xlib(XlibDisplayHandle::new(
                Some(self.display),
                self.screen,
            )))
        })
    }
}

impl HasDisplayHandle for X11Window {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        self.connection.display_handle()
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
            self.connection
                .library
                .get::<unsafe extern "C" fn(*mut c_void, c_ulong) -> c_int>(b"XDestroyWindow\0")
                .unwrap()(self.connection.display.as_ptr(), self.window);
        }
    }
}

impl Drop for X11Display {
    fn drop(&mut self) {
        unsafe {
            self.library
                .get::<unsafe extern "C" fn(*mut c_void) -> c_int>(b"XCloseDisplay\0")
                .unwrap()(self.display.as_ptr());
        }
    }
}

use crate::device::{GpuBackend, RenderPassDescriptor, DrawTarget, LoadOp, StoreOp};
use super::super::super::tests::{validation_logging, VALIDATION_ERRORS};
use std::sync::atomic::Ordering;

fn setup() -> (Rc<X11Window>, Arc<Device>, Surface) {
    validation_logging();
    let window = Rc::new(unsafe { X11Window::new() });
    let options = Options {
        window: Some(window.clone()),
        display_owner: Some(window.connection.clone()),
        validation: true,
        ..Default::default()
    };
    let (owner, initial) = Device::new_with_surface(&options).unwrap();
    let owner = Arc::new(owner);
    let surface = Surface::new(&owner, initial.unwrap());
    (window, owner, surface)
}
fn clear(owner: &Device, image: &Image, color: wgt::Color) -> Arc<core::command::CommandBuffer> {
    let encoder = owner
        .create_command_encoder("native surface clear")
        .unwrap();
    let mut pass = encoder.begin_render_pass(core::command::ResolvedRenderPassDescriptor {
        color_attachments: std::borrow::Cow::Owned(vec![Some(
            core::command::RenderPassColorAttachment {
                view: image.levels[0].clone(),
                depth_slice: None,
                resolve_target: None,
                load_op: wgt::LoadOp::Clear(color),
                store_op: wgt::StoreOp::Store,
            },
        )]),
        ..Default::default()
    });
    pass.end();
    encoder.finish(&Default::default())
}
fn presented(window: &X11Window, expected: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        if window
            .pixels([64, 48])
            .iter()
            .all(|&pixel| pixel as u64 == expected)
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "native X11 pixels did not match {:x}",
            expected
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}
#[test]
#[ignore = "Requires native X11, Vulkan presentation and Khronos validation"]
fn native_surface_direct_output_discard_resize_and_snapshot() {
    let (window, owner, mut surface) = setup();
    let before = VALIDATION_ERRORS.load(Ordering::Relaxed);
    for _ in 0..3 {
        let image = surface.acquire([64, 48]).unwrap().unwrap();
        assert!(!image
            .raw
            .descriptor()
            .usage
            .contains(wgt::TextureUsages::COPY_SRC));
        let abandoned = clear(&owner, &image, wgt::Color::BLUE);
        drop(abandoned);
        assert!(surface
            .snapshot(
                &owner
                    .create_command_encoder("unsupported snapshot")
                    .unwrap()
            )
            .is_err());
        assert!(surface.configure([64, 48]).is_err());
        assert!(surface.pause(true).is_err());
        drop(image);
        surface.discard().unwrap();
        surface.begin_frame();
    }
    surface.enable_readback();
    let image = surface.acquire([64, 48]).unwrap().unwrap();
    let cmd = clear(&owner, &image, wgt::Color::RED);
    let first = owner.submit(&[cmd]).unwrap();
    let encoder = owner.create_command_encoder("surface snapshot").unwrap();
    let snapshot = surface.snapshot(&encoder).unwrap();
    let index = owner
        .submit(&[encoder.finish(&Default::default())])
        .unwrap();
    assert!(index > first);
    drop(image);
    surface.present().unwrap();
    owner.wait_for(index).unwrap();
    presented(&window, 0xff0000);
    let encoder = owner.create_command_encoder("snapshot readback").unwrap();
    let readback = owner
        .encode_readback(
            &encoder,
            &snapshot.raw,
            0,
            api::units::DeviceIntRect::from_size(api::units::DeviceIntSize::new(64, 48)),
            Some(api::ImageFormat::RGBA8),
        )
        .unwrap();
    let index = owner
        .submit(&[encoder.finish(&Default::default())])
        .unwrap();
    let bytes = readback.map(&owner, index).unwrap().wait(&owner).unwrap();
    assert!(bytes.chunks_exact(4).all(|pixel| pixel == [255, 0, 0, 255]));
    surface.begin_frame();
    window.resize([32, 24]);
    let image = surface.acquire([32, 24]).unwrap().unwrap();
    let index = owner
        .submit(&[clear(&owner, &image, wgt::Color::GREEN)])
        .unwrap();
    drop(image);
    surface.present().unwrap();
    owner.wait_for(index).unwrap();
    assert!(window.pixels([32, 24]).iter().all(|&p| p == 0x00ff00));
    drop(surface);
    drop(owner);
    drop(window);
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), before);
}
#[test]
#[ignore = "Requires native X11, Vulkan presentation and Khronos validation"]
fn native_surface_status_recovery_detach_replace_and_owner_lifetimes() {
    let (window, owner, mut surface) = setup();
    let before = VALIDATION_ERRORS.load(Ordering::Relaxed);
    for (status, expected) in [
        (wgt::SurfaceStatus::Timeout, Outcome::Retry),
        (wgt::SurfaceStatus::Occluded, Outcome::Occluded),
        (wgt::SurfaceStatus::Outdated, Outcome::Retry),
        (wgt::SurfaceStatus::Lost, Outcome::Retry),
    ] {
        surface.begin_frame();
        testing::ACQUIRE.with(|s| s.set(Some(status)));
        assert!(surface.acquire([64, 48]).unwrap().is_none());
        assert_eq!(surface.outcome, Some(expected));
        assert!(surface.acquire([64, 48]).unwrap().is_none());
        surface.begin_frame();
        let image = surface.acquire([64, 48]).unwrap().unwrap();
        owner
            .wait_for(
                owner
                    .submit(&[clear(&owner, &image, wgt::Color::BLUE)])
                    .unwrap(),
            )
            .unwrap();
        drop(image);
        surface.present().unwrap();
    }
    surface.begin_frame();
    testing::ACQUIRE.with(|s| s.set(Some(wgt::SurfaceStatus::Suboptimal)));
    let image = surface.acquire([64, 48]).unwrap().unwrap();
    owner
        .wait_for(
            owner
                .submit(&[clear(&owner, &image, wgt::Color::BLUE)])
                .unwrap(),
        )
        .unwrap();
    drop(image);
    surface.present().unwrap();
    assert!(surface.reconfigure);
    for status in [
        wgt::SurfaceStatus::Suboptimal,
        wgt::SurfaceStatus::Outdated,
        wgt::SurfaceStatus::Lost,
        wgt::SurfaceStatus::Timeout,
        wgt::SurfaceStatus::Occluded,
    ] {
        surface.begin_frame();
        let image = surface.acquire([64, 48]).unwrap().unwrap();
        owner
            .wait_for(
                owner
                    .submit(&[clear(&owner, &image, wgt::Color::BLUE)])
                    .unwrap(),
            )
            .unwrap();
        drop(image);
        testing::PRESENT.with(|s| s.set(Some(status)));
        surface.present().unwrap();
    }
    surface.pause(true).unwrap();
    surface.begin_frame();
    assert!(surface.acquire([64, 48]).unwrap().is_none());
    surface.pause(false).unwrap();
    surface.replace(None, SurfaceOptions::default()).unwrap();
    surface.begin_frame();
    assert!(surface.acquire([64, 48]).unwrap().is_none());
    let replacement = Rc::new(unsafe { X11Window::with_display(window.connection.clone()) });
    let weak = Rc::downgrade(&replacement);
    surface
        .replace(Some(replacement.clone()), SurfaceOptions::default())
        .unwrap();
    drop(replacement);
    surface.begin_frame();
    let image = surface.acquire([64, 48]).unwrap().unwrap();
    drop(image);
    assert!(surface.replace(None, SurfaceOptions::default()).is_err());
    surface.discard().unwrap();
    surface.replace(None, SurfaceOptions::default()).unwrap();
    assert!(weak.upgrade().is_none());
    drop(surface);
    drop(owner);
    drop(window);
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), before);
}
#[test]
#[ignore = "Requires native X11, Vulkan presentation and Khronos validation"]
fn native_render_device_uses_acquired_output_and_skips_unavailable_window_passes() {
    validation_logging();
    let window = Rc::new(unsafe { X11Window::new() });
    let mut backend = super::super::super::render_device::RenderDevice::new_windowed(&Options {
        window: Some(window.clone()),
        validation: true,
        ..Default::default()
    })
    .unwrap();
    let owner = backend.owner.clone();
    let before = VALIDATION_ERRORS.load(Ordering::Relaxed);
    let desc = RenderPassDescriptor {
        target: DrawTarget::new_default(api::units::DeviceIntSize::new(64, 48), true),
        render_area: None,
        color_load: LoadOp::Clear([1., 0., 0., 1.]),
        depth_load: LoadOp::Load,
    };
    backend.begin_frame().unwrap();
    backend.begin_render_pass(&desc).unwrap();
    backend.end_render_pass(StoreOp::Store).unwrap();
    let index = backend.end_frame().unwrap();
    owner.wait_for(index).unwrap();
    presented(&window, 0xff0000);
    backend.begin_frame().unwrap();
    testing::ACQUIRE.with(|s| s.set(Some(wgt::SurfaceStatus::Timeout)));
    backend.begin_render_pass(&desc).unwrap();
    backend
        .clear_target(Some([0., 1., 0., 1.]), None, None)
        .unwrap();
    backend.end_render_pass(StoreOp::Store).unwrap();
    backend.end_frame().unwrap();
    assert!(backend.failure().is_none());
    backend.begin_frame().unwrap();
    backend.begin_render_pass(&desc).unwrap();
    backend.end_render_pass(StoreOp::Store).unwrap();
    backend.end_frame().unwrap();
    backend.deinit();
    drop(backend);
    drop(owner);
    drop(window);
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), before);
}

#[test]
#[ignore = "Requires native X11, Vulkan presentation and Khronos validation"]
fn native_surface_detached_startup_preserves_no_frame_work_and_rejects_changed_display() {
    validation_logging();
    let display = Rc::new(unsafe { X11Display::new() });
    let weak_display = Rc::downgrade(&display);
    let (owner, initial) = Device::new_with_surface(&Options {
        display_owner: Some(display.clone()),
        validation: true,
        ..Default::default()
    })
    .unwrap();
    let owner = Arc::new(owner);
    let mut surface = Surface::new(&owner, initial.unwrap());
    let before = VALIDATION_ERRORS.load(Ordering::Relaxed);
    assert!(surface.acquire([64, 48]).unwrap().is_none());
    assert_eq!(surface.outcome, Some(Outcome::Occluded));
    let data = owner
        .create_buffer_with_data("detached work", &[1, 2, 3, 4], wgt::BufferUsages::COPY_SRC)
        .unwrap();
    let index = owner.submit(&[]).unwrap();
    drop(data);
    owner.wait_for(index).unwrap();
    let window = Rc::new(unsafe { X11Window::with_display(display.clone()) });
    surface
        .replace(Some(window.clone()), SurfaceOptions::default())
        .unwrap();
    surface.begin_frame();
    let image = surface.acquire([64, 48]).unwrap().unwrap();
    let index = owner
        .submit(&[clear(&owner, &image, wgt::Color::GREEN)])
        .unwrap();
    drop(image);
    surface.present().unwrap();
    owner.wait_for(index).unwrap();
    presented(&window, 0x00ff00);
    let unrelated = Rc::new(unsafe { X11Window::new() });
    assert!(surface
        .replace(Some(unrelated.clone()), SurfaceOptions::default())
        .unwrap_err()
        .contains("cannot change displays"));
    drop(unrelated);
    surface.begin_frame();
    let image = surface.acquire([64, 48]).unwrap().unwrap();
    let index = owner
        .submit(&[clear(&owner, &image, wgt::Color::RED)])
        .unwrap();
    drop(image);
    surface.present().unwrap();
    owner.wait_for(index).unwrap();
    presented(&window, 0xff0000);
    surface.begin_frame();
    let image = surface.acquire([64, 48]).unwrap().unwrap();
    drop(image);
    drop(window);
    drop(display);
    assert!(weak_display.upgrade().is_some());
    drop(surface);
    assert!(weak_display.upgrade().is_none());
    drop(owner);
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), before);
}

#[test]
#[ignore = "Requires native X11, Vulkan presentation and Khronos validation"]
fn native_empty_surface_present_includes_core_submission_in_completion() {
    validation_logging();
    let window = Rc::new(unsafe { X11Window::new() });
    let mut backend = super::super::super::render_device::RenderDevice::new_windowed(&Options {
        window: Some(window.clone()),
        validation: true,
        ..Default::default()
    })
    .unwrap();
    let before = VALIDATION_ERRORS.load(Ordering::Relaxed);
    backend.begin_frame().unwrap();
    assert!(backend
        .prepare_default_target(DrawTarget::new_default(
            api::units::DeviceIntSize::new(64, 48),
            true
        ))
        .unwrap());
    let index = backend.end_frame().unwrap();
    let observed = backend.owner.queue.on_submitted_work_done(Box::new(|| {}));
    assert!(observed.is_none_or(|tail| tail <= index));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while backend.completed.load(Ordering::Acquire) < index {
        backend.poll().unwrap();
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
    presented(&window, 0x000000);
    backend.deinit();
    assert!(backend.failure().is_none());
    drop(backend);
    drop(window);
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), before);
}

#[test]
#[ignore = "Requires native X11, Vulkan presentation and Khronos validation"]
fn native_surface_destroys_core_surface_before_pinned_window_owner() {
    use std::cell::{Cell, RefCell};
    struct ObservedWindow {
        window: X11Window,
        core_surface: Rc<RefCell<Option<std::sync::Weak<core::instance::Surface>>>>,
        dropped: Rc<Cell<bool>>,
    }
    impl HasDisplayHandle for ObservedWindow {
        fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
            self.window.display_handle()
        }
    }
    impl HasWindowHandle for ObservedWindow {
        fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
            self.window.window_handle()
        }
    }
    // The owned X11Window pins its window and display until this wrapper is dropped.
    unsafe impl SurfaceWindow for ObservedWindow {}
    impl Drop for ObservedWindow {
        fn drop(&mut self) {
            assert!(
                self.core_surface
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .upgrade()
                    .is_none(),
                "core surface outlived its pinned native owner"
            );
            self.dropped.set(true);
        }
    }
    validation_logging();
    let before = VALIDATION_ERRORS.load(Ordering::Relaxed);
    for detach in [true, false] {
        let probe = Rc::new(RefCell::new(None));
        let dropped = Rc::new(Cell::new(false));
        let window = Rc::new(ObservedWindow {
            window: unsafe { X11Window::new() },
            core_surface: probe.clone(),
            dropped: dropped.clone(),
        });
        let (owner, initial) = Device::new_with_surface(&Options {
            window: Some(window.clone()),
            display_owner: Some(window.window.connection.clone()),
            validation: true,
            ..Default::default()
        })
        .unwrap();
        let owner = Arc::new(owner);
        let mut surface = Surface::new(&owner, initial.unwrap());
        *probe.borrow_mut() = Some(Arc::downgrade(&surface.window.as_ref().unwrap().raw));
        drop(window);
        assert!(!dropped.get());
        let image = surface.acquire([64, 48]).unwrap().unwrap();
        drop(image);
        if detach {
            surface.discard().unwrap();
            surface.replace(None, SurfaceOptions::default()).unwrap();
            assert!(dropped.get());
        }
        drop(surface);
        assert!(dropped.get());
        drop(owner);
    }
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), before);
}
