/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use api::units::{DeviceIntPoint, DeviceIntRect, DeviceIntSize};
use std::{
    borrow::Cow,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use wgpu_core as core;
use hal::GraphicsRecorderFactory as _;

struct GraphicsProvider {
    graphics: Arc<hal::DynOpenDevice>,
    creations: AtomicUsize,
}

impl fmt::Debug for GraphicsProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RealWRGraphicsProvider")
            .finish_non_exhaustive()
    }
}

impl core::graphics::GraphicsRecordingProvider for GraphicsProvider {
    unsafe fn create_encoder(
        &self,
        _device: &dyn hal::DynDevice,
        native: Box<dyn hal::DynCommandEncoder>,
    ) -> Result<Box<dyn hal::DynCommandEncoder>, hal::DeviceError> {
        self.creations.fetch_add(1, Ordering::SeqCst);
        unsafe { GraphicsRecorderFactory.create(self.graphics.clone()) }.map(|graphics| {
            Box::new(wgpu_graphics::RoutedEncoder::new(native, graphics))
                as Box<dyn hal::DynCommandEncoder>
        })
    }
}

fn webgpu_triangle(device: &Arc<core::device::Device>) -> Arc<core::command::CommandBuffer> {
    use core::{command, pipeline};

    device.push_error_scope(wgt::error::ErrorFilter::Validation);
    let shader = device.create_shader_module(
        &pipeline::ShaderModuleDescriptor {
            label: None,
            runtime_checks: Default::default(),
        },
        pipeline::ShaderModuleSource::Wgsl(Cow::Borrowed(
            "@vertex fn vs(@builtin(vertex_index) id: u32) -> @builtin(position) vec4f {
                let x = f32((id << 1u) & 2u) * 2.0 - 1.0;
                let y = f32(id & 2u) * 2.0 - 1.0;
                return vec4f(x, y, 0.0, 1.0);
            }
            @fragment fn fs() -> @location(0) vec4f {
                return vec4f(0.0, 1.0, 0.0, 1.0);
            }",
        )),
    );
    let stage = |entry_point: &'static str| pipeline::ProgrammableStageDescriptor {
        module: shader.clone(),
        entry_point: Some(Cow::Borrowed(entry_point)),
        constants: Default::default(),
        zero_initialize_workgroup_memory: true,
    };
    let pipeline = device.create_render_pipeline(
        pipeline::RenderPipelineDescriptor {
            label: None,
            layout: None,
            vertex: pipeline::VertexState {
                stage: stage("vs"),
                buffers: Cow::Borrowed(&[]),
            },
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(pipeline::FragmentState {
                stage: stage("fs"),
                targets: Cow::Owned(vec![Some(wgt::ColorTargetState {
                    format: wgt::TextureFormat::Rgba8Unorm,
                    blend: None,
                    write_mask: wgt::ColorWrites::ALL,
                })]),
            }),
            multiview_mask: None,
            cache: None,
        }
        .into(),
    );
    let texture = device.create_texture(&core::resource::TextureDescriptor {
        label: None,
        size: wgt::Extent3d {
            width: 2,
            height: 1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgt::TextureDimension::D2,
        format: wgt::TextureFormat::Rgba8Unorm,
        usage: wgt::TextureUsages::RENDER_ATTACHMENT,
        view_formats: Vec::new(),
    });
    let view = texture.create_view(&Default::default());
    let encoder = device.create_command_encoder(&Default::default());
    let mut pass = encoder.begin_render_pass(command::ResolvedRenderPassDescriptor {
        label: None,
        color_attachments: Cow::Owned(vec![Some(command::RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            load_op: command::LoadOp::Clear(wgt::Color::BLACK),
            store_op: command::StoreOp::Store,
        })]),
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    pass.set_pipeline(pipeline);
    pass.draw(3, 1, 0, 0);
    pass.end();
    let buffer = encoder.finish(&Default::default());
    assert!(device.pop_error_scope().unwrap().is_none());
    buffer
}

#[test]
#[ignore = "Requires Vulkan, two universal queues and the Khronos validation layer"]
fn shared_endpoint_records_webgpu_and_renders_webrender() {
    tests::validation_logging();
    let flags = wgt::InstanceFlags::DEBUG
        | wgt::InstanceFlags::VALIDATION
        | wgt::InstanceFlags::STRICT_WEBGPU_COMPLIANCE
        | wgt::InstanceFlags::AUTOMATIC_TIMESTAMP_NORMALIZATION;
    let original = core::instance::Instance::new(
        "Shared WR/core proof",
        wgt::InstanceDescriptor {
            backends: wgt::Backends::VULKAN,
            flags,
            backend_options: Default::default(),
            memory_budget_thresholds: wgt::MemoryBudgetThresholds {
                for_resource_creation: Some(95),
                for_device_loss: Some(99),
            },
            display: None,
        },
        None,
    );
    let mut adapters = unsafe {
        original
            .raw(wgt::Backend::Vulkan)
            .unwrap()
            .enumerate_adapters(None)
    };
    let index = select_adapter(
        adapters
            .iter()
            .map(|adapter| (adapter.info.name.as_str(), adapter.info.device_type)),
        None,
    )
    .unwrap();
    let exposed = adapters.swap_remove(index);
    drop(adapters);
    let features = Device::shared_features(exposed.features);
    let pair = unsafe {
        hal::shared::open_shared_device(
            &*exposed.adapter,
            features,
            &exposed.capabilities.limits,
            &wgt::MemoryHints::MemoryUsage,
        )
    }
    .unwrap();
    let hal::shared::SharedDeviceEndpoints {
        context,
        graphics,
        compute: initial_compute,
    } = pair;
    drop(initial_compute);
    let graphics = Arc::new(graphics);
    let (_, wr_instance) = context.clone_instance();
    let owner = Rc::new(
        unsafe { Device::from_shared_endpoint(graphics.clone(), wr_instance, exposed) }.unwrap(),
    );
    assert!(Arc::ptr_eq(&owner.open, &graphics));
    assert!(
        hal::renderer::graphics_capabilities(
            &*owner.instance,
            &*owner.adapter,
            &*owner.open.device,
        )
        .unwrap()
        .validation_enabled,
        "Khronos validation must be active for the native proof"
    );

    let (backend, core_instance) = context.clone_instance();
    let core_instance = unsafe {
        core::instance::Instance::from_hal_instance_dynamic(
            "Private WebGPU core view".into(),
            backend,
            core_instance,
            flags,
            None,
        )
    };
    drop(original);
    let adapter = core_instance
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
        graphics: graphics.clone(),
        creations: AtomicUsize::new(0),
    });
    let (core_device, core_queue) = unsafe {
        adapter.create_device_and_queue_from_hal_with_graphics(
            compute,
            &desc,
            Some(provider.clone()),
        )
    }
    .unwrap();
    let recorded = webgpu_triangle(&core_device);
    assert!(provider.creations.load(Ordering::SeqCst) >= 2);

    let target = Texture::new(
        &owner,
        2,
        1,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Nearest,
        true,
    )
    .unwrap();
    let alignment = owner
        .capabilities()
        .alignments
        .buffer_copy_pitch
        .get()
        .max(4);
    let pitch = 8u64.div_ceil(alignment) * alignment;
    let (buffer, _) = unsafe {
        owner.open.device.create_buffer(&hal::BufferDescriptor {
            label: Some("shared renderer proof readback"),
            size: pitch,
            usage: wgt::BufferUses::COPY_DST | wgt::BufferUses::MAP_READ,
            memory_flags: hal::MemoryFlags::PREFER_COHERENT,
        })
    }
    .unwrap();
    let readback = Rc::new(resources::Owned::new(
        &owner,
        buffer,
        <dyn hal::DynDevice>::destroy_buffer,
    ));
    let mut submission = Submission::new(&owner).unwrap();
    let pass = draw::DrawPass {
        target: &target,
        origin: DeviceIntPoint::zero(),
        viewport: None,
        depth: None,
        clear_color: None,
        clear_depth: None,
        depth_range: 0.0..1.0,
    };
    let full = DeviceIntRect::from_size(DeviceIntSize::new(2, 1));
    {
        let mut commands = submission.recording().unwrap();
        pass.clear_rect(&mut commands, full, Some([0.0, 0.0, 0.0, 1.0]), None)
            .unwrap();
        pass.clear_rect(
            &mut commands,
            DeviceIntRect::from_size(DeviceIntSize::new(1, 1)),
            Some([1.0, 0.0, 0.0, 1.0]),
            None,
        )
        .unwrap();
        commands.keep(&readback);
        target
            .transition(&mut commands, wgt::TextureUses::COPY_SRC)
            .unwrap();
        unsafe {
            let encoder = commands.encoder();
            encoder.transition_buffers(&[hal::BufferBarrier {
                buffer: &**readback,
                usage: hal::StateTransition {
                    from: wgt::BufferUses::empty(),
                    to: wgt::BufferUses::COPY_DST,
                },
            }]);
            encoder.copy_texture_to_buffer(
                target.raw_texture(),
                wgt::TextureUses::COPY_SRC,
                &**readback,
                &[hal::BufferTextureCopy {
                    buffer_layout: wgt::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(pitch as u32),
                        rows_per_image: None,
                    },
                    texture_base: hal::TextureCopyBase {
                        mip_level: 0,
                        array_layer: 0,
                        origin: wgt::Origin3d::ZERO,
                        aspect: hal::FormatAspects::COLOR,
                    },
                    size: wgt::Extent3d {
                        width: 2,
                        height: 1,
                        depth_or_array_layers: 1,
                    }
                    .into(),
                }],
            );
            encoder.transition_buffers(&[hal::BufferBarrier {
                buffer: &**readback,
                usage: hal::StateTransition {
                    from: wgt::BufferUses::COPY_DST,
                    to: wgt::BufferUses::MAP_READ,
                },
            }]);
        }
    }
    let submitted = submission.submit();
    let retired = context.drain();
    if submitted.is_err() || retired.is_err() {
        let failure = format!("Shared proof submit={submitted:?}, drain={retired:?}");
        std::mem::forget((
            context,
            graphics,
            owner,
            target,
            submission,
            readback,
            recorded,
            core_queue,
            core_device,
            adapter,
            core_instance,
            provider,
        ));
        panic!("{failure}");
    }
    let pixels = unsafe {
        let mapping = owner.open.device.map_buffer(&**readback, 0..pitch).unwrap();
        if !mapping.is_coherent {
            owner
                .open
                .device
                .invalidate_mapped_ranges(&**readback, &[0..pitch]);
        }
        let pixels = std::slice::from_raw_parts(mapping.ptr.as_ptr(), 8).to_vec();
        owner.open.device.unmap_buffer(&**readback);
        pixels
    };
    drop(recorded);
    drop((core_queue, core_device, adapter, core_instance, provider));
    drop((submission, readback, target, owner, graphics, context));
    assert_eq!(pixels, [255, 0, 0, 255, 0, 0, 0, 255]);
    assert_eq!(tests::ERRORS.load(Ordering::Relaxed), 0);
}
