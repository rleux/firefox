/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::webgpu_import::readback;
use super::webgpu_timeline::{adapter, device_with_extensions};
use ash::{ext, khr};
use std::os::fd::AsFd;
use std::rc::Rc;
use webrender::vulkan::{Device, DmaBufImageDescriptor, Options};
use wgpu_bindings::server::create_webrender_dma_buf;
use wgpu_bindings::vulkan_image::import_image;

#[no_mangle]
pub extern "C" fn wr_test_webgpu_dmabuf_allocation() {
    let adapter = adapter();
    let (unsupported, _queue) = device_with_extensions(&adapter, &[]);
    assert!(create_webrender_dma_buf(
        unsupported,
        [4, 3],
        wgt::TextureFormat::Rgba8Unorm,
        wgt::TextureUsages::RENDER_ATTACHMENT
    )
    .is_err());
    let (producer, _queue) = device_with_extensions(
        &adapter,
        &[
            khr::external_memory_fd::NAME,
            ext::external_memory_dma_buf::NAME,
            ext::image_drm_format_modifier::NAME,
            khr::external_semaphore_fd::NAME,
            khr::dedicated_allocation::NAME,
            khr::get_memory_requirements2::NAME,
        ],
    );
    let consumer = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Default::default()
        })
        .unwrap(),
    );
    for format in [wgt::TextureFormat::Rgba8Unorm, wgt::TextureFormat::Bgra8Unorm] {
        for usage in [
            wgt::TextureUsages::empty(),
            wgt::TextureUsages::RENDER_ATTACHMENT,
            wgt::TextureUsages::COPY_SRC | wgt::TextureUsages::TEXTURE_BINDING,
            wgt::TextureUsages::RENDER_ATTACHMENT | wgt::TextureUsages::COPY_SRC | wgt::TextureUsages::COPY_DST,
        ] {
            let (info, fd) = create_webrender_dma_buf(producer.clone(), [37, 19], format, usage).unwrap();
            assert!(info.layout.is_valid);
            assert_eq!(info.layout.plane_count, 1);
            assert_eq!(info.rgba, format == wgt::TextureFormat::Rgba8Unorm);
            assert!(info.copy_dst);
            assert_eq!(info.copy_src, usage.contains(wgt::TextureUsages::COPY_SRC));
            assert_eq!(info.color_target, usage.contains(wgt::TextureUsages::RENDER_ATTACHMENT));
            let mut uses = wgt::TextureUses::RESOURCE | wgt::TextureUses::COPY_DST;
            if info.copy_src {
                uses |= wgt::TextureUses::COPY_SRC;
            }
            if info.color_target {
                uses |= wgt::TextureUses::COLOR_TARGET;
            }
            let image = unsafe {
                consumer.import_dma_buf(
                    fd.as_fd(),
                    DmaBufImageDescriptor {
                        size: [37, 19],
                        format,
                        usage: uses,
                        modifier: info.layout.modifier,
                        offset: info.layout.offsets[0],
                        row_pitch: info.layout.strides[0],
                        device_uuid: info.device_uuid,
                        driver_uuid: info.driver_uuid,
                    },
                )
            }
            .unwrap();
            assert_eq!(image.descriptor().size, [37, 19]);
            assert!(fd.try_clone().is_ok());
            drop(fd);
            drop(image);
        }
    }
    for (size, format, usage) in [
        (
            [0, 3],
            wgt::TextureFormat::Rgba8Unorm,
            wgt::TextureUsages::RENDER_ATTACHMENT,
        ),
        (
            [4, u32::MAX],
            wgt::TextureFormat::Bgra8Unorm,
            wgt::TextureUsages::RENDER_ATTACHMENT,
        ),
        (
            [4, 3],
            wgt::TextureFormat::Rgba16Float,
            wgt::TextureUsages::RENDER_ATTACHMENT,
        ),
        (
            [4, 3],
            wgt::TextureFormat::Rgba8Unorm,
            wgt::TextureUsages::STORAGE_BINDING,
        ),
    ] {
        assert!(create_webrender_dma_buf(producer.clone(), size, format, usage).is_err());
    }
    producer.destroy();
    assert!(create_webrender_dma_buf(
        producer,
        [4, 3],
        wgt::TextureFormat::Rgba8Unorm,
        wgt::TextureUsages::RENDER_ATTACHMENT
    )
    .is_err());
}

#[no_mangle]
pub unsafe extern "C" fn wr_test_webgpu_dmabuf_import() {
    let adapter = adapter();
    let (producer, queue) = device_with_extensions(
        &adapter,
        &[
            khr::external_memory_fd::NAME,
            ext::external_memory_dma_buf::NAME,
            ext::image_drm_format_modifier::NAME,
            khr::external_semaphore_fd::NAME,
            khr::dedicated_allocation::NAME,
            khr::get_memory_requirements2::NAME,
        ],
    );
    for format in [wgt::TextureFormat::Rgba8Unorm, wgt::TextureFormat::Bgra8Unorm] {
        for attachment in [false, true] {
            let mut usage = wgt::TextureUsages::COPY_SRC | wgt::TextureUsages::COPY_DST;
            if attachment {
                usage |= wgt::TextureUsages::RENDER_ATTACHMENT;
            }
            let (mut info, fd) = create_webrender_dma_buf(producer.clone(), [4, 3], format, usage).unwrap();
            let desc = wgc::resource::TextureDescriptor {
                label: None,
                size: wgt::Extent3d {
                    width: 4,
                    height: 3,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgt::TextureDimension::D2,
                format,
                usage,
                view_formats: vec![],
            };
            info.device_uuid[0] ^= 1;
            assert!(import_image(producer.clone(), fd.as_fd(), &desc, &info).is_err());
            info.device_uuid[0] ^= 1;
            info.driver_uuid[15] ^= 1;
            assert!(import_image(producer.clone(), fd.as_fd(), &desc, &info).is_err());
            info.driver_uuid[15] ^= 1;
            info.copy_dst = false;
            assert!(import_image(producer.clone(), fd.as_fd(), &desc, &info).is_err());
            info.copy_dst = true;
            info.layout.plane_count = 2;
            assert!(import_image(producer.clone(), fd.as_fd(), &desc, &info).is_err());
            info.layout.plane_count = 1;
            let stride = info.layout.strides[0];
            info.layout.strides[0] = 0;
            assert!(import_image(producer.clone(), fd.as_fd(), &desc, &info).is_err());
            info.layout.strides[0] = stride;
            let mut bad = desc.clone();
            bad.size.width = 0;
            assert!(import_image(producer.clone(), fd.as_fd(), &bad, &info).is_err());
            bad = desc.clone();
            bad.usage |= wgt::TextureUsages::STORAGE_BINDING;
            assert!(import_image(producer.clone(), fd.as_fd(), &bad, &info).is_err());

            let texture = import_image(producer.clone(), fd.as_fd(), &desc, &info).unwrap();
            assert!(fd.try_clone().is_ok());
            drop(fd);
            let bytes = readback(&producer, &queue, texture.clone()).unwrap();
            for row in bytes.chunks(256) {
                assert_eq!(&row[..16], &[0; 16]);
            }
            queue.write_texture(
                wgt::TexelCopyTextureInfo {
                    texture: texture.clone(),
                    mip_level: 0,
                    origin: wgt::Origin3d { x: 1, y: 1, z: 0 },
                    aspect: wgt::TextureAspect::All,
                },
                &[0, 255, 0, 255],
                &wgt::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: None,
                    rows_per_image: None,
                },
                &wgt::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
            );
            let bytes = readback(&producer, &queue, texture).unwrap();
            for y in 0..3 {
                for x in 0..4 {
                    let expected = if x == 1 && y == 1 { [0, 255, 0, 255] } else { [0; 4] };
                    assert_eq!(&bytes[y * 256 + x * 4..y * 256 + x * 4 + 4], &expected);
                }
            }
        }
    }
}
