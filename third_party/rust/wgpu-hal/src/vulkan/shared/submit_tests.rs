use super::*;
use crate::shared::{SharedQueueLane as Lane, SharedSubmissionReceipt, SharedSubmitSegment};
use core::{
    mem::ManuallyDrop,
    ops::{Deref, DerefMut},
};

struct Encoder {
    raw: Box<dyn crate::DynCommandEncoder>,
    buffers: Vec<Box<dyn crate::DynCommandBuffer>>,
    open: bool,
}

struct Resources {
    context: Arc<dyn crate::shared::SharedDeviceContext>,
    graphics: Arc<crate::DynOpenDevice>,
    compute: Arc<crate::DynOpenDevice>,
    others: Vec<Arc<crate::DynOpenDevice>>,
    encoders: Vec<Encoder>,
    buffers: Vec<Box<dyn crate::DynBuffer>>,
    textures: Vec<Box<dyn crate::DynTexture>>,
    views: Vec<Box<dyn crate::DynTextureView>>,
    fences: Vec<Box<dyn crate::DynFence>>,
    receipts: Vec<Box<dyn SharedSubmissionReceipt>>,
    native_semaphores: Vec<vk::Semaphore>,
    armed_gates: RefCell<Vec<usize>>,
    gates: Vec<Arc<dyn crate::external_sync::Semaphore>>,
    gate_bindings: RefCell<Vec<(Arc<crate::DynOpenDevice>, usize)>>,
}

struct Work {
    resources: ManuallyDrop<Resources>,
    retired: bool,
    quarantined: bool,
}

impl Deref for Work {
    type Target = Resources;
    fn deref(&self) -> &Resources {
        &self.resources
    }
}
impl DerefMut for Work {
    fn deref_mut(&mut self) -> &mut Resources {
        &mut self.resources
    }
}

impl Work {
    fn new() -> Self {
        let SharedDeviceEndpoints {
            context,
            graphics,
            compute,
        } = native_factory().unwrap();
        Self {
            resources: ManuallyDrop::new(Resources {
                context,
                graphics: Arc::new(graphics),
                compute: Arc::new(compute),
                others: Vec::new(),
                encoders: Vec::new(),
                buffers: Vec::new(),
                textures: Vec::new(),
                views: Vec::new(),
                fences: Vec::new(),
                receipts: Vec::new(),
                native_semaphores: Vec::new(),
                armed_gates: RefCell::new(Vec::new()),
                gates: Vec::new(),
                gate_bindings: RefCell::new(Vec::new()),
            }),
            retired: false,
            quarantined: false,
        }
    }

    fn retire(&mut self) -> Result<(), crate::DeviceError> {
        let gates = self.armed_gates.borrow().clone();
        for gate in gates {
            if let Err(error) = self.signal_gate(gate) {
                self.quarantined = true;
                return Err(error);
            }
        }
        let result = self.context.drain();
        self.retired = matches!(result, Ok(()) | Err(crate::DeviceError::Lost));
        self.quarantined = !self.retired;
        result
    }

    fn retain(
        &mut self,
        result: Result<Box<dyn SharedSubmissionReceipt>, crate::shared::SharedSubmitError>,
    ) -> Result<(), crate::DeviceError> {
        match result {
            Ok(receipt) => {
                self.receipts.push(receipt);
                Ok(())
            }
            Err(error) => {
                if let Some(receipt) = error.receipt {
                    self.receipts.push(receipt);
                }
                Err(error.error)
            }
        }
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        if self.quarantined {
            return;
        }
        if !self.retired {
            let result = self.retire();
            if self.quarantined {
                if !std::thread::panicking() {
                    panic!("Native test work quarantined after unconfirmed drain: {result:?}");
                }
                return;
            }
        }
        unsafe {
            let data = &mut *self.resources;
            for encoder in &mut data.encoders {
                if encoder.open {
                    encoder.raw.discard_encoding();
                }
                encoder.raw.reset_all(core::mem::take(&mut encoder.buffers));
            }
            data.encoders.clear();
            data.receipts.clear();
            let native = data
                .compute
                .device
                .as_any()
                .downcast_ref::<crate::vulkan::Device>()
                .unwrap();
            for semaphore in data.native_semaphores.drain(..) {
                native.shared.raw.destroy_semaphore(semaphore, None);
            }
            let device = &*data.compute.device;
            for view in data.views.drain(..) {
                device.destroy_texture_view(view);
            }
            for texture in data.textures.drain(..) {
                device.destroy_texture(texture);
            }
            for buffer in data.buffers.drain(..) {
                device.destroy_buffer(buffer);
            }
            for fence in data.fences.drain(..) {
                device.destroy_fence(fence);
            }
            ManuallyDrop::drop(&mut self.resources);
        }
    }
}

impl Resources {
    fn buffer(&mut self, size: u64, usage: wgt::BufferUses) -> usize {
        let (buffer, _) = unsafe {
            self.compute.device.create_buffer(&crate::BufferDescriptor {
                label: None,
                size,
                usage,
                memory_flags: crate::MemoryFlags::PREFER_COHERENT,
            })
        }
        .unwrap();
        let index = self.buffers.len();
        self.buffers.push(buffer);
        index
    }

    fn encoder_for(&mut self, endpoint: &Arc<crate::DynOpenDevice>) -> usize {
        let raw = unsafe {
            endpoint
                .device
                .create_command_encoder(&crate::CommandEncoderDescriptor {
                    label: None,
                    queue: &*endpoint.queue,
                })
        }
        .unwrap();
        let index = self.encoders.len();
        self.encoders.push(Encoder {
            raw,
            buffers: Vec::new(),
            open: false,
        });
        index
    }

    fn encoder(&mut self, lane: Lane) -> usize {
        let endpoint = match lane {
            Lane::Compute => self.compute.clone(),
            Lane::Graphics => self.graphics.clone(),
        };
        self.encoder_for(&endpoint)
    }

    fn begin(&mut self, encoder: usize) {
        unsafe { self.encoders[encoder].raw.begin_encoding(None) }.unwrap();
        self.encoders[encoder].open = true;
    }

    fn end(&mut self, encoder: usize) -> usize {
        self.encoders[encoder].open = false;
        let buffer = unsafe { self.encoders[encoder].raw.end_encoding() }.unwrap();
        let index = self.encoders[encoder].buffers.len();
        self.encoders[encoder].buffers.push(buffer);
        index
    }

    fn fence(&mut self) -> usize {
        let fence = unsafe { self.compute.device.create_fence() }.unwrap();
        let index = self.fences.len();
        self.fences.push(fence);
        index
    }

    fn other_compute(&mut self) -> Arc<crate::DynOpenDevice> {
        let (_, instance) = self.context.clone_instance();
        let adapter = unsafe { instance.enumerate_adapters(None) }.pop().unwrap();
        let endpoint = Arc::new(
            unsafe {
                self.context.clone().open_compute_device(
                    &*adapter.adapter,
                    wgt::Features::empty(),
                    &adapter.capabilities.limits,
                    &wgt::MemoryHints::default(),
                )
            }
            .unwrap(),
        );
        self.others.push(endpoint.clone());
        endpoint
    }

    fn synchronization(
        &self,
        endpoint: &Arc<crate::DynOpenDevice>,
    ) -> crate::external_sync::SubmissionSync {
        let mut waits = Vec::new();
        self.gate_bindings.borrow_mut().retain(|(queue, gate)| {
            if Arc::ptr_eq(queue, endpoint) {
                waits.push(crate::external_sync::SemaphoreValue {
                    semaphore: self.gates[*gate].clone(),
                    value: Some(1),
                });
                false
            } else {
                true
            }
        });
        crate::external_sync::SubmissionSync {
            waits,
            signals: Vec::new(),
        }
    }

    fn ordered(
        &self,
        parts: &[(Lane, usize, usize)],
        fence: usize,
        value: u64,
    ) -> Result<Box<dyn SharedSubmissionReceipt>, crate::shared::SharedSubmitError> {
        let buffers: Vec<_> = parts
            .iter()
            .map(|&(_, encoder, buffer)| [&*self.encoders[encoder].buffers[buffer]])
            .collect();
        let synchronizations: Vec<_> = parts
            .iter()
            .map(|(lane, _, _)| {
                self.synchronization(match lane {
                    Lane::Compute => &self.compute,
                    Lane::Graphics => &self.graphics,
                })
            })
            .collect();
        let segments: Vec<_> = parts
            .iter()
            .zip(&buffers)
            .zip(&synchronizations)
            .map(|((&(lane, _, _), buffers), sync)| SharedSubmitSegment {
                lane,
                queue: match lane {
                    Lane::Compute => &*self.compute.queue,
                    Lane::Graphics => &*self.graphics.queue,
                },
                command_buffers: buffers,
                synchronization: Some(sync),
            })
            .collect();
        unsafe {
            self.context.submit_ordered(
                &*self.compute.queue,
                &segments,
                (&*self.fences[fence], value),
            )
        }
    }

    fn scoped(
        &self,
        endpoint: &Arc<crate::DynOpenDevice>,
        encoder: usize,
        buffer: usize,
        fence: usize,
        value: u64,
    ) -> Result<Box<dyn SharedSubmissionReceipt>, crate::shared::SharedSubmitError> {
        let sync = self.synchronization(endpoint);
        let buffers = [&*self.encoders[encoder].buffers[buffer]];
        if Arc::ptr_eq(endpoint, &self.graphics) {
            unsafe {
                self.context.submit_graphics(
                    &*endpoint.queue,
                    &buffers,
                    &[],
                    (&*self.fences[fence], value),
                    &sync,
                )
            }
        } else {
            unsafe {
                self.context.submit_ordered(
                    &*endpoint.queue,
                    &[SharedSubmitSegment {
                        lane: Lane::Compute,
                        queue: &*endpoint.queue,
                        command_buffers: &buffers,
                        synchronization: Some(&sync),
                    }],
                    (&*self.fences[fence], value),
                )
            }
        }
    }

    fn gate_token(&mut self) -> usize {
        let device = self
            .compute
            .device
            .as_any()
            .downcast_ref::<crate::vulkan::Device>()
            .unwrap();
        let semaphore = crate::vulkan::external_sync::create(device, false, false).unwrap();
        let index = self.gates.len();
        self.gates.push(semaphore);
        index
    }

    fn gate(&self, index: usize) -> vk::Semaphore {
        self.gates[index]
            .as_any()
            .downcast_ref::<crate::vulkan::external_sync::Semaphore>()
            .unwrap()
            .raw
    }

    fn add_gate(&self, endpoint: &Arc<crate::DynOpenDevice>, gate: usize) {
        self.armed_gates.borrow_mut().push(gate);
        self.gate_bindings
            .borrow_mut()
            .push((endpoint.clone(), gate));
    }

    fn signal_gate(&self, fence: usize) -> Result<(), crate::DeviceError> {
        let device = self
            .compute
            .device
            .as_any()
            .downcast_ref::<crate::vulkan::Device>()
            .unwrap();
        let info = vk::SemaphoreSignalInfo::default()
            .semaphore(self.gate(fence))
            .value(1);
        let result = match device.shared.extension_fns.timeline_semaphore {
            Some(crate::vulkan::ExtensionFn::Promoted) => unsafe {
                device.shared.raw.signal_semaphore(&info)
            },
            Some(crate::vulkan::ExtensionFn::Extension(ref extension)) => unsafe {
                extension.signal_semaphore(&info)
            },
            None => unreachable!(),
        };
        let result = result.map_err(crate::vulkan::map_host_device_oom_and_lost_err);
        if result.is_ok() {
            self.armed_gates
                .borrow_mut()
                .retain(|armed| *armed != fence);
        }
        result
    }
}

struct ImageWork {
    parts: [(Lane, usize, usize); 3],
    output: usize,
    pitch: u64,
    fence: usize,
}
const PIXELS: [u8; 8] = [31, 79, 203, 255, 251, 127, 3, 255];
const RENDERED_PIXELS: [u8; 8] = [31, 79, 203, 255, 255, 0, 0, 255];

fn image_work(data: &mut Resources) -> ImageWork {
    let native = data
        .compute
        .device
        .as_any()
        .downcast_ref::<crate::vulkan::Device>()
        .unwrap();
    let properties = unsafe {
        native
            .shared
            .instance
            .raw
            .get_physical_device_properties(native.shared.physical_device)
    };
    let alignment = properties
        .limits
        .optimal_buffer_copy_row_pitch_alignment
        .max(4);
    let pitch = 8u64.div_ceil(alignment) * alignment;
    let input = data.buffer(
        pitch,
        wgt::BufferUses::MAP_WRITE | wgt::BufferUses::COPY_SRC,
    );
    let output = data.buffer(pitch, wgt::BufferUses::COPY_DST | wgt::BufferUses::MAP_READ);
    unsafe {
        let mapping = data
            .compute
            .device
            .map_buffer(&*data.buffers[input], 0..pitch)
            .unwrap();
        core::ptr::copy_nonoverlapping(PIXELS.as_ptr(), mapping.ptr.as_ptr(), PIXELS.len());
        if !mapping.is_coherent {
            data.compute
                .device
                .flush_mapped_ranges(&*data.buffers[input], &[0..pitch]);
        }
        data.compute.device.unmap_buffer(&*data.buffers[input]);
    }
    let size = wgt::Extent3d {
        width: 2,
        height: 1,
        depth_or_array_layers: 1,
    };
    let usage =
        wgt::TextureUses::COPY_DST | wgt::TextureUses::COLOR_TARGET | wgt::TextureUses::COPY_SRC;
    let texture = unsafe {
        data.compute
            .device
            .create_texture(&crate::TextureDescriptor {
                label: None,
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgt::TextureDimension::D2,
                format: wgt::TextureFormat::Rgba8Unorm,
                usage,
                memory_flags: crate::MemoryFlags::empty(),
                view_formats: Vec::new(),
            })
    }
    .unwrap();
    let texture_index = data.textures.len();
    data.textures.push(texture);
    let range = wgt::ImageSubresourceRange {
        aspect: wgt::TextureAspect::All,
        base_mip_level: 0,
        mip_level_count: Some(1),
        base_array_layer: 0,
        array_layer_count: Some(1),
    };
    let view = unsafe {
        data.compute.device.create_texture_view(
            &*data.textures[texture_index],
            &crate::TextureViewDescriptor {
                label: None,
                format: wgt::TextureFormat::Rgba8Unorm,
                dimension: wgt::TextureViewDimension::D2,
                usage: wgt::TextureUses::COLOR_TARGET,
                range,
                swizzle: Default::default(),
            },
        )
    }
    .unwrap();
    let view_index = data.views.len();
    data.views.push(view);
    let compute = data.encoder(Lane::Compute);
    let graphics = data.encoder(Lane::Graphics);
    let copy = crate::BufferTextureCopy {
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
        size: size.into(),
    };
    data.begin(compute);
    unsafe {
        let encoder = &mut data.encoders[compute].raw;
        encoder.transition_buffers(&[crate::BufferBarrier {
            buffer: &*data.buffers[input],
            usage: crate::StateTransition {
                from: wgt::BufferUses::MAP_WRITE,
                to: wgt::BufferUses::COPY_SRC,
            },
        }]);
        encoder.transition_textures(&[crate::TextureBarrier {
            texture: &*data.textures[texture_index],
            range,
            usage: crate::StateTransition {
                from: wgt::TextureUses::UNINITIALIZED,
                to: wgt::TextureUses::COPY_DST,
            },
            queue_family_ownership_transfer: None,
        }]);
        encoder.copy_buffer_to_texture(
            &*data.buffers[input],
            &*data.textures[texture_index],
            &[copy.clone()],
        );
    }
    let upload = data.end(compute);
    data.begin(graphics);
    unsafe {
        let encoder = &mut data.encoders[graphics].raw;
        encoder.transition_textures(&[crate::TextureBarrier {
            texture: &*data.textures[texture_index],
            range,
            usage: crate::StateTransition {
                from: wgt::TextureUses::COPY_DST,
                to: wgt::TextureUses::COLOR_TARGET,
            },
            queue_family_ownership_transfer: None,
        }]);
        encoder
            .begin_render_pass(&crate::RenderPassDescriptor {
                label: None,
                extent: size,
                sample_count: 1,
                color_attachments: &[Some(crate::ColorAttachment {
                    target: crate::Attachment {
                        view: &*data.views[view_index],
                        usage: wgt::TextureUses::COLOR_TARGET,
                    },
                    depth_slice: None,
                    resolve_target: None,
                    ops: crate::AttachmentOps::LOAD | crate::AttachmentOps::STORE,
                    clear_value: wgt::Color::BLACK,
                })],
                depth_stencil_attachment: None,
                multiview_mask: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            })
            .unwrap();
        let native = encoder
            .as_any_mut()
            .downcast_mut::<crate::vulkan::CommandEncoder>()
            .unwrap();
        native.device.raw.cmd_clear_attachments(
            native.active,
            &[vk::ClearAttachment {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                color_attachment: 0,
                clear_value: vk::ClearValue {
                    color: vk::ClearColorValue {
                        float32: [1.0, 0.0, 0.0, 1.0],
                    },
                },
            }],
            &[vk::ClearRect {
                rect: vk::Rect2D {
                    offset: vk::Offset2D { x: 1, y: 0 },
                    extent: vk::Extent2D {
                        width: 1,
                        height: 1,
                    },
                },
                base_array_layer: 0,
                layer_count: 1,
            }],
        );
        encoder.end_render_pass();
    }
    let render = data.end(graphics);
    data.begin(compute);
    unsafe {
        let encoder = &mut data.encoders[compute].raw;
        encoder.transition_textures(&[crate::TextureBarrier {
            texture: &*data.textures[texture_index],
            range,
            usage: crate::StateTransition {
                from: wgt::TextureUses::COLOR_TARGET,
                to: wgt::TextureUses::COPY_SRC,
            },
            queue_family_ownership_transfer: None,
        }]);
        encoder.transition_buffers(&[crate::BufferBarrier {
            buffer: &*data.buffers[output],
            usage: crate::StateTransition {
                from: wgt::BufferUses::empty(),
                to: wgt::BufferUses::COPY_DST,
            },
        }]);
        encoder.copy_texture_to_buffer(
            &*data.textures[texture_index],
            wgt::TextureUses::COPY_SRC,
            &*data.buffers[output],
            &[copy],
        );
        encoder.transition_buffers(&[crate::BufferBarrier {
            buffer: &*data.buffers[output],
            usage: crate::StateTransition {
                from: wgt::BufferUses::COPY_DST,
                to: wgt::BufferUses::MAP_READ,
            },
        }]);
    }
    let readback = data.end(compute);
    let fence = data.fence();
    ImageWork {
        parts: [
            (Lane::Compute, compute, upload),
            (Lane::Graphics, graphics, render),
            (Lane::Compute, compute, readback),
        ],
        output,
        pitch,
        fence,
    }
}

fn read_pixels(data: &Resources, image: &ImageWork) -> Vec<u8> {
    unsafe {
        let buffer = &*data.buffers[image.output];
        let mapping = data
            .compute
            .device
            .map_buffer(buffer, 0..image.pitch)
            .unwrap();
        if !mapping.is_coherent {
            data.compute
                .device
                .invalidate_mapped_ranges(buffer, &[0..image.pitch]);
        }
        let pixels = core::slice::from_raw_parts(mapping.ptr.as_ptr(), PIXELS.len()).to_vec();
        data.compute.device.unmap_buffer(buffer);
        pixels
    }
}

#[test]
#[ignore = "Requires Vulkan timeline semaphores and the Khronos validation layer"]
fn ordered_compute_lane_graphics_compute_lane_preserves_pixels() {
    let mut work = Work::new();
    let image = image_work(&mut work);
    let result = work.ordered(&image.parts, image.fence, 1);
    let submitted = work.retain(result);
    let retired = work.retire();
    retired.unwrap();
    assert!(submitted.is_ok(), "{submitted:?}");
    assert_eq!(work.receipts[0].poll(), Ok(true));
    assert_eq!(
        unsafe {
            work.compute
                .device
                .get_fence_value(&*work.fences[image.fence])
        }
        .unwrap(),
        1
    );
    assert_eq!(read_pixels(&work, &image), RENDERED_PIXELS);
    drop(work);
    assert_validation_clean();
}

#[test]
#[ignore = "Requires Vulkan timeline semaphores and the Khronos validation layer"]
fn ordered_terminal_graphics_precedes_later_direct_compute() {
    let mut work = Work::new();
    let image = image_work(&mut work);
    let gate = work.gate_token();
    work.add_gate(&work.graphics, gate);
    let result = work.ordered(&image.parts[..2], image.fence, 1);
    let submitted = work.retain(result);
    let (_, encoder, buffer) = image.parts[2];
    let later = unsafe {
        work.compute.queue.submit(
            &[&*work.encoders[encoder].buffers[buffer]],
            &[],
            (&*work.fences[image.fence], 2),
        )
    };
    let before_gate = unsafe {
        work.compute.device.wait(
            &*work.fences[image.fence],
            2,
            Some(std::time::Duration::from_millis(100)),
        )
    };
    if let Err(error) = work.signal_gate(gate) {
        core::mem::forget(work);
        panic!("Could not release native test gate: {error:?}");
    }
    let retired = work.retire();
    retired.unwrap();
    assert!(submitted.is_ok(), "{submitted:?}");
    assert!(later.is_ok(), "{later:?}");
    assert_eq!(before_gate, Ok(false));
    assert_eq!(read_pixels(&work, &image), RENDERED_PIXELS);
    drop(work);
    assert_validation_clean();
}

#[test]
#[ignore = "Requires Vulkan timeline semaphores and the Khronos validation layer"]
fn scoped_compute_idle_does_not_wait_for_later_other_logical_queue() {
    let mut work = Work::new();
    let encoder = work.encoder(Lane::Compute);
    work.begin(encoder);
    let buffer = work.end(encoder);
    let fence = work.fence();
    let submitted = unsafe {
        work.compute.queue.submit(
            &[&*work.encoders[encoder].buffers[buffer]],
            &[],
            (&*work.fences[fence], 1),
        )
    };
    let initial_idle = unsafe { work.compute.queue.wait_for_idle() };
    if submitted.is_err() || initial_idle.is_err() {
        work.retire().unwrap();
        panic!("Initial own work failed: submit={submitted:?} idle={initial_idle:?}");
    }
    let retired_caller_fence = work.fences.swap_remove(fence);
    unsafe { work.compute.device.destroy_fence(retired_caller_fence) };
    let other = work.other_compute();
    let other_encoder = work.encoder_for(&other);
    work.begin(other_encoder);
    let other_buffer = work.end(other_encoder);
    let other_fence = work.fence();
    let gate = work.gate_token();
    work.add_gate(&other, gate);
    let later = work.scoped(&other, other_encoder, other_buffer, other_fence, 1);
    let later = work.retain(later);
    let endpoint = work.compute.clone();
    let (sender, receiver) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = unsafe { endpoint.queue.wait_for_idle() };
        let _ = sender.send(result.clone());
        result
    });
    let early = receiver.recv_timeout(std::time::Duration::from_secs(1));
    if let Err(error) = work.signal_gate(gate) {
        core::mem::forget((work, worker, other));
        panic!("Could not release unrelated native work: {error:?}");
    }
    let joined = worker.join();
    let retired = work.retire();
    retired.unwrap();
    assert!(later.is_ok(), "{later:?}");
    assert!(matches!(early, Ok(Ok(()))), "{early:?}");
    assert!(matches!(joined, Ok(Ok(()))), "{joined:?}");
    drop(other);
    drop(work);
    assert_validation_clean();
}

#[test]
#[ignore = "Requires Vulkan timeline semaphores and the Khronos validation layer"]
fn ordered_partial_and_uncertain_acceptance_retain_error_receipts() {
    for (after_native, index) in [(false, 1), (false, 2), (true, 0), (true, 1), (true, 2)] {
        let mut work = Work::new();
        let image = image_work(&mut work);
        if after_native {
            super::super::submit::inject_after_native(index);
        } else {
            super::super::submit::inject_failure(index);
        }
        let result = work.ordered(&image.parts, image.fence, 1);
        let submitted = work.retain(result);
        let receipt_poll = work.receipts.first().map(|receipt| receipt.poll());
        let rejected = unsafe {
            work.compute
                .queue
                .submit(&[], &[], (&*work.fences[image.fence], 2))
        };
        let retired = work.retire();
        retired.unwrap();
        assert_eq!(submitted, Err(crate::DeviceError::OutOfMemory));
        assert_eq!(receipt_poll, Some(Err(crate::DeviceError::Unexpected)));
        assert_eq!(rejected, Err(crate::DeviceError::Unexpected));
        let terminal = unsafe {
            work.compute
                .device
                .get_fence_value(&*work.fences[image.fence])
        }
        .unwrap();
        assert_eq!(terminal, u64::from(after_native && index == 2));
        let queue = work
            .compute
            .queue
            .as_any()
            .downcast_ref::<crate::vulkan::Queue>()
            .unwrap();
        assert_eq!(
            queue.progress.as_ref().unwrap().accepted(),
            u64::from(index != 0)
        );
        let graphics = work
            .graphics
            .queue
            .as_any()
            .downcast_ref::<crate::vulkan::Queue>()
            .unwrap();
        assert_eq!(
            graphics.progress.as_ref().unwrap().accepted(),
            u64::from(index == 2)
        );
        if after_native {
            let progress = if index == 1 {
                graphics.progress.as_ref().unwrap()
            } else {
                queue.progress.as_ref().unwrap()
            };
            let actual = if index == 2 { 2 } else { 1 };
            assert_eq!(
                progress.wait(actual, Some(std::time::Duration::ZERO)),
                Ok(true)
            );
        }
        drop(work);
        assert_validation_clean();
    }
}

#[test]
#[ignore = "Requires Vulkan timeline semaphores and the Khronos validation layer"]
fn ordered_preflight_rejects_foreign_buffers_and_other_logical_compute_queue() {
    let mut work = Work::new();
    let mut foreign = Work::new();
    let encoder = foreign.encoder(Lane::Compute);
    foreign.begin(encoder);
    let buffer = foreign.end(encoder);
    let fence = work.fence();
    let other = work.other_compute();
    let result = unsafe {
        work.context.submit_ordered(
            &*work.compute.queue,
            &[SharedSubmitSegment {
                lane: Lane::Compute,
                queue: &*work.compute.queue,
                command_buffers: &[&*foreign.encoders[encoder].buffers[buffer]],
                synchronization: None,
            }],
            (&*work.fences[fence], 1),
        )
    };
    let foreign_receipt = match &result {
        Ok(_) => true,
        Err(error) => error.receipt.is_some(),
    };
    let foreign_error = work.retain(result);
    let result_other = unsafe {
        work.context.submit_ordered(
            &*work.compute.queue,
            &[SharedSubmitSegment {
                lane: Lane::Compute,
                queue: &*other.queue,
                command_buffers: &[],
                synchronization: None,
            }],
            (&*work.fences[fence], 1),
        )
    };
    let other_receipt = match &result_other {
        Ok(_) => true,
        Err(error) => error.receipt.is_some(),
    };
    let other_error = work.retain(result_other);
    let result_empty = work.ordered(&[], fence, 1);
    let submitted = work.retain(result_empty);
    work.retire().unwrap();
    foreign.retire().unwrap();
    assert_eq!(foreign_error, Err(crate::DeviceError::Unexpected));
    assert_eq!(other_error, Err(crate::DeviceError::Unexpected));
    assert!(!foreign_receipt && !other_receipt);
    assert!(submitted.is_ok(), "{submitted:?}");
    drop((other, work, foreign));
    assert_validation_clean();
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn shared_factory_rejects_disabled_native_timeline_capability() {
    let instance = native_instance();
    let mut adapter = unsafe { instance.enumerate_adapters(None) }
        .into_iter()
        .next()
        .unwrap();
    adapter.adapter.private_caps.timeline_semaphores = false;
    let result = unsafe {
        crate::shared::open_shared_device(
            &adapter.adapter,
            wgt::Features::empty(),
            &adapter.capabilities.limits,
            &wgt::MemoryHints::default(),
        )
    };
    assert!(matches!(
        result,
        Err(SharedDeviceError::TimelineSemaphoresUnavailable)
    ));
    drop((adapter, instance));
    assert_validation_clean();
}

#[test]
#[ignore = "Requires Vulkan timeline semaphores and the Khronos validation layer"]
fn graphics_idle_rechecks_uncertainty_after_waiting_for_host_lock() {
    let mut work = Work::new();
    let other = work.other_compute();
    let encoder = work.encoder_for(&other);
    work.begin(encoder);
    let buffer = work.end(encoder);
    let fence = work.fence();
    let gate = work.gate_token();
    work.add_gate(&other, gate);
    let submitted = work.scoped(&other, encoder, buffer, fence, 1);
    let submitted = work.retain(submitted);
    let native = work
        .graphics
        .queue
        .as_any()
        .downcast_ref::<crate::vulkan::Queue>()
        .unwrap();
    let group = native.device.shared_queue_group.as_ref().unwrap().clone();
    let host = group.host[0].lock();
    let endpoint = work.graphics.clone();
    let (sender, receiver) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = unsafe { endpoint.queue.wait_for_idle() };
        let _ = sender.send(result.clone());
        result
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while group.graphics_idle_waiters.load(Ordering::SeqCst) != 1 {
        assert!(
            std::time::Instant::now() < deadline,
            "Graphics idle did not reach host lock"
        );
        std::thread::yield_now();
    }
    group.close_uncertain();
    drop(host);
    let early = receiver.recv_timeout(std::time::Duration::from_millis(100));
    if let Err(error) = work.signal_gate(gate) {
        core::mem::forget((work, worker, other));
        panic!("Could not release uncertainty test gate: {error:?}");
    }
    let joined = worker.join();
    work.retire().unwrap();
    assert!(submitted.is_ok(), "{submitted:?}");
    assert!(
        matches!(early, Err(std::sync::mpsc::RecvTimeoutError::Timeout)),
        "{early:?}"
    );
    assert!(matches!(joined, Ok(Ok(()))), "{joined:?}");
    drop((other, work));
    assert_validation_clean();
}

#[test]
#[ignore = "Requires Vulkan timeline semaphores and the Khronos validation layer"]
fn aggregate_receipt_uses_batch_targets_without_waiting_for_later_graphics() {
    let mut work = Work::new();
    let image = image_work(&mut work);
    let result = work.ordered(&image.parts, image.fence, 1);
    let submitted = work.retain(result);
    let completed = work.receipts.first().map(|receipt| receipt.wait(None));
    if submitted.is_err() || completed != Some(Ok(true)) {
        work.retire().unwrap();
        panic!("Initial batch failed: submit={submitted:?}, wait={completed:?}");
    }
    let encoder = work.encoder(Lane::Graphics);
    work.begin(encoder);
    let buffer = work.end(encoder);
    let fence = work.fence();
    let gate = work.gate_token();
    work.add_gate(&work.graphics, gate);
    let later = work.scoped(&work.graphics, encoder, buffer, fence, 1);
    let later = work.retain(later);
    let original_complete = work.receipts[0].poll();
    let later_complete = unsafe {
        work.compute
            .device
            .wait(&*work.fences[fence], 1, Some(std::time::Duration::ZERO))
    };
    if let Err(error) = work.signal_gate(gate) {
        core::mem::forget(work);
        panic!("Could not release later graphics work: {error:?}");
    }
    work.retire().unwrap();
    assert!(later.is_ok(), "{later:?}");
    assert_eq!(original_complete, Ok(true));
    assert_eq!(later_complete, Ok(false));
    drop(work);
    assert_validation_clean();
}

#[cfg(unix)]
#[path = "sync_tests.rs"]
mod sync_tests;

#[test]
#[ignore = "Requires Vulkan timeline semaphores and the Khronos validation layer"]
fn ordered_empty_graphics_tails_preserve_logical_timeline_order() {
    let mut work = Work::new();
    const ITERATIONS: usize = 128;
    const SIZE: u64 = 4 * 1024 * 1024;
    let resource = work.buffer(SIZE, wgt::BufferUses::COPY_DST);
    let encoder = work.encoder(Lane::Compute);
    let fence = work.fence();
    let mut value = 0;
    for iteration in 0..ITERATIONS {
        work.begin(encoder);
        unsafe {
            let data = &mut *work.resources;
            let commands = &mut data.encoders[encoder].raw;
            commands.transition_buffers(&[crate::BufferBarrier {
                buffer: &*data.buffers[resource],
                usage: crate::StateTransition {
                    from: if iteration == 0 {
                        wgt::BufferUses::empty()
                    } else {
                        wgt::BufferUses::COPY_DST
                    },
                    to: wgt::BufferUses::COPY_DST,
                },
            }]);
            commands.clear_buffer(&*data.buffers[resource], 0..SIZE);
        }
        let buffer = work.end(encoder);
        value += 1;
        let result = unsafe {
            work.context.submit_ordered(
                &*work.compute.queue,
                &[
                    SharedSubmitSegment {
                        lane: Lane::Compute,
                        queue: &*work.compute.queue,
                        command_buffers: &[&*work.encoders[encoder].buffers[buffer]],
                        synchronization: None,
                    },
                    SharedSubmitSegment {
                        lane: Lane::Graphics,
                        queue: &*work.graphics.queue,
                        command_buffers: &[],
                        synchronization: None,
                    },
                ],
                (&*work.fences[fence], value),
            )
        };
        work.retain(result).unwrap();
        value += 1;
        unsafe {
            work.compute
                .queue
                .submit(&[], &[], (&*work.fences[fence], value))
        }
        .unwrap();
        if iteration % 8 == 0 {
            value += 1;
            let result = unsafe {
                work.context.submit_ordered(
                    &*work.compute.queue,
                    &[SharedSubmitSegment {
                        lane: Lane::Graphics,
                        queue: &*work.graphics.queue,
                        command_buffers: &[],
                        synchronization: None,
                    }],
                    (&*work.fences[fence], value),
                )
            };
            work.retain(result).unwrap();
        }
    }
    work.retire().unwrap();
    assert_eq!(work.receipts.len(), ITERATIONS + ITERATIONS / 8);
    for receipt in &work.receipts {
        assert_eq!(receipt.poll(), Ok(true));
    }
    assert_eq!(
        unsafe { work.compute.device.get_fence_value(&*work.fences[fence]) }.unwrap(),
        value
    );
    drop(work);
    assert_validation_clean();
}
