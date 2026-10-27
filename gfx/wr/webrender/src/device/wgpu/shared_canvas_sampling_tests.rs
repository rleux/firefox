/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::shared_canvas_tests::NativeGate;
use crate::device::{BlendMode, ExternalTexture, RenderState, TextureSlot};
use crate::device::wgpu::{Buffer, BufferPool, Samplers, SubmissionQueue};
use crate::device::wgpu::draw::{Draw, DrawPass};
use crate::device::wgpu::pipeline::DrawPipeline;
use crate::device::wgpu::program::{ProgramState, ShaderResource};
use crate::device::wgpu::shader::select_draw_shader;
use crate::device::wgpu::texture_store::TextureStore;
use api::{ExternalTextureHandle, ImageBufferKind, ImageRendering};
use api::units::TexelRect;
use euclid::default::Transform3D;

pub(super) const PATTERN: [[u8; 4]; 4] = [
    [17, 51, 102, 255],
    [204, 153, 68, 128],
    [7, 31, 199, 64],
    [137, 11, 83, 192],
];
pub(super) const BACKGROUND: [u8; 4] = [23, 47, 89, 255];

fn counts(graphics: &hal::DynOpenDevice) -> (isize, isize) {
    let counters = graphics.device.get_internal_counters();
    (counters.textures.read(), counters.texture_memory.read())
}

pub(super) fn external(handle: ExternalTextureHandle) -> ExternalTexture {
    ExternalTexture::new(
        handle,
        ImageBufferKind::Texture2D,
        TexelRect::new(0.0, 0.0, 2.0, 2.0),
        ImageRendering::Pixelated,
    )
}

struct Source {
    texture: Arc<core::resource::Texture>,
    lease: Option<Arc<dyn hal::shared_image::ImageLease>>,
    epoch: Option<Arc<hal::shared_image::ImageEpoch>>,
    descriptor: hal::TextureDescriptor<'static>,
}

struct Consumer {
    target: Rc<Texture>,
    submission: Submission,
    source: std::rc::Weak<Texture>,
}

struct Resources {
    store: TextureStore,
    uploads: SubmissionQueue,
    quad: Rc<Buffer>,
    samplers: Rc<Samplers>,
    plain: Rc<DrawPipeline>,
    blend: Rc<DrawPipeline>,
}

impl Resources {
    fn new(owner: &Rc<Device>) -> Self {
        let shader = select_draw_shader("cs_scale", &["TEXTURE_2D"], false).unwrap();
        let state = RenderState::default();
        let plain =
            DrawPipeline::new(owner, shader, wgt::TextureFormat::Rgba8Unorm, false, state).unwrap();
        let blend = DrawPipeline::new(
            owner,
            shader,
            wgt::TextureFormat::Rgba8Unorm,
            false,
            RenderState {
                blend_mode: BlendMode::Alpha,
                ..state
            },
        )
        .unwrap();
        Self {
            store: TextureStore::new(owner),
            uploads: SubmissionQueue::new(&Rc::new(BufferPool::new(owner)), 2).unwrap(),
            quad: Buffer::new(
                owner,
                &[0, 0, 0, 0, 255, 0, 0, 0, 0, 255, 0, 0, 255, 255, 0, 0],
                wgt::BufferUses::VERTEX,
            )
            .unwrap(),
            samplers: Rc::new(Samplers::new(owner).unwrap()),
            plain,
            blend,
        }
    }
}

struct ForeignRegistry {
    registry: Rc<ExternalTextureRegistry>,
    nonshared: Rc<ExternalTextureRegistry>,
    context: Arc<dyn hal::shared::SharedDeviceContext>,
}

impl ForeignRegistry {
    fn new(owner: &Rc<Device>) -> Self {
        let pair = unsafe {
            hal::shared::open_shared_device(
                &*owner.adapter,
                owner.features,
                &owner.capabilities().limits,
                &wgt::MemoryHints::MemoryUsage,
            )
        }
        .unwrap();
        let context = pair.context;
        let (_, instance) = context.clone_instance();
        let adapter = unsafe { instance.enumerate_adapters(None) }.pop().unwrap();
        drop(pair.compute);
        let foreign = Rc::new(
            unsafe {
                Device::from_shared_endpoint(
                    Arc::new(pair.graphics),
                    instance,
                    adapter,
                    SharedDeviceOptions {
                        context: context.clone(),
                        recover: Rc::new(|_| {
                            panic!("Foreign registry must not submit native work")
                        }),
                        fatal_process: terminate_test_process,
                    },
                )
            }
            .unwrap(),
        );
        let nonshared = Rc::new(
            Device::new(&Options {
                validation: true,
                ..Default::default()
            })
            .unwrap(),
        );
        Self {
            registry: ExternalTextureRegistry::new(&foreign),
            nonshared: ExternalTextureRegistry::new(&nonshared),
            context,
        }
    }
}

#[derive(Default)]
pub(super) struct CanvasSampling {
    sources: Vec<Source>,
    consumers: Vec<Consumer>,
    resources: Option<Resources>,
    foreign: Option<ForeignRegistry>,
    gate: Option<NativeGate>,
}

impl CanvasSampling {
    pub(super) fn release_gate(&mut self) -> Result<bool, String> {
        self.gate.as_mut().map_or(Ok(true), NativeGate::release)
    }

    fn create_source(
        &mut self,
        device: &Arc<core::device::Device>,
        queue: &core::device::queue::Queue,
        graphics: &Arc<hal::DynOpenDevice>,
        format: wgt::TextureFormat,
    ) -> usize {
        let size = wgt::Extent3d {
            width: 2,
            height: 2,
            depth_or_array_layers: 1,
        };
        let view_formats = vec![format.add_srgb_suffix()];
        let desc = core::resource::TextureDescriptor {
            label: Some(Cow::Borrowed("native sampled canvas pattern")),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgt::TextureDimension::D2,
            format,
            usage: wgt::TextureUsages::COPY_SRC
                | wgt::TextureUsages::COPY_DST
                | wgt::TextureUsages::RENDER_ATTACHMENT,
            view_formats: view_formats.clone(),
        };
        let (texture, lease) =
            unsafe { device.create_texture_with_shared_image(&desc, graphics.clone()) };
        self.sources.push(Source {
            texture,
            lease: Some(lease.expect("Native sampled allocation must succeed")),
            epoch: None,
            descriptor: hal::TextureDescriptor {
                label: None,
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgt::TextureDimension::D2,
                format,
                usage: wgt::TextureUses::COPY_SRC
                    | wgt::TextureUses::COPY_DST
                    | wgt::TextureUses::COLOR_TARGET,
                memory_flags: hal::MemoryFlags::empty(),
                view_formats,
            },
        });
        let index = self.sources.len() - 1;
        let bytes: Vec<_> = PATTERN
            .iter()
            .flat_map(|pixel| {
                let mut pixel = *pixel;
                if format == wgt::TextureFormat::Bgra8Unorm {
                    pixel.swap(0, 2);
                }
                pixel
            })
            .collect();
        queue.write_texture(
            wgt::TexelCopyTextureInfo {
                texture: self.sources[index].texture.clone(),
                mip_level: 0,
                origin: wgt::Origin3d::ZERO,
                aspect: wgt::TextureAspect::All,
            },
            &bytes,
            &wgt::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(8),
                rows_per_image: None,
            },
            &size,
        );
        index
    }

    fn register(&self, index: usize, srgb: bool, opaque: bool) -> ExternalTextureHandle {
        let source = &self.sources[index];
        let format = source.descriptor.format;
        unsafe {
            self.resources
                .as_ref()
                .unwrap()
                .store
                .external_textures()
                .register_shared_image(
                    source.epoch.as_ref().unwrap().clone(),
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

    fn rejected_registrations(&self, index: usize) {
        let source = &self.sources[index];
        let epoch = source.epoch.as_ref().unwrap();
        let registry = self.resources.as_ref().unwrap().store.external_textures();
        let format = source.descriptor.format;
        assert_eq!(epoch.descriptor().size, source.descriptor.size);
        assert_eq!(epoch.descriptor().format, format);
        assert_eq!(epoch.descriptor().usage, source.descriptor.usage);
        assert_eq!(
            epoch.descriptor().view_formats,
            source.descriptor.view_formats
        );
        let before = counts(&registry.device().open);
        for (view, filter) in [
            (wgt::TextureFormat::Bgra8UnormSrgb, TextureFilter::Nearest),
            (wgt::TextureFormat::Rgba32Float, TextureFilter::Nearest),
            (format, TextureFilter::Trilinear),
        ] {
            assert!(
                unsafe { registry.register_shared_image(epoch.clone(), view, filter, false) }
                    .is_err()
            );
            assert_eq!(counts(&registry.device().open), before);
        }
        let foreign = self.foreign.as_ref().unwrap();
        let foreign_before = counts(&foreign.registry.device().open);
        assert!(unsafe {
            foreign.registry.register_shared_image(
                epoch.clone(),
                format,
                TextureFilter::Nearest,
                false,
            )
        }
        .is_err());
        assert_eq!(counts(&foreign.registry.device().open), foreign_before);
        let owner = foreign.nonshared.device();
        let ordinary = Texture::new(owner, 2, 2, format, TextureFilter::Nearest, false).unwrap();
        let first = foreign.nonshared.register(&ordinary).unwrap();
        let nonshared_before = counts(&owner.open);
        assert!(unsafe {
            foreign.nonshared.register_shared_image(
                epoch.clone(),
                format,
                TextureFilter::Nearest,
                false,
            )
        }
        .is_err());
        assert_eq!(counts(&owner.open), nonshared_before);
        let next = foreign.nonshared.register(&ordinary).unwrap();
        assert_eq!(first.0, 1);
        assert_eq!(next.0, first.0 + 1);
        foreign.nonshared.unregister(first).unwrap();
        foreign.nonshared.unregister(next).unwrap();
        assert_eq!(counts(&registry.device().open), before);
    }

    fn record(
        &mut self,
        index: usize,
        handle: ExternalTextureHandle,
        check_readonly: bool,
    ) -> (bool, bool) {
        let resources = self.resources.as_mut().unwrap();
        let consumer = &mut self.consumers[index];
        let full = DeviceIntRect::from_size(DeviceIntSize::new(4, 2));
        let source_rect = DeviceIntRect::from_size(DeviceIntSize::new(2, 2));
        resources
            .store
            .bind_external(TextureSlot(0), &external(handle))
            .unwrap();
        let saved = resources.store.bindings();
        let source = match &saved[0] {
            Some(ShaderResource::Texture { texture, .. }) => texture.clone(),
            _ => panic!("Registered canvas must resolve as a sampled texture"),
        };
        consumer.source = Rc::downgrade(&source);
        let source_state = || {
            source.current_usage() == wgt::TextureUses::RESOURCE
                && source.initialized()
                && source.sample_initialized()
                && source.target_view().is_none()
        };
        let mut read_only =
            source_state() && source.format() == self.sources[index].descriptor.format;
        let pass = DrawPass {
            target: &consumer.target,
            origin: DeviceIntPoint::zero(),
            viewport: None,
            depth: None,
            clear_color: Some(wgt::Color {
                r: f64::from(BACKGROUND[0]) / 255.0,
                g: f64::from(BACKGROUND[1]) / 255.0,
                b: f64::from(BACKGROUND[2]) / 255.0,
                a: 1.0,
            }),
            clear_depth: None,
            depth_range: 0.0..1.0,
        };
        let shader = select_draw_shader("cs_scale", &["TEXTURE_2D"], false).unwrap();
        let program = ProgramState::new(shader);
        program.set_transform(&Transform3D::ortho(0.0, 4.0, 0.0, 2.0, -1.0, 1.0));
        let plain = program
            .resolve(
                &resources.plain,
                &pass,
                |slot| saved.get(slot).cloned(),
                None,
            )
            .unwrap();
        let blend = program
            .resolve(
                &resources.blend,
                &pass,
                |slot| saved.get(slot).cloned(),
                None,
            )
            .unwrap();
        {
            let mut commands = consumer.submission.recording().unwrap();
            if check_readonly {
                pass.clear_rect(
                    &mut commands,
                    full,
                    Some([
                        f32::from(BACKGROUND[0]) / 255.0,
                        f32::from(BACKGROUND[1]) / 255.0,
                        f32::from(BACKGROUND[2]) / 255.0,
                        1.0,
                    ]),
                    None,
                )
                .unwrap();
                read_only &= source.invalidate(&mut commands).is_err();
                read_only &= source.initialize(&mut commands).is_err();
                read_only &= source
                    .copy_from_texture(&mut commands, &consumer.target, source_rect, source_rect)
                    .is_err();
                read_only &= source.copy_source().is_ok();
                read_only &= source_state();
            }
            let mut instances = |left: f32| {
                let values = [left, 0.0, left + 2.0, 2.0, 0.0, 0.0, 2.0, 2.0, 1.0f32];
                resources
                    .uploads
                    .upload_in_recording(&mut commands, 36, wgt::BufferUses::VERTEX, |bytes| {
                        for (destination, value) in bytes.chunks_exact_mut(4).zip(values.iter()) {
                            destination.copy_from_slice(&value.to_ne_bytes());
                        }
                        Ok(())
                    })
                    .unwrap()
            };
            let left_instances = instances(0.0);
            let right_instances = instances(2.0);
            let draws = [
                Draw {
                    bindings: plain
                        .bindings(&mut commands, &resources.uploads, Some(&resources.samplers))
                        .unwrap(),
                    instances: left_instances,
                    instance_offset: 0,
                    instance_count: 1,
                    scissor: full,
                },
                Draw {
                    bindings: blend
                        .bindings(&mut commands, &resources.uploads, Some(&resources.samplers))
                        .unwrap(),
                    instances: right_instances,
                    instance_offset: 0,
                    instance_count: 1,
                    scissor: full,
                },
            ];
            let registry = resources.store.external_textures();
            registry.unregister(handle).unwrap();
            resources.store.reset_bindings();
            let removed = resources
                .store
                .bind_external(TextureSlot(0), &external(handle))
                .is_err();
            pass.record(&mut commands, &resources.quad, &draws).unwrap();
            drop(draws);
            drop(plain);
            drop(blend);
            drop(saved);
            drop(source);
            return (read_only, removed);
        }
    }
}

pub(super) fn linear(value: u8, srgb: bool) -> f64 {
    let encoded = f64::from(value) / 255.0;
    if !srgb {
        encoded
    } else if encoded <= 0.04045 {
        encoded / 12.92
    } else {
        ((encoded + 0.055) / 1.055).powf(2.4)
    }
}

pub(super) fn verify_pixels(bytes: &[u8], srgb: bool, opaque: bool) {
    assert_eq!(bytes.len(), 32);
    for row in 0..2 {
        for panel in 0..2 {
            for column in 0..2 {
                let pixel = PATTERN[row * 2 + column];
                let alpha = if opaque {
                    1.0
                } else {
                    f64::from(pixel[3]) / 255.0
                };
                let offset = (row * 4 + panel * 2 + column) * 4;
                for channel in 0..4 {
                    let expected = if channel == 3 {
                        if panel == 0 {
                            (alpha * 255.0).round() as u8
                        } else {
                            255
                        }
                    } else {
                        let sample = linear(pixel[channel], srgb);
                        let result = if panel == 0 {
                            sample
                        } else {
                            sample * alpha + f64::from(BACKGROUND[channel]) / 255.0 * (1.0 - alpha)
                        };
                        (result * 255.0).round() as u8
                    };
                    let tolerance = u8::from(channel < 3 && (srgb || panel == 1));
                    assert!(bytes[offset + channel].abs_diff(expected) <= tolerance,
                        "Sampled canvas sRGB{srgb}, opaque{opaque}, row{row}, panel{panel}, column{column}, channel{channel}: {} != {expected} (tolerance {tolerance})",
                        bytes[offset + channel]);
                }
            }
        }
    }
}

impl CanvasSampling {
    fn arm_gate(&mut self, graphics: &hal::DynOpenDevice, provider: &GraphicsProvider) {
        self.gate = Some(NativeGate::new(graphics));
        *provider.native_wait.lock().unwrap() = Some(hal::external_sync::SemaphoreValue {
            semaphore: self.gate.as_ref().unwrap().imported.clone(),
            value: Some(1),
        });
    }

    fn never_sampled(
        &mut self,
        device: &Arc<core::device::Device>,
        queue: &Arc<core::device::queue::Queue>,
        graphics: &Arc<hal::DynOpenDevice>,
        provider: &GraphicsProvider,
    ) {
        let before = counts(graphics);
        let logical_before = device.get_hal_counters().textures.read();
        let index = self.create_source(device, queue, graphics, wgt::TextureFormat::Rgba8Unorm);
        self.arm_gate(graphics, provider);
        let source = &self.sources[index];
        let finalized = unsafe {
            queue.finalize_shared_canvas(&source.texture, source.lease.as_ref().unwrap().clone())
        }
        .unwrap();
        let submission = finalized.submission_index();
        self.sources[index].epoch = Some(finalized.publish_epoch());
        let weak = Arc::downgrade(self.sources[index].lease.as_ref().unwrap());
        let handle = self.register(index, false, false);
        self.resources
            .as_ref()
            .unwrap()
            .store
            .external_textures()
            .unregister(handle)
            .unwrap();
        drop(self.sources[index].lease.take());
        drop(self.sources[index].epoch.take());
        let held = counts(graphics);
        let lease_alive = weak.upgrade().is_some();
        let logical_held = device.get_hal_counters().textures.read();
        let producer_ready = provider.receipts.lock().unwrap().last().map(|receipt| {
            receipt
                .native
                .wait(Some(std::time::Duration::from_millis(50)))
        });
        let explicit = self.release_gate().unwrap();
        assert!(explicit, "Never-sampled controller fallback fired");
        assert!(
            matches!(producer_ready, Some(Ok(false))),
            "Early-drop producer was not gated: {producer_ready:?}"
        );
        assert!(lease_alive, "Core producer did not retain physical storage");
        assert_eq!(held.0, before.0 + 1);
        assert!(held.1 > before.1);
        assert_eq!(logical_held, logical_before + 1);
        provider.release_receipts();
        device
            .poll(wgt::PollType::Wait {
                submission_index: Some(submission),
                timeout: Some(std::time::Duration::from_secs(30)),
            })
            .unwrap();
        assert!(weak.upgrade().is_none());
        assert_eq!(counts(graphics), before);
        assert_eq!(device.get_hal_counters().textures.read(), logical_before);
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
        self.resources = Some(Resources::new(owner));
        self.foreign = Some(ForeignRegistry::new(owner));
        device.push_error_scope(wgt::error::ErrorFilter::Validation);
        for format in [
            wgt::TextureFormat::Rgba8Unorm,
            wgt::TextureFormat::Bgra8Unorm,
        ] {
            for srgb in [false, true] {
                for opaque in [false, true] {
                    let case_before = counts(graphics);
                    let logical_before = device.get_hal_counters().textures.read();
                    let index = self.create_source(device, queue, graphics, format);
                    self.consumers.push(Consumer {
                        target: Texture::new(
                            owner,
                            4,
                            2,
                            wgt::TextureFormat::Rgba8Unorm,
                            TextureFilter::Nearest,
                            true,
                        )
                        .unwrap(),
                        submission: Submission::new(owner).unwrap(),
                        source: std::rc::Weak::new(),
                    });
                    let gated = format == wgt::TextureFormat::Bgra8Unorm && srgb && !opaque;
                    if gated {
                        self.arm_gate(graphics, provider);
                    }
                    let source = &self.sources[index];
                    let finalized = unsafe {
                        queue.finalize_shared_canvas(
                            &source.texture,
                            source.lease.as_ref().unwrap().clone(),
                        )
                    }
                    .unwrap();
                    let submission = finalized.submission_index();
                    self.sources[index].epoch = Some(finalized.publish_epoch());
                    if index == 0 {
                        self.rejected_registrations(index);
                    }
                    let handle = self.register(index, srgb, opaque);
                    let lease = Arc::downgrade(self.sources[index].lease.as_ref().unwrap());
                    let (read_only, removed) = self.record(index, handle, index == 0);
                    drop(self.sources[index].lease.take());
                    drop(self.sources[index].epoch.take());
                    self.consumers[index].submission.submit().unwrap();
                    let kept = self.consumers[index].source.upgrade().is_some();
                    if gated {
                        let producer_ready =
                            provider.receipts.lock().unwrap().last().map(|receipt| {
                                receipt
                                    .native
                                    .wait(Some(std::time::Duration::from_millis(50)))
                            });
                        let consumer_ready = self.consumers[index]
                            .submission
                            .wait(Some(std::time::Duration::from_millis(250)));
                        let explicit = self.release_gate().unwrap();
                        assert!(explicit, "Sampling controller fallback fired");
                        assert!(
                            matches!(producer_ready, Some(Ok(false))),
                            "Sampling producer was not gated: {producer_ready:?}"
                        );
                        assert!(
                            matches!(consumer_ready, Ok(false)),
                            "WR shader draw overtook producer: {consumer_ready:?}"
                        );
                        assert!(provider.native_wait.lock().unwrap().is_none());
                    }
                    assert!(
                        read_only,
                        "Registered canvas view lost its read-only RESOURCE state"
                    );
                    assert!(removed, "Unregistered canvas handle resolved again");
                    assert!(
                        kept,
                        "Prepared draw resources did not retain the unregistered image"
                    );
                    provider.release_receipts();
                    device
                        .poll(wgt::PollType::Wait {
                            submission_index: Some(submission),
                            timeout: Some(std::time::Duration::from_secs(30)),
                        })
                        .unwrap();
                    let retained = counts(graphics);
                    assert_eq!(device.get_hal_counters().textures.read(), logical_before);
                    assert!(lease.upgrade().is_some());
                    assert_eq!(retained.0, case_before.0 + 3);
                    assert!(retained.1 > case_before.1);
                    assert!(self.consumers[index].source.upgrade().is_some());
                    assert!(self.consumers[index]
                        .submission
                        .wait(Some(std::time::Duration::from_secs(30)))
                        .unwrap());
                    assert!(self.consumers[index].source.upgrade().is_none());
                    assert!(lease.upgrade().is_none());
                    let after = counts(graphics);
                    assert_eq!(after.0, case_before.0 + 1);
                    assert!(after.1 < retained.1);
                    let full = DeviceIntRect::from_size(DeviceIntSize::new(4, 2));
                    let bytes = self.consumers[index]
                        .target
                        .readback(full)
                        .unwrap()
                        .wait()
                        .unwrap();
                    verify_pixels(&bytes, srgb, opaque);
                }
            }
        }
        self.never_sampled(device, queue, graphics, provider);
        self.foreign.as_ref().unwrap().context.drain().unwrap();
        assert!(device.pop_error_scope().unwrap().is_none());
        self.consumers.clear();
        drop(self.resources.take());
        assert_eq!(counts(graphics), before);
    }
}
