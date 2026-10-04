/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::webgpu_timeline::{adapter, device_with_extensions};
use ash::{ext, khr};
use std::os::fd::AsFd;
use std::rc::Rc;
use webrender::vulkan::{Device, DmaBufImageDescriptor, Options};
use wgpu_bindings::server::create_webrender_dma_buf;

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
