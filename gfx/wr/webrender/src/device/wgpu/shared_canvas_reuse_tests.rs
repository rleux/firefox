/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::shared_canvas_tests::NativeGate;
use super::shared_canvas_sampling_tests::{BACKGROUND, PATTERN};
use super::shared_canvas_snapshot_tests::CanvasSnapshots;
use crate::device::{DrawTarget, LoadOp, RenderPassDescriptor, StoreOp};
use crate::device::wgpu::bindings::DrawBindings;
use crate::device::wgpu::Samplers;
use api::ExternalTextureHandle;
use hal::shared_image::{ImageEpoch, ImageLease, ImageLeaseError, ReturnedImage};

const NEW_PIXEL: [u8; 4] = [13, 29, 79, 255];

fn counts(graphics: &hal::DynOpenDevice) -> (isize, isize) {
    let counters = graphics.device.get_internal_counters();
    (counters.textures.read(), counters.texture_memory.read())
}

fn logical_counts(device: &core::device::Device) -> (isize, isize) {
    let counters = device.get_hal_counters();
    (counters.textures.read(), counters.texture_memory.read())
}

fn rect() -> DeviceIntRect {
    DeviceIntRect::from_size(DeviceIntSize::new(2, 2))
}

fn descriptor(
    format: wgt::TextureFormat,
    render_pass: bool,
    copy_src: bool,
    srgb: bool,
) -> core::resource::TextureDescriptor<'static> {
    let mut usage = wgt::TextureUsages::COPY_DST;
    if render_pass {
        usage |= wgt::TextureUsages::RENDER_ATTACHMENT;
    }
    if copy_src {
        usage |= wgt::TextureUsages::COPY_SRC;
    }
    core::resource::TextureDescriptor {
        label: Some(Cow::Borrowed("canonical canvas reuse")),
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
        view_formats: if srgb {
            vec![format.add_srgb_suffix()]
        } else {
            Vec::new()
        },
    }
}

fn storage_pixel(mut pixel: [u8; 4], format: wgt::TextureFormat) -> [u8; 4] {
    if format == wgt::TextureFormat::Bgra8Unorm {
        pixel.swap(0, 2);
    }
    pixel
}

fn poison_bytes(format: wgt::TextureFormat) -> Vec<u8> {
    PATTERN
        .iter()
        .flat_map(|pixel| storage_pixel(*pixel, format))
        .collect()
}

struct Frame {
    texture: Option<Arc<core::resource::Texture>>,
    lease: Arc<dyn ImageLease>,
    epoch: Option<Arc<ImageEpoch>>,
    submission: Option<core::SubmissionIndex>,
}

struct LogicalProducer {
    _instance: Arc<core::instance::Instance>,
    _adapter: Arc<core::instance::Adapter>,
    device: Arc<core::device::Device>,
    queue: Arc<core::device::queue::Queue>,
    provider: Arc<GraphicsProvider>,
}

impl LogicalProducer {
    fn new(
        context: Arc<dyn hal::shared::SharedDeviceContext>,
        graphics: Arc<hal::DynOpenDevice>,
    ) -> Self {
        let (backend, instance) = context.clone_instance();
        let flags = wgt::InstanceFlags::DEBUG
            | wgt::InstanceFlags::VALIDATION
            | wgt::InstanceFlags::STRICT_WEBGPU_COMPLIANCE
            | wgt::InstanceFlags::AUTOMATIC_TIMESTAMP_NORMALIZATION;
        let instance = unsafe {
            core::instance::Instance::from_hal_instance_dynamic(
                "Canonical reuse private producer".into(),
                backend,
                instance,
                flags,
                None,
            )
        };
        let adapter = instance
            .enumerate_adapters(wgt::Backends::VULKAN, false)
            .pop()
            .unwrap();
        let mut desc = core::device::DeviceDescriptor::default();
        adapter.validate_device_descriptor(&mut desc).unwrap();
        let compute = unsafe {
            context.clone().open_compute_device(
                adapter.as_hal_dynamic(),
                desc.required_features,
                &desc.required_limits,
                &desc.memory_hints,
            )
        }
        .unwrap();
        let provider = Arc::new(GraphicsProvider {
            graphics,
            context,
            creations: AtomicUsize::new(0),
            recoveries: AtomicUsize::new(0),
            receipts: Mutex::new(Vec::new()),
            lanes: Mutex::new(Vec::new()),
            native_wait: Mutex::new(None),
        });
        let (device, queue) = unsafe {
            adapter.create_device_and_queue_from_hal_with_graphics(
                compute,
                &desc,
                Some(provider.clone()),
            )
        }
        .unwrap();
        Self {
            _instance: instance,
            _adapter: adapter,
            device,
            queue,
            provider,
        }
    }
}

struct IsolatedRealm {
    context: Arc<dyn hal::shared::SharedDeviceContext>,
    graphics: Arc<hal::DynOpenDevice>,
    owner: Rc<Device>,
    recoveries: Rc<Cell<usize>>,
    producer: LogicalProducer,
    helper: CanvasSnapshots,
}

impl IsolatedRealm {
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
        let graphics = Arc::new(pair.graphics);
        drop(pair.compute);
        let (_, instance) = context.clone_instance();
        let adapter = unsafe { instance.enumerate_adapters(None) }.pop().unwrap();
        let recoveries = Rc::new(Cell::new(0));
        let recovered = recoveries.clone();
        let owner = Rc::new(
            unsafe {
                Device::from_shared_endpoint(
                    graphics.clone(),
                    instance,
                    adapter,
                    SharedDeviceOptions {
                        context: context.clone(),
                        recover: Rc::new(move |_| recovered.set(recovered.get() + 1)),
                        fatal_process: terminate_test_process,
                    },
                )
            }
            .unwrap(),
        );
        let producer = LogicalProducer::new(context.clone(), graphics.clone());
        let mut helper = CanvasSnapshots::default();
        helper.setup(&owner);
        Self {
            context,
            graphics,
            owner,
            recoveries,
            producer,
            helper,
        }
    }
}

#[derive(Default)]
pub(super) struct CanvasReuse {
    primary: CanvasSnapshots,
    secondary: CanvasSnapshots,
    frames: Vec<Frame>,
    producers: Vec<LogicalProducer>,
    realms: Vec<IsolatedRealm>,
    rejected_textures: Vec<Arc<core::resource::Texture>>,
    held: Option<DrawBindings>,
    samplers: Option<Rc<Samplers>>,
    gate: Option<NativeGate>,
    destroyed_token: Option<ReturnedImage>,
    destroyed_frame: Option<(usize, ExternalTextureHandle)>,
    baseline: Option<(isize, isize)>,
}

impl CanvasReuse {
    pub(super) fn release_gate(&mut self) -> Result<bool, String> {
        self.gate.as_mut().map_or(Ok(true), NativeGate::release)
    }

    fn finish_realm(&mut self, frame_start: usize) {
        self.realms.last_mut().unwrap().helper.cleanup();
        self.frames.truncate(frame_start);
        drop(self.realms.pop());
    }

    fn allocate(
        &mut self,
        device: &Arc<core::device::Device>,
        graphics: &Arc<hal::DynOpenDevice>,
        desc: &core::resource::TextureDescriptor,
    ) -> usize {
        let (texture, lease) =
            unsafe { device.create_texture_with_shared_image(desc, graphics.clone()) };
        self.frames.push(Frame {
            texture: Some(texture),
            lease: lease.unwrap(),
            epoch: None,
            submission: None,
        });
        self.frames.len() - 1
    }

    fn reuse(
        &mut self,
        device: &Arc<core::device::Device>,
        graphics: &Arc<hal::DynOpenDevice>,
        desc: &core::resource::TextureDescriptor,
        returned: ReturnedImage,
        previous: usize,
    ) -> usize {
        let before = counts(graphics);
        let logical_before = logical_counts(device);
        let (texture, lease) = device.create_texture_from_returned_shared_image(desc, returned);
        let lease = lease.expect("Returned storage must create a fresh logical image");
        assert!(Arc::ptr_eq(&lease, &self.frames[previous].lease));
        assert_eq!(counts(graphics), before);
        assert_eq!(
            logical_counts(device),
            (logical_before.0 + 1, logical_before.1)
        );
        self.frames.push(Frame {
            texture: Some(texture),
            lease,
            epoch: None,
            submission: None,
        });
        self.frames.len() - 1
    }

    fn publish(
        &mut self,
        index: usize,
        queue: &Arc<core::device::queue::Queue>,
    ) -> Arc<ImageEpoch> {
        let frame = &self.frames[index];
        let finalized = unsafe {
            queue.finalize_shared_canvas(frame.texture.as_ref().unwrap(), frame.lease.clone())
        }
        .unwrap();
        let submission = finalized.submission_index();
        let epoch = finalized.publish_epoch();
        self.frames[index].submission = Some(submission);
        self.frames[index].epoch = Some(epoch.clone());
        epoch
    }

    fn write(
        &self,
        index: usize,
        queue: &core::device::queue::Queue,
        format: wgt::TextureFormat,
        partial: bool,
    ) {
        let bytes = if partial {
            storage_pixel(NEW_PIXEL, format).to_vec()
        } else {
            poison_bytes(format)
        };
        queue.write_texture(
            wgt::TexelCopyTextureInfo {
                texture: self.frames[index].texture.as_ref().unwrap().clone(),
                mip_level: 0,
                origin: if partial {
                    wgt::Origin3d { x: 1, y: 1, z: 0 }
                } else {
                    wgt::Origin3d::ZERO
                },
                aspect: wgt::TextureAspect::All,
            },
            &bytes,
            &wgt::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: if partial { None } else { Some(8) },
                rows_per_image: None,
            },
            &wgt::Extent3d {
                width: if partial { 1 } else { 2 },
                height: if partial { 1 } else { 2 },
                depth_or_array_layers: 1,
            },
        );
    }

    fn wait_producer(
        &self,
        index: usize,
        device: &core::device::Device,
        provider: &GraphicsProvider,
    ) {
        provider.release_receipts();
        device
            .poll(wgt::PollType::Wait {
                submission_index: self.frames[index].submission,
                timeout: Some(std::time::Duration::from_secs(30)),
            })
            .unwrap();
    }

    fn register(helper: &CanvasSnapshots, epoch: &Arc<ImageEpoch>) -> ExternalTextureHandle {
        unsafe {
            helper
                .renderer
                .as_ref()
                .unwrap()
                .textures
                .external_textures()
                .register_shared_image(
                    epoch.clone(),
                    epoch.descriptor().format,
                    TextureFilter::Nearest,
                    false,
                )
        }
        .unwrap()
    }

    fn unregister(helper: &mut CanvasSnapshots, handle: ExternalTextureHandle) {
        let renderer = helper.renderer.as_mut().unwrap();
        renderer
            .textures
            .external_textures()
            .unregister(handle)
            .unwrap();
        renderer.textures.reset_bindings();
    }

    fn sentinel(helper: &mut CanvasSnapshots) {
        let renderer = helper.renderer.as_mut().unwrap();
        renderer.begin_frame().unwrap();
        renderer
            .begin_render_pass(&RenderPassDescriptor {
                target: DrawTarget::new_default(DeviceIntSize::new(4, 2), true),
                render_area: None,
                color_load: LoadOp::Clear([1.0, 0.0, 1.0, 1.0]),
                depth_load: LoadOp::Load,
            })
            .unwrap();
        renderer.end_render_pass(StoreOp::Store).unwrap();
        renderer.end_frame().unwrap();
        renderer.submissions.wait().unwrap();
        let output = renderer.textures.output().unwrap();
        assert_eq!(
            output
                .readback(DeviceIntRect::from_size(DeviceIntSize::new(4, 2)))
                .unwrap()
                .wait()
                .unwrap(),
            [255, 0, 255, 255].repeat(8)
        );
    }

    fn hold_binding(&mut self) {
        let renderer = self.secondary.renderer.as_ref().unwrap();
        let pass = renderer.passes.draw_pass().unwrap().unwrap();
        let resolved = renderer
            .programs
            .resolve_current(&pass, |slot| renderer.textures.binding(slot), None)
            .unwrap();
        self.held = Some(
            resolved
                .bindings(
                    &mut renderer.submissions.recording().unwrap(),
                    &renderer.submissions,
                    self.samplers.as_ref(),
                )
                .unwrap(),
        );
    }

    fn close_and_return(
        &mut self,
        index: usize,
        device: &Arc<core::device::Device>,
        queue: &Arc<core::device::queue::Queue>,
        provider: &GraphicsProvider,
        owner: &Rc<Device>,
        hold: bool,
    ) -> ReturnedImage {
        let epoch = self.frames[index].epoch.as_ref().unwrap().clone();
        let first = Self::register(&self.primary, &epoch);
        let second = Self::register(&self.secondary, &epoch);
        let first_registry = self
            .primary
            .renderer
            .as_ref()
            .unwrap()
            .textures
            .external_textures();
        let second_registry = self
            .secondary
            .renderer
            .as_ref()
            .unwrap()
            .textures
            .external_textures();
        let first_view = first_registry.get(first.0 as u32).unwrap();
        let second_view = second_registry.get(second.0 as u32).unwrap();
        let first_weak = Rc::downgrade(&first_view);
        let second_weak = Rc::downgrade(&second_view);
        drop((first_view, second_view));
        if hold {
            let renderer = self.primary.renderer.as_mut().unwrap();
            let submissions = renderer.submissions.clone();
            let borrowed = submissions.recording().unwrap();
            assert!(renderer.close_shared_image(&epoch).is_err());
            assert!(!epoch.is_closed());
            drop(borrowed);
        }
        let (format, bytes) = self
            .primary
            .renderer
            .as_mut()
            .unwrap()
            .snapshot_shared_image(first, rect())
            .unwrap();
        assert_eq!(
            format,
            if epoch.descriptor().format == wgt::TextureFormat::Rgba8Unorm {
                api::ImageFormat::RGBA8
            } else {
                api::ImageFormat::BGRA8
            }
        );
        assert_eq!(bytes, poison_bytes(epoch.descriptor().format));
        Self::sentinel(&mut self.primary);
        self.primary.begin_draw(first);
        self.secondary.begin_draw(second);
        if hold {
            self.hold_binding();
        }
        self.secondary
            .renderer
            .as_mut()
            .unwrap()
            .end_render_pass(StoreOp::Store)
            .unwrap();
        let marker = self
            .primary
            .renderer
            .as_mut()
            .unwrap()
            .close_shared_image(&epoch)
            .unwrap()
            .unwrap();
        self.primary.verify_draw(false, false);
        self.primary
            .renderer
            .as_ref()
            .unwrap()
            .submissions
            .wait_for(marker)
            .unwrap();
        self.wait_producer(index, device, provider);
        assert!(queue
            .try_take_returned_shared_image(&epoch)
            .unwrap()
            .is_none());
        assert!(self
            .primary
            .renderer
            .as_mut()
            .unwrap()
            .snapshot_shared_image(first, rect())
            .is_err());
        if let Some(binding) = &self.held {
            let mut rejected = Submission::new(owner).unwrap();
            assert!(binding.prepare(&mut rejected.recording().unwrap()).is_err());
        }
        let source = second_registry.get(second.0 as u32).unwrap();
        assert!(source.readback(rect()).is_err());
        drop(source);
        Self::unregister(&mut self.primary, first);
        Self::unregister(&mut self.secondary, second);
        self.primary.finish_draw();
        assert!(first_weak.upgrade().is_none());
        assert!(second_weak.upgrade().is_some());
        self.secondary
            .renderer
            .as_mut()
            .unwrap()
            .end_frame()
            .unwrap();
        assert!(queue
            .try_take_returned_shared_image(&epoch)
            .unwrap()
            .is_none());
        self.secondary
            .renderer
            .as_ref()
            .unwrap()
            .submissions
            .wait()
            .unwrap();
        self.secondary.verify_draw(false, false);
        assert!(self
            .secondary
            .renderer
            .as_mut()
            .unwrap()
            .close_shared_image(&epoch)
            .unwrap()
            .is_none());
        if hold {
            assert!(queue
                .try_take_returned_shared_image(&epoch)
                .unwrap()
                .is_none());
            assert!(second_weak.upgrade().is_some());
            drop(self.held.take());
        }
        assert!(second_weak.upgrade().is_none());
        assert!(self
            .primary
            .renderer
            .as_ref()
            .unwrap()
            .has_cached_readback());
        let returned = queue
            .try_take_returned_shared_image(&epoch)
            .unwrap()
            .expect("Retired zero-payload epoch must return once");
        assert!(queue
            .try_take_returned_shared_image(&epoch)
            .ok()
            .flatten()
            .is_none());
        returned
    }
}

impl CanvasReuse {
    fn verify_new(&mut self, index: usize, partial: bool, snapshot: bool) -> ExternalTextureHandle {
        let epoch = self.frames[index].epoch.as_ref().unwrap();
        let handle = Self::register(&self.primary, epoch);
        let registry = self
            .primary
            .renderer
            .as_ref()
            .unwrap()
            .textures
            .external_textures();
        let before = counts(&registry.device().open);
        assert!(unsafe {
            registry.register_shared_image(
                epoch.clone(),
                epoch.descriptor().format.add_srgb_suffix(),
                TextureFilter::Nearest,
                false,
            )
        }
        .is_err());
        assert_eq!(counts(&registry.device().open), before);
        self.primary.begin_draw(handle);
        if snapshot {
            let (_, bytes) = self
                .primary
                .renderer
                .as_mut()
                .unwrap()
                .snapshot_shared_image(handle, rect())
                .unwrap();
            let mut expected = vec![0; 16];
            if partial {
                expected[12..16]
                    .copy_from_slice(&storage_pixel(NEW_PIXEL, epoch.descriptor().format));
            }
            assert_eq!(
                bytes, expected,
                "Reused canvas exposed previous poisoned pixels"
            );
        } else {
            assert!(self
                .primary
                .renderer
                .as_mut()
                .unwrap()
                .snapshot_shared_image(handle, rect())
                .is_err());
        }
        self.primary.finish_draw();
        let output = self
            .primary
            .renderer
            .as_ref()
            .unwrap()
            .textures
            .output()
            .unwrap();
        let bytes = output
            .readback(DeviceIntRect::from_size(DeviceIntSize::new(4, 2)))
            .unwrap()
            .wait()
            .unwrap();
        for row in 0..2 {
            for panel in 0..2 {
                for column in 0..2 {
                    let expected = if partial && row == 1 && column == 1 {
                        NEW_PIXEL
                    } else if panel == 1 {
                        BACKGROUND
                    } else {
                        [0; 4]
                    };
                    let offset = (row * 4 + panel * 2 + column) * 4;
                    assert_eq!(
                        &bytes[offset..offset + 4],
                        &expected,
                        "Fresh logical initialization row{row}, panel{panel}, column{column}"
                    );
                }
            }
        }
        handle
    }

    fn return_new(
        &mut self,
        index: usize,
        handle: ExternalTextureHandle,
        device: &Arc<core::device::Device>,
        queue: &Arc<core::device::queue::Queue>,
        provider: &GraphicsProvider,
    ) -> ReturnedImage {
        let epoch = self.frames[index].epoch.as_ref().unwrap();
        let marker = self
            .primary
            .renderer
            .as_mut()
            .unwrap()
            .close_shared_image(epoch)
            .unwrap()
            .unwrap();
        Self::unregister(&mut self.primary, handle);
        self.primary
            .renderer
            .as_ref()
            .unwrap()
            .submissions
            .wait_for(marker)
            .unwrap();
        self.wait_producer(index, device, provider);
        let returned = queue
            .try_take_returned_shared_image(epoch)
            .unwrap()
            .unwrap();
        assert!(queue
            .try_take_returned_shared_image(epoch)
            .ok()
            .flatten()
            .is_none());
        returned
    }

    fn gated_never_sampled(
        &mut self,
        device: &Arc<core::device::Device>,
        queue: &Arc<core::device::queue::Queue>,
        graphics: &Arc<hal::DynOpenDevice>,
        provider: &GraphicsProvider,
    ) {
        let index = self.allocate(
            device,
            graphics,
            &descriptor(wgt::TextureFormat::Rgba8Unorm, true, true, true),
        );
        self.write(index, queue, wgt::TextureFormat::Rgba8Unorm, false);
        self.gate = Some(NativeGate::new(graphics));
        *provider.native_wait.lock().unwrap() = Some(hal::external_sync::SemaphoreValue {
            semaphore: self.gate.as_ref().unwrap().imported.clone(),
            value: Some(1),
        });
        let epoch = self.publish(index, queue);
        let handle = Self::register(&self.primary, &epoch);
        Self::unregister(&mut self.primary, handle);
        let marker = self
            .primary
            .renderer
            .as_mut()
            .unwrap()
            .close_shared_image(&epoch)
            .unwrap()
            .unwrap();
        let premature = queue.try_take_returned_shared_image(&epoch);
        let status = self.primary.renderer.as_ref().unwrap().submissions.status();
        let producer = provider.receipts.lock().unwrap().last().map(|receipt| {
            receipt
                .native
                .wait(Some(std::time::Duration::from_millis(50)))
        });
        let explicit = self.release_gate().unwrap();
        assert!(explicit, "Never-sampled return used native gate fallback");
        assert!(
            matches!(producer, Some(Ok(false))),
            "Never-sampled producer was not gated: {producer:?}"
        );
        assert!(
            premature.unwrap().is_none(),
            "Zero payloads bypassed the pending close marker"
        );
        assert!(status.unwrap().completed < marker);
        self.wait_producer(index, device, provider);
        self.primary
            .renderer
            .as_ref()
            .unwrap()
            .submissions
            .wait_for(marker)
            .unwrap();
        let returned = queue
            .try_take_returned_shared_image(&epoch)
            .unwrap()
            .unwrap();
        assert!(queue
            .try_take_returned_shared_image(&epoch)
            .ok()
            .flatten()
            .is_none());
        let next = self.reuse(
            device,
            graphics,
            &descriptor(wgt::TextureFormat::Rgba8Unorm, false, true, false),
            returned,
            index,
        );
        self.publish(next, queue);
        let handle = self.verify_new(next, false, true);
        drop(self.return_new(next, handle, device, queue, provider));
    }

    pub(super) fn run(
        &mut self,
        device: &Arc<core::device::Device>,
        queue: &Arc<core::device::queue::Queue>,
        graphics: &Arc<hal::DynOpenDevice>,
        provider: &GraphicsProvider,
        owner: &Rc<Device>,
    ) {
        self.baseline = Some(counts(graphics));
        self.primary.setup(owner);
        self.secondary.setup(owner);
        self.samplers = Some(Rc::new(Samplers::new(owner).unwrap()));
        self.primary
            .renderer
            .as_ref()
            .unwrap()
            .submissions
            .wait()
            .unwrap();
        self.secondary
            .renderer
            .as_ref()
            .unwrap()
            .submissions
            .wait()
            .unwrap();
        device.push_error_scope(wgt::error::ErrorFilter::Validation);
        for format in [
            wgt::TextureFormat::Rgba8Unorm,
            wgt::TextureFormat::Bgra8Unorm,
        ] {
            for render_pass in [true, false] {
                for partial in [false, true] {
                    let logical_before = logical_counts(device);
                    let first =
                        self.allocate(device, graphics, &descriptor(format, true, true, true));
                    self.write(first, queue, format, false);
                    let old_epoch = self.publish(first, queue);
                    assert!(Arc::ptr_eq(old_epoch.lease(), &self.frames[first].lease));
                    let returned = self.close_and_return(
                        first,
                        device,
                        queue,
                        provider,
                        owner,
                        self.frames.len() == 1,
                    );
                    assert_eq!(logical_counts(device), logical_before);
                    let next = self.reuse(
                        device,
                        graphics,
                        &descriptor(format, render_pass, true, false),
                        returned,
                        first,
                    );
                    if partial {
                        self.write(next, queue, format, true);
                    }
                    let new_epoch = self.publish(next, queue);
                    assert!(!Arc::ptr_eq(&old_epoch, &new_epoch));
                    assert!(Arc::ptr_eq(old_epoch.lease(), new_epoch.lease()));
                    assert_eq!(
                        old_epoch.descriptor().view_formats,
                        [format.add_srgb_suffix()]
                    );
                    assert!(new_epoch.descriptor().view_formats.is_empty());
                    let registry = self
                        .primary
                        .renderer
                        .as_ref()
                        .unwrap()
                        .textures
                        .external_textures();
                    let before = counts(graphics);
                    assert!(unsafe {
                        registry.register_shared_image(
                            old_epoch.clone(),
                            format,
                            TextureFilter::Nearest,
                            false,
                        )
                    }
                    .is_err());
                    assert_eq!(counts(graphics), before);
                    let handle = self.verify_new(next, partial, true);
                    let returned = self.return_new(next, handle, device, queue, provider);
                    assert_eq!(logical_counts(device), logical_before);
                    if format == wgt::TextureFormat::Rgba8Unorm && render_pass && !partial {
                        let narrow = self.reuse(
                            device,
                            graphics,
                            &descriptor(format, false, false, false),
                            returned,
                            next,
                        );
                        self.write(narrow, queue, format, true);
                        let epoch = self.publish(narrow, queue);
                        assert!(!epoch
                            .descriptor()
                            .usage
                            .contains(wgt::TextureUses::COPY_SRC));
                        let handle = self.verify_new(narrow, true, false);
                        drop(self.return_new(narrow, handle, device, queue, provider));
                    } else if format == wgt::TextureFormat::Bgra8Unorm && !render_pass && partial {
                        self.wrong_context(owner, returned, graphics);
                    } else {
                        drop(returned);
                    }
                }
            }
        }
        self.gated_never_sampled(device, queue, graphics, provider);
        self.producer_expiry(owner);
        self.generation_loss(owner);
        self.orphaned_finalization(owner);
        self.destroyed_fresh_texture(owner);
        self.close_failure(owner, false);
        self.close_failure(owner, true);
        self.invalid_descriptor(device, queue, graphics, provider, owner);
        self.prepare_destroyed_checks(device, queue, graphics, provider, owner);
        assert!(device.pop_error_scope().unwrap().is_none());
    }
}

impl CanvasReuse {
    fn producer_expiry(&mut self, owner: &Rc<Device>) {
        let frame_start = self.frames.len();
        self.realms.push(IsolatedRealm::new(owner));
        let realm = self.realms.last().unwrap();
        let graphics = realm.graphics.clone();
        let context = realm.context.clone();
        let producer = LogicalProducer::new(context.clone(), graphics.clone());
        let device = producer.device.clone();
        let queue = producer.queue.clone();
        let provider = producer.provider.clone();
        let weak = Arc::downgrade(&device);
        self.producers.push(producer);
        let claimed = self.allocate(
            &device,
            &graphics,
            &descriptor(wgt::TextureFormat::Rgba8Unorm, true, true, true),
        );
        self.write(claimed, &queue, wgt::TextureFormat::Rgba8Unorm, false);
        let claimed_epoch = self.publish(claimed, &queue);
        let realm = self.realms.last_mut().unwrap();
        let handle = Self::register(&realm.helper, &claimed_epoch);
        realm
            .helper
            .renderer
            .as_mut()
            .unwrap()
            .snapshot_shared_image(handle, rect())
            .unwrap();
        let marker = realm
            .helper
            .renderer
            .as_mut()
            .unwrap()
            .close_shared_image(&claimed_epoch)
            .unwrap()
            .unwrap();
        Self::unregister(&mut realm.helper, handle);
        realm
            .helper
            .renderer
            .as_ref()
            .unwrap()
            .submissions
            .wait_for(marker)
            .unwrap();
        self.wait_producer(claimed, &device, &provider);
        let returned = queue
            .try_take_returned_shared_image(&claimed_epoch)
            .unwrap()
            .unwrap();
        let index = self.allocate(
            &device,
            &graphics,
            &descriptor(wgt::TextureFormat::Rgba8Unorm, true, true, true),
        );
        self.write(index, &queue, wgt::TextureFormat::Rgba8Unorm, false);
        let epoch = self.publish(index, &queue);
        let handle = Self::register(&self.realms.last().unwrap().helper, &epoch);
        self.wait_producer(index, &device, &provider);
        drop(self.frames[claimed].texture.take());
        drop(self.frames[index].texture.take());
        drop((device, queue, provider));
        drop(self.producers.pop());
        assert!(weak.upgrade().is_none());
        let realm = self.realms.last_mut().unwrap();
        realm.helper.begin_draw(handle);
        let (_, bytes) = realm
            .helper
            .renderer
            .as_mut()
            .unwrap()
            .snapshot_shared_image(handle, rect())
            .unwrap();
        assert_eq!(bytes, poison_bytes(wgt::TextureFormat::Rgba8Unorm));
        realm.helper.verify_draw(false, false);
        realm.helper.finish_draw();
        let marker = realm
            .helper
            .renderer
            .as_mut()
            .unwrap()
            .close_shared_image(&epoch)
            .unwrap()
            .unwrap();
        Self::unregister(&mut realm.helper, handle);
        realm
            .helper
            .renderer
            .as_ref()
            .unwrap()
            .submissions
            .wait_for(marker)
            .unwrap();
        assert!(matches!(
            epoch.try_take_returned(&*graphics.device, &*graphics.queue, &*graphics.queue),
            Err(ImageLeaseError::ProducerGone)
        ));
        assert!(realm.producer.device.is_valid());
        let before = counts(&graphics);
        let logical_before = logical_counts(&realm.producer.device);
        let (texture, lease) = realm
            .producer
            .device
            .create_texture_from_returned_shared_image(
                &descriptor(wgt::TextureFormat::Rgba8Unorm, false, true, false),
                returned,
            );
        self.rejected_textures.push(texture);
        assert!(lease.is_none());
        assert_eq!(counts(&graphics), before);
        assert_eq!(logical_counts(&realm.producer.device), logical_before);
        context.drain().unwrap();
        drop(self.rejected_textures.pop());
        self.finish_realm(frame_start);
    }

    fn wrong_context(
        &mut self,
        owner: &Rc<Device>,
        returned: ReturnedImage,
        source_graphics: &Arc<hal::DynOpenDevice>,
    ) {
        let frame_start = self.frames.len();
        self.realms.push(IsolatedRealm::new(owner));
        let realm = self.realms.last().unwrap();
        let before = counts(source_graphics);
        let foreign_before = counts(&realm.graphics);
        let logical_before = logical_counts(&realm.producer.device);
        let (texture, lease) = realm
            .producer
            .device
            .create_texture_from_returned_shared_image(
                &descriptor(wgt::TextureFormat::Bgra8Unorm, false, true, false),
                returned,
            );
        self.rejected_textures.push(texture);
        assert!(lease.is_none());
        assert_eq!(counts(source_graphics), before);
        assert_eq!(counts(&realm.graphics), foreign_before);
        assert_eq!(logical_counts(&realm.producer.device), logical_before);
        realm.context.drain().unwrap();
        drop(self.rejected_textures.pop());
        self.finish_realm(frame_start);
    }

    fn ready_in_realm(&mut self, realm_index: usize, format: wgt::TextureFormat) -> usize {
        let realm = &self.realms[realm_index];
        let device = realm.producer.device.clone();
        let queue = realm.producer.queue.clone();
        let graphics = realm.graphics.clone();
        let provider = realm.producer.provider.clone();
        let index = self.allocate(&device, &graphics, &descriptor(format, true, true, true));
        self.write(index, &queue, format, false);
        let epoch = self.publish(index, &queue);
        let realm = &mut self.realms[realm_index];
        let handle = Self::register(&realm.helper, &epoch);
        let (_, bytes) = realm
            .helper
            .renderer
            .as_mut()
            .unwrap()
            .snapshot_shared_image(handle, rect())
            .unwrap();
        assert_eq!(bytes, poison_bytes(format));
        let marker = realm
            .helper
            .renderer
            .as_mut()
            .unwrap()
            .close_shared_image(&epoch)
            .unwrap()
            .unwrap();
        Self::unregister(&mut realm.helper, handle);
        realm
            .helper
            .renderer
            .as_ref()
            .unwrap()
            .submissions
            .wait_for(marker)
            .unwrap();
        self.wait_producer(index, &device, &provider);
        index
    }

    fn generation_loss(&mut self, owner: &Rc<Device>) {
        let frame_start = self.frames.len();
        self.realms.push(IsolatedRealm::new(owner));
        let realm_index = self.realms.len() - 1;
        let claimed = self.ready_in_realm(realm_index, wgt::TextureFormat::Rgba8Unorm);
        let epoch = self.frames[claimed].epoch.as_ref().unwrap();
        let returned = self.realms[realm_index]
            .producer
            .queue
            .try_take_returned_shared_image(epoch)
            .unwrap()
            .unwrap();
        let waiting = self.ready_in_realm(realm_index, wgt::TextureFormat::Rgba8Unorm);
        let epoch = self.frames[waiting].epoch.as_ref().unwrap();
        let realm = &self.realms[realm_index];
        assert!(realm.producer.device.is_valid());
        let before = counts(&realm.graphics);
        let logical_before = logical_counts(&realm.producer.device);
        realm.context.drain().unwrap();
        assert!(realm.producer.device.is_valid());
        assert!(matches!(
            epoch.try_take_returned(
                &*realm.graphics.device,
                &*realm.graphics.queue,
                &*realm.graphics.queue
            ),
            Err(ImageLeaseError::Closed)
        ));
        let (texture, lease) = realm
            .producer
            .device
            .create_texture_from_returned_shared_image(
                &descriptor(wgt::TextureFormat::Rgba8Unorm, false, true, false),
                returned,
            );
        self.rejected_textures.push(texture);
        assert!(lease.is_none());
        assert_eq!(counts(&realm.graphics), before);
        assert_eq!(logical_counts(&realm.producer.device), logical_before);
        drop(self.rejected_textures.pop());
        self.finish_realm(frame_start);
    }

    fn orphaned_finalization(&mut self, owner: &Rc<Device>) {
        let frame_start = self.frames.len();
        self.realms.push(IsolatedRealm::new(owner));
        let realm = self.realms.last().unwrap();
        let device = realm.producer.device.clone();
        let queue = realm.producer.queue.clone();
        let graphics = realm.graphics.clone();
        let provider = realm.producer.provider.clone();
        let context = realm.context.clone();
        let index = self.allocate(
            &device,
            &graphics,
            &descriptor(wgt::TextureFormat::Rgba8Unorm, true, true, true),
        );
        self.write(index, &queue, wgt::TextureFormat::Rgba8Unorm, false);
        self.gate = Some(NativeGate::new(&graphics));
        *provider.native_wait.lock().unwrap() = Some(hal::external_sync::SemaphoreValue {
            semaphore: self.gate.as_ref().unwrap().imported.clone(),
            value: Some(1),
        });
        let frame = &self.frames[index];
        let finalized = unsafe {
            queue.finalize_shared_canvas(frame.texture.as_ref().unwrap(), frame.lease.clone())
        }
        .unwrap();
        let submission = finalized.submission_index();
        drop(finalized);
        let retained = logical_counts(&device);
        let pending = provider.receipts.lock().unwrap().last().map(|receipt| {
            receipt
                .native
                .wait(Some(std::time::Duration::from_millis(50)))
        });
        let explicit = self.release_gate().unwrap();
        assert!(
            explicit,
            "Orphaned publication used the native gate fallback"
        );
        assert!(matches!(pending, Some(Ok(false))));
        assert_eq!(retained.0, 1);
        provider.release_receipts();
        device
            .poll(wgt::PollType::Wait {
                submission_index: Some(submission),
                timeout: Some(std::time::Duration::from_secs(30)),
            })
            .unwrap();
        assert_eq!(logical_counts(&device).0, 0);
        let accepted = provider.receipts.lock().unwrap().len();
        let frame = &self.frames[index];
        assert!(unsafe {
            queue.finalize_shared_canvas(frame.texture.as_ref().unwrap(), frame.lease.clone())
        }
        .is_err());
        assert_eq!(provider.receipts.lock().unwrap().len(), accepted);
        context.drain().unwrap();
        drop(self.gate.take());
        self.finish_realm(frame_start);
    }

    fn destroyed_fresh_texture(&mut self, owner: &Rc<Device>) {
        let frame_start = self.frames.len();
        self.realms.push(IsolatedRealm::new(owner));
        let realm = self.realms.last().unwrap();
        let device = realm.producer.device.clone();
        let queue = realm.producer.queue.clone();
        let graphics = realm.graphics.clone();
        let provider = realm.producer.provider.clone();
        let context = realm.context.clone();
        let index = self.allocate(
            &device,
            &graphics,
            &descriptor(wgt::TextureFormat::Rgba8Unorm, true, true, true),
        );
        let frame = &self.frames[index];
        frame.texture.as_ref().unwrap().destroy();
        assert!(unsafe {
            queue.finalize_shared_canvas(frame.texture.as_ref().unwrap(), frame.lease.clone())
        }
        .is_err());
        assert!(provider.receipts.lock().unwrap().is_empty());
        assert_eq!(logical_counts(&device).0, 0);
        context.drain().unwrap();
        self.finish_realm(frame_start);
    }
}

impl CanvasReuse {
    fn invalid_descriptor(
        &mut self,
        device: &Arc<core::device::Device>,
        queue: &Arc<core::device::queue::Queue>,
        graphics: &Arc<hal::DynOpenDevice>,
        provider: &GraphicsProvider,
        owner: &Rc<Device>,
    ) {
        let index = self.allocate(
            device,
            graphics,
            &descriptor(wgt::TextureFormat::Rgba8Unorm, true, true, true),
        );
        self.write(index, queue, wgt::TextureFormat::Rgba8Unorm, false);
        self.publish(index, queue);
        let returned = self.close_and_return(index, device, queue, provider, owner, false);
        let before = counts(graphics);
        let logical_before = logical_counts(device);
        let mut invalid = descriptor(wgt::TextureFormat::Rgba8Unorm, false, true, false);
        invalid.size.width = 0;
        device.push_error_scope(wgt::error::ErrorFilter::Validation);
        let (texture, lease) = device.create_texture_from_returned_shared_image(&invalid, returned);
        assert!(device.pop_error_scope().unwrap().is_some());
        self.rejected_textures.push(texture);
        assert!(lease.is_none());
        assert_eq!(counts(graphics), before);
        assert_eq!(logical_counts(device), logical_before);
        assert!(queue
            .try_take_returned_shared_image(self.frames[index].epoch.as_ref().unwrap())
            .ok()
            .flatten()
            .is_none());
        assert!(device.is_valid());
        drop(self.rejected_textures.pop());
    }

    fn prepare_destroyed_checks(
        &mut self,
        device: &Arc<core::device::Device>,
        queue: &Arc<core::device::queue::Queue>,
        graphics: &Arc<hal::DynOpenDevice>,
        provider: &GraphicsProvider,
        owner: &Rc<Device>,
    ) {
        let index = self.allocate(
            device,
            graphics,
            &descriptor(wgt::TextureFormat::Rgba8Unorm, true, true, true),
        );
        self.write(index, queue, wgt::TextureFormat::Rgba8Unorm, false);
        self.publish(index, queue);
        self.destroyed_token =
            Some(self.close_and_return(index, device, queue, provider, owner, false));
        let published = self.allocate(
            device,
            graphics,
            &descriptor(wgt::TextureFormat::Rgba8Unorm, true, true, true),
        );
        self.write(published, queue, wgt::TextureFormat::Rgba8Unorm, false);
        let epoch = self.publish(published, queue);
        let handle = Self::register(&self.primary, &epoch);
        self.destroyed_frame = Some((published, handle));
    }

    pub(super) fn after_producer_destroy(
        &mut self,
        device: &Arc<core::device::Device>,
        queue: &Arc<core::device::queue::Queue>,
        graphics: &Arc<hal::DynOpenDevice>,
    ) {
        assert!(!device.is_valid());
        let before = counts(graphics);
        let logical_before = logical_counts(device);
        let returned = self.destroyed_token.take().unwrap();
        let (texture, lease) = device.create_texture_from_returned_shared_image(
            &descriptor(wgt::TextureFormat::Rgba8Unorm, false, true, false),
            returned,
        );
        self.rejected_textures.push(texture);
        assert!(lease.is_none());
        assert_eq!(counts(graphics), before);
        assert_eq!(logical_counts(device), logical_before);
        {
            let (index, handle) = self.destroyed_frame.take().unwrap();
            let epoch = self.frames[index].epoch.as_ref().unwrap().clone();
            self.primary.begin_draw(handle);
            let (_, bytes) = self
                .primary
                .renderer
                .as_mut()
                .unwrap()
                .snapshot_shared_image(handle, rect())
                .unwrap();
            assert_eq!(bytes, poison_bytes(wgt::TextureFormat::Rgba8Unorm));
            self.primary.verify_draw(false, false);
            self.primary.finish_draw();
            let marker = self
                .primary
                .renderer
                .as_mut()
                .unwrap()
                .close_shared_image(&epoch)
                .unwrap()
                .unwrap();
            Self::unregister(&mut self.primary, handle);
            self.primary
                .renderer
                .as_ref()
                .unwrap()
                .submissions
                .wait_for(marker)
                .unwrap();
            assert!(queue.try_take_returned_shared_image(&epoch).is_err());
        }
        self.primary.cleanup();
        self.secondary.cleanup();
        for realm in &mut self.realms {
            realm.helper.cleanup();
        }
        drop(self.samplers.take());
        self.rejected_textures.clear();
        self.frames.clear();
        self.producers.clear();
        self.realms.clear();
        assert_eq!(counts(graphics), self.baseline.unwrap());
    }
}

impl CanvasReuse {
    fn close_failure(&mut self, owner: &Rc<Device>, after_acceptance: bool) {
        let frame_start = self.frames.len();
        self.realms.push(IsolatedRealm::new(owner));
        let realm = self.realms.last().unwrap();
        let device = realm.producer.device.clone();
        let queue = realm.producer.queue.clone();
        let graphics = realm.graphics.clone();
        let provider = realm.producer.provider.clone();
        let context = realm.context.clone();
        let index = self.allocate(
            &device,
            &graphics,
            &descriptor(wgt::TextureFormat::Rgba8Unorm, true, true, true),
        );
        self.write(index, &queue, wgt::TextureFormat::Rgba8Unorm, false);
        let epoch = self.publish(index, &queue);
        self.wait_producer(index, &device, &provider);
        let realm = self.realms.last_mut().unwrap();
        let handle = Self::register(&realm.helper, &epoch);
        let registry = realm
            .helper
            .renderer
            .as_ref()
            .unwrap()
            .textures
            .external_textures();
        let source = registry.get(handle.0 as u32).unwrap();
        let weak = Rc::downgrade(&source);
        drop(source);
        realm.helper.begin_draw(handle);
        let renderer = realm.helper.renderer.as_mut().unwrap();
        drop(renderer.submissions.recording().unwrap());
        if after_acceptance {
            renderer.submissions.fail_active_after_submit_for_test();
        } else {
            renderer.submissions.fail_active_before_submit_for_test();
        }
        assert!(renderer.close_shared_image(&epoch).is_err());
        assert!(realm.owner.is_lost());
        if after_acceptance {
            assert_eq!(realm.recoveries.get(), 1);
        }
        Self::unregister(&mut realm.helper, handle);
        assert!(weak.upgrade().is_none());
        assert!(queue
            .try_take_returned_shared_image(&epoch)
            .ok()
            .flatten()
            .is_none());
        context.drain().unwrap();
        self.finish_realm(frame_start);
    }
}
