/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::{
    borrow::Cow,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use wgpu_core as core;
use wgpu_types as wgt;

static VALIDATION_ERRORS: AtomicUsize = AtomicUsize::new(0);
struct Logger;
impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info
    }
    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            if record.level() == log::Level::Error {
                VALIDATION_ERRORS.fetch_add(1, Ordering::Relaxed);
            }
            eprintln!("{}: {}", record.target(), record.args());
        }
    }
    fn flush(&self) {}
}

struct Fixture {
    _instance: Arc<core::instance::Instance>,
    device: Arc<core::device::Device>,
    queue: Arc<core::device::queue::Queue>,
    pipeline: Arc<core::pipeline::RenderPipeline>,
    target: Arc<core::resource::Texture>,
}

impl Fixture {
    fn new() -> Self {
        static START: std::sync::Once = std::sync::Once::new();
        START.call_once(|| {
            log::set_logger(&Logger).unwrap();
            log::set_max_level(log::LevelFilter::Info);
        });
        let instance = core::instance::Instance::new(
            "Bundle replay regression",
            wgt::InstanceDescriptor {
                backends: wgt::Backends::VULKAN,
                flags: wgt::InstanceFlags::DEBUG | wgt::InstanceFlags::VALIDATION,
                ..wgt::InstanceDescriptor::new_without_display_handle()
            },
            None,
        );
        let adapter = instance
            .enumerate_adapters(wgt::Backends::VULKAN, false)
            .into_iter()
            .next()
            .unwrap();
        eprintln!("Bundle replay adapter: {:?}", adapter.get_info());
        let (device, queue) = adapter
            .request_device(&core::device::DeviceDescriptor {
                required_limits: adapter.limits(),
                ..Default::default()
            })
            .unwrap();
        device.on_uncaptured_error(Arc::new(|e| panic!("Unexpected core error: {e:?}")));
        let shader = device.create_shader_module(
            &core::pipeline::ShaderModuleDescriptor {
                label: None,
                runtime_checks: Default::default(),
            },
            core::pipeline::ShaderModuleSource::Wgsl(Cow::Borrowed(
                r#"
            @group(0) @binding(0) var<uniform> color: vec4f;
            @vertex fn vs(@builtin(vertex_index) i:u32) -> @builtin(position) vec4f {
                return vec4f(f32((i<<1u)&2u)*2.0-1.0,f32(i&2u)*2.0-1.0,0,1);
            }
            @fragment fn fs() -> @location(0) vec4f { return color; }
        "#,
            )),
        );
        let stage = |entry: &str| core::pipeline::ProgrammableStageDescriptor {
            module: shader.clone(),
            entry_point: Some(entry.to_string().into()),
            constants: Default::default(),
            zero_initialize_workgroup_memory: true,
        };
        let pipeline = device.create_render_pipeline(
            core::pipeline::RenderPipelineDescriptor {
                label: None,
                layout: None,
                vertex: core::pipeline::VertexState {
                    stage: stage("vs"),
                    buffers: Cow::Borrowed(&[]),
                },
                fragment: Some(core::pipeline::FragmentState {
                    stage: stage("fs"),
                    targets: Cow::Owned(vec![Some(wgt::ColorTargetState {
                        format: wgt::TextureFormat::Rgba8Unorm,
                        blend: None,
                        write_mask: wgt::ColorWrites::ALL,
                    })]),
                }),
                primitive: Default::default(),
                depth_stencil: None,
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            }
            .into(),
        );
        let target = device.create_texture(&core::resource::TextureDescriptor {
            label: None,
            size: wgt::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgt::TextureDimension::D2,
            format: wgt::TextureFormat::Rgba8Unorm,
            usage: wgt::TextureUsages::RENDER_ATTACHMENT | wgt::TextureUsages::COPY_SRC,
            view_formats: vec![],
        });
        Self {
            _instance: instance,
            device,
            queue,
            pipeline,
            target,
        }
    }

    fn wait(&self) {
        self.device
            .poll(wgt::PollType::Wait {
                submission_index: None,
                timeout: Some(Duration::from_secs(10)),
            })
            .unwrap();
    }

    fn buffer(&self) -> Arc<core::resource::Buffer> {
        self.device
            .create_buffer(&core::resource::BufferDescriptor {
                label: None,
                size: 512,
                usage: wgt::BufferUsages::UNIFORM | wgt::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
    }

    fn poison_allocator(&self) {
        let buffers = (0..64).map(|_| self.buffer()).collect::<Vec<_>>();
        let data = [1.0f32, 0.5, 0.25, 1.0]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .cycle()
            .take(512)
            .collect::<Vec<_>>();
        for buffer in &buffers {
            self.queue.write_buffer(buffer.clone(), 0, &data);
        }
        self.queue.submit(&[]).unwrap();
        self.wait();
        for buffer in buffers {
            buffer.destroy();
        }
        self.wait();
    }

    fn bundle(
        &self,
        buffer: Arc<core::resource::Buffer>,
        offset: u64,
    ) -> Arc<core::command::RenderBundle> {
        let group = self
            .device
            .create_bind_group(&core::binding_model::BindGroupDescriptor {
                label: None,
                layout: self.pipeline.get_bind_group_layout(0),
                entries: Cow::Owned(vec![core::binding_model::BindGroupEntry {
                    binding: 0,
                    resource: core::binding_model::BindingResource::Buffer(
                        core::binding_model::BufferBinding {
                            buffer,
                            offset,
                            size: Some(16),
                        },
                    ),
                }]),
            });
        let mut recorder = self.device.create_render_bundle_encoder(
            &core::command::RenderBundleEncoderDescriptor {
                label: None,
                color_formats: Cow::Owned(vec![Some(wgt::TextureFormat::Rgba8Unorm)]),
                depth_stencil: None,
                sample_count: 1,
                multiview: None,
            },
        );
        recorder.set_pipeline(self.pipeline.clone());
        recorder.set_bind_group(0, Some(group), &[]);
        recorder.draw(3, 1, 0, 0);
        recorder.finish(&Default::default())
    }

    fn encode(
        &self,
        bundle: Arc<core::command::RenderBundle>,
    ) -> Arc<core::command::CommandBuffer> {
        let encoder = self.device.create_command_encoder(&Default::default());
        let mut pass = encoder.begin_render_pass(core::command::ResolvedRenderPassDescriptor {
            color_attachments: Cow::Owned(vec![Some(core::command::RenderPassColorAttachment {
                view: self.target.create_view(&Default::default()),
                depth_slice: None,
                resolve_target: None,
                load_op: wgt::LoadOp::Clear(wgt::Color::WHITE),
                store_op: wgt::StoreOp::Store,
            })]),
            ..Default::default()
        });
        pass.execute_bundles(&[bundle]);
        pass.end();
        encoder.finish(&Default::default())
    }

    fn draw(&self, bundle: Arc<core::command::RenderBundle>) -> [u8; 4] {
        self.queue.submit(&[self.encode(bundle)]).unwrap();
        self.wait();
        let buffer = self
            .device
            .create_buffer(&core::resource::BufferDescriptor {
                label: None,
                size: 256,
                usage: wgt::BufferUsages::MAP_READ | wgt::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        let encoder = self.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            &wgt::TexelCopyTextureInfo {
                texture: self.target.clone(),
                mip_level: 0,
                origin: wgt::Origin3d::ZERO,
                aspect: wgt::TextureAspect::All,
            },
            &wgt::TexelCopyBufferInfo {
                buffer: buffer.clone(),
                layout: wgt::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(256),
                    rows_per_image: Some(1),
                },
            },
            &wgt::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        self.queue
            .submit(&[encoder.finish(&Default::default())])
            .unwrap();
        self.wait();
        let (send, receive) = std::sync::mpsc::channel();
        buffer.map_async(
            wgt::MapMode::Read,
            0,
            Some(256),
            Some(Box::new(move |result| send.send(result).unwrap())),
        );
        self.wait();
        receive
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        let view = buffer.get_mapped_range(0, Some(256)).unwrap();
        let bytes = view.read_slice();
        let pixel = [bytes[0], bytes[1], bytes[2], bytes[3]];
        drop(bytes);
        drop(view);
        buffer.unmap();
        pixel
    }
}

#[test]
#[ignore = "Requires hardware Vulkan with synchronization validation; run serially"]
fn dropped_recording_does_not_initialize_buffers() {
    let f = Fixture::new();
    f.poison_allocator();
    let buffer = f.buffer();
    let bundle = f.bundle(buffer.clone(), 0);
    drop(f.encode(bundle.clone()));
    assert_eq!(f.draw(bundle.clone()), [0, 0, 0, 0]);
    let green = [0.0f32, 1.0, 0.0, 1.0]
        .into_iter()
        .flat_map(f32::to_le_bytes)
        .collect::<Vec<_>>();
    f.queue.write_buffer(buffer, 0, &green);
    for _ in 0..3 {
        assert_eq!(f.draw(bundle.clone()), [0, 255, 0, 255]);
    }
    drop(bundle);
    drop(f);
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires hardware Vulkan with synchronization validation; run serially"]
fn partial_initialization_and_later_writes_preserve_bundle_ranges() {
    let f = Fixture::new();
    f.poison_allocator();
    let buffer = f.buffer();
    let bundle = f.bundle(buffer.clone(), 256);
    f.queue
        .write_buffer(buffer.clone(), 0, &1.0f32.to_le_bytes());
    assert_eq!(f.draw(bundle.clone()), [0, 0, 0, 0]);
    assert_eq!(f.draw(bundle.clone()), [0, 0, 0, 0]);
    let green = [0.0f32, 1.0, 0.0, 1.0]
        .into_iter()
        .flat_map(f32::to_le_bytes)
        .collect::<Vec<_>>();
    f.queue.write_buffer(buffer, 256, &green);
    assert_eq!(f.draw(bundle), [0, 255, 0, 255]);
    drop(f);
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires hardware Vulkan with synchronization validation; diagnostic"]
fn overwritten_binding_keeps_queue_submission_validation() {
    let f = Fixture::new();
    let dead = f.buffer();
    let live = f.buffer();
    let green = [0.0f32, 1.0, 0.0, 1.0]
        .into_iter()
        .flat_map(f32::to_le_bytes)
        .collect::<Vec<_>>();
    f.queue.write_buffer(live.clone(), 0, &green);
    let group = |buffer| {
        f.device
            .create_bind_group(&core::binding_model::BindGroupDescriptor {
                label: None,
                layout: f.pipeline.get_bind_group_layout(0),
                entries: Cow::Owned(vec![core::binding_model::BindGroupEntry {
                    binding: 0,
                    resource: core::binding_model::BindingResource::Buffer(
                        core::binding_model::BufferBinding {
                            buffer,
                            offset: 0,
                            size: Some(16),
                        },
                    ),
                }]),
            })
    };
    let mut recorder =
        f.device
            .create_render_bundle_encoder(&core::command::RenderBundleEncoderDescriptor {
                label: None,
                color_formats: Cow::Owned(vec![Some(wgt::TextureFormat::Rgba8Unorm)]),
                depth_stencil: None,
                sample_count: 1,
                multiview: None,
            });
    recorder.set_pipeline(f.pipeline.clone());
    recorder.set_bind_group(0, Some(group(dead.clone())), &[]);
    recorder.set_bind_group(0, Some(group(live)), &[]);
    recorder.draw(3, 1, 0, 0);
    let bundle = recorder.finish(&Default::default());
    assert_eq!(f.draw(bundle.clone()), [0, 255, 0, 255]);
    dead.destroy();
    f.device
        .push_error_scope(wgt::error::ErrorFilter::Validation);
    let submitted = f.queue.submit(&[f.encode(bundle.clone())]);
    let error = f.device.pop_error_scope().unwrap();
    assert!(submitted.is_none());
    let context = match error.expect("Expected destroyed resource validation") {
        wgt::error::Error::Validation { source, .. } => {
            source.downcast::<core::error::ContextError>().unwrap()
        }
        error => panic!("Expected validation error: {error:?}"),
    };
    assert_eq!(context.fn_ident, "Queue::submit");
    assert!(matches!(
        context
            .source
            .downcast_ref::<core::device::queue::QueueSubmitError>(),
        Some(core::device::queue::QueueSubmitError::DestroyedResource(_))
    ));
    assert!(f.device.is_valid());
    drop(bundle);
    drop(f);
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), 0);
}

fn validation_error(device: &Arc<core::device::Device>) -> Box<core::error::ContextError> {
    match device
        .pop_error_scope()
        .unwrap()
        .expect("Expected validation failure")
    {
        wgt::error::Error::Validation { source, .. } => {
            source.downcast::<core::error::ContextError>().unwrap()
        }
        error => panic!("Expected validation error: {error:?}"),
    }
}

#[test]
#[ignore = "Requires hardware Vulkan with synchronization validation; run serially"]
fn destroyed_buffers_are_rejected_before_and_after_bundle_encoding() {
    for recorded_first in [false, true] {
        let f = Fixture::new();
        let buffer = f.buffer();
        let green = [0.0f32, 1.0, 0.0, 1.0]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect::<Vec<_>>();
        let bundle = f.bundle(buffer.clone(), 0);
        f.queue.write_buffer(buffer.clone(), 0, &green);
        for _ in 0..3 {
            assert_eq!(f.draw(bundle.clone()), [0, 255, 0, 255]);
        }
        // An unrelated snatch invalidates the cached generation without invalidating this bundle.
        let unrelated = f.buffer();
        unrelated.destroy();
        assert_eq!(f.draw(bundle.clone()), [0, 255, 0, 255]);
        let recorded = recorded_first.then(|| f.encode(bundle.clone()));
        buffer.destroy();
        let command = if let Some(command) = recorded {
            command
        } else {
            f.device
                .push_error_scope(wgt::error::ErrorFilter::Validation);
            let command = f.encode(bundle.clone());
            assert!(
                f.device.pop_error_scope().unwrap().is_none(),
                "Destroyed-resource errors must remain deferred until submission"
            );
            command
        };
        f.device
            .push_error_scope(wgt::error::ErrorFilter::Validation);
        assert!(f.queue.submit(&[command]).is_none());
        let error = validation_error(&f.device);
        assert!(
            format!("{error:?}").contains("DestroyedResource"),
            "{error:?}"
        );
        assert_eq!(error.fn_ident, "Queue::submit");
        assert!(f.device.is_valid());
        drop(bundle);
        drop(f);
        assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), 0);
    }
}

#[test]
#[ignore = "Requires hardware Vulkan with synchronization validation; run serially"]
fn texture_discard_and_destruction_remain_checked_on_replay() {
    let f = Fixture::new();
    let source = f.device.create_texture(&core::resource::TextureDescriptor {
        label: None,
        size: wgt::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgt::TextureDimension::D2,
        format: wgt::TextureFormat::Rgba8Unorm,
        usage: wgt::TextureUsages::RENDER_ATTACHMENT | wgt::TextureUsages::TEXTURE_BINDING,
        view_formats: vec![],
    });
    let shader = f.device.create_shader_module(
        &core::pipeline::ShaderModuleDescriptor {
            label: None,
            runtime_checks: Default::default(),
        },
        core::pipeline::ShaderModuleSource::Wgsl(Cow::Borrowed(
            r#"
        @group(0) @binding(0) var image:texture_2d<f32>;
        @vertex fn vs(@builtin(vertex_index) i:u32)->@builtin(position) vec4f {
            return vec4f(f32((i<<1u)&2u)*2.0-1.0,f32(i&2u)*2.0-1.0,0,1);
        }
        @fragment fn fs()->@location(0) vec4f{return textureLoad(image,vec2i(0),0);}
    "#,
        )),
    );
    let stage = |entry: &str| core::pipeline::ProgrammableStageDescriptor {
        module: shader.clone(),
        entry_point: Some(entry.to_string().into()),
        constants: Default::default(),
        zero_initialize_workgroup_memory: true,
    };
    let pipeline = f.device.create_render_pipeline(
        core::pipeline::RenderPipelineDescriptor {
            label: None,
            layout: None,
            vertex: core::pipeline::VertexState {
                stage: stage("vs"),
                buffers: Cow::Borrowed(&[]),
            },
            fragment: Some(core::pipeline::FragmentState {
                stage: stage("fs"),
                targets: Cow::Owned(vec![Some(wgt::ColorTargetState {
                    format: wgt::TextureFormat::Rgba8Unorm,
                    blend: None,
                    write_mask: wgt::ColorWrites::ALL,
                })]),
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        }
        .into(),
    );
    let group = f
        .device
        .create_bind_group(&core::binding_model::BindGroupDescriptor {
            label: None,
            layout: pipeline.get_bind_group_layout(0),
            entries: Cow::Owned(vec![core::binding_model::BindGroupEntry {
                binding: 0,
                resource: core::binding_model::BindingResource::TextureView(
                    source.create_view(&Default::default()),
                ),
            }]),
        });
    let mut recorder =
        f.device
            .create_render_bundle_encoder(&core::command::RenderBundleEncoderDescriptor {
                label: None,
                color_formats: Cow::Owned(vec![Some(wgt::TextureFormat::Rgba8Unorm)]),
                depth_stencil: None,
                sample_count: 1,
                multiview: None,
            });
    recorder.set_pipeline(pipeline);
    recorder.set_bind_group(0, Some(group), &[]);
    recorder.draw(3, 1, 0, 0);
    let bundle = recorder.finish(&Default::default());
    for (store, expected) in [
        (wgt::StoreOp::Store, [255, 0, 0, 255]),
        (wgt::StoreOp::Discard, [0, 0, 0, 0]),
    ] {
        let encoder = f.device.create_command_encoder(&Default::default());
        let mut pass = encoder.begin_render_pass(core::command::ResolvedRenderPassDescriptor {
            color_attachments: Cow::Owned(vec![Some(core::command::RenderPassColorAttachment {
                view: source.create_view(&Default::default()),
                depth_slice: None,
                resolve_target: None,
                load_op: wgt::LoadOp::Clear(wgt::Color::RED),
                store_op: store,
            })]),
            ..Default::default()
        });
        pass.end();
        f.queue
            .submit(&[encoder.finish(&Default::default())])
            .unwrap();
        f.wait();
        assert_eq!(f.draw(bundle.clone()), expected);
    }
    source.destroy();
    f.device
        .push_error_scope(wgt::error::ErrorFilter::Validation);
    let command = f.encode(bundle.clone());
    assert!(
        f.device.pop_error_scope().unwrap().is_none(),
        "Destroyed-resource errors must remain deferred until submission"
    );
    f.device
        .push_error_scope(wgt::error::ErrorFilter::Validation);
    assert!(f.queue.submit(&[command]).is_none());
    let error = validation_error(&f.device);
    assert_eq!(error.fn_ident, "Queue::submit");
    assert!(
        format!("{error:?}").contains("DestroyedResource"),
        "{error:?}"
    );
    assert!(f.device.is_valid());
    drop(bundle);
    drop(source);
    drop(f);
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), 0);
}
