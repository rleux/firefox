/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::shared_canvas_tests::NativeGate;
use super::shared_canvas_sampling_tests::{external, verify_pixels, BACKGROUND, PATTERN};
#[cfg(feature = "capture")]
use super::shared_canvas_sampling_tests::linear;
use crate::device::{
    BlendMode, BufferKind, DrawTarget, LoadOp, Program, RenderPassDescriptor, RenderState, StoreOp,
    TextureSlot, VertexArray,
};
use crate::device::wgpu::render_device::RenderDevice;
use crate::renderer::desc;
use api::{ExternalTextureHandle, ImageFormat};
use euclid::default::Transform3D;

fn counts(graphics: &hal::DynOpenDevice) -> (isize, isize) {
    let counters = graphics.device.get_internal_counters();
    (counters.textures.read(), counters.texture_memory.read())
}

fn full() -> DeviceIntRect {
    DeviceIntRect::from_size(DeviceIntSize::new(2, 2))
}

fn output_rect() -> DeviceIntRect {
    DeviceIntRect::from_size(DeviceIntSize::new(4, 2))
}

fn native_bytes(format: wgt::TextureFormat) -> Vec<u8> {
    PATTERN
        .iter()
        .flat_map(|pixel| {
            let mut pixel = *pixel;
            if format == wgt::TextureFormat::Bgra8Unorm {
                pixel.swap(0, 2);
            }
            pixel
        })
        .collect()
}

struct Source {
    texture: Arc<core::resource::Texture>,
    lease: Option<Arc<dyn hal::shared_image::ImageLease>>,
    descriptor: hal::TextureDescriptor<'static>,
}

#[derive(Default)]
pub(super) struct CanvasSnapshots {
    renderer: Option<RenderDevice>,
    program: Option<Program>,
    vao: Option<VertexArray>,
    vertices: Option<crate::device::Buffer>,
    instances: Option<crate::device::Buffer>,
    sources: Vec<Source>,
    gate: Option<NativeGate>,
    trigger: Option<std::thread::JoinHandle<()>>,
}

impl CanvasSnapshots {
    pub(super) fn release_gate(&mut self) -> Result<bool, String> {
        let released = self.gate.as_mut().map_or(Ok(true), NativeGate::release);
        if let Some(trigger) = self.trigger.take() {
            trigger
                .join()
                .map_err(|_| "Snapshot release trigger panicked".to_owned())?;
        }
        released
    }

    fn setup(&mut self, owner: &Rc<Device>) {
        self.renderer = Some(RenderDevice::new(owner).unwrap());
        let renderer = self.renderer.as_mut().unwrap();
        let mut program = renderer
            .programs
            .create("cs_scale", &["TEXTURE_2D"], false)
            .unwrap();
        renderer.programs.link(&mut program, &desc::SCALE).unwrap();
        renderer
            .programs
            .state_mut(&program)
            .unwrap()
            .bind_samplers(&[("sColor0", TextureSlot(0))]);
        renderer
            .programs
            .state(&program)
            .unwrap()
            .set_transform(&Transform3D::ortho(0.0, 4.0, 0.0, 2.0, -1.0, 1.0));
        let vertices = renderer
            .vertex_arrays
            .create_buffer(BufferKind::Vertex)
            .unwrap();
        let mut instances = renderer
            .vertex_arrays
            .create_buffer(BufferKind::Vertex)
            .unwrap();
        let vao = renderer
            .vertex_arrays
            .create(&desc::SCALE, &vertices, Some(&instances), None, 1)
            .unwrap();
        let values = [
            0.0, 0.0, 2.0, 2.0, 0.0, 0.0, 2.0, 2.0, 1.0f32, 2.0, 0.0, 4.0, 2.0, 0.0, 0.0, 2.0, 2.0,
            1.0,
        ];
        let bytes: Vec<_> = values
            .iter()
            .flat_map(|value| value.to_ne_bytes())
            .collect();
        renderer
            .vertex_arrays
            .write_buffer(&mut instances, &bytes)
            .unwrap();
        self.program = Some(program);
        self.vao = Some(vao);
        self.vertices = Some(vertices);
        self.instances = Some(instances);
    }

    fn create_source(
        &mut self,
        device: &Arc<core::device::Device>,
        queue: &Arc<core::device::queue::Queue>,
        graphics: &Arc<hal::DynOpenDevice>,
        format: wgt::TextureFormat,
        copy_src: bool,
    ) -> usize {
        let mut usage = wgt::TextureUsages::COPY_DST | wgt::TextureUsages::RENDER_ATTACHMENT;
        let mut hal_usage = wgt::TextureUses::COPY_DST | wgt::TextureUses::COLOR_TARGET;
        if copy_src {
            usage |= wgt::TextureUsages::COPY_SRC;
            hal_usage |= wgt::TextureUses::COPY_SRC;
        }
        let desc = core::resource::TextureDescriptor {
            label: Some(Cow::Borrowed("native explicit snapshot source")),
            size: wgt::Extent3d {
                width: 2,
                height: 2,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgt::TextureDimension::D2,
            format,
            usage,
            view_formats: vec![format.add_srgb_suffix()],
        };
        let (texture, lease) =
            unsafe { device.create_texture_with_shared_image(&desc, graphics.clone()) };
        self.sources.push(Source {
            texture,
            lease: Some(lease.expect("Native snapshot allocation must succeed")),
            descriptor: hal::TextureDescriptor {
                label: None,
                size: desc.size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgt::TextureDimension::D2,
                format,
                usage: hal_usage,
                memory_flags: hal::MemoryFlags::empty(),
                view_formats: desc.view_formats,
            },
        });
        let index = self.sources.len() - 1;
        queue.write_texture(
            wgt::TexelCopyTextureInfo {
                texture: self.sources[index].texture.clone(),
                mip_level: 0,
                origin: wgt::Origin3d::ZERO,
                aspect: wgt::TextureAspect::All,
            },
            &native_bytes(format),
            &wgt::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(8),
                rows_per_image: None,
            },
            &desc.size,
        );
        index
    }

    fn register(&self, index: usize, srgb: bool, opaque: bool) -> ExternalTextureHandle {
        let source = &self.sources[index];
        let format = source.descriptor.format;
        unsafe {
            self.renderer
                .as_ref()
                .unwrap()
                .textures
                .external_textures()
                .register_shared_image(
                    source.lease.as_ref().unwrap().clone(),
                    &source.descriptor,
                    if srgb {
                        format.add_srgb_suffix()
                    } else {
                        format
                    },
                    TextureFilter::Nearest,
                    opaque,
                )
        }
        .unwrap()
    }

    fn begin_draw(&mut self, handle: ExternalTextureHandle) {
        let renderer = self.renderer.as_mut().unwrap();
        renderer.begin_frame().unwrap();
        renderer
            .vertex_arrays
            .bind(self.vao.as_ref().unwrap())
            .unwrap();
        renderer
            .begin_render_pass(&RenderPassDescriptor {
                target: DrawTarget::new_default(DeviceIntSize::new(4, 2), true),
                render_area: None,
                color_load: LoadOp::Clear([
                    f32::from(BACKGROUND[0]) / 255.0,
                    f32::from(BACKGROUND[1]) / 255.0,
                    f32::from(BACKGROUND[2]) / 255.0,
                    1.0,
                ]),
                depth_load: LoadOp::Load,
            })
            .unwrap();
        renderer
            .textures
            .bind_external(TextureSlot(0), &external(handle))
            .unwrap();
        self.draw();
    }

    fn draw(&mut self) {
        let renderer = self.renderer.as_mut().unwrap();
        let program = self.program.as_ref().unwrap();
        renderer
            .bind_pipeline(program, RenderState::default())
            .unwrap();
        renderer.draw_instanced(0, 1).unwrap();
        renderer
            .bind_pipeline(
                program,
                RenderState {
                    blend_mode: BlendMode::Alpha,
                    ..Default::default()
                },
            )
            .unwrap();
        renderer.draw_instanced(1, 1).unwrap();
    }

    fn verify_draw(&self, srgb: bool, opaque: bool) {
        let renderer = self.renderer.as_ref().unwrap();
        let output = renderer.textures.output().unwrap();
        let bytes = output.readback(output_rect()).unwrap().wait().unwrap();
        verify_pixels(&bytes, srgb, opaque);
    }

    fn finish_draw(&mut self) {
        let renderer = self.renderer.as_mut().unwrap();
        renderer.end_render_pass(StoreOp::Store).unwrap();
        renderer.end_frame().unwrap();
        renderer.submissions.wait().unwrap();
    }

    fn arm_gate(&mut self, graphics: &hal::DynOpenDevice, provider: &GraphicsProvider) {
        self.gate = Some(NativeGate::new(graphics));
        *provider.native_wait.lock().unwrap() = Some(hal::external_sync::SemaphoreValue {
            semaphore: self.gate.as_ref().unwrap().imported.clone(),
            value: Some(1),
        });
    }

    fn trigger_release(&mut self) -> Arc<AtomicBool> {
        let release = self.gate.as_ref().unwrap().release_signal();
        let requested = Arc::new(AtomicBool::new(false));
        let signal = requested.clone();
        self.trigger = Some(std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            signal.store(true, Ordering::SeqCst);
            let _ = release.send(());
        }));
        requested
    }
}

impl CanvasSnapshots {
    fn no_copy_src(
        &mut self,
        device: &Arc<core::device::Device>,
        queue: &Arc<core::device::queue::Queue>,
        graphics: &Arc<hal::DynOpenDevice>,
        provider: &GraphicsProvider,
    ) {
        let before = counts(graphics);
        let index = self.create_source(
            device,
            queue,
            graphics,
            wgt::TextureFormat::Bgra8Unorm,
            false,
        );
        let source = &self.sources[index];
        let finalized = unsafe {
            queue.finalize_shared_canvas(&source.texture, &**source.lease.as_ref().unwrap())
        }
        .unwrap();
        let lease = Arc::downgrade(source.lease.as_ref().unwrap());
        let handle = self.register(index, true, true);
        let renderer = self.renderer.as_mut().unwrap();
        let registry = renderer.textures.external_textures();
        let source = registry.get(handle.0 as u32).unwrap();
        let weak = Rc::downgrade(&source);
        assert!(!source.supports_copy_src());
        drop(source);
        drop(self.sources[index].lease.take());
        assert!(renderer.snapshot_shared_image(handle, full()).is_err());
        assert!(renderer.failure().is_none());
        self.begin_draw(handle);
        self.finish_draw();
        self.verify_draw(true, true);
        #[cfg(feature = "capture")]
        self.verify_capture_view(handle, true, true);
        let renderer = self.renderer.as_mut().unwrap();
        registry.unregister(handle).unwrap();
        renderer.textures.reset_bindings();
        renderer.submissions.wait().unwrap();
        provider.release_receipts();
        device
            .poll(wgt::PollType::Wait {
                submission_index: Some(finalized),
                timeout: Some(std::time::Duration::from_secs(30)),
            })
            .unwrap();
        assert!(weak.upgrade().is_none());
        assert!(lease.upgrade().is_none());
        assert_eq!(counts(graphics), before);
    }

    pub(super) fn run(
        &mut self,
        device: &Arc<core::device::Device>,
        queue: &Arc<core::device::queue::Queue>,
        graphics: &Arc<hal::DynOpenDevice>,
        provider: &GraphicsProvider,
        owner: &Rc<Device>,
    ) {
        let before = counts(graphics);
        let counter = wgt::InternalCounter::new();
        counter.add(1);
        assert_eq!(
            counter.read(),
            1,
            "Snapshot tests require wgpu-types/counters"
        );
        self.setup(owner);
        eprintln!(
            "Snapshot crop row bytes4, native pitch alignment {}",
            owner.capabilities().alignments.buffer_copy_pitch.get()
        );
        device.push_error_scope(wgt::error::ErrorFilter::Validation);
        for format in [
            wgt::TextureFormat::Rgba8Unorm,
            wgt::TextureFormat::Bgra8Unorm,
        ] {
            for srgb in [false, true] {
                for opaque in [false, true] {
                    let logical_before = device.get_hal_counters().textures.read();
                    let index = self.create_source(device, queue, graphics, format, true);
                    let gated = format == wgt::TextureFormat::Bgra8Unorm && srgb && !opaque;
                    if gated {
                        self.arm_gate(graphics, provider);
                    }
                    let source = &self.sources[index];
                    let finalized = unsafe {
                        queue.finalize_shared_canvas(
                            &source.texture,
                            &**source.lease.as_ref().unwrap(),
                        )
                    }
                    .unwrap();
                    let lease = Arc::downgrade(source.lease.as_ref().unwrap());
                    let handle = self.register(index, srgb, opaque);
                    let registry = self.renderer.as_ref().unwrap().textures.external_textures();
                    let sampled = registry.get(handle.0 as u32).unwrap();
                    let weak = Rc::downgrade(&sampled);
                    let copy_src = sampled.supports_copy_src();
                    drop(sampled);
                    drop(self.sources[index].lease.take());
                    self.begin_draw(handle);
                    let mut producer_ready = None;
                    let requested = if gated {
                        producer_ready = provider.receipts.lock().unwrap().last().map(|receipt| {
                            receipt
                                .native
                                .wait(Some(std::time::Duration::from_millis(50)))
                        });
                        Some(self.trigger_release())
                    } else {
                        None
                    };
                    let snapshot = self
                        .renderer
                        .as_mut()
                        .unwrap()
                        .snapshot_shared_image(handle, full());
                    let signal_before_return = requested
                        .as_ref()
                        .map(|requested| requested.load(Ordering::SeqCst));
                    if gated {
                        let explicit = self.release_gate().unwrap();
                        assert!(explicit, "Synchronous snapshot used the failure fallback instead of explicit release");
                        assert_eq!(
                            signal_before_return,
                            Some(true),
                            "Snapshot returned before its explicit producer release"
                        );
                        assert!(
                            matches!(producer_ready, Some(Ok(false))),
                            "Snapshot producer was not pending: {producer_ready:?}"
                        );
                    }
                    assert!(copy_src);
                    let (actual_format, bytes) = snapshot.unwrap();
                    let expected_format = if format == wgt::TextureFormat::Rgba8Unorm {
                        ImageFormat::RGBA8
                    } else {
                        ImageFormat::BGRA8
                    };
                    assert_eq!(actual_format, expected_format);
                    assert_eq!(
                        bytes,
                        native_bytes(format),
                        "Raw snapshot changed sampled-view sRGB{srgb}/alphaONE{opaque}"
                    );
                    assert!(self.renderer.as_ref().unwrap().has_cached_readback());
                    self.verify_draw(srgb, opaque);
                    #[cfg(feature = "capture")]
                    self.verify_capture_view(handle, srgb, opaque);
                    let renderer = self.renderer.as_mut().unwrap();
                    renderer
                        .clear_target(
                            Some([
                                f32::from(BACKGROUND[0]) / 255.0,
                                f32::from(BACKGROUND[1]) / 255.0,
                                f32::from(BACKGROUND[2]) / 255.0,
                                1.0,
                            ]),
                            None,
                            None,
                        )
                        .unwrap();
                    self.draw();
                    let crop = DeviceIntRect::from_origin_and_size(
                        DeviceIntPoint::new(1, 0),
                        DeviceIntSize::new(1, 2),
                    );
                    let (actual_format, cropped) = self
                        .renderer
                        .as_mut()
                        .unwrap()
                        .snapshot_shared_image(handle, crop)
                        .unwrap();
                    let native = native_bytes(format);
                    assert_eq!(actual_format, expected_format);
                    assert_eq!(cropped, [&native[4..8], &native[12..16]].concat());
                    self.verify_draw(srgb, opaque);
                    registry.unregister(handle).unwrap();
                    self.renderer.as_mut().unwrap().textures.reset_bindings();
                    self.finish_draw();
                    provider.release_receipts();
                    device
                        .poll(wgt::PollType::Wait {
                            submission_index: Some(finalized),
                            timeout: Some(std::time::Duration::from_secs(30)),
                        })
                        .unwrap();
                    assert_eq!(device.get_hal_counters().textures.read(), logical_before);
                    assert!(self.renderer.as_ref().unwrap().has_cached_readback());
                    assert!(
                        weak.upgrade().is_none(),
                        "Completed capture cache retained the source Texture"
                    );
                    assert!(
                        lease.upgrade().is_none(),
                        "Completed capture cache retained the image lease"
                    );
                    assert!(self
                        .renderer
                        .as_mut()
                        .unwrap()
                        .snapshot_shared_image(handle, full())
                        .is_err());
                    assert!(self.renderer.as_ref().unwrap().failure().is_none());
                }
            }
        }
        self.no_copy_src(device, queue, graphics, provider);
        assert!(device.pop_error_scope().unwrap().is_none());
        let renderer = self.renderer.as_mut().unwrap();
        renderer
            .vertex_arrays
            .delete(self.vao.as_mut().unwrap())
            .unwrap();
        renderer
            .vertex_arrays
            .delete_buffer(self.vertices.as_mut().unwrap())
            .unwrap();
        renderer
            .vertex_arrays
            .delete_buffer(self.instances.as_mut().unwrap())
            .unwrap();
        renderer
            .programs
            .delete(self.program.as_mut().unwrap())
            .unwrap();
        drop(self.renderer.take());
        assert_eq!(counts(graphics), before);
    }
}

#[cfg(feature = "capture")]
impl CanvasSnapshots {
    fn verify_capture_view(&mut self, handle: ExternalTextureHandle, srgb: bool, opaque: bool) {
        use crate::device::GpuBackend;
        use api::{ImageBufferKind, ImageDescriptor, ImageDescriptorFlags};
        let descriptor =
            ImageDescriptor::new(2, 2, ImageFormat::RGBA8, ImageDescriptorFlags::empty());
        let bytes = GpuBackend::read_external_texture(
            self.renderer.as_mut().unwrap(),
            handle,
            ImageBufferKind::Texture2D,
            &descriptor,
        );
        assert_eq!(bytes.len(), 16);
        for (index, pixel) in PATTERN.iter().enumerate() {
            for channel in 0..4 {
                let expected = if channel == 3 {
                    if opaque {
                        255
                    } else {
                        pixel[3]
                    }
                } else {
                    (linear(pixel[channel], srgb) * 255.0).round() as u8
                };
                let tolerance = u8::from(srgb && channel < 3);
                assert!(bytes[index * 4 + channel].abs_diff(expected) <= tolerance,
                    "Capture bypassed sampled view sRGB{srgb}/alphaONE{opaque}, pixel{index}, channel{channel}: {} != {expected}",
                    bytes[index * 4 + channel]);
            }
        }
    }
}
