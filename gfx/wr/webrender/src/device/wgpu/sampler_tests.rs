/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::super::resources::Owned;
use api::units::{DeviceIntRect, DeviceIntSize};
use std::convert::TryInto;

fn sample(
    device: &Rc<Device>,
    texture: &Rc<Texture>,
    samplers: Rc<Samplers>,
    filter: TextureFilter,
) -> Vec<f32> {
    let words: Vec<_> = include_bytes!("sampler_probe.spv")
        .chunks_exact(4)
        .map(|word| u32::from_le_bytes(word.try_into().unwrap()))
        .collect();
    unsafe {
        let raw = device.open.device.as_ref();
        let entries: Vec<_> = [
            wgt::BindingType::Texture {
                sample_type: wgt::TextureSampleType::Float { filterable: true },
                view_dimension: wgt::TextureViewDimension::D2,
                multisampled: false,
            },
            wgt::BindingType::Sampler(wgt::SamplerBindingType::Filtering),
            wgt::BindingType::Buffer {
                ty: wgt::BufferBindingType::Storage { read_only: false },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
        ]
        .iter()
        .copied()
        .enumerate()
        .map(|(binding, ty)| wgt::BindGroupLayoutEntry {
            binding: binding as u32,
            visibility: wgt::ShaderStages::COMPUTE,
            ty,
            count: None,
        })
        .collect();
        let layout = raw
            .create_bind_group_layout(&hal::BindGroupLayoutDescriptor {
                label: Some("WR sampler test bindings"),
                flags: hal::BindGroupLayoutFlags::empty(),
                entries: &entries,
            })
            .unwrap();
        let layout = Owned::new(
            device,
            layout,
            <dyn hal::DynDevice>::destroy_bind_group_layout,
        );
        let pipeline_layout = raw
            .create_pipeline_layout(&hal::PipelineLayoutDescriptor {
                label: Some("WR sampler test layout"),
                flags: hal::PipelineLayoutFlags::empty(),
                bind_group_layouts: &[Some(&*layout)],
                immediate_size: 0,
            })
            .unwrap();
        let pipeline_layout = Owned::new(
            device,
            pipeline_layout,
            <dyn hal::DynDevice>::destroy_pipeline_layout,
        );
        let shader = raw
            .create_shader_module(
                &hal::ShaderModuleDescriptor {
                    label: Some("WR sampler probe"),
                    runtime_checks: wgt::ShaderRuntimeChecks::unchecked(),
                },
                hal::ShaderInput::SpirV(&words),
            )
            .unwrap();
        let shader = Owned::new(device, shader, <dyn hal::DynDevice>::destroy_shader_module);
        let pipeline = raw
            .create_compute_pipeline(&hal::ComputePipelineDescriptor {
                label: Some("WR sampler probe"),
                layout: &*pipeline_layout,
                stage: hal::ProgrammableStage {
                    module: &*shader,
                    entry_point: "main",
                    constants: &Default::default(),
                    zero_initialize_workgroup_memory: false,
                },
                cache: None,
            })
            .unwrap();
        let pipeline = Owned::new(
            device,
            pipeline,
            <dyn hal::DynDevice>::destroy_compute_pipeline,
        );
        let (target, _) = raw
            .create_buffer(&hal::BufferDescriptor {
                label: Some("WR sampler probe output"),
                size: 64,
                usage: wgt::BufferUses::STORAGE_READ_WRITE | wgt::BufferUses::MAP_READ,
                memory_flags: hal::MemoryFlags::PREFER_COHERENT,
            })
            .unwrap();
        let target = Owned::new(device, target, <dyn hal::DynDevice>::destroy_buffer);
        let group = raw
            .create_bind_group(&hal::BindGroupDescriptor {
                label: Some("WR sampler probe group"),
                layout: &*layout,
                buffers: &[hal::BufferBinding::new_unchecked(
                    &*target,
                    0,
                    std::num::NonZeroU64::new(64).unwrap(),
                )],
                samplers: &[samplers.get(filter)],
                textures: &[hal::TextureBinding {
                    view: texture.view(),
                    usage: wgt::TextureUses::RESOURCE,
                }],
                acceleration_structures: &[],
                external_textures: &[],
                entries: &[0, 1, 2].map(|binding| hal::BindGroupEntry {
                    binding,
                    resource_index: 0,
                    count: 1,
                }),
            })
            .unwrap();
        let group = Owned::new(device, group, <dyn hal::DynDevice>::destroy_bind_group);
        let mut commands_submission = Submission::new(device).unwrap();
        let mut commands = commands_submission.recording().unwrap();
        texture
            .transition(&mut commands, wgt::TextureUses::RESOURCE)
            .unwrap();
        let encoder = commands.encoder();
        encoder.begin_compute_pass(&hal::ComputePassDescriptor {
            label: Some("WR sampler probe"),
            timestamp_writes: None,
        });
        encoder.set_compute_pipeline(&*pipeline);
        encoder.set_bind_group(&*pipeline_layout, 0, &*group, &[]);
        encoder.dispatch_workgroups([1, 1, 1]);
        encoder.end_compute_pass();
        encoder.transition_buffers(&[hal::BufferBarrier {
            buffer: &*target,
            usage: hal::StateTransition {
                from: wgt::BufferUses::STORAGE_READ_WRITE,
                to: wgt::BufferUses::MAP_READ,
            },
        }]);
        let remaining = Rc::strong_count(&samplers) - 1;
        let weak = Rc::downgrade(&samplers);
        commands.keep(&samplers);
        drop(samplers);
        drop(commands);
        commands_submission.submit().unwrap();
        assert!(weak.upgrade().is_some());
        assert!(commands_submission
            .wait(Some(std::time::Duration::from_secs(10)))
            .unwrap());
        assert_eq!(weak.strong_count(), remaining);
        map_upload(device, &*target, 64)
            .chunks_exact(4)
            .map(|bytes| f32::from_ne_bytes(bytes.try_into().unwrap()))
            .collect()
    }
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn sampler_filters_clamp_edges_and_select_mips() {
    validation_logging();
    let device = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    eprintln!("Vulkan adapter: {:?}", device.info());
    let samplers = Rc::new(Samplers::new(&device).unwrap());
    let texture = Texture::new(
        &device,
        2,
        2,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Trilinear,
        false,
    )
    .unwrap();
    let queue = upload_queue(&device);
    texture
        .upload(
            &queue,
            DeviceIntRect::from_size(DeviceIntSize::new(2, 2)),
            &[
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
            ],
            None,
            0,
            None,
        )
        .unwrap();
    let mip = texture.mip_view(1).unwrap();
    mip.upload(
        &queue,
        DeviceIntRect::from_size(DeviceIntSize::new(1, 1)),
        &[0, 0, 0, 255],
        None,
        0,
        None,
    )
    .unwrap();
    queue.wait().unwrap();
    let white = [1.0, 1.0, 1.0, 1.0];
    let blue = [0.0, 0.0, 1.0, 1.0];
    let black = [0.0, 0.0, 0.0, 1.0];
    let bilinear = [0.625, 0.75, 0.75, 1.0];
    let blended_mips = [0.3125, 0.375, 0.375, 1.0];
    for (filter, expected) in [
        (TextureFilter::Nearest, [white, blue, white, white]),
        (TextureFilter::Linear, [bilinear, blue, bilinear, bilinear]),
        (
            TextureFilter::Trilinear,
            [bilinear, blue, black, blended_mips],
        ),
    ] {
        let actual = sample(&device, &texture, samplers.clone(), filter);
        for (actual, expected) in actual.iter().zip(expected.iter().flatten()) {
            assert!(
                (actual - expected).abs() < 1.0 / 255.0,
                "{filter:?}: {actual} != {expected}"
            );
        }
    }
    drop(texture);
    assert_eq!(
        sample(&device, &mip, samplers, TextureFilter::Trilinear),
        black.repeat(4)
    );
    drop(mip);
    drop(queue);
    assert_eq!(Rc::strong_count(&device), 1);
    device.lost.set(true);
    assert!(Samplers::new(&device).is_err());
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
