/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::wgpu::{BufferPool, Device, Options, Texture, TextureFilter};
use crate::device::wgpu::resources::Owned;
use crate::device::wgpu::tests::{validation_logging, ERRORS};
use api::units::{DeviceIntRect, DeviceIntSize};
use std::convert::TryInto;
use std::sync::atomic::Ordering;
use wgpu_hal::{CommandEncoder as _, Device as _};

#[test]
fn instance_ranges_align_and_keep_oversized_draws_intact() {
    let (ranges, sizes) = instance_layout([0, 5, 12, 80, 4].iter().copied(), 32).unwrap();
    assert_eq!(sizes, [24, 80, 4]);
    assert_eq!(
        ranges
            .iter()
            .map(|r| (r.buffer, r.offset, r.size))
            .collect::<Vec<_>>(),
        [(0, 0, 0), (0, 4, 5), (0, 12, 12), (1, 0, 80), (2, 0, 4)]
    );
    assert!(instance_layout([usize::MAX, 1].iter().copied(), 32).is_err());
    assert!(instance_layout([4].iter().copied(), 0).is_err());
    assert!(instance_layout([].iter().copied(), 32).unwrap().0.is_empty());
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn packed_instances_draw_from_shared_ranges_and_recycle_after_completion() {
    validation_logging();
    let device = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    eprintln!("Vulkan adapter: {:?}", device.info());
    let pool = Rc::new(BufferPool::new(&device));
    let queue = SubmissionQueue::new(&pool, 3).unwrap();
    let seed = pool.upload(&[0xa5; 16], wgt::BufferUses::VERTEX).unwrap();
    pool.recycle(seed);
    let mut recording = queue.recording().unwrap();
    let mut called = false;
    assert!(queue
        .upload_instances_with(&mut recording, &[usize::MAX], |_, _| {
            called = true;
            Ok(())
        })
        .is_err());
    assert!(!called);
    assert!(queue
        .upload_instances_with(&mut recording, &[4], |_, _| Err("writer failed".into()))
        .is_err());
    let seed = pool.upload(&[0xa5; 16], wgt::BufferUses::VERTEX).unwrap();
    let seed_id = Rc::as_ptr(&seed);
    pool.recycle(seed);
    let empty = queue
        .upload_instances_with(&mut recording, &[], |_, _| unreachable!())
        .unwrap();
    assert!(empty.buffers.is_empty());
    assert!(empty.binding(0).is_err());
    let colors = [0xff0000ffu32, 0, 0xff00ff00, 0xffff0000];
    let mut calls = Vec::new();
    let instances = queue
        .upload_instances_with(&mut recording, &[5, 0, 4, 1024 * 1024], |draw, bytes| {
            calls.push(draw);
            bytes.fill(0xa5);
            if !bytes.is_empty() {
                bytes[..4].copy_from_slice(&colors[draw].to_ne_bytes());
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(calls, [0, 1, 2, 3]);
    assert_eq!(instances.buffers.len(), 2);
    assert_eq!(Rc::as_ptr(&instances.buffers[0]), seed_id);
    assert_eq!(instances.buffers[0].binding_size(), 16);
    assert!(instances.binding(4).is_err());

    let target = Texture::new(
        &device,
        4,
        1,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Nearest,
        true,
    )
    .unwrap();
    let raw = device.open.device.as_ref();
    unsafe {
        let layout = raw
            .create_pipeline_layout(&hal::PipelineLayoutDescriptor {
                label: Some("WR instance probe layout"),
                flags: hal::PipelineLayoutFlags::empty(),
                bind_group_layouts: &[],
                immediate_size: 0,
            })
            .unwrap();
        let layout = Owned::new(
            &device,
            layout,
            <dyn hal::DynDevice>::destroy_pipeline_layout,
        );
        let module = |bytes: &[u8]| {
            let words: Vec<_> = bytes
                .chunks_exact(4)
                .map(|word| u32::from_le_bytes(word.try_into().unwrap()))
                .collect();
            let shader = raw
                .create_shader_module(
                    &hal::ShaderModuleDescriptor {
                        label: Some("WR instance probe"),
                        runtime_checks: wgt::ShaderRuntimeChecks::unchecked(),
                    },
                    hal::ShaderInput::SpirV(&words),
                )
                .unwrap();
            Owned::new(&device, shader, <dyn hal::DynDevice>::destroy_shader_module)
        };
        let vs = module(include_bytes!("instance_probe.vert.spv"));
        let fs = module(include_bytes!("instance_probe.frag.spv"));
        let constants = Default::default();
        let stage = |module| hal::ProgrammableStage {
            module,
            entry_point: "main",
            constants: &constants,
            zero_initialize_workgroup_memory: false,
        };
        let pipeline = raw
            .create_render_pipeline(&hal::RenderPipelineDescriptor {
                label: Some("WR instance probe"),
                layout: &*layout,
                vertex_processor: hal::VertexProcessor::Standard {
                    vertex_buffers: &[Some(hal::VertexBufferLayout {
                        array_stride: 4,
                        step_mode: wgt::VertexStepMode::Instance,
                        attributes: &[wgt::VertexAttribute {
                            format: wgt::VertexFormat::Uint32,
                            offset: 0,
                            shader_location: 0,
                        }],
                    })],
                    vertex_stage: stage(&*vs),
                },
                fragment_stage: Some(stage(&*fs)),
                primitive: wgt::PrimitiveState {
                    topology: wgt::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: Default::default(),
                color_targets: &[Some(wgt::ColorTargetState {
                    format: target.format(),
                    blend: None,
                    write_mask: wgt::ColorWrites::ALL,
                })],
                multiview_mask: None,
                cache: None,
            })
            .unwrap();
        let pipeline = Owned::new(
            &device,
            pipeline,
            <dyn hal::DynDevice>::destroy_render_pipeline,
        );
        target
            .transition(&mut recording, wgt::TextureUses::COLOR_TARGET)
            .unwrap();
        let encoder = recording.encoder();
        encoder
            .begin_render_pass(&hal::RenderPassDescriptor {
                label: Some("WR instance probe"),
                extent: target.size(),
                sample_count: 1,
                color_attachments: &[Some(hal::ColorAttachment {
                    target: hal::Attachment {
                        view: target.target_view().unwrap(),
                        usage: wgt::TextureUses::COLOR_TARGET,
                    },
                    depth_slice: None,
                    resolve_target: None,
                    ops: hal::AttachmentOps::LOAD_CLEAR | hal::AttachmentOps::STORE,
                    clear_value: wgt::Color {
                        r: 1.0,
                        g: 1.0,
                        b: 1.0,
                        a: 1.0,
                    },
                })],
                depth_stencil_attachment: None,
                multiview_mask: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            })
            .unwrap();
        encoder.set_render_pipeline(&*pipeline);
        encoder.set_viewport(
            &hal::Rect {
                x: 0.0,
                y: 0.0,
                w: 4.0,
                h: 1.0,
            },
            0.0..1.0,
        );
        for draw in 0..4 {
            encoder.set_scissor_rect(&hal::Rect {
                x: draw as u32,
                y: 0,
                w: 1,
                h: 1,
            });
            encoder.set_vertex_buffer(0, instances.binding(draw).unwrap());
            encoder.draw(0, 4, 0, 1);
        }
        encoder.end_render_pass();
        target.initialize(&mut recording).unwrap();
        drop(instances);
        drop(recording);
        queue.wait().unwrap();
    }
    assert_eq!(
        target
            .readback(DeviceIntRect::from_size(DeviceIntSize::new(4, 1)))
            .unwrap()
            .wait()
            .unwrap(),
        [255, 0, 0, 255, 0, 0, 0, 0, 0, 255, 0, 255, 0, 0, 255, 255]
    );
    let mut recording = queue.recording().unwrap();
    let recycled = queue
        .upload_instances_with(&mut recording, &[4], |_, bytes| {
            bytes.fill(0);
            Ok(())
        })
        .unwrap();
    assert_eq!(Rc::as_ptr(&recycled.buffers[0]), seed_id);
    drop(recycled);
    drop(recording);
    queue.discard_recording();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
