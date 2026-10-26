use super::*;
use crate::vulkan::swapchain::{Generation, NativeSurface, NativeSurfaceOwner, NativeSwapchain};
use core::borrow::Borrow;
use raw_window_handle::{RawDisplayHandle, RawWindowHandle, XlibDisplayHandle, XlibWindowHandle};
use std::{
    os::raw::{c_char, c_int, c_uint, c_ulong, c_void},
    ptr::NonNull,
    sync::Weak,
    time::{Duration, Instant},
};

const SIZE: u32 = 32;

struct Window {
    library: libloading::Library,
    display: NonNull<c_void>,
    screen: c_int,
    window: c_ulong,
}

impl Window {
    unsafe fn new(index: usize) -> Self {
        let library = unsafe { libloading::Library::new("libX11.so.6") }.unwrap();
        let threads = unsafe {
            library
                .get::<unsafe extern "C" fn() -> c_int>(b"XInitThreads\0")
                .unwrap()()
        };
        assert_ne!(
            threads, 0,
            "XInitThreads is required for presentation races"
        );
        let display = NonNull::new(unsafe {
            library
                .get::<unsafe extern "C" fn(*const c_char) -> *mut c_void>(b"XOpenDisplay\0")
                .unwrap()(core::ptr::null())
        })
        .expect("A 24-bit Xvfb display is required");
        let screen = unsafe {
            library
                .get::<unsafe extern "C" fn(*mut c_void) -> c_int>(b"XDefaultScreen\0")
                .unwrap()(display.as_ptr())
        };
        let root = unsafe {
            library
                .get::<unsafe extern "C" fn(*mut c_void, c_int) -> c_ulong>(b"XRootWindow\0")
                .unwrap()(display.as_ptr(), screen)
        };
        let window = unsafe {
            library
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
                .unwrap()(
                display.as_ptr(),
                root,
                (index * 64) as c_int,
                0,
                SIZE,
                SIZE,
                0,
                0,
                0,
            )
        };
        assert_ne!(window, 0);
        unsafe {
            library
                .get::<unsafe extern "C" fn(*mut c_void, c_ulong) -> c_int>(b"XMapWindow\0")
                .unwrap()(display.as_ptr(), window);
            library
                .get::<unsafe extern "C" fn(*mut c_void, c_int) -> c_int>(b"XSync\0")
                .unwrap()(display.as_ptr(), 0);
        }
        Self {
            library,
            display,
            screen,
            window,
        }
    }

    fn handles(&self) -> (RawDisplayHandle, RawWindowHandle) {
        (
            RawDisplayHandle::Xlib(XlibDisplayHandle::new(Some(self.display), self.screen)),
            RawWindowHandle::Xlib(XlibWindowHandle::new(self.window)),
        )
    }

    fn pixels(&self) -> Vec<u32> {
        unsafe {
            self.library
                .get::<unsafe extern "C" fn(*mut c_void, c_int) -> c_int>(b"XSync\0")
                .unwrap()(self.display.as_ptr(), 0);
            let image = self
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
                .unwrap()(
                self.display.as_ptr(), self.window, 0, 0, SIZE, SIZE, !0, 2
            );
            assert!(!image.is_null());
            let get_pixel = self
                .library
                .get::<unsafe extern "C" fn(*mut c_void, c_int, c_int) -> c_ulong>(b"XGetPixel\0")
                .unwrap();
            let mut pixels = Vec::new();
            for y in 0..SIZE {
                for x in 0..SIZE {
                    pixels.push(get_pixel(image, x as c_int, y as c_int) as u32 & 0x00ff_ffff);
                }
            }
            self.library
                .get::<unsafe extern "C" fn(*mut c_void) -> c_int>(b"XDestroyImage\0")
                .unwrap()(image);
            pixels
        }
    }

    fn assert_pixels(&self, expected: u32) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if self.pixels().iter().all(|pixel| *pixel == expected) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "Native presentation pixels did not match"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Window {
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

struct Payload {
    work: Work,
    frames: Vec<Option<Arc<crate::DynAcquiredSurfaceTexture>>>,
    surfaces: Vec<Option<Arc<dyn crate::DynSurface>>>,
    adapter: crate::DynExposedAdapter,
    instance: Box<dyn crate::DynInstance>,
    windows: Vec<Window>,
    configs: Vec<crate::SurfaceConfiguration>,
    fences: Vec<usize>,
    serials: Vec<u64>,
}

struct Fixture(ManuallyDrop<Payload>);

impl Deref for Fixture {
    type Target = Payload;
    fn deref(&self) -> &Payload {
        &self.0
    }
}
impl DerefMut for Fixture {
    fn deref_mut(&mut self) -> &mut Payload {
        &mut self.0
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let retired = self.work.retire();
        if retired.is_err() {
            self.work.quarantined = true;
            if !std::thread::panicking() {
                panic!("Presentation fixture owners quarantined: {retired:?}");
            }
            return;
        }
        unsafe { ManuallyDrop::drop(&mut self.0) };
    }
}

impl Fixture {
    fn new(count: usize) -> Self {
        assert!(cfg!(all(feature = "validation_canary", debug_assertions)));
        let windows: Vec<_> = (0..count)
            .map(|index| unsafe { Window::new(index) })
            .collect();
        let (backend, instance) = unsafe {
            crate::shared::create_presentation_instance(&crate::InstanceDescriptor {
                name: "shared presentation tests",
                flags: wgt::InstanceFlags::VALIDATION | wgt::InstanceFlags::DEBUG,
                memory_budget_thresholds: Default::default(),
                backend_options: Default::default(),
                telemetry: None,
                display: None,
            })
        }
        .unwrap();
        assert_eq!(backend, wgt::Backend::Vulkan);
        let surfaces: Vec<Option<Arc<dyn crate::DynSurface>>> = windows
            .iter()
            .map(|window| {
                let (display, handle) = window.handles();
                Some(Arc::from(
                    unsafe { instance.create_surface(display, handle) }.unwrap(),
                ))
            })
            .collect();
        let adapter = unsafe { instance.enumerate_adapters(None) }
            .into_iter()
            .find(|adapter| {
                adapter.info.device_type != wgt::DeviceType::Cpu
                    && surfaces.iter().all(|surface| unsafe {
                        adapter
                            .adapter
                            .surface_capabilities(&**surface.as_ref().unwrap())
                            .is_some()
                    })
            })
            .expect("Hardware adapter presenting to the test windows");
        let pair = unsafe {
            crate::shared::open_shared_device(
                &*adapter.adapter,
                wgt::Features::empty(),
                &adapter.capabilities.limits,
                &wgt::MemoryHints::default(),
            )
        }
        .unwrap();
        assert!(
            crate::renderer::graphics_capabilities(
                &*instance,
                &*adapter.adapter,
                &*pair.graphics.device
            )
            .unwrap()
            .validation_enabled
        );
        let mut work = Work::with_endpoints(pair);
        let configs = surfaces
            .iter()
            .map(|surface| {
                let caps = unsafe {
                    adapter
                        .adapter
                        .surface_capabilities(&**surface.as_ref().unwrap())
                }
                .unwrap();
                let format = [
                    wgt::TextureFormat::Bgra8Unorm,
                    wgt::TextureFormat::Rgba8Unorm,
                ]
                .into_iter()
                .find(|format| {
                    caps.formats.iter().any(|entry| {
                        entry.format == *format
                            && entry.color_spaces.contains(wgt::SurfaceColorSpaces::SRGB)
                    })
                })
                .expect("RGB8 sRGB surface format");
                let usage = wgt::TextureUses::COLOR_TARGET | wgt::TextureUses::COPY_SRC;
                assert!(caps.usage.contains(usage));
                assert!(caps.present_modes.contains(&wgt::PresentMode::Fifo));
                assert!(caps
                    .composite_alpha_modes
                    .contains(&wgt::CompositeAlphaMode::Opaque));
                crate::SurfaceConfiguration {
                    maximum_frame_latency: 2u32.clamp(
                        *caps.maximum_frame_latency.start(),
                        *caps.maximum_frame_latency.end(),
                    ),
                    present_mode: wgt::PresentMode::Fifo,
                    composite_alpha_mode: wgt::CompositeAlphaMode::Opaque,
                    format,
                    color_space: wgt::SurfaceColorSpace::Srgb,
                    extent: wgt::Extent3d {
                        width: SIZE,
                        height: SIZE,
                        depth_or_array_layers: 1,
                    },
                    usage,
                    view_formats: Vec::new(),
                }
            })
            .collect();
        let fences = (0..count).map(|_| work.fence()).collect();
        Self(ManuallyDrop::new(Payload {
            work,
            frames: Vec::new(),
            surfaces,
            adapter,
            instance,
            windows,
            configs,
            fences,
            serials: vec![0; count],
        }))
    }

    fn surface(&self, index: usize) -> &Arc<dyn crate::DynSurface> {
        self.surfaces[index].as_ref().unwrap()
    }

    fn configure(&self, index: usize, enabled: bool) -> Result<(), crate::SurfaceError> {
        unsafe {
            crate::renderer::configure_surface(
                &**self.surface(index),
                &*self.work.graphics.device,
                enabled.then_some(&self.configs[index]),
            )
        }
    }

    fn generation(&self, index: usize) -> Arc<Generation> {
        let surface = self
            .surface(index)
            .as_any()
            .downcast_ref::<crate::vulkan::Surface>()
            .unwrap();
        let chain = surface.swapchain.read();
        chain
            .as_ref()
            .unwrap()
            .as_any()
            .downcast_ref::<NativeSwapchain>()
            .unwrap()
            .shared_generation()
            .unwrap()
    }

    fn surface_owner(&self, index: usize) -> Weak<NativeSurfaceOwner> {
        self.surface(index)
            .as_any()
            .downcast_ref::<crate::vulkan::Surface>()
            .unwrap()
            .inner
            .as_any()
            .downcast_ref::<NativeSurface>()
            .unwrap()
            .owner_weak()
    }

    fn acquire(&mut self, index: usize) -> usize {
        let frame = unsafe {
            self.surface(index).acquire_texture(
                Some(Duration::from_secs(5)),
                &*self.work.fences[self.fences[index]],
            )
        }
        .unwrap();
        let position = self.frames.len();
        self.frames.push(Some(Arc::new(frame)));
        position
    }

    fn view(&mut self, surface: usize, frame: usize) -> usize {
        let view = unsafe {
            self.work.graphics.device.create_texture_view(
                self.frames[frame]
                    .as_ref()
                    .unwrap()
                    .texture
                    .as_ref()
                    .borrow(),
                &crate::TextureViewDescriptor {
                    label: None,
                    format: self.configs[surface].format,
                    dimension: wgt::TextureViewDimension::D2,
                    usage: wgt::TextureUses::COLOR_TARGET,
                    range: wgt::ImageSubresourceRange {
                        aspect: wgt::TextureAspect::All,
                        base_mip_level: 0,
                        mip_level_count: Some(1),
                        base_array_layer: 0,
                        array_layer_count: Some(1),
                    },
                    swizzle: Default::default(),
                },
            )
        }
        .unwrap();
        let index = self.work.graphics_views.len();
        self.work.graphics_views.push(view);
        index
    }

    fn render(&mut self, surface: usize, frame: usize, color: wgt::Color) {
        let view = self.view(surface, frame);
        let alignment = self
            .adapter
            .capabilities
            .alignments
            .buffer_copy_pitch
            .get()
            .max(4);
        let pitch = (u64::from(SIZE) * 4).div_ceil(alignment) * alignment;
        let size = pitch * u64::from(SIZE);
        let output = self
            .work
            .buffer(size, wgt::BufferUses::COPY_DST | wgt::BufferUses::MAP_READ);
        let encoder = self.work.encoder(Lane::Graphics);
        self.work.begin(encoder);
        let frame = self.frames[frame].as_ref().unwrap().clone();
        let texture: &dyn crate::DynTexture = frame.texture.as_ref().borrow();
        {
            let data: &mut Resources = &mut self.work;
            let raw = &mut data.encoders[encoder].raw;
            unsafe {
                raw.transition_textures(&[crate::TextureBarrier {
                    texture,
                    range: wgt::ImageSubresourceRange::default(),
                    usage: crate::StateTransition {
                        from: wgt::TextureUses::UNINITIALIZED,
                        to: wgt::TextureUses::COLOR_TARGET,
                    },
                    queue_family_ownership_transfer: None,
                }]);
                raw.begin_render_pass(&crate::RenderPassDescriptor {
                    label: None,
                    extent: wgt::Extent3d {
                        width: SIZE,
                        height: SIZE,
                        depth_or_array_layers: 1,
                    },
                    sample_count: 1,
                    color_attachments: &[Some(crate::ColorAttachment {
                        target: crate::Attachment {
                            view: &*data.graphics_views[view],
                            usage: wgt::TextureUses::COLOR_TARGET,
                        },
                        depth_slice: None,
                        resolve_target: None,
                        ops: crate::AttachmentOps::LOAD_CLEAR | crate::AttachmentOps::STORE,
                        clear_value: color,
                    })],
                    depth_stencil_attachment: None,
                    multiview_mask: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                })
                .unwrap();
                raw.end_render_pass();
                raw.transition_textures(&[crate::TextureBarrier {
                    texture,
                    range: wgt::ImageSubresourceRange::default(),
                    usage: crate::StateTransition {
                        from: wgt::TextureUses::COLOR_TARGET,
                        to: wgt::TextureUses::COPY_SRC,
                    },
                    queue_family_ownership_transfer: None,
                }]);
                raw.transition_buffers(&[crate::BufferBarrier {
                    buffer: &*data.buffers[output],
                    usage: crate::StateTransition {
                        from: wgt::BufferUses::empty(),
                        to: wgt::BufferUses::COPY_DST,
                    },
                }]);
                raw.copy_texture_to_buffer(
                    texture,
                    wgt::TextureUses::COPY_SRC,
                    &*data.buffers[output],
                    &[crate::BufferTextureCopy {
                        buffer_layout: wgt::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(pitch as u32),
                            rows_per_image: None,
                        },
                        texture_base: crate::TextureCopyBase {
                            mip_level: 0,
                            array_layer: 0,
                            origin: wgt::Origin3d::ZERO,
                            aspect: crate::FormatAspects::COLOR,
                        },
                        size: wgt::Extent3d {
                            width: SIZE,
                            height: SIZE,
                            depth_or_array_layers: 1,
                        }
                        .into(),
                    }],
                );
                raw.transition_buffers(&[crate::BufferBarrier {
                    buffer: &*data.buffers[output],
                    usage: crate::StateTransition {
                        from: wgt::BufferUses::COPY_DST,
                        to: wgt::BufferUses::MAP_READ,
                    },
                }]);
                raw.transition_textures(&[crate::TextureBarrier {
                    texture,
                    range: wgt::ImageSubresourceRange::default(),
                    usage: crate::StateTransition {
                        from: wgt::TextureUses::COPY_SRC,
                        to: wgt::TextureUses::PRESENT,
                    },
                    queue_family_ownership_transfer: None,
                }]);
            }
        }
        let buffer = self.work.end(encoder);
        self.serials[surface] += 1;
        let submitted = unsafe {
            self.work.context.submit_graphics(
                &*self.work.graphics.queue,
                &[&*self.work.encoders[encoder].buffers[buffer]],
                &[&*frame.texture],
                (
                    &*self.work.fences[self.fences[surface]],
                    self.serials[surface],
                ),
                &crate::external_sync::SubmissionSync::default(),
            )
        };
        self.work.retain(submitted).unwrap();
        assert!(self.work.receipts.last().unwrap().wait(None).unwrap());
        let bytes = unsafe {
            let mapping = self
                .work
                .compute
                .device
                .map_buffer(&*self.work.buffers[output], 0..size)
                .unwrap();
            if !mapping.is_coherent {
                self.work
                    .compute
                    .device
                    .invalidate_mapped_ranges(&*self.work.buffers[output], &[0..size]);
            }
            let bytes = core::slice::from_raw_parts(mapping.ptr.as_ptr(), size as usize).to_vec();
            self.work
                .compute
                .device
                .unmap_buffer(&*self.work.buffers[output]);
            bytes
        };
        let mut expected = [
            (color.r * 255.0) as u8,
            (color.g * 255.0) as u8,
            (color.b * 255.0) as u8,
            255,
        ];
        if self.configs[surface].format == wgt::TextureFormat::Bgra8Unorm {
            expected.swap(0, 2);
        }
        for row in 0..SIZE {
            for column in 0..SIZE {
                let offset = (u64::from(row) * pitch + u64::from(column) * 4) as usize;
                assert_eq!(&bytes[offset..offset + 4], &expected);
            }
        }
    }

    fn take(&mut self, frame: usize) -> crate::DynAcquiredSurfaceTexture {
        Arc::try_unwrap(self.frames[frame].take().unwrap())
            .ok()
            .unwrap()
    }

    fn present(&mut self, surface: usize, frame: usize) -> Result<(), crate::SurfaceError> {
        let frame = self.take(frame);
        unsafe {
            self.work
                .graphics
                .queue
                .present(&**self.surface(surface), frame.texture)
        }
    }

    fn draw(&mut self, surface: usize, color: wgt::Color, rgb: u32) {
        let frame = self.acquire(surface);
        self.render(surface, frame, color);
        self.present(surface, frame).unwrap();
        self.windows[surface].assert_pixels(rgb);
    }
}

#[test]
#[ignore = "Requires 24-bit Xvfb, shared Vulkan maintenance1 and Khronos validation"]
fn shared_surface_graphics_presents_and_rejects_other_endpoints() {
    let mut f = Fixture::new(2);
    f.configure(0, true).unwrap();
    f.configure(1, true).unwrap();
    let original = f.generation(0);
    assert!(unsafe {
        crate::renderer::configure_surface(
            &**f.surface(0),
            &*f.work.compute.device,
            Some(&f.configs[0]),
        )
    }
    .is_err());
    let foreign = unsafe {
        crate::shared::open_shared_device(
            &*f.adapter.adapter,
            wgt::Features::empty(),
            &f.adapter.capabilities.limits,
            &wgt::MemoryHints::default(),
        )
    }
    .unwrap();
    for config in [Some(&f.configs[0]), None] {
        assert!(unsafe {
            crate::renderer::configure_surface(&**f.surface(0), &*foreign.graphics.device, config)
        }
        .is_err());
        assert!(Arc::ptr_eq(&original, &f.generation(0)));
        original.check_open().unwrap();
    }
    drop(foreign);
    let frame = f.acquire(0);
    let result = unsafe {
        f.work.context.submit_graphics(
            &*f.work.compute.queue,
            &[],
            &[&*f.frames[frame].as_ref().unwrap().texture],
            (&*f.work.fences[f.fences[0]], 1),
            &crate::external_sync::SubmissionSync::default(),
        )
    };
    let rejected = matches!(&result, Err(error) if error.receipt.is_none());
    let kept = f.work.retain(result);
    assert!(rejected && kept.is_err());
    let texture = f.take(frame).texture;
    assert!(unsafe { f.work.compute.queue.present(&**f.surface(0), texture) }.is_err());
    assert_eq!(original.pending_present_count(), 0);
    f.configure(0, true).unwrap();
    drop(original);
    f.draw(0, wgt::Color::RED, 0xff0000);
    f.draw(0, wgt::Color::GREEN, 0x00ff00);
    f.configure(0, false).unwrap();
    f.draw(1, wgt::Color::GREEN, 0x00ff00);
    let encoder = f.work.encoder(Lane::Compute);
    f.work.begin(encoder);
    let buffer = f.work.end(encoder);
    let fence = f.work.fence();
    let result = f
        .work
        .ordered(&[(Lane::Compute, encoder, buffer)], fence, 1);
    f.work.retain(result).unwrap();
    assert!(f.work.receipts.last().unwrap().wait(None).unwrap());
    drop(f);
    assert_validation_clean();
}

#[test]
#[ignore = "Requires 24-bit Xvfb, shared Vulkan maintenance1 and Khronos validation"]
fn shared_surface_views_retain_generations_and_stale_tokens_are_rejected() {
    let mut f = Fixture::new(1);
    f.configure(0, true).unwrap();
    let owner = f.surface_owner(0);
    let old = f.acquire(0);
    f.view(0, old);
    let generation = Arc::downgrade(&f.generation(0));
    f.configure(0, true).unwrap();
    assert!(generation.upgrade().unwrap().retirement_confirmed());
    let current = f.acquire(0);
    let result = unsafe {
        f.work.context.submit_graphics(
            &*f.work.graphics.queue,
            &[],
            &[&*f.frames[old].as_ref().unwrap().texture],
            (&*f.work.fences[f.fences[0]], 1),
            &crate::external_sync::SubmissionSync::default(),
        )
    };
    let rejected = matches!(&result, Err(error) if error.receipt.is_none());
    let kept = f.work.retain(result);
    assert!(rejected && kept.is_err());
    assert!(f.present(0, old).is_err());
    assert_eq!(f.generation(0).pending_present_count(), 0);
    f.generation(0).check_open().unwrap();
    f.render(0, current, wgt::Color::GREEN);
    f.present(0, current).unwrap();
    f.windows[0].assert_pixels(0x00ff00);
    f.configure(0, false).unwrap();
    drop(f.surfaces[0].take());
    assert!(generation.upgrade().is_some());
    assert!(owner.upgrade().is_some());
    drop(f);
    assert!(generation.upgrade().is_none());
    assert!(owner.upgrade().is_none());
    assert_validation_clean();
}

#[test]
#[ignore = "Requires 24-bit Xvfb, shared Vulkan maintenance1 and Khronos validation"]
fn shared_surface_failed_retirement_keeps_owners_and_unknown_present_fence() {
    for execution_error in [true, false] {
        let mut f = Fixture::new(1);
        f.configure(0, true).unwrap();
        let owner = f.surface_owner(0);
        let frame = f.acquire(0);
        f.render(0, frame, wgt::Color::RED);
        f.present(0, frame).unwrap();
        let generation = f.generation(0);
        if execution_error {
            generation
                .group()
                .generation_execution_error
                .store(true, Ordering::SeqCst);
        } else {
            generation
                .group()
                .presentation_wait_error
                .store(true, Ordering::SeqCst);
        }
        assert!(f.configure(0, false).is_err());
        assert!(!generation.retirement_confirmed());
        assert!(owner.upgrade().is_some());
        f.work.retire().unwrap();
        assert!(generation.retirement_confirmed());
        drop(generation);
        drop(f);
        assert!(owner.upgrade().is_none());
    }
    let mut f = Fixture::new(1);
    f.configure(0, true).unwrap();
    let generation = f.generation(0);
    let frame = f.acquire(0);
    f.render(0, frame, wgt::Color::GREEN);
    generation
        .group()
        .present_after_native_error
        .store(true, Ordering::SeqCst);
    assert!(f.present(0, frame).is_err());
    assert!(generation.pending_present_count() > 0);
    assert!(!generation.retirement_confirmed());
    generation.wait_present_fences_for_test().unwrap();
    f.work.retire().unwrap();
    assert!(generation.retirement_confirmed());
    drop(generation);
    drop(f);
    assert_validation_clean();
}

fn reached(counter: &core::sync::atomic::AtomicUsize) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while counter.load(Ordering::SeqCst) == 0 {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::yield_now();
    }
    true
}

#[test]
#[ignore = "Requires 24-bit Xvfb, shared Vulkan maintenance1 and Khronos validation"]
fn shared_surface_admission_rechecks_generation_after_host_lock() {
    for present in [false, true] {
        let mut f = Fixture::new(1);
        f.configure(0, true).unwrap();
        let generation = f.generation(0);
        let group = generation.group().clone();
        let frame = f.acquire(0);
        if present {
            f.render(0, frame, wgt::Color::RED);
        }
        let graphics = f.work.graphics.clone();
        if present {
            let surface = f.surface(0).clone();
            let texture = f.take(frame).texture;
            let (observed, result) = std::thread::scope(|scope| {
                let host = group.host[0].lock();
                let pending =
                    scope.spawn(move || unsafe { graphics.queue.present(&*surface, texture) });
                let observed = reached(&group.surface_present_waiters);
                generation.close();
                drop(host);
                (observed, pending.join().unwrap())
            });
            assert!(observed && result.is_err());
            assert_eq!(generation.pending_present_count(), 0);
        } else {
            let token = f.frames[frame].as_ref().unwrap().clone();
            let context = f.work.context.clone();
            let fence = &*f.work.fences[f.fences[0]];
            let (observed, result) = std::thread::scope(|scope| {
                let host = group.host[0].lock();
                let pending = scope.spawn(move || unsafe {
                    context.submit_graphics(
                        &*graphics.queue,
                        &[],
                        &[&*token.texture],
                        (fence, 1),
                        &crate::external_sync::SubmissionSync::default(),
                    )
                });
                let observed = reached(&group.surface_submit_waiters);
                generation.close();
                drop(host);
                (observed, pending.join().unwrap())
            });
            let rejected = matches!(&result, Err(error) if error.receipt.is_none());
            let kept = f.work.retain(result);
            assert!(observed && rejected && kept.is_err());
        }
        drop(generation);
        drop(group);
        drop(f);
    }
    assert_validation_clean();
}

#[test]
fn shared_surface_missing_generation_requires_retirement_certificate() {
    let group = QueueGroup::new(true);
    group
        .presentations
        .lock()
        .push((Weak::new(), Arc::new(AtomicBool::new(true))));
    assert!(group.snapshot_generations().unwrap().is_empty());
    assert!(group.presentations.lock().is_empty());
    group
        .presentations
        .lock()
        .push((Weak::new(), Arc::new(AtomicBool::new(false))));
    assert!(group.snapshot_generations().is_err());
    assert!(group.closed.load(Ordering::Acquire));
    assert!(group.uncertain.load(Ordering::Acquire));
    group.presentations.lock().clear();
    assert!(group.snapshot_generations().is_err());
}

fn orphaned_generation_child() -> ! {
    let mut fixture = ManuallyDrop::new(Fixture::new(1));
    fixture.configure(0, true).unwrap();
    let frame_index = fixture.acquire(0);
    fixture.view(0, frame_index);
    let generation = fixture.generation(0);
    let weak = Arc::downgrade(&generation);
    let native_surface = fixture.surface_owner(0);
    let group = generation.group().clone();
    let surface = fixture.surfaces[0].take().unwrap();
    let frame = fixture.frames[frame_index].take().unwrap();
    let view = fixture.work.graphics_views.pop().unwrap();
    let graphics = fixture.work.graphics.clone();

    drop(generation);
    drop(surface);
    drop(frame);
    unsafe { graphics.device.destroy_texture_view(view) };

    let generation_gone = weak.upgrade().is_none();
    let surface_retained = native_surface.upgrade().is_some();
    let orphaned = group.orphaned_presentation.load(Ordering::Acquire);
    let drain_rejected = fixture.work.context.drain().is_err();
    #[cfg(feature = "validation_canary")]
    let validation_clean = crate::VALIDATION_CANARY.get_and_reset().is_empty();
    #[cfg(not(feature = "validation_canary"))]
    let validation_clean = false;
    let code =
        if generation_gone && surface_retained && orphaned && drain_rejected && validation_clean {
            86
        } else {
            87
        };
    unsafe extern "C" {
        fn _exit(code: i32) -> !;
    }
    unsafe { _exit(code) }
}

#[test]
#[ignore = "Requires 24-bit Xvfb, shared Vulkan maintenance1 and Khronos validation"]
fn shared_surface_orphaned_generation_retains_native_payload() {
    const CHILD: &str = "WGPU_HAL_ORPHANED_PRESENTATION_CHILD";
    if std::env::var_os(CHILD).is_some() {
        orphaned_generation_child();
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "vulkan::shared::tests::ordered::presentation_tests::shared_surface_orphaned_generation_retains_native_payload",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let timed_out = loop {
        if child.try_wait().unwrap().is_some() {
            break false;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            break true;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let output = child.wait_with_output().unwrap();
    assert!(!timed_out, "Orphaned presentation child did not terminate");
    assert_eq!(
        output.status.code(),
        Some(86),
        "{}",
        alloc::string::String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
#[ignore = "Requires 24-bit Xvfb, shared Vulkan maintenance1 and Khronos validation"]
fn surface_destruction_certificate_waits_for_hidden_generation_snapshot() {
    let mut f = Fixture::new(1);
    let lifetime = crate::renderer::surface_lifetime(&**f.surface(0)).unwrap();
    f.configure(0, true).unwrap();
    let snapshots = f.work.graphics.device.as_any()
        .downcast_ref::<crate::vulkan::Device>().unwrap()
        .shared.shared_queue_group.as_ref().unwrap().snapshot_generations().unwrap();
    assert_eq!(snapshots.len(), 1);
    f.configure(0, false).unwrap();
    drop(f.surfaces[0].take());
    assert!(!lifetime.is_destroyed());
    let (release, blocked) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        blocked.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(snapshots);
    });
    assert!(!lifetime.is_destroyed());
    release.send(()).unwrap();
    worker.join().unwrap();
    assert!(lifetime.is_destroyed());
    drop(f);
    assert_validation_clean();
}

#[test]
#[ignore = "Requires 24-bit Xvfb, shared Vulkan maintenance1 and Khronos validation"]
fn surface_destruction_certificate_covers_unconfigured_surface() {
    let mut f = Fixture::new(1);
    let lifetime = crate::renderer::surface_lifetime(&**f.surface(0)).unwrap();
    assert!(!lifetime.is_destroyed());
    drop(f.surfaces[0].take());
    assert!(lifetime.is_destroyed());
    drop(f);
    assert_validation_clean();
}
