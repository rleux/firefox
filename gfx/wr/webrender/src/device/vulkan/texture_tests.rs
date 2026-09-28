/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::super::TextureFilter;

pub(super) fn clear_and_read(device: &Rc<Device>, texture: Rc<Texture>) -> Vec<u8> {
    let remaining_references = Rc::strong_count(&texture) - 1;
    let size = texture.size();
    let depth = texture.format() == wgt::TextureFormat::Depth32Float;
    let alignment = device.capabilities().alignments.buffer_copy_pitch.get();
    let pitch = (u64::from(size.width) * 4).div_ceil(alignment) * alignment;
    let bytes = pitch * u64::from(size.height);
    let target = unsafe {
        device.raw_device().create_buffer(&hal::BufferDescriptor {
            label: Some("WR texture test readback"),
            size: bytes,
            usage: wgt::BufferUses::COPY_DST | wgt::BufferUses::MAP_READ,
            memory_flags: hal::MemoryFlags::PREFER_COHERENT,
        })
    }
    .unwrap();
    let target = Rc::new(super::super::resources::Owned::new(
        device,
        target,
        hal::vulkan::Device::destroy_buffer,
    ));
    let mut submission = Submission::new(device).unwrap();
    let mut commands = submission.recording().unwrap();
    let usage = if depth {
        wgt::TextureUses::DEPTH_WRITE
    } else {
        wgt::TextureUses::COLOR_TARGET
    };
    unsafe {
        texture.transition(&mut commands, usage).unwrap();
        let encoder = commands.encoder();
        let attachment = || hal::Attachment {
            view: texture.target_view().unwrap(),
            usage,
        };
        let color = if depth {
            None
        } else {
            Some(hal::ColorAttachment {
                target: attachment(),
                depth_slice: None,
                resolve_target: None,
                ops: hal::AttachmentOps::LOAD_CLEAR | hal::AttachmentOps::STORE,
                clear_value: wgt::Color {
                    r: 64.0 / 255.0,
                    g: 128.0 / 255.0,
                    b: 192.0 / 255.0,
                    a: 1.0,
                },
            })
        };
        let depth_attachment = if depth {
            Some(hal::DepthStencilAttachment {
                depth_read_only: false,
                stencil_read_only: true,
                target: attachment(),
                depth_ops: hal::AttachmentOps::LOAD_CLEAR | hal::AttachmentOps::STORE,
                stencil_ops: hal::AttachmentOps::LOAD_DONT_CARE | hal::AttachmentOps::STORE_DISCARD,
                clear_value: (0.25, 0),
            })
        } else {
            None
        };
        encoder
            .begin_render_pass(&hal::RenderPassDescriptor {
                label: Some("WR texture clear test"),
                extent: size,
                sample_count: 1,
                color_attachments: &[color],
                depth_stencil_attachment: depth_attachment,
                multiview_mask: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            })
            .unwrap();
        encoder.end_render_pass();
        texture.initialize(&mut commands).unwrap();
        texture
            .transition(&mut commands, wgt::TextureUses::COPY_SRC)
            .unwrap();
        let encoder = commands.encoder();
        encoder.copy_texture_to_buffer(
            texture.raw_texture(),
            wgt::TextureUses::COPY_SRC,
            &target,
            std::iter::once(hal::BufferTextureCopy {
                buffer_layout: wgt::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(pitch as u32),
                    rows_per_image: Some(size.height),
                },
                texture_base: hal::TextureCopyBase {
                    mip_level: 0,
                    array_layer: 0,
                    origin: wgt::Origin3d::ZERO,
                    aspect: if depth {
                        hal::FormatAspects::DEPTH
                    } else {
                        hal::FormatAspects::COLOR
                    },
                },
                size: size.into(),
            }),
        );
        encoder.transition_buffers(std::iter::once(hal::BufferBarrier {
            buffer: &**target,
            usage: hal::StateTransition {
                from: wgt::BufferUses::COPY_DST,
                to: wgt::BufferUses::MAP_READ,
            },
        }));
    }
    let weak = Rc::downgrade(&texture);
    drop(texture);
    commands.keep(target.clone());
    drop(commands);
    submission.submit().unwrap();
    assert!(weak.upgrade().is_some());
    assert!(submission
        .wait(Some(std::time::Duration::from_secs(10)))
        .unwrap());
    assert_eq!(weak.strong_count(), remaining_references);
    unsafe {
        let raw = device.raw_device();
        let mapping = raw.map_buffer(&target, 0..bytes).unwrap();
        if !mapping.is_coherent {
            raw.invalidate_mapped_ranges(&target, std::iter::once(0..bytes));
        }
        let mut pixels = Vec::new();
        for row in 0..size.height {
            let start = mapping.ptr.as_ptr().add(row as usize * pitch as usize);
            pixels.extend_from_slice(std::slice::from_raw_parts(start, size.width as usize * 4));
        }
        raw.unmap_buffer(&target);
        pixels
    }
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn owned_textures_validate_and_clear() {
    validation_logging();
    let device = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    eprintln!("Vulkan adapter: {:?}", device.info());
    for format in [
        wgt::TextureFormat::Rgba8Unorm,
        wgt::TextureFormat::Bgra8Unorm,
        wgt::TextureFormat::R8Unorm,
        wgt::TextureFormat::Rg8Unorm,
        wgt::TextureFormat::R16Unorm,
        wgt::TextureFormat::Rg16Unorm,
        wgt::TextureFormat::Rgba32Float,
        wgt::TextureFormat::Rgba32Sint,
    ] {
        let texture = Texture::new(&device, 7, 5, format, TextureFilter::Nearest, false);
        if !device.features().contains(format.required_features()) {
            assert!(texture.is_err());
            continue;
        }
        let texture = texture.unwrap();
        assert_eq!(texture.format(), format);
        assert_eq!(texture.size().width, 7);
        assert_eq!(texture.filter(), TextureFilter::Nearest);
        assert_eq!(texture.mip_count(), 1);
        assert!(texture.target_view().is_none());
    }
    let limit = device.capabilities().limits.max_texture_dimension_2d;
    for (width, height) in [(0, 1), (1, 0), (limit + 1, 1), (1, limit + 1)] {
        assert!(Texture::new(
            &device,
            width,
            height,
            wgt::TextureFormat::Rgba8Unorm,
            TextureFilter::Nearest,
            false
        )
        .is_err());
    }
    for (format, filter, renderable) in [
        (
            wgt::TextureFormat::Bc1RgbaUnorm,
            TextureFilter::Nearest,
            false,
        ),
        (wgt::TextureFormat::Rgba32Sint, TextureFilter::Linear, false),
        (
            wgt::TextureFormat::Depth32Float,
            TextureFilter::Nearest,
            false,
        ),
        (
            wgt::TextureFormat::Depth32Float,
            TextureFilter::Trilinear,
            true,
        ),
    ] {
        assert!(Texture::new(&device, 7, 5, format, filter, renderable).is_err());
    }
    let mipmapped = Texture::new(
        &device,
        7,
        5,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Trilinear,
        false,
    )
    .unwrap();
    assert_eq!(mipmapped.mip_count(), 3);
    assert!(mipmapped.target_view().is_some());
    drop(mipmapped);
    for (format, pixel) in [
        (wgt::TextureFormat::Rgba8Unorm, [64, 128, 192, 255]),
        (wgt::TextureFormat::Bgra8Unorm, [192, 128, 64, 255]),
        (wgt::TextureFormat::Depth32Float, 0.25f32.to_ne_bytes()),
    ] {
        let texture = Texture::new(&device, 7, 5, format, TextureFilter::Nearest, true).unwrap();
        let pixels = clear_and_read(&device, texture);
        assert_eq!(pixels.len(), 7 * 5 * 4);
        assert!(pixels.chunks_exact(4).all(|actual| actual == pixel));
    }
    assert_eq!(Rc::strong_count(&device), 1);
    let texture = Texture::new(
        &device,
        1,
        1,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Linear,
        true,
    )
    .unwrap();
    let weak_device = Rc::downgrade(&device);
    drop(device);
    assert!(weak_device.upgrade().is_some());
    drop(texture);
    assert!(weak_device.upgrade().is_none());
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
