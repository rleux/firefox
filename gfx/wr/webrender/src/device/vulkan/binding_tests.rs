/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::super::{shaders, BufferPool, Device, Options, Submission, SubmissionQueue};
use super::super::tests::{validation_logging, ERRORS};
use api::units::{DeviceIntRect, DeviceIntSize};
use crate::device::RenderState;
use std::sync::atomic::Ordering;
use webrender_build::vulkan::ShaderArtifact;

fn device() -> Rc<Device> {
    validation_logging();
    let device = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    eprintln!("Vulkan adapter: {:?}", device.info());
    device
}

fn pipeline(device: &Rc<Device>, shader: &'static ShaderArtifact) -> Rc<DrawPipeline> {
    let format = if shader.name == "ps_quad_mask" || shader.features == "ALPHA_TARGET" {
        wgt::TextureFormat::R8Unorm
    } else {
        wgt::TextureFormat::Rgba8Unorm
    };
    DrawPipeline::new(
        device,
        shader,
        format,
        false,
        RenderState::default(),
    )
    .unwrap()
}

fn textures(device: &Rc<Device>, shader: &ShaderArtifact) -> Vec<(Rc<Texture>, TextureFilter)> {
    shader
        .textures
        .iter()
        .map(|binding| {
            let format = match binding.scalar {
                ScalarType::Float if binding.name.starts_with("sColor") => {
                    wgt::TextureFormat::Rgba8Unorm
                }
                ScalarType::Float => wgt::TextureFormat::Rgba32Float,
                ScalarType::Sint => wgt::TextureFormat::Rgba32Sint,
                ScalarType::Uint => panic!("Unexpected unsigned texture {}", binding.name),
            };
            (
                Texture::new(device, 1, 1, format, TextureFilter::Nearest, false).unwrap(),
                TextureFilter::Nearest,
            )
        })
        .collect()
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn generated_draw_shaders_allocate_bindings() {
    let device = device();
    let projection = Buffer::new(&device, &[0; 64], wgt::BufferUses::UNIFORM).unwrap();
    let storage = Buffer::new(&device, &[0; 16], wgt::BufferUses::STORAGE_READ_ONLY).unwrap();
    let samplers = Rc::new(Samplers::new(&device).unwrap());
    let mut count = 0;
    for shader in shaders::SHADERS {
        if shader.features.contains("DUAL_SOURCE_BLENDING")
            && !device
                .features()
                .contains(wgt::Features::DUAL_SOURCE_BLENDING)
        {
            continue;
        }
        let pipeline = pipeline(&device, shader);
        let group = DrawBindings::new(
            &pipeline,
            Some(projection.clone()),
            textures(&device, shader),
            vec![storage.clone(); shader.storage_buffers.len()],
            Some(samplers.clone()),
        )
        .unwrap_or_else(|error| panic!("{} {}: {error}", shader.name, shader.features));
        let weak = Rc::downgrade(&pipeline);
        drop(pipeline);
        assert!(weak.upgrade().is_some());
        drop(group);
        assert!(weak.upgrade().is_none());
        count += 1;
    }
    eprintln!("Allocated bindings for {count} draw shaders");
    assert!(count > 1);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn sampled_bindings_validate_resources_and_retain_them_until_completion() {
    let device = device();
    let shader = shaders::SHADERS
        .iter()
        .find(|s| s.name == "cs_scale" && s.features == "TEXTURE_2D" && !s.buffer_tables)
        .unwrap();
    assert_eq!(shader.textures.len(), 1);
    assert_ne!(shader.textures[0].sampler_stages, 0);
    let pipeline = pipeline(&device, shader);
    let projection = Buffer::new(&device, &[0; 64], wgt::BufferUses::UNIFORM).unwrap();
    let texture = Texture::new(
        &device,
        1,
        1,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Linear,
        false,
    )
    .unwrap();
    let samplers = Rc::new(Samplers::new(&device).unwrap());
    let create = |projection, texture: Rc<Texture>, samplers| {
        DrawBindings::new(
            &pipeline,
            projection,
            vec![(texture, TextureFilter::Linear)],
            Vec::new(),
            samplers,
        )
    };
    assert!(create(None, texture.clone(), Some(samplers.clone())).is_err());
    assert!(create(Some(projection.clone()), texture.clone(), None).is_err());
    assert!(DrawBindings::new(
        &pipeline,
        Some(projection.clone()),
        Vec::new(),
        Vec::new(),
        Some(samplers.clone())
    )
    .is_err());
    let short = Buffer::new(&device, &[0; 16], wgt::BufferUses::UNIFORM).unwrap();
    assert!(create(Some(short), texture.clone(), Some(samplers.clone())).is_err());
    let integer = Texture::new(
        &device,
        1,
        1,
        wgt::TextureFormat::Rgba32Sint,
        TextureFilter::Nearest,
        false,
    )
    .unwrap();
    assert!(create(Some(projection.clone()), integer, Some(samplers.clone())).is_err());
    let foreign = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    let foreign_buffer = Buffer::new(&foreign, &[0; 64], wgt::BufferUses::UNIFORM).unwrap();
    assert!(create(
        Some(foreign_buffer),
        texture.clone(),
        Some(samplers.clone())
    )
    .is_err());
    let foreign_sampler = Rc::new(Samplers::new(&foreign).unwrap());
    assert!(create(
        Some(projection.clone()),
        texture.clone(),
        Some(foreign_sampler)
    )
    .is_err());
    let foreign_texture = Texture::new(
        &foreign,
        1,
        1,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Nearest,
        false,
    )
    .unwrap();
    assert!(create(
        Some(projection.clone()),
        foreign_texture,
        Some(samplers.clone())
    )
    .is_err());
    let group = create(
        Some(projection.clone()),
        texture.clone(),
        Some(samplers.clone()),
    )
    .unwrap();
    let mut commands_submission = Submission::new(&device).unwrap();
    let mut commands = commands_submission.recording().unwrap();
    assert!(group.prepare(&mut commands).is_err());
    assert_eq!(projection.current_usage(), wgt::BufferUses::MAP_WRITE);
    let pool = Rc::new(BufferPool::new(&device));
    let queue = SubmissionQueue::new(&pool, 3).unwrap();
    texture
        .upload(
            &queue,
            DeviceIntRect::from_size(DeviceIntSize::new(1, 1)),
            &[255, 0, 0, 255],
            None,
            0,
            None,
        )
        .unwrap();
    queue.submit().unwrap();
    queue.wait().unwrap();
    let mut foreign_commands_submission = Submission::new(&foreign).unwrap();
    let mut foreign_commands = foreign_commands_submission.recording().unwrap();
    assert!(group.prepare(&mut foreign_commands).is_err());
    drop(group);
    drop(projection);
    drop(pipeline);
    let weak_texture = Rc::downgrade(&texture);
    let weak_samplers = Rc::downgrade(&samplers);
    let pixels = super::super::tests::draw_scaled_texture(&device, texture, samplers);
    assert_eq!(pixels, [255, 0, 0, 255].repeat(4));
    assert!(weak_texture.upgrade().is_none());
    assert!(weak_samplers.upgrade().is_none());
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
