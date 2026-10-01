/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::vulkan::{
    hal, wgt, Device, Options, SurfaceOptions,
    resources::Owned,
    swapchain::{PresentationStatus, Swapchain},
    tests::{upload_queue, validation_logging, ERRORS},
};
use std::os::raw::{c_char, c_int, c_long, c_uint, c_ulong, c_void};
use std::sync::atomic::Ordering;
use wgpu_hal::{Adapter as _, CommandEncoder as _, Device as _};

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

#[test]
#[ignore = "Requires 24-bit Xvfb, a presentation-capable Vulkan adapter and validation"]
fn swapchain_consecutive_passes_order_color_and_depth_writes() {
    use crate::device::vulkan::{Texture, TextureFilter};
    use crate::device::vulkan::draw::{DrawPass, tests::synchronization::record_overlapping_passes};
    use api::units::DeviceIntPoint;

    validation_logging();
    let window = Rc::new(unsafe { X11Window::new() });
    let device = Rc::new(
        Device::new(&Options {
            window: Some(window.clone()),
            validation: true,
            ..Default::default()
        }).unwrap(),
    );
    let queue = Rc::new(upload_queue(&device));
    let mut swapchain = Swapchain::new(&queue).unwrap();
    swapchain.configure([64, 48], SurfaceOptions::default()).unwrap();
    let depth = Texture::new(
        &device, 64, 48, wgt::TextureFormat::Depth32Float, TextureFilter::Nearest, true,
    ).unwrap();
    let target = swapchain.acquire().unwrap().unwrap().into_target().unwrap();
    record_overlapping_passes(
        &DrawPass {
            target: &target,
            origin: DeviceIntPoint::zero(),
            viewport: None,
            depth: Some(&depth),
            clear_color: None,
            clear_depth: None,
            depth_range: 0.0..1.0,
        },
        &mut queue.recording().unwrap(),
    );
    assert!(matches!(
        target.present().unwrap(),
        PresentationStatus::Presented { .. }
    ));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if window.pixels([64, 48]).iter().all(|&pixel| {
            ((pixel >> 16) as u8).abs_diff(128) <= 1
                && ((pixel >> 8) as u8).abs_diff(128) <= 1
                && pixel as u8 == 0
        }) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Presented attachment pixels did not match"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    drop(swapchain);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

impl X11Window {
    fn pixels(&self, size: [u32; 2]) -> Vec<c_ulong> {
        unsafe {
            let get_image = self
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
                .library
                .get::<unsafe extern "C" fn(*mut c_void, c_int, c_int) -> c_ulong>(b"XGetPixel\0")
                .unwrap();
            let destroy = self
                .library
                .get::<unsafe extern "C" fn(*mut c_void) -> c_int>(b"XDestroyImage\0")
                .unwrap();
            let image = get_image(
                self.display.as_ptr(),
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
        let caps = unsafe { device.adapter.surface_capabilities(&surface.raw) }.unwrap();
        assert_eq!(surface.options.vsync, vsync);
        device.surface.set(Some(surface));
        let queue = Rc::new(upload_queue(&device));
        let mut swapchain = Swapchain::new(&queue).unwrap();
        assert!(Swapchain::new(&queue).is_none());
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
            adapter_name: Some("no such Vulkan adapter".into()),
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
    let queue = Rc::new(upload_queue(&device));
    let mut swapchain = Swapchain::new(&queue).unwrap();
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

#[test]
#[ignore = "Requires X11, a presentation-capable Vulkan adapter and validation"]
fn swapchain_acquisition_discards_and_drains_the_shared_queue() {
    validation_logging();
    let device = Rc::new(
        Device::new(&Options {
            window: Some(Rc::new(unsafe { X11Window::new() })),
            validation: true,
            ..Default::default()
        })
        .unwrap(),
    );
    let queue = Rc::new(upload_queue(&device));
    let mut swapchain = Swapchain::new(&queue).unwrap();
    assert!(swapchain.acquire().unwrap().is_none());
    for explicit in [false, true, false, true] {
        swapchain
            .configure([64, 48], SurfaceOptions::default())
            .unwrap();
        let marker = Rc::new(());
        queue.recording().unwrap().keep(marker.clone());
        let image = swapchain.acquire().unwrap().unwrap();
        if explicit {
            image.discard().unwrap();
        } else {
            drop(image);
        }
        assert_eq!(Rc::strong_count(&marker), 1);
        assert!(!queue.has_pending_work());
        assert!(swapchain.configuration().is_none());
        assert!(swapchain.acquire().unwrap().is_none());
    }
    swapchain
        .configure([64, 48], SurfaceOptions::default())
        .unwrap();
    std::mem::forget(swapchain.acquire().unwrap().unwrap());
    assert!(swapchain.acquire().is_err());
    assert!(swapchain
        .configure([0, 0], SurfaceOptions::default())
        .is_err());
    drop(swapchain);
    drop(queue);
    drop(device);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires X11, a presentation-capable Vulkan adapter and validation"]
fn swapchain_submissions_clear_the_acquired_image_with_queue_fences() {
    validation_logging();
    let device = Rc::new(
        Device::new(&Options {
            window: Some(Rc::new(unsafe { X11Window::new() })),
            validation: true,
            ..Default::default()
        })
        .unwrap(),
    );
    let queue = Rc::new(upload_queue(&device));
    let mut swapchain = Swapchain::new(&queue).unwrap();
    for _ in 0..3 {
        swapchain
            .configure([64, 48], SurfaceOptions::default())
            .unwrap();
        let image = swapchain.acquire().unwrap().unwrap();
        let config = image.configuration();
        let range = wgt::ImageSubresourceRange {
            mip_level_count: Some(1),
            array_layer_count: Some(1),
            ..Default::default()
        };
        let view = Rc::new(Owned::new(
            &device,
            unsafe {
                device
                    .raw_device()
                    .create_texture_view(
                        image.texture(),
                        &hal::TextureViewDescriptor {
                            label: Some("WR acquired image test"),
                            format: config.format,
                            swizzle: Default::default(),
                            dimension: wgt::TextureViewDimension::D2,
                            usage: wgt::TextureUses::COLOR_TARGET,
                            range: range.clone(),
                        },
                    )
                    .unwrap()
            },
            hal::vulkan::Device::destroy_texture_view,
        ));
        let weak = Rc::downgrade(&view);
        for step in 0..3 {
            let mut commands = queue.recording().unwrap();
            unsafe {
                commands
                    .encoder()
                    .transition_textures(std::iter::once(hal::TextureBarrier {
                        queue_family_ownership_transfer: None,
                        texture: image.texture(),
                        range: range.clone(),
                        usage: hal::StateTransition {
                            from: if step == 0 {
                                wgt::TextureUses::UNINITIALIZED
                            } else {
                                wgt::TextureUses::COLOR_TARGET
                            },
                            to: wgt::TextureUses::COLOR_TARGET,
                        },
                    }));
                commands
                    .encoder()
                    .begin_render_pass(&hal::RenderPassDescriptor {
                        label: Some("WR direct swapchain clear"),
                        extent: config.extent,
                        sample_count: 1,
                        color_attachments: &[Some(hal::ColorAttachment {
                            target: hal::Attachment {
                                view: &view,
                                usage: wgt::TextureUses::COLOR_TARGET,
                            },
                            depth_slice: None,
                            resolve_target: None,
                            ops: hal::AttachmentOps::LOAD_CLEAR | hal::AttachmentOps::STORE,
                            clear_value: wgt::Color {
                                r: step as f64 * 0.5,
                                g: 0.5,
                                b: 0.0,
                                a: 1.0,
                            },
                        })],
                        depth_stencil_attachment: None,
                        multiview_mask: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                    })
                    .unwrap();
                commands.encoder().end_render_pass();
            }
            commands.keep(view.clone());
            drop(commands);
            match step {
                0 => {
                    queue.submit().unwrap();
                }
                1 => {
                    queue.create_fence().unwrap();
                }
                _ => {
                    queue.wait().unwrap();
                }
            }
        }
        drop(view);
        assert!(weak.upgrade().is_none());
        image.discard().unwrap();
        assert!(swapchain.configuration().is_none());
        queue.recording().unwrap();
        queue.wait().unwrap();
    }
    drop(swapchain);
    drop(queue);
    drop(device);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires 24-bit Xvfb, a presentation-capable Vulkan adapter and validation"]
fn swapchain_presentation_displays_directly_rendered_pixels() {
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
    let queue = Rc::new(upload_queue(&device));
    let mut swapchain = Swapchain::new(&queue).unwrap();
    let mut views = Vec::new();
    for size in [[64, 48], [32, 24]] {
        window.resize(size);
        swapchain
            .configure(size, SurfaceOptions::default())
            .unwrap();
        for rgb in [[255u8, 0, 0], [0, 255, 0], [0, 0, 255], [64, 128, 192]] {
            let image = swapchain.acquire().unwrap().unwrap();
            let config = image.configuration();
            let range = wgt::ImageSubresourceRange {
                mip_level_count: Some(1),
                array_layer_count: Some(1),
                ..Default::default()
            };
            let view = Rc::new(Owned::new(
                &device,
                unsafe {
                    device
                        .raw_device()
                        .create_texture_view(
                            image.texture(),
                            &hal::TextureViewDescriptor {
                                label: Some("WR presented image test"),
                                format: config.format,
                                swizzle: Default::default(),
                                dimension: wgt::TextureViewDimension::D2,
                                usage: wgt::TextureUses::COLOR_TARGET,
                                range: range.clone(),
                            },
                        )
                        .unwrap()
                },
                hal::vulkan::Device::destroy_texture_view,
            ));
            views.push(Rc::downgrade(&view));
            let mut commands = queue.recording().unwrap();
            unsafe {
                commands
                    .encoder()
                    .transition_textures(std::iter::once(hal::TextureBarrier {
                        queue_family_ownership_transfer: None,
                        texture: image.texture(),
                        range: range.clone(),
                        usage: hal::StateTransition {
                            from: wgt::TextureUses::UNINITIALIZED,
                            to: wgt::TextureUses::COLOR_TARGET,
                        },
                    }));
                commands
                    .encoder()
                    .begin_render_pass(&hal::RenderPassDescriptor {
                        label: Some("WR direct presentation test"),
                        extent: config.extent,
                        sample_count: 1,
                        color_attachments: &[Some(hal::ColorAttachment {
                            target: hal::Attachment {
                                view: &view,
                                usage: wgt::TextureUses::COLOR_TARGET,
                            },
                            depth_slice: None,
                            resolve_target: None,
                            ops: hal::AttachmentOps::LOAD_CLEAR | hal::AttachmentOps::STORE,
                            clear_value: wgt::Color {
                                r: f64::from(rgb[0]) / 255.0,
                                g: f64::from(rgb[1]) / 255.0,
                                b: f64::from(rgb[2]) / 255.0,
                                a: 1.0,
                            },
                        })],
                        depth_stencil_attachment: None,
                        multiview_mask: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                    })
                    .unwrap();
                commands.encoder().end_render_pass();
                commands
                    .encoder()
                    .transition_textures(std::iter::once(hal::TextureBarrier {
                        queue_family_ownership_transfer: None,
                        texture: image.texture(),
                        range,
                        usage: hal::StateTransition {
                            from: wgt::TextureUses::COLOR_TARGET,
                            to: wgt::TextureUses::PRESENT,
                        },
                    }));
            }
            commands.keep(view);
            drop(commands);
            assert!(matches!(
                unsafe { image.present() }.unwrap(),
                PresentationStatus::Presented { .. }
            ));
            assert!(queue.has_pending_work());
            let expected = (c_ulong::from(rgb[0]) << 16)
                | (c_ulong::from(rgb[1]) << 8)
                | c_ulong::from(rgb[2]);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let pixels = window.pixels(size);
                if pixels.iter().all(|&pixel| pixel == expected) {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "Expected {expected:#x}, got {:?}",
                    &pixels[..8]
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
    queue.wait().unwrap();
    assert!(views.iter().all(|view| view.upgrade().is_none()));
    drop(swapchain);
    drop(queue);
    drop(device);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires 24-bit Xvfb, Vulkan shaders, a presentation-capable adapter and validation"]
#[cfg(wr_vulkan_shaders)]
fn swapchain_attachment_uses_webrender_draw_pass_and_tracks_abandonment() {
    use crate::device::vulkan::{
        Buffer,
        draw::{ColorAttachment, DrawBatch, DrawPass},
        pipeline::DrawPipeline,
        shader::select_draw_shader,
    };
    use crate::device::RenderState;
    use api::units::{DeviceIntPoint, DeviceIntRect, DeviceIntSize};
    use euclid::default::Transform3D;

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
    let queue = Rc::new(upload_queue(&device));
    let mut swapchain = Swapchain::new(&queue).unwrap();
    let full = |size: [u32; 2]| {
        DeviceIntRect::from_size(DeviceIntSize::new(size[0] as i32, size[1] as i32))
    };
    swapchain
        .configure([64, 48], SurfaceOptions::default())
        .unwrap();
    {
        let target = swapchain.acquire().unwrap().unwrap().into_target().unwrap();
        let pass = DrawPass {
            target: &target,
            origin: DeviceIntPoint::zero(),
            viewport: None,
            depth: None,
            clear_color: None,
            clear_depth: None,
            depth_range: 0.0..1.0,
        };
        let other_queue = upload_queue(&device);
        assert!(pass
            .clear_rect(
                &mut other_queue.recording().unwrap(),
                full([64, 48]),
                Some([0.0, 0.0, 1.0, 1.0]),
                None
            )
            .is_err());
        pass.clear_rect(
            &mut queue.recording().unwrap(),
            full([64, 48]),
            Some([0.0, 0.0, 1.0, 1.0]),
            None,
        )
        .unwrap();
        queue.discard_recording();
        assert!(target.present().unwrap_err().contains("uninitialized"));
    }
    assert!(swapchain.configuration().is_none());
    assert!(!device.is_lost());
    let quad = Buffer::new(
        &device,
        &[0, 0, 0, 0, 255, 0, 0, 0, 0, 255, 0, 0, 255, 255, 0, 0],
        wgt::BufferUses::VERTEX,
    )
    .unwrap();
    for size in [[64, 48], [32, 24]] {
        window.resize(size);
        swapchain
            .configure(size, SurfaceOptions::default())
            .unwrap();
        let target = swapchain.acquire().unwrap().unwrap().into_target().unwrap();
        let pipeline = DrawPipeline::new(
            &device,
            select_draw_shader("ps_clear", &[], false).unwrap(),
            (&target).format(),
            false,
            RenderState::default(),
        )
        .unwrap();
        {
            let pass = DrawPass {
                target: &target,
                origin: DeviceIntPoint::zero(),
                viewport: None,
                depth: None,
                clear_color: None,
                clear_depth: None,
                depth_range: 0.0..1.0,
            };
            pass.clear_rect(
                &mut queue.recording().unwrap(),
                full(size),
                Some([0.0, 0.0, 1.0, 1.0]),
                None,
            )
            .unwrap();
            queue.create_fence().unwrap();
            let values = [
                8.0f32,
                8.0,
                (size[0] - 8) as f32,
                (size[1] - 8) as f32,
                0.0,
                1.0,
                0.0,
                1.0,
            ];
            let bytes: Vec<_> = values
                .iter()
                .flat_map(|value| value.to_ne_bytes())
                .collect();
            let projection = pass.projection(&Transform3D::ortho(
                0.0,
                size[0] as f32,
                0.0,
                size[1] as f32,
                -1.0,
                1.0,
            ));
            pass.record_batches(
                &mut queue.recording().unwrap(),
                &queue,
                &quad,
                None,
                &[DrawBatch {
                    pipeline: &pipeline,
                    projection: Some(&projection),
                    textures: &[],
                    buffers: &[],
                    instances: &bytes,
                    instance_count: 1,
                    scissor: full(size),
                }],
            )
            .unwrap();
            pass.clear_rect(
                &mut queue.recording().unwrap(),
                DeviceIntRect::from_origin_and_size(
                    DeviceIntPoint::new(2, 2),
                    DeviceIntSize::new(4, 4),
                ),
                Some([1.0, 0.0, 0.0, 1.0]),
                None,
            )
            .unwrap();
        }
        assert!(matches!(
            target.present().unwrap(),
            PresentationStatus::Presented { .. }
        ));
        let expected: Vec<_> = (0..size[1])
            .flat_map(|y| {
                (0..size[0]).map(move |x| {
                    if (2..6).contains(&x) && (2..6).contains(&y) {
                        0xff0000
                    } else if (8..size[0] - 8).contains(&x) && (8..size[1] - 8).contains(&y) {
                        0x00ff00
                    } else {
                        0x0000ff
                    }
                })
            })
            .collect();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let actual = window.pixels(size);
            if actual == expected {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "Swapchain draw mismatch: {:?}",
                actual.iter().zip(&expected).position(|(a, b)| a != b)
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    queue.wait().unwrap();
    swapchain
        .configure([32, 24], SurfaceOptions::default())
        .unwrap();
    std::mem::forget(swapchain.acquire().unwrap().unwrap().into_target().unwrap());
    assert!(swapchain
        .configure([0, 0], SurfaceOptions::default())
        .is_err());
    let weak = Rc::downgrade(&device);
    drop(swapchain);
    drop(queue);
    drop(quad);
    drop(device);
    assert!(weak.upgrade().is_none());
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
