/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::vulkan::{renderer, TestVulkanImage};
use super::webgpu_import::{dirty_texture, readback};
use super::webgpu_timeline::{adapter, device_with_extensions};
use ash::{ext, khr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;
use wgpu_bindings::server::create_webrender_dma_buf;
use wgpu_bindings::vulkan_image::{import_image, release_image};
use wgpu_bindings::vulkan_timeline::{
    submit_with_timelines, VulkanTimeline, VulkanTimelineDescriptor, VulkanTimelinePoint,
};

pub struct Fixture {
    renderer: Option<webrender::Renderer>,
    device: Arc<wgc::device::Device>,
    queue: Arc<wgc::device::queue::Queue>,
    _memory: OwnedFd,
    _ready_fd: OwnedFd,
    _ready: VulkanTimeline,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(renderer) = self.renderer.take() {
            renderer.deinit();
        }
        self.device
            .poll(wgt::PollType::Wait {
                submission_index: None,
                timeout: Some(Duration::from_secs(5)),
            })
            .unwrap();
    }
}

#[no_mangle]
pub unsafe extern "C" fn wr_test_webgpu_image_new(output: &mut TestVulkanImage) -> *mut Fixture {
    let adapter = adapter();
    let (device, queue) = device_with_extensions(
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
    let usage = wgt::TextureUsages::COPY_DST | wgt::TextureUsages::TEXTURE_BINDING;
    let (info, memory) =
        create_webrender_dma_buf(device.clone(), [2, 2], wgt::TextureFormat::Rgba8Unorm, usage).unwrap();
    let desc = wgc::resource::TextureDescriptor {
        label: None,
        size: wgt::Extent3d {
            width: 2,
            height: 2,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgt::TextureDimension::D2,
        format: wgt::TextureFormat::Rgba8Unorm,
        usage,
        view_formats: vec![],
    };
    use std::os::fd::AsFd;
    let texture = import_image(device.clone(), memory.as_fd(), &desc, &info).unwrap();
    queue.write_texture(
        wgt::TexelCopyTextureInfo {
            texture: texture.clone(),
            mip_level: 0,
            origin: wgt::Origin3d::ZERO,
            aspect: wgt::TextureAspect::All,
        },
        &[255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255],
        &wgt::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(8),
            rows_per_image: Some(2),
        },
        &desc.size,
    );
    let ready = VulkanTimeline::new(device.clone()).unwrap();
    let signal = ready.export().unwrap();
    let ready_fd = OwnedFd::from_raw_fd(signal.fd);
    assert!(release_image(&queue, texture.clone(), &ready, 1).is_some());
    assert!(texture.as_hal::<wgc::api::Vulkan>().is_none());
    *output = TestVulkanImage {
        memory_fd: memory.as_raw_fd(),
        ready_fd: ready_fd.as_raw_fd(),
        offset: info.layout.offsets[0],
        stride: info.layout.strides[0],
        device_uuid: info.device_uuid,
        driver_uuid: info.driver_uuid,
    };
    Box::into_raw(Box::new(Fixture {
        renderer: Some(renderer()),
        device,
        queue,
        _memory: memory,
        _ready_fd: ready_fd,
        _ready: ready,
    }))
}

#[no_mangle]
pub extern "C" fn wr_test_webgpu_image_renderer(fixture: &mut Fixture) -> &mut webrender::Renderer {
    fixture.renderer.as_mut().unwrap()
}

#[no_mangle]
pub extern "C" fn wr_test_webgpu_image_submit(fixture: &mut Fixture) {
    fixture.renderer.take().unwrap().deinit();
}

#[no_mangle]
pub unsafe extern "C" fn wr_test_webgpu_image_wait(
    fixture: &Fixture,
    fd: i32,
    device_uuid: &[u8; 16],
    driver_uuid: &[u8; 16],
    value: u64,
) -> bool {
    let timeline = VulkanTimeline::import(
        fixture.device.clone(),
        &VulkanTimelineDescriptor {
            fd,
            device_uuid: *device_uuid,
            driver_uuid: *driver_uuid,
        },
    )
    .unwrap();
    let index = submit_with_timelines(
        &fixture.queue,
        &[],
        &[VulkanTimelinePoint {
            timeline: Some(&timeline),
            value,
        }],
        &[],
    )
    .unwrap();
    fixture
        .device
        .poll(wgt::PollType::Wait {
            submission_index: Some(index),
            timeout: Some(Duration::from_secs(5)),
        })
        .is_ok()
}

#[no_mangle]
pub unsafe extern "C" fn wr_test_webgpu_image_delete(fixture: *mut Fixture) {
    drop(Box::from_raw(fixture));
}

#[no_mangle]
pub unsafe extern "C" fn wr_test_webgpu_read_transition_initializes() -> bool {
    let (device, queue) = device_with_extensions(&adapter(), &[]);
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
        format: wgt::TextureFormat::Rgba8Unorm,
        usage: wgt::TextureUsages::COPY_SRC,
        view_formats: vec![],
    };
    let raw = dirty_texture(&device, &queue, false);
    let (texture, error) = device.create_texture_from_hal(Box::new(raw), &desc, wgt::TextureUses::COPY_DST, false);
    assert!(error.is_none());
    let encoder = device.create_command_encoder(&Default::default());
    encoder.transition_resources(
        std::iter::empty(),
        std::iter::once(wgt::TextureTransition {
            texture: texture.clone(),
            selector: None,
            state: wgt::TextureUses::RESOURCE,
        }),
    );
    let index = queue.submit(&[encoder.finish(&Default::default())]);
    device
        .poll(wgt::PollType::Wait {
            submission_index: Some(index),
            timeout: Some(Duration::from_secs(5)),
        })
        .unwrap();
    let hal = device.clone().as_hal::<wgc::api::Vulkan>().unwrap();
    let borrowed = texture.clone().as_hal::<wgc::api::Vulkan>().unwrap();
    let alias = wgh::vulkan::Device::texture_from_raw(
        &hal,
        borrowed.raw_handle(),
        &wgh::TextureDescriptor {
            label: None,
            size: desc.size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: desc.dimension,
            format: desc.format,
            usage: wgt::TextureUses::COPY_SRC | wgt::TextureUses::COPY_DST | wgt::TextureUses::RESOURCE,
            memory_flags: wgh::MemoryFlags::empty(),
            view_formats: vec![],
        },
        Some(Box::new(move || drop(texture))),
        wgh::vulkan::TextureMemory::External,
    );
    drop(borrowed);
    let (alias, error) = device.create_texture_from_hal(Box::new(alias), &desc, wgt::TextureUses::RESOURCE, true);
    assert!(error.is_none());
    let bytes = readback(&device, &queue, alias).unwrap();
    for row in bytes.chunks(256) {
        if row[..16] != [0; 16] {
            return false;
        }
    }
    true
}
