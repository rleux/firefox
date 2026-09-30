/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::super::{shaders, BufferPool, Device, Options};
use super::super::shader::select_draw_shader;
use super::super::tests::{validation_logging, ERRORS};
use crate::device::{BlendMode, RenderState};
use api::units::{DeviceIntRect, DeviceIntSize};
use std::sync::atomic::Ordering;

#[test]
fn generated_resource_lists_fit_inline_storage() {
    for shader in shaders::SHADERS {
        assert!(
            shader.textures.len() <= 16,
            "{} {}",
            shader.name,
            shader.features
        );
        assert!(
            shader.storage_buffers.len() <= 4,
            "{} {}",
            shader.name,
            shader.features
        );
    }
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn cache_hits_share_resources_across_pipelines_and_offsets() {
    validation_logging();
    let device = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Default::default()
        })
        .unwrap(),
    );
    let pool = Rc::new(BufferPool::new(&device));
    let queue = SubmissionQueue::new(&pool, 2).unwrap();
    let shader = select_draw_shader("cs_scale", &["TEXTURE_2D"], false).unwrap();
    let pipeline = DrawPipeline::new(
        &device,
        shader,
        wgt::TextureFormat::Rgba8Unorm,
        false,
        RenderState::default(),
    )
    .unwrap();
    let blended = DrawPipeline::new(
        &device,
        shader,
        wgt::TextureFormat::Rgba8Unorm,
        false,
        RenderState {
            blend_mode: BlendMode::PremultipliedAlpha,
            ..Default::default()
        },
    )
    .unwrap();
    let texture = Texture::new(
        &device,
        1,
        1,
        pipeline.format,
        TextureFilter::Nearest,
        false,
    )
    .unwrap();
    texture
        .upload(
            &queue,
            DeviceIntRect::from_size(DeviceIntSize::new(1, 1)),
            &[255; 4],
            None,
            0,
            None,
        )
        .unwrap();
    let samplers = Rc::new(Samplers::new(&device).unwrap());
    let mut cache = BindingCache::default();
    let mut commands = queue.recording().unwrap();
    let mut projection = [0.0; 16];
    let textures = [(texture.clone(), TextureFilter::Nearest)];
    let first = cache
        .resolve(
            &mut commands,
            &queue,
            &pipeline,
            Some(&projection),
            &textures,
            &[],
            Some(&samplers),
        )
        .unwrap();
    let texture_refs = Rc::strong_count(&texture);
    let sampler_refs = Rc::strong_count(&samplers);
    projection[12] = 1.0;
    let second = cache
        .resolve(
            &mut commands,
            &queue,
            &blended,
            Some(&projection),
            &textures,
            &[],
            Some(&samplers),
        )
        .unwrap();
    assert!(Rc::ptr_eq(&first.resources, &second.resources));
    assert!(!Rc::ptr_eq(&first.pipeline, &second.pipeline));
    assert_ne!(first.projection_offset, second.projection_offset);
    for _ in 0..64 {
        let hit = cache
            .resolve(
                &mut commands,
                &queue,
                &pipeline,
                Some(&projection),
                &textures,
                &[],
                Some(&samplers),
            )
            .unwrap();
        assert!(Rc::ptr_eq(&first.resources, &hit.resources));
        assert_eq!(second.projection_offset, hit.projection_offset);
        assert_eq!(Rc::strong_count(&texture), texture_refs);
        assert_eq!(Rc::strong_count(&samplers), sampler_refs);
    }
    assert_eq!(cache.bindings.len(), 1);
    let key = cache.bindings.keys().next().unwrap();
    assert!(!key.textures.spilled() && !key.buffers.spilled());
    let filtered = cache
        .resolve(
            &mut commands,
            &queue,
            &pipeline,
            Some(&projection),
            &[(texture.clone(), TextureFilter::Linear)],
            &[],
            Some(&samplers),
        )
        .unwrap();
    assert!(!Rc::ptr_eq(&first.resources, &filtered.resources));
    drop(filtered);
    let weak_pipeline = Rc::downgrade(&pipeline);
    let weak_texture = Rc::downgrade(&texture);
    let weak_resources = Rc::downgrade(&second.resources);
    drop(first);
    drop(pipeline);
    assert!(weak_pipeline.upgrade().is_none());
    cache.clear();
    drop(textures);
    drop(texture);
    second.prepare(&mut commands).unwrap();
    drop(second);
    assert!(weak_resources.upgrade().is_some());
    assert!(weak_texture.upgrade().is_some());
    drop(commands);
    queue.wait().unwrap();
    assert!(weak_resources.upgrade().is_none());
    assert!(weak_texture.upgrade().is_none());
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn recent_generations_reuse_bindings_across_the_capacity_boundary() {
    validation_logging();
    let device = Rc::new(Device::new(&Options { validation: true, ..Default::default() }).unwrap());
    let queue = SubmissionQueue::new(&Rc::new(BufferPool::new(&device)), 2).unwrap();
    let pipeline = DrawPipeline::new(&device, select_draw_shader("cs_scale", &["TEXTURE_2D"], false).unwrap(),
        wgt::TextureFormat::Rgba8Unorm, false, RenderState::default()).unwrap();
    let textures: Vec<_> = (0..257).map(|_| Texture::new(&device, 1, 1, pipeline.format, TextureFilter::Nearest, false).unwrap()).collect();
    let samplers = Rc::new(Samplers::new(&device).unwrap());
    let mut cache = BindingCache::default();
    let mut commands = queue.recording().unwrap();
    device.trace.borrow_mut().clear();
    for _ in 0..6 {
        for texture in &textures {
            cache.resolve(&mut commands, &queue, &pipeline, Some(&[0.0; 16]),
                &[(texture.clone(), TextureFilter::Nearest)], &[], Some(&samplers)).unwrap();
            assert!(cache.bindings.len() <= 256);
            assert!(cache.bindings.len() + cache.previous_bindings.len() <= 512);
        }
    }
    assert_eq!(device.trace.borrow().iter().filter(|c| matches!(c, super::super::tests::Command::BindGroup)).count(), 257);
    cache.clear();
    assert!(cache.bindings.is_empty() && cache.previous_bindings.is_empty());
    drop(commands);
    queue.wait().unwrap();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
