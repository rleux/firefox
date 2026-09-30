/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::super::{Device, Options, SubmissionQueue, Texture, TextureFilter};
use super::super::draw::DrawPass;
use super::super::pipeline::DrawPipeline;
use super::super::program::ProgramState;
use super::super::shader::select_draw_shader;
use super::super::tests::{validation_logging, ERRORS};
use api::units::{DeviceIntPoint, DeviceIntSize};
use crate::device::RenderState;
use crate::renderer::desc;
use std::sync::atomic::Ordering;

fn pool() -> Rc<BufferPool> {
    validation_logging();
    let device = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    eprintln!("Vulkan adapter: {:?}", device.info());
    Rc::new(BufferPool::new(&device))
}

fn bytes(slice: &InstanceSlice) -> &[u8] {
    &slice.buffer.mapped_read_only().unwrap()
        [slice.offset as usize..(slice.offset + slice.stride * u64::from(slice.count)) as usize]
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn vertex_arrays_share_logical_buffers_and_survive_base_deletion() {
    let pool = pool();
    let mut store = VertexArrayStore::new(&pool);
    let mut vertices = store.create_buffer(BufferKind::Vertex).unwrap();
    let mut indices = store.create_buffer(BufferKind::Index).unwrap();
    let mut instances = store.create_buffer(BufferKind::Vertex).unwrap();
    let mut separate_instances = store.create_buffer(BufferKind::Vertex).unwrap();
    let mut base = store
        .create(
            &desc::PRIM_INSTANCES,
            &vertices,
            Some(&instances),
            Some(&indices),
            1,
        )
        .unwrap();
    store.write_buffer(&mut vertices, &[1; 8]).unwrap();
    store.write_buffer(&mut indices, &[2; 12]).unwrap();
    store.write_buffer(&mut instances, &[3; 32]).unwrap();
    let mut shared = store
        .create(&desc::CLEAR, &vertices, Some(&instances), Some(&indices), 1)
        .unwrap();
    let mut separate = store
        .create(
            &desc::SCALE,
            &vertices,
            Some(&separate_instances),
            Some(&indices),
            1,
        )
        .unwrap();
    assert_eq!(shared.instances, base.instances);
    assert_ne!(separate.instances, base.instances);
    store.bind(&shared).unwrap();
    let old = store.instances(0, 1).unwrap().unwrap();
    assert_eq!(bytes(&old), [3; 32]);
    store.update_range(&instances, 16, &[9; 16]).unwrap();
    store.bind(&base).unwrap();
    assert_eq!(bytes(&store.instances(1, 1).unwrap().unwrap()), [9; 16]);
    assert_eq!(bytes(&old), [3; 32]);
    store.delete(&mut base).unwrap();
    store.bind(&shared).unwrap();
    assert_eq!(
        &bytes(&store.instances(0, 1).unwrap().unwrap())[16..],
        &[9; 16]
    );
    store.write_buffer(&mut vertices, &[7; 8]).unwrap();
    assert_eq!(
        &store.buffers[&separate.vertices.0]
            .buffer
            .as_ref()
            .unwrap()
            .mapped_read_only()
            .unwrap()[..8],
        &[7; 8]
    );
    assert_eq!(
        &store.buffers[&separate.indices.unwrap().0]
            .buffer
            .as_ref()
            .unwrap()
            .mapped_read_only()
            .unwrap()[..12],
        &[2; 12]
    );
    store.bind(&separate).unwrap();
    assert!(store.instances(0, 1).is_err());
    store
        .write_buffer(&mut separate_instances, &[11; 36])
        .unwrap();
    assert_eq!(bytes(&store.instances(0, 1).unwrap().unwrap()), [11; 36]);
    store.delete(&mut shared).unwrap();
    store.delete(&mut separate).unwrap();
    assert!(store.arrays.is_empty());
    assert_eq!(store.buffers.len(), 4);
    for buffer in [
        &mut vertices,
        &mut indices,
        &mut instances,
        &mut separate_instances,
    ] {
        store.delete_buffer(buffer).unwrap();
    }
    assert!(store.buffers.is_empty());
    assert_eq!(bytes(&old), [3; 32]);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn mapped_vertex_updates_reuse_exclusive_storage_and_preserve_snapshots() {
    let pool = pool();
    let mut store = VertexArrayStore::new(&pool);
    let mut vertices = store.create_buffer(BufferKind::Vertex).unwrap();
    let mut instances = store.create_buffer(BufferKind::Vertex).unwrap();
    let mut vao = store
        .create(&desc::CLEAR, &vertices, Some(&instances), None, 1)
        .unwrap();
    let id = instances.id;
    store.write_buffer(&mut instances, &[1; 64]).unwrap();
    let allocation = Rc::as_ptr(store.buffers[&id].buffer.as_ref().unwrap());
    store.update_range(&instances, 8, &[2; 8]).unwrap();
    assert_eq!(
        allocation,
        Rc::as_ptr(store.buffers[&id].buffer.as_ref().unwrap())
    );
    store.bind(&vao).unwrap();
    let old = store.instances(0, 2).unwrap().unwrap();
    store.update_range(&instances, 40, &[3; 4]).unwrap();
    let current = store.instances(0, 2).unwrap().unwrap();
    assert!(!Rc::ptr_eq(&old.buffer, &current.buffer));
    assert_eq!(&bytes(&old)[40..44], &[1; 4]);
    assert_eq!(&bytes(&current)[40..44], &[3; 4]);
    assert_eq!(&bytes(&current)[8..16], &[2; 8]);
    store.reallocate(&mut instances, 16).unwrap();
    assert!(store.instances(0, 1).is_err());
    store.reallocate(&mut instances, 64).unwrap();
    assert_eq!(bytes(&store.instances(0, 2).unwrap().unwrap()), [0; 64]);
    let records = [[4; 32], [5; 32]].concat();
    store
        .write_buffer_repeated(&mut instances, &records, 32, NonZeroUsize::new(2).unwrap())
        .unwrap();
    assert_eq!(
        bytes(&store.instances(0, 4).unwrap().unwrap()),
        [[4; 32], [4; 32], [5; 32], [5; 32]].concat()
    );
    assert_eq!(bytes(&store.instances(1, 2).unwrap().unwrap()), records);
    store.write_buffer(&mut instances, &[]).unwrap();
    assert!(store.instances(0, 1).is_err());
    assert_eq!(&bytes(&old)[40..44], &[1; 4]);
    store.delete(&mut vao).unwrap();
    store.delete_buffer(&mut vertices).unwrap();
    store.delete_buffer(&mut instances).unwrap();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn vertex_array_snapshots_draw_directly_after_updates_and_deletion() {
    let pool = pool();
    let device = &pool.owner;
    let queue = SubmissionQueue::new(&pool, 2).unwrap();
    let mut store = VertexArrayStore::new(&pool);
    let mut vertices = store.create_buffer(BufferKind::Vertex).unwrap();
    let mut instances = store.create_buffer(BufferKind::Vertex).unwrap();
    let mut vao = store
        .create(&desc::CLEAR, &vertices, Some(&instances), None, 1)
        .unwrap();
    let floats = |data: &[f32]| {
        data.iter()
            .flat_map(|v| v.to_ne_bytes())
            .collect::<Vec<_>>()
    };
    store
        .write_buffer(
            &mut instances,
            &floats(&[-1.0, -1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 1.0]),
        )
        .unwrap();
    store.bind(&vao).unwrap();
    let red = store.instances(0, 1).unwrap().unwrap();
    store
        .update_range(&instances, 16, &floats(&[0.0, 1.0, 0.0, 1.0]))
        .unwrap();
    let green = store.instances(0, 1).unwrap().unwrap();
    let target = Texture::new(
        device,
        2,
        2,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Nearest,
        true,
    )
    .unwrap();
    let pass = DrawPass {
            viewport: None,
        target: &target,
        origin: DeviceIntPoint::zero(),
        depth: None,
        clear_color: None,
        clear_depth: None,
        depth_range: 0.0..1.0,
    };
    let shader = select_draw_shader("ps_clear", &[], false).unwrap();
    let pipeline = DrawPipeline::new(
        device,
        shader,
        target.format(),
        false,
        RenderState::default(),
    )
    .unwrap();
    let program = ProgramState::new(shader)
        .resolve(&pipeline, &pass, |_| None, None)
        .unwrap();
    let quad = Buffer::new(
        device,
        &[0, 0, 0, 0, 255, 0, 0, 0, 0, 255, 0, 0, 255, 255, 0, 0],
        wgt::BufferUses::VERTEX,
    )
    .unwrap();
    {
        let mut commands = queue.recording().unwrap();
        let bindings = program.bindings(&mut commands, &queue, None).unwrap();
        let draws = [
            red.draw(
                bindings.clone(),
                DeviceIntRect::from_size(DeviceIntSize::new(1, 2)),
            )
            .unwrap(),
            green
                .draw(
                    bindings,
                    DeviceIntRect::from_origin_and_size(
                        DeviceIntPoint::new(1, 0),
                        DeviceIntSize::new(1, 2),
                    ),
                )
                .unwrap(),
        ];
        store.delete(&mut vao).unwrap();
        store.delete_buffer(&mut vertices).unwrap();
        store.delete_buffer(&mut instances).unwrap();
        drop(store);
        drop(red);
        drop(green);
        pass.record(&mut commands, &quad, &draws).unwrap();
    }
    queue.wait().unwrap();
    assert_eq!(pool.bytes(), 128);
    assert_eq!(
        target
            .readback(DeviceIntRect::from_size(DeviceIntSize::new(2, 2)))
            .unwrap()
            .wait()
            .unwrap(),
        [255, 0, 0, 255, 0, 255, 0, 255].repeat(2)
    );
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn vertex_array_ranges_and_identifiers_are_checked() {
    let pool = pool();
    let mut store = VertexArrayStore::new(&pool);
    assert!(store.instances(0, 0).unwrap().is_none());
    assert!(store.instances(0, 1).is_err());
    let mut vertices = store.create_buffer(BufferKind::Vertex).unwrap();
    let mut instances = store.create_buffer(BufferKind::Vertex).unwrap();
    let mut vao = store
        .create(&desc::CLEAR, &vertices, Some(&instances), None, 1)
        .unwrap();
    store.bind(&vao).unwrap();
    assert!(store.instances(0, 1).is_err());
    for (data, stride, repeat) in [
        (&[0; 32][..], 0, None),
        (&[0; 31][..], 32, None),
        (&[0; 32][..], 32, NonZeroUsize::new(usize::MAX)),
    ] {
        assert!(store
            .write_buffer_repeated(
                &mut instances,
                data,
                stride,
                repeat.unwrap_or(NonZeroUsize::new(1).unwrap())
            )
            .is_err());
    }
    assert!(store.buffers[&instances.id].buffer.is_none());
    store.reallocate(&mut instances, 8).unwrap();
    assert!(store.update_range(&instances, usize::MAX, &[1]).is_err());
    assert!(store.update_range(&instances, 7, &[1, 2]).is_err());
    store.update_range(&instances, 8, &[]).unwrap();
    store.write_buffer(&mut instances, &[0; 32]).unwrap();
    assert!(store.instances(u32::MAX, 1).is_err());
    assert!(store.instances(0, u32::MAX).is_err());
    let writable = Buffer::new(
        &pool.owner,
        &[0; 16],
        wgt::BufferUses::VERTEX | wgt::BufferUses::COPY_DST,
    )
    .unwrap();
    assert!(writable.mapped_read_only().is_err());
    store.last_id = u32::MAX - 1;
    let mut last = store
        .create(&desc::CLEAR, &vertices, Some(&instances), None, 1)
        .unwrap();
    assert_eq!(last.id, u32::MAX);
    assert!(store
        .create(&desc::CLEAR, &vertices, Some(&instances), None, 1)
        .is_err());
    store.delete(&mut last).unwrap();
    store.delete(&mut vao).unwrap();
    store.delete_buffer(&mut vertices).unwrap();
    store.delete_buffer(&mut instances).unwrap();
    assert!(store.bind(&vao).is_err());
    assert!(store.arrays.is_empty() && store.buffers.is_empty());
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
