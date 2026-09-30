/* This Source Code Form is subject to the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::{RenderState, TextureSlot, VertexAttribute, VertexAttributeKind};
use crate::device::vulkan::{
    shaders, wgt, Buffer, BufferPool, Device, Options, Samplers, SubmissionQueue, Texture,
    TextureFilter,
};
use crate::device::vulkan::draw::DrawPass;
use crate::device::vulkan::pipeline::DrawPipeline;
use crate::device::vulkan::program::ShaderResource;
use crate::device::vulkan::tests::{validation_logging, ERRORS};
use crate::renderer::desc;
use api::units::{DeviceIntPoint, DeviceIntRect, DeviceIntSize};
use euclid::default::Transform3D;
use std::sync::atomic::Ordering;

#[test]
fn program_handles_link_every_compiled_variant_and_delete_cleanly() {
    let mut store = ProgramStore::default();
    let mut previous_id = 0;
    assert!(store.current().is_err());
    for shader in shaders::SHADERS {
        let features: Vec<_> = shader
            .features
            .split(',')
            .filter(|f| !f.is_empty())
            .collect();
        let mut program = store
            .create(shader.name, &features, shader.buffer_tables)
            .unwrap();
        assert!(program.id > previous_id);
        previous_id = program.id;
        assert!(!program.is_initialized());
        assert!(store.state(&program).is_err());
        assert!(store.state_mut(&program).is_err());
        assert!(store.bind(&program).is_err());
        assert_eq!(program.source_info.base_filename, shader.name);
        assert_eq!(program.source_info.features, features);
        assert_ne!(program.source_info.digest, Default::default());
        store
            .link(&mut program, draw_vertex_descriptor(shader).unwrap())
            .unwrap();
        assert!(program.is_initialized());
        assert_eq!(
            program.u_transform,
            if shader.projection_stages != 0 { 0 } else { -1 }
        );
        assert_eq!(program.u_texture_size, -1);
        assert!(store.bind(&program).unwrap());
        assert!(!store.bind(&program).unwrap());
        assert!(std::ptr::eq(store.current().unwrap().shader(), shader));
        store.unbind();
        assert!(store.current().is_err());
        assert!(store.bind(&program).unwrap());
        assert!(std::ptr::eq(
            store.state(&program).unwrap().shader(),
            shader
        ));
        store.delete(&mut program).unwrap();
        assert_eq!(program.id, 0);
        assert!(!program.is_initialized());
        assert!(store.state(&program).is_err());
        assert!(store.current().is_err());
        store.delete(&mut program).unwrap();
        assert!(store.entries.is_empty());
    }
}

#[test]
fn program_metadata_and_store_identity_are_independent_of_handle_numbers() {
    let mut store = ProgramStore::default();
    let mut other_store = ProgramStore::default();
    let mut rect = store.create("cs_scale", &["TEXTURE_RECT"], false).unwrap();
    let mut normal = store.create("cs_scale", &["TEXTURE_2D"], false).unwrap();
    assert!(store.create("cs_scale", &["TEXTURE_2D"], true).is_err());
    let mut foreign = other_store
        .create("cs_scale", &["TEXTURE_2D"], false)
        .unwrap();
    assert_eq!(rect.id, foreign.id);
    assert_eq!(rect.source_info.digest, normal.source_info.digest);
    assert_eq!(
        normal.source_info.full_name_cstr,
        rect.source_info.full_name_cstr
    );
    assert_eq!(rect.source_info.features, ["TEXTURE_RECT"]);
    assert!(store.link(&mut foreign, &desc::SCALE).is_err());
    assert!(!foreign.is_initialized());
    other_store.link(&mut foreign, &desc::SCALE).unwrap();
    assert!(store.state(&foreign).is_err());
    assert!(store.state_mut(&foreign).is_err());
    assert!(store.delete(&mut foreign).is_err());
    assert!(other_store.state(&foreign).is_ok());
    store.link(&mut normal, &desc::SCALE).unwrap();
    store.bind(&normal).unwrap();
    assert!(store.bind(&foreign).is_err());
    assert_eq!(store.bound, Some(normal.id));
    assert!(store.link(&mut normal, &desc::SCALE).is_err());
    assert!(store.state(&normal).is_ok());
    store.delete(&mut rect).unwrap();
    assert_eq!(store.bound, Some(normal.id));
    store.delete(&mut normal).unwrap();
    assert!(store.current().is_err());
    other_store.delete(&mut foreign).unwrap();
}

#[test]
fn program_creation_and_link_failures_leave_no_dangling_handles() {
    let mut store = ProgramStore::default();
    assert!(matches!(
        store.create("missing", &[], false),
        Err(ShaderError::Compilation(..))
    ));
    assert!(matches!(
        store.create("cs_scale", &["unknown"], false),
        Err(ShaderError::Compilation(..))
    ));
    assert_eq!(store.last_id, 0);
    const PADDED_VERTEX: &[VertexAttribute] = &[
        VertexAttribute::quad_instance_vertex(),
        VertexAttribute {
            name: "unused",
            count: 1,
            kind: VertexAttributeKind::F32,
        },
    ];
    let padded = VertexDescriptor {
        vertex_attributes: PADDED_VERTEX,
        instance_attributes: desc::SCALE.instance_attributes,
    };
    for descriptor in [&desc::CLEAR, &padded] {
        let mut program = store.create("cs_scale", &["TEXTURE_2D"], false).unwrap();
        assert!(matches!(
            store.link(&mut program, descriptor),
            Err(ShaderError::Link(..))
        ));
        assert_eq!(program.id, 0);
        assert!(!program.is_initialized());
        assert!(store.entries.is_empty());
    }
    store.last_id = u32::MAX - 1;
    let mut last = store.create("ps_clear", &[], false).unwrap();
    assert_eq!(last.id, u32::MAX);
    store.delete(&mut last).unwrap();
    assert!(store.create("ps_clear", &[], false).is_err());
    assert!(store.entries.is_empty());
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn queued_program_snapshot_survives_shared_handle_deletion() {
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
    let queue = SubmissionQueue::new(&pool, 2).unwrap();
    let mut store = ProgramStore::default();
    let mut program = store.create("cs_scale", &["TEXTURE_2D"], false).unwrap();
    store.link(&mut program, &desc::SCALE).unwrap();
    store.bind(&program).unwrap();
    store
        .state_mut(&program)
        .unwrap()
        .bind_samplers(&[("sColor0", TextureSlot(0))]);
    store
        .state(&program)
        .unwrap()
        .set_transform(&Transform3D::ortho(0.0, 2.0, 0.0, 1.0, -1.0, 1.0));
    let pipeline = DrawPipeline::new(
        &device,
        store.state(&program).unwrap().shader(),
        wgt::TextureFormat::Rgba8Unorm,
        false,
        RenderState::default(),
    )
    .unwrap();
    let weak_pipeline = Rc::downgrade(&pipeline);
    let source = Texture::new(
        &device,
        2,
        1,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Nearest,
        false,
    )
    .unwrap();
    let target = Texture::new(
        &device,
        2,
        1,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Nearest,
        true,
    )
    .unwrap();
    let rect = DeviceIntRect::from_size(DeviceIntSize::new(2, 1));
    let colors = [255, 0, 0, 255, 0, 0, 255, 255];
    source.upload(&queue, rect, &colors, None, 0, None).unwrap();
    let pass = DrawPass {
        target: &target,
        origin: DeviceIntPoint::zero(),
        depth: None,
        clear_color: None,
        clear_depth: None,
        depth_range: 0.0..1.0,
    };
    let snapshot = store
        .current()
        .unwrap()
        .resolve(
            &pipeline,
            &pass,
            &[Some(ShaderResource::Texture {
                texture: source,
                filter: None,
            })],
        )
        .unwrap();
    store.delete(&mut program).unwrap();
    assert!(store.current().is_err());
    drop(program);
    drop(store);
    drop(pipeline);
    let quad = Buffer::new(
        &device,
        &[0, 0, 0, 0, 255, 0, 0, 0, 0, 255, 0, 0, 255, 255, 0, 0],
        wgt::BufferUses::VERTEX,
    )
    .unwrap();
    let samplers = Rc::new(Samplers::new(&device).unwrap());
    let values = [0.0f32, 0.0, 2.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0];
    let instances: Vec<_> = values.iter().flat_map(|v| v.to_ne_bytes()).collect();
    pass.record_batches(
        &mut queue.recording().unwrap(),
        &queue,
        &quad,
        Some(&samplers),
        &[snapshot.batch(&instances, 1, rect)],
    )
    .unwrap();
    drop(snapshot);
    assert!(weak_pipeline.upgrade().is_some());
    queue.wait().unwrap();
    assert!(weak_pipeline.upgrade().is_none());
    assert_eq!(target.readback(rect).unwrap().wait().unwrap(), colors);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
