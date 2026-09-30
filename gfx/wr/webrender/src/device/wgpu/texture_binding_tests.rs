/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::{RenderState, TextureSlot};
use crate::device::wgpu::{shaders, Buffer, Samplers};
use crate::device::wgpu::pipeline::DrawPipeline;
use crate::device::wgpu::program::{ProgramState, ShaderResource};
use crate::device::wgpu::shader::select_draw_shader;
use euclid::default::Transform3D;

fn create(store: &mut TextureStore, renderable: bool) -> TextureHandle {
    store
        .create(
            ImageBufferKind::Texture2D,
            ImageFormat::RGBA8,
            DeviceIntSize::new(2, 2),
            TextureFilter::Nearest,
            renderable.then_some(RenderTargetInfo { has_depth: true }),
        )
        .unwrap()
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn texture_slot_resets_and_deletion_preserve_saved_resources() {
    let device = device();
    let mut store = TextureStore::new(&device);
    let mut first = create(&mut store, false);
    let mut second = create(&mut store, false);
    for index in [0, 5] {
        store.bind(TextureSlot(index), &first).unwrap();
    }
    for index in [2, 15] {
        store.bind(TextureSlot(index), &second).unwrap();
    }
    assert!(store.bind(TextureSlot(16), &first).is_err());
    assert!(store.bind(TextureSlot(usize::MAX), &second).is_err());
    store.clear_color_bindings();
    let saved = store.bindings();
    assert!(saved[..3].iter().all(Option::is_none));
    assert!(saved[5].is_some() && saved[15].is_some());
    let first_image = store.image(&first).unwrap();
    let weak = Rc::downgrade(&first_image);
    assert!(
        matches!(&saved[5], Some(ShaderResource::Texture { texture, filter: None }) if Rc::ptr_eq(texture, &first_image))
    );
    drop(first_image);
    store.delete(&mut first).unwrap();
    assert!(store.bindings()[5].is_none());
    assert!(store.bindings()[15].is_some());
    assert!(store.bind(TextureSlot(15), &first).is_err());
    assert!(store.bindings()[15].is_some());
    assert!(weak.upgrade().is_some());
    drop(saved);
    assert!(weak.upgrade().is_none());
    store.begin_frame(GpuFrameId::new(1));
    assert!(store.bindings().iter().all(Option::is_none));
    store.bind(TextureSlot(3), &second).unwrap();
    store.reset_bindings();
    assert!(store.bindings().iter().all(Option::is_none));
    store.delete(&mut second).unwrap();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn unbound_color_slots_use_fallbacks_without_masking_invalid_tables() {
    let device = device();
    let pool = Rc::new(BufferPool::new(&device));
    let queue = SubmissionQueue::new(&pool, 2).unwrap();
    let mut store = TextureStore::new(&device);
    let mut handle = create(&mut store, false);
    let image = store.image(&handle).unwrap();
    let rect = DeviceIntRect::from_size(DeviceIntSize::new(2, 2));
    image
        .upload(&queue, rect, &[0, 255, 0, 255].repeat(4), None, 0, None)
        .unwrap();
    let fallback =
        Texture::new(&device, 1, 1, image.format(), TextureFilter::Nearest, false).unwrap();
    fallback
        .upload(
            &queue,
            DeviceIntRect::from_size(DeviceIntSize::new(1, 1)),
            &[0; 4],
            None,
            0,
            None,
        )
        .unwrap();
    let target = Texture::new(&device, 2, 2, image.format(), TextureFilter::Nearest, true).unwrap();
    let pass = DrawPass {
            viewport: None,
        target: &target,
        origin: DeviceIntPoint::zero(),
        depth: None,
        clear_color: Some(wgt::Color::RED),
        clear_depth: None,
        depth_range: 0.0..1.0,
    };
    let shader = select_draw_shader("cs_scale", &["TEXTURE_2D"], false).unwrap();
    let pipeline = DrawPipeline::new(
        &device,
        shader,
        image.format(),
        false,
        RenderState::default(),
    )
    .unwrap();
    let mut program = ProgramState::new(shader);
    store.bind(TextureSlot(0), &handle).unwrap();
    let mut transform = Transform3D::ortho(0.0, 2.0, 0.0, 2.0, -1.0, 1.0);
    transform.m11 *= 0.5;
    program.set_transform(&transform);
    let left = program
        .resolve(&pipeline, &pass, |slot| store.binding(slot), Some(&fallback))
        .unwrap();
    store.clear_color_bindings();
    transform.m41 = 0.0;
    program.set_transform(&transform);
    let right = program
        .resolve(&pipeline, &pass, |slot| store.binding(slot), Some(&fallback))
        .unwrap();
    assert!(program
        .resolve(&pipeline, &pass, |slot| store.binding(slot), None)
        .is_err());
    program.bind_samplers(&[("sColor0", TextureSlot(16))]);
    assert!(program
        .resolve(&pipeline, &pass, |slot| store.binding(slot), Some(&fallback))
        .is_err());
    program.bind_samplers(&[("sColor0", TextureSlot(0))]);
    let mut wrong = store.bindings();
    wrong[0] = Some(ShaderResource::Storage(
        Buffer::new(&device, &[0; 16], wgt::BufferUses::STORAGE_READ_ONLY).unwrap(),
    ));
    assert!(program
        .resolve(&pipeline, &pass, |slot| wrong.get(slot).cloned(), Some(&fallback))
        .is_err());
    store.delete(&mut handle).unwrap();
    let quad = Buffer::new(
        &device,
        &[0, 0, 0, 0, 255, 0, 0, 0, 0, 255, 0, 0, 255, 255, 0, 0],
        wgt::BufferUses::VERTEX,
    )
    .unwrap();
    let samplers = Rc::new(Samplers::new(&device).unwrap());
    let values = [0.0f32, 0.0, 2.0, 2.0, 0.0, 0.0, 1.0, 1.0, 0.0];
    let bytes: Vec<_> = values.iter().flat_map(|v| v.to_ne_bytes()).collect();
    pass.record_batches(
        &mut queue.recording().unwrap(),
        &queue,
        &quad,
        Some(&samplers),
        &[left.batch(&bytes, 1, rect), right.batch(&bytes, 1, rect)],
    )
    .unwrap();
    queue.wait().unwrap();
    assert_eq!(
        target.readback(rect).unwrap().wait().unwrap(),
        [0, 255, 0, 255, 0, 0, 0, 0].repeat(2)
    );
    let data_shader = shaders::SHADERS
        .iter()
        .find(|s| {
            !s.buffer_tables
                && !s.features.contains("DUAL_SOURCE_BLENDING")
                && s.textures
                    .iter()
                    .any(|b| !matches!(b.name, "sColor0" | "sColor1" | "sColor2" | "sClipMask"))
        })
        .unwrap();
    let format = if data_shader.name == "ps_quad_mask" || data_shader.features == "ALPHA_TARGET" {
        wgt::TextureFormat::R8Unorm
    } else {
        image.format()
    };
    let data_pipeline =
        DrawPipeline::new(&device, data_shader, format, false, RenderState::default()).unwrap();
    let mut data_program = ProgramState::new(data_shader);
    let bindings: Vec<_> = data_shader
        .textures
        .iter()
        .enumerate()
        .map(|(i, b)| (b.name, TextureSlot(i)))
        .collect();
    data_program.bind_samplers(&bindings);
    let error = data_program
        .resolve(&data_pipeline, &pass, |slot| store.binding(slot), Some(&fallback))
        .err()
        .unwrap();
    let missing = data_shader
        .textures
        .iter()
        .find(|b| !matches!(b.name, "sColor0" | "sColor1" | "sColor2" | "sClipMask"))
        .unwrap();
    assert!(error.contains(missing.name), "{}", error);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn render_target_invalidation_unbinds_and_discards_color_and_depth() {
    let device = device();
    let mut store = TextureStore::new(&device);
    let mut handle = create(&mut store, true);
    let (color, depth) = store.render_target(handle.target_id, true).unwrap();
    let depth = depth.unwrap();
    let pool = Rc::new(BufferPool::new(&device));
    let queue = SubmissionQueue::new(&pool, 2).unwrap();
    let rect = DeviceIntRect::from_size(DeviceIntSize::new(2, 2));
    let pass = DrawPass {
            viewport: None,
        target: &color,
        origin: DeviceIntPoint::zero(),
        depth: Some(&depth),
        clear_color: None,
        clear_depth: None,
        depth_range: 0.0..1.0,
    };
    pass.clear_rect(
        &mut queue.recording().unwrap(),
        rect,
        Some([0.0, 1.0, 0.0, 1.0]),
        Some(0.25),
    )
    .unwrap();
    queue.wait().unwrap();
    for slot in [0, 4] {
        store.bind(TextureSlot(slot), &handle).unwrap();
    }
    store
        .invalidate_render_target(&handle, &mut queue.recording().unwrap())
        .unwrap();
    assert!(!color.initialized() && !depth.initialized());
    assert!(store.bindings().iter().all(Option::is_none));
    queue.discard_recording();
    assert!(color.initialized() && depth.initialized());
    assert!(store.bindings().iter().all(Option::is_none));
    assert_eq!(
        color.readback(rect).unwrap().wait().unwrap(),
        [0, 255, 0, 255].repeat(4)
    );
    store.bind(TextureSlot(4), &handle).unwrap();
    store
        .invalidate_render_target(&handle, &mut queue.recording().unwrap())
        .unwrap();
    queue.wait().unwrap();
    assert!(!color.initialized() && !depth.initialized());
    assert!(store.bindings()[4].is_none());
    pass.clear_rect(
        &mut queue.recording().unwrap(),
        DeviceIntRect::from_size(DeviceIntSize::new(1, 1)),
        Some([0.0, 0.0, 1.0, 1.0]),
        Some(0.75),
    )
    .unwrap();
    queue.wait().unwrap();
    assert_eq!(
        color.readback(rect).unwrap().wait().unwrap(),
        [0, 0, 255, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
    );
    store.delete(&mut handle).unwrap();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
