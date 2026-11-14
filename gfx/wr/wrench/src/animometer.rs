/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::{
    borrow::Cow,
    sync::Arc,
    time::{Duration, Instant},
};
use serde_json::{json, Value};
use wgpu_core as core;
use wgpu_types as wgt;

const FORMAT: wgt::TextureFormat = wgt::TextureFormat::Rgba8Unorm;
const SIDE: u32 = 256;

struct Scene {
    _instance: Arc<core::instance::Instance>,
    device: Arc<core::device::Device>,
    queue: Arc<core::device::queue::Queue>,
    pipeline: Arc<core::pipeline::RenderPipeline>,
    vertices: Arc<core::resource::Buffer>,
    uniforms: Arc<core::resource::Buffer>,
    time_group: Arc<core::binding_model::BindGroup>,
    groups: Vec<Arc<core::binding_model::BindGroup>>,
    texture: Arc<core::resource::Texture>,
    view: Arc<core::resource::TextureView>,
    bundle: Arc<core::command::RenderBundle>,
    triangles: u32,
    stride: u32,
    time_offset: u64,
    dynamic: bool,
    adapter: String,
    bundle_build_ns: u64,
}

fn bytes(values: impl IntoIterator<Item = f32>) -> Vec<u8> {
    values.into_iter().flat_map(f32::to_le_bytes).collect()
}

fn random(seed: &mut u32) -> f32 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 17;
    *seed ^= *seed << 5;
    (*seed >> 8) as f32 / 16_777_216.0
}

impl Scene {
    fn new(triangles: u32, dynamic: bool, validation: bool) -> Result<Self, String> {
        let instance = core::instance::Instance::new(
            "Wrench Animometer",
            wgt::InstanceDescriptor {
                backends: wgt::Backends::VULKAN,
                flags: if validation {
                    wgt::InstanceFlags::DEBUG | wgt::InstanceFlags::VALIDATION
                } else {
                    wgt::InstanceFlags::empty()
                },
                ..wgt::InstanceDescriptor::new_without_display_handle()
            },
            None,
        );
        let adapter = instance
            .enumerate_adapters(wgt::Backends::VULKAN, false)
            .into_iter()
            .next()
            .ok_or("No Vulkan adapter")?;
        let adapter_name = format!("{:?}", adapter.get_info());
        eprintln!("Animometer adapter: {adapter_name}");
        let stride = adapter
            .limits()
            .min_uniform_buffer_offset_alignment
            .max(256);
        let (device, queue) = adapter
            .request_device(&core::device::DeviceDescriptor {
                required_limits: adapter.limits(),
                ..Default::default()
            })
            .map_err(|e| format!("{e:?}"))?;
        device.on_uncaptured_error(Arc::new(|e| panic!("Animometer core error: {e:?}")));
        let layout = |dynamic, size| {
            device.create_bind_group_layout(&core::binding_model::BindGroupLayoutDescriptor {
                label: None,
                entries: Cow::Owned(vec![wgt::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgt::ShaderStages::VERTEX,
                    ty: wgt::BindingType::Buffer {
                        ty: wgt::BufferBindingType::Uniform,
                        has_dynamic_offset: dynamic,
                        min_binding_size: std::num::NonZeroU64::new(size),
                    },
                    count: None,
                }]),
            })
        };
        let time_layout = layout(false, 4);
        let triangle_layout = layout(dynamic, 20);
        let pipeline_layout =
            device.create_pipeline_layout(&core::binding_model::PipelineLayoutDescriptor {
                label: None,
                bind_group_layouts: Cow::Owned(vec![
                    Some(time_layout.clone()),
                    Some(triangle_layout.clone()),
                ]),
                immediate_size: 0,
            });
        let shader = device.create_shader_module(
            &core::pipeline::ShaderModuleDescriptor {
                label: None,
                runtime_checks: Default::default(),
            },
            core::pipeline::ShaderModuleSource::Wgsl(Cow::Borrowed(include_str!(
                "../benchmarks/animometer.wgsl"
            ))),
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
                layout: Some(pipeline_layout),
                vertex: core::pipeline::VertexState {
                    stage: stage("vert_main"),
                    buffers: Cow::Owned(vec![Some(core::pipeline::VertexBufferLayout {
                        array_stride: 32,
                        step_mode: wgt::VertexStepMode::Vertex,
                        attributes: Cow::Owned(vec![
                            wgt::VertexAttribute {
                                format: wgt::VertexFormat::Float32x4,
                                offset: 0,
                                shader_location: 0,
                            },
                            wgt::VertexAttribute {
                                format: wgt::VertexFormat::Float32x4,
                                offset: 16,
                                shader_location: 1,
                            },
                        ]),
                    })]),
                },
                fragment: Some(core::pipeline::FragmentState {
                    stage: stage("frag_main"),
                    targets: Cow::Owned(vec![Some(wgt::ColorTargetState {
                        format: FORMAT,
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
        let vertices = device.create_buffer(&core::resource::BufferDescriptor {
            label: None,
            size: 96,
            usage: wgt::BufferUsages::VERTEX,
            mapped_at_creation: true,
        });
        vertices
            .get_mapped_range(0, Some(96))
            .map_err(|e| format!("{e:?}"))?
            .write_slice()
            .copy_from_slice(&bytes([
                0., 0.1, 0., 1., 1., 0., 0., 1., -0.1, -0.1, 0., 1., 0., 1., 0., 1., 0.1, -0.1, 0.,
                1., 0., 0., 1., 1.,
            ]));
        vertices.unmap();
        let time_offset = u64::from(triangles) * u64::from(stride);
        let uniforms = device.create_buffer(&core::resource::BufferDescriptor {
            label: None,
            size: time_offset + 4,
            usage: wgt::BufferUsages::UNIFORM | wgt::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut data = vec![0; time_offset as usize];
        let mut seed = 0x12345678;
        for chunk in data.chunks_exact_mut(stride as usize) {
            let values = bytes([
                random(&mut seed) * 0.2 + 0.2,
                1.8 * (random(&mut seed) - 0.5),
                1.8 * (random(&mut seed) - 0.5),
                random(&mut seed) * 1.5 + 0.5,
                random(&mut seed) * 10.,
            ]);
            chunk[..20].copy_from_slice(&values);
        }
        let group = |layout, offset, size| {
            device.create_bind_group(&core::binding_model::BindGroupDescriptor {
                label: None,
                layout,
                entries: Cow::Owned(vec![core::binding_model::BindGroupEntry {
                    binding: 0,
                    resource: core::binding_model::BindingResource::Buffer(
                        core::binding_model::BufferBinding {
                            buffer: uniforms.clone(),
                            offset,
                            size: Some(size),
                        },
                    ),
                }]),
            })
        };
        let time_group = group(time_layout, time_offset, 4);
        let groups = (0..if dynamic { 1 } else { triangles })
            .map(|i| {
                group(
                    triangle_layout.clone(),
                    u64::from(i) * u64::from(stride),
                    24,
                )
            })
            .collect::<Vec<_>>();
        // Bind groups must capture init actions before the sample's initial uploads.
        for (index, chunk) in data.chunks(14 * 1024 * 1024).enumerate() {
            queue.write_buffer(uniforms.clone(), (index * 14 * 1024 * 1024) as u64, chunk);
        }
        let texture = Self::target(&device);
        let view = texture.create_view(&Default::default());
        let start = Instant::now();
        let mut recorder =
            device.create_render_bundle_encoder(&core::command::RenderBundleEncoderDescriptor {
                label: None,
                color_formats: Cow::Owned(vec![Some(FORMAT)]),
                depth_stencil: None,
                sample_count: 1,
                multiview: None,
            });
        recorder.set_pipeline(pipeline.clone());
        recorder.set_vertex_buffer(0, Some(vertices.clone()), 0, Some(96));
        recorder.set_bind_group(0, Some(time_group.clone()), &[]);
        for i in 0..triangles {
            let offsets = [i * stride];
            recorder.set_bind_group(
                1,
                Some(groups[if dynamic { 0 } else { i as usize }].clone()),
                if dynamic { &offsets } else { &[] },
            );
            recorder.draw(3, 1, 0, 0);
        }
        let bundle = recorder.finish(&Default::default());
        Ok(Self {
            _instance: instance,
            device,
            queue,
            pipeline,
            vertices,
            uniforms,
            time_group,
            groups,
            texture,
            view,
            bundle,
            triangles,
            stride,
            time_offset,
            dynamic,
            adapter: adapter_name,
            bundle_build_ns: start.elapsed().as_nanos() as u64,
        })
    }

    fn wait(&self, index: Option<core::SubmissionIndex>) -> Result<(), String> {
        self.device
            .poll(wgt::PollType::Wait {
                submission_index: index,
                timeout: Some(Duration::from_secs(30)),
            })
            .map(|_| ())
            .map_err(|e| format!("{e:?}"))
    }

    fn target(device: &Arc<core::device::Device>) -> Arc<core::resource::Texture> {
        device.create_texture(&core::resource::TextureDescriptor {
            label: None,
            size: wgt::Extent3d {
                width: SIDE,
                height: SIDE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgt::TextureDimension::D2,
            format: FORMAT,
            usage: wgt::TextureUsages::RENDER_ATTACHMENT | wgt::TextureUsages::COPY_SRC,
            view_formats: vec![],
        })
    }

    fn frame(&mut self, time: f32, inline: bool, transient: bool) -> Result<Value, String> {
        let start = Instant::now();
        self.queue
            .write_buffer(self.uniforms.clone(), self.time_offset, &time.to_le_bytes());
        let update_ns = start.elapsed().as_nanos() as u64;
        let start = Instant::now();
        if transient {
            // Canvas publication retires the previous target on the same device.
            self.texture.destroy();
            self.texture = Self::target(&self.device);
            self.view = self.texture.create_view(&Default::default());
        }
        let encoder = self.device.create_command_encoder(&Default::default());
        let mut pass = encoder.begin_render_pass(core::command::ResolvedRenderPassDescriptor {
            color_attachments: Cow::Owned(vec![Some(core::command::RenderPassColorAttachment {
                view: self.view.clone(),
                depth_slice: None,
                resolve_target: None,
                load_op: wgt::LoadOp::Clear(wgt::Color::BLACK),
                store_op: wgt::StoreOp::Store,
            })]),
            ..Default::default()
        });
        if inline {
            pass.set_pipeline(self.pipeline.clone());
            pass.set_vertex_buffer(0, Some(self.vertices.clone()), 0, Some(96));
            pass.set_bind_group(0, Some(self.time_group.clone()), &[]);
            for i in 0..self.triangles {
                let offsets = [i * self.stride];
                pass.set_bind_group(
                    1,
                    Some(self.groups[if self.dynamic { 0 } else { i as usize }].clone()),
                    if self.dynamic { &offsets } else { &[] },
                );
                pass.draw(3, 1, 0, 0);
            }
        } else {
            pass.execute_bundles(&[self.bundle.clone()]);
        }
        pass.end();
        let command = encoder.finish(&Default::default());
        let encode_ns = start.elapsed().as_nanos() as u64;
        let start = Instant::now();
        let index = self
            .queue
            .submit(&[command])
            .ok_or("Animometer submission was not accepted")?;
        let submit_ns = start.elapsed().as_nanos() as u64;
        let start = Instant::now();
        self.wait(Some(index))?;
        Ok(
            json!({ "update_ns": update_ns, "encode_ns": encode_ns, "submit_ns": submit_ns,
            "wait_ns": start.elapsed().as_nanos() as u64 }),
        )
    }

    fn pixels(&self) -> Result<Vec<u8>, String> {
        let size = u64::from(SIDE) * u64::from(SIDE) * 4;
        let buffer = self
            .device
            .create_buffer(&core::resource::BufferDescriptor {
                label: None,
                size,
                usage: wgt::BufferUsages::MAP_READ | wgt::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        let encoder = self.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            &wgt::TexelCopyTextureInfo {
                texture: self.texture.clone(),
                mip_level: 0,
                origin: wgt::Origin3d::ZERO,
                aspect: wgt::TextureAspect::All,
            },
            &wgt::TexelCopyBufferInfo {
                buffer: buffer.clone(),
                layout: wgt::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(SIDE * 4),
                    rows_per_image: Some(SIDE),
                },
            },
            &wgt::Extent3d {
                width: SIDE,
                height: SIDE,
                depth_or_array_layers: 1,
            },
        );
        let index = self
            .queue
            .submit(&[encoder.finish(&Default::default())])
            .ok_or("Animometer submission was not accepted")?;
        self.wait(Some(index))?;
        let (send, receive) = std::sync::mpsc::channel();
        buffer.map_async(
            wgt::MapMode::Read,
            0,
            Some(size),
            Some(Box::new(move |result| {
                send.send(result).unwrap();
            })),
        );
        self.wait(None)?;
        receive
            .recv_timeout(Duration::from_secs(30))
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("{e:?}"))?;
        let pixels = buffer
            .get_mapped_range(0, Some(size))
            .map_err(|e| format!("{e:?}"))?
            .read_slice()
            .to_vec();
        buffer.unmap();
        Ok(pixels)
    }

    fn check(&mut self, transient: bool) -> Result<Value, String> {
        let mut hashes = Vec::new();
        for time in [0.25, 1.25] {
            self.frame(time, false, transient)?;
            let bundled = self.pixels()?;
            self.frame(time, true, transient)?;
            let inline = self.pixels()?;
            if bundled != inline {
                return Err("Bundle and inline pixels differ".into());
            }
            if !bundled.chunks_exact(4).any(|p| p[..3] != [0, 0, 0]) {
                return Err("Animometer produced no colored pixels".into());
            }
            hashes.push(bundled.iter().fold(0xcbf29ce484222325u64, |h, b| {
                (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
            }));
        }
        if hashes[0] == hashes[1] {
            return Err("Animated uniform update did not change the image".into());
        }
        Ok(json!({"bundle_matches_inline": true, "animated": true, "pixel_hashes": hashes}))
    }
}

pub fn run(args: &clap::ArgMatches) -> Result<(), String> {
    let number = |key, default| {
        args.value_of(key)
            .unwrap_or(default)
            .parse::<u32>()
            .map_err(|e| format!("{key}: {e}"))
    };
    let triangles = number("triangles", "100000")?;
    let iterations = number("iterations", "30")?;
    let warmup = number("warmup", "5")?;
    if triangles == 0 || triangles > 200000 || iterations == 0 {
        return Err("Require 1..200000 triangles and positive iterations".into());
    }
    let setup = Instant::now();
    let mut scene = Scene::new(
        triangles,
        args.is_present("dynamic_offsets"),
        args.is_present("validation"),
    )?;
    let setup_ns = setup.elapsed().as_nanos() as u64;
    let transient = args.is_present("transient_target");
    let correctness = if args.is_present("check") {
        scene.check(transient)?
    } else {
        Value::Null
    };
    let inline = args.is_present("inline");
    for i in 0..warmup {
        scene.frame(i as f32 / 60., inline, transient)?;
    }
    let mut samples = Vec::new();
    for i in 0..iterations {
        samples.push(scene.frame((i + warmup) as f32 / 60., inline, transient)?);
    }
    let mut sorted = samples
        .iter()
        .map(|v| v["encode_ns"].as_u64().unwrap())
        .collect::<Vec<_>>();
    sorted.sort_unstable();
    let report = json!({"benchmark": "animometer", "triangles": triangles,
        "render_bundles": !inline, "transient_target": transient, "dynamic_offsets": scene.dynamic, "uniform_stride": scene.stride,
        "target_size": [SIDE, SIDE], "adapter": scene.adapter, "validation": args.is_present("validation"),
        "warmup": warmup, "iterations": iterations, "setup_ns": setup_ns,
        "bundle_build_ns": scene.bundle_build_ns, "encode_median_ns": sorted[sorted.len()/2],
        "samples": samples, "correctness": correctness,
        "scope": "Offscreen normal-core command encoding, submission and GPU wait; no browser/WR frame building, presentation or GPU timestamps"});
    let report = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    println!("{report}");
    if let Some(path) = args.value_of("output") {
        std::fs::write(path, report).map_err(|e| e.to_string())?;
    }
    Ok(())
}
