/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::webgpu_timeline::{adapter, device_with_extensions};
use ash::vk;
use std::sync::{mpsc, Arc};
use std::time::Duration;

fn wait(device: &Arc<wgc::device::Device>, index: u64) {
    device
        .poll(wgt::PollType::Wait {
            submission_index: Some(index),
            timeout: Some(Duration::from_secs(5)),
        })
        .unwrap();
}

unsafe fn dirty_texture(
    device: &Arc<wgc::device::Device>,
    queue: &Arc<wgc::device::queue::Queue>,
    render_pass: bool,
) -> wgh::vulkan::Texture {
    let hal = device.clone().as_hal::<wgc::api::Vulkan>().unwrap();
    let raw = hal.raw_device();
    let mut usage = vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST;
    let mut uses = wgt::TextureUses::COPY_SRC | wgt::TextureUses::COPY_DST;
    if render_pass {
        usage |= vk::ImageUsageFlags::COLOR_ATTACHMENT;
        uses |= wgt::TextureUses::COLOR_TARGET;
    }
    let image = raw
        .create_image(
            &vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(vk::Format::R8G8B8A8_UNORM)
                .extent(vk::Extent3D {
                    width: 4,
                    height: 3,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(usage),
            None,
        )
        .unwrap();
    let requirements = raw.get_image_memory_requirements(image);
    let properties = hal
        .shared_instance()
        .raw_instance()
        .get_physical_device_memory_properties(hal.raw_physical_device());
    let index = (0..properties.memory_type_count)
        .find(|&index| {
            requirements.memory_type_bits & (1 << index) != 0
                && !properties.memory_types[index as usize]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::PROTECTED)
        })
        .unwrap();
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let memory = raw
        .allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(requirements.size)
                .memory_type_index(index)
                .push_next(&mut dedicated),
            None,
        )
        .unwrap();
    raw.bind_image_memory(image, memory, 0).unwrap();
    let encoder = device.create_command_encoder(&Default::default());
    encoder.as_hal_mut::<wgc::api::Vulkan, _, _>(|encoder| {
        let command = encoder.unwrap().raw_handle();
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        raw.cmd_pipeline_barrier(
            command,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[vk::ImageMemoryBarrier::default()
                .image(image)
                .subresource_range(range)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)],
        );
        raw.cmd_clear_color_image(
            command,
            image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &vk::ClearColorValue {
                float32: [1.0, 0.0, 0.0, 1.0],
            },
            &[range],
        );
    });
    wait(device, queue.submit(&[encoder.finish(&Default::default())]));
    wgh::vulkan::Device::texture_from_raw(
        &hal,
        image,
        &wgh::TextureDescriptor {
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
            usage: uses,
            memory_flags: wgh::MemoryFlags::empty(),
            view_formats: vec![],
        },
        None,
        wgh::vulkan::TextureMemory::Dedicated(memory),
    )
}

pub(super) fn readback(
    device: &Arc<wgc::device::Device>,
    queue: &Arc<wgc::device::queue::Queue>,
    texture: Arc<wgc::resource::Texture>,
) -> Option<Vec<u8>> {
    let buffer = device.create_buffer(&wgc::resource::BufferDescriptor {
        label: None,
        size: 256 * 3,
        usage: wgt::BufferUsages::COPY_DST | wgt::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let encoder = device.create_command_encoder(&Default::default());
    encoder.copy_texture_to_buffer(
        &wgt::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgt::Origin3d::ZERO,
            aspect: wgt::TextureAspect::All,
        },
        &wgt::TexelCopyBufferInfo {
            buffer: buffer.clone(),
            layout: wgt::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(256),
                rows_per_image: Some(3),
            },
        },
        &wgt::Extent3d {
            width: 4,
            height: 3,
            depth_or_array_layers: 1,
        },
    );
    let commands = encoder.finish(&Default::default());
    for filter in [
        wgt::error::ErrorFilter::Validation,
        wgt::error::ErrorFilter::OutOfMemory,
        wgt::error::ErrorFilter::Internal,
    ] {
        device.push_error_scope(filter);
    }
    let index = queue.submit(&[commands]);
    let mut failed = false;
    for _ in 0..3 {
        if let Some(error) = device.pop_error_scope().unwrap() {
            eprintln!("Imported texture readback: {error:?}");
            failed = true;
        }
    }
    if failed || !device.is_valid() {
        return None;
    }
    wait(device, index);
    let (sender, receiver) = mpsc::channel();
    buffer.map_async(
        0,
        None,
        wgc::resource::BufferMapOperation {
            host: wgc::device::HostMap::Read,
            callback: Some(Box::new(move |result| sender.send(result).unwrap())),
        },
    );
    device.poll(wgt::PollType::Poll).unwrap();
    receiver.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
    let (ptr, size) = buffer.get_mapped_range(0, None).unwrap();
    let bytes = unsafe { std::slice::from_raw_parts(ptr.as_ptr(), size as usize) }.to_vec();
    buffer.unmap();
    Some(bytes)
}

#[no_mangle]
pub unsafe extern "C" fn wr_test_webgpu_import_initialization() -> bool {
    let adapter = adapter();
    let (device, queue) = device_with_extensions(&adapter, &[]);
    for render_pass in [false, true] {
        for cleared in [true, false] {
            for partial_write in [false, true] {
                let raw = dirty_texture(&device, &queue, render_pass);
                let mut usage = wgt::TextureUsages::COPY_SRC;
                if render_pass {
                    usage |= wgt::TextureUsages::RENDER_ATTACHMENT;
                }
                if partial_write {
                    usage |= wgt::TextureUsages::COPY_DST;
                }
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
                    usage,
                    view_formats: vec![],
                };
                let (texture, error) =
                    device.create_texture_from_hal(Box::new(raw), &desc, wgt::TextureUses::COPY_DST, cleared);
                assert!(error.is_none());
                if partial_write {
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
                }
                let Some(bytes) = readback(&device, &queue, texture) else {
                    eprintln!("render_pass={render_pass}, cleared={cleared}, partial_write={partial_write}");
                    return false;
                };
                for y in 0..3 {
                    for x in 0..4 {
                        let expected = if partial_write && x == 1 && y == 1 {
                            [0, 255, 0, 255]
                        } else if cleared {
                            [255, 0, 0, 255]
                        } else {
                            [0; 4]
                        };
                        assert_eq!(&bytes[y * 256 + x * 4..y * 256 + x * 4 + 4], &expected);
                    }
                }
            }
        }
    }
    for render_pass in [false, true] {
        let mut usage = wgt::TextureUsages::COPY_SRC | wgt::TextureUsages::COPY_DST;
        if render_pass {
            usage |= wgt::TextureUsages::RENDER_ATTACHMENT;
        }
        let texture = device.create_texture(&wgc::resource::TextureDescriptor {
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
            usage,
            view_formats: vec![],
        });
        let bytes = readback(&device, &queue, texture).unwrap();
        for row in bytes.chunks(256) {
            assert_eq!(&row[..16], &[0; 16]);
        }
    }
    true
}
