/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::vulkan::{hal, Options, Recording, SharedTimeline, Submission};
use crate::device::vulkan::resources::Owned;
use crate::device::vulkan::tests::{map_upload, validation_logging, ERRORS};
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::sync::atomic::Ordering;
use std::time::Duration;
use wgpu_hal::{CommandEncoder as _, Device as _};

#[cfg(wr_vulkan_shaders)]
#[path = "dmabuf_access_tests.rs"]
mod access;

fn descriptor() -> DmaBufImageDescriptor {
    DmaBufImageDescriptor {
        size: [3, 2],
        format: wgt::TextureFormat::Rgba8Unorm,
        usage: wgt::TextureUses::RESOURCE,
        modifier: 0,
        offset: 128,
        row_pitch: 64,
        device_uuid: [0; 16],
        driver_uuid: [0; 16],
    }
}

#[test]
fn dma_buf_linear_bounds_include_offset_and_last_pixel() {
    let mut desc = descriptor();
    assert!(desc.validate_plane(203).is_err());
    assert!(desc.validate_plane(204).is_ok());
    desc.size = [1, 1];
    desc.row_pitch = 4096;
    assert!(desc.validate_plane(132).is_ok());
    desc.offset = u64::MAX - 3;
    assert!(desc.validate_plane(u64::MAX).is_err());
    desc.offset = 0;
    desc.size = [1, 3];
    desc.row_pitch = u64::MAX - 3;
    assert!(desc.validate_plane(u64::MAX).is_err());
}

#[test]
fn dma_buf_plane_checks_preserve_opaque_modifier_layouts() {
    let mut desc = descriptor();
    for (pitch, offset) in [(0, 0), (8, 0), (13, 0), (16, 1)] {
        desc.row_pitch = pitch;
        desc.offset = offset;
        assert!(desc.validate_plane(4096).is_err());
    }
    desc.modifier = 1;
    desc.row_pitch = 1;
    desc.offset = 7;
    assert!(desc.validate_plane(8).is_ok());
    assert!(desc.validate_plane(7).is_err());
    desc.size[0] = 0;
    assert!(desc.validate_plane(8).is_err());
}

#[test]
fn dma_buf_memory_types_intersect_and_exclude_protected_memory() {
    let mut properties = vk::PhysicalDeviceMemoryProperties::default();
    properties.memory_type_count = 3;
    properties.memory_types[0].property_flags = vk::MemoryPropertyFlags::PROTECTED;
    properties.memory_types[1].property_flags = vk::MemoryPropertyFlags::DEVICE_LOCAL;
    assert_eq!(memory_type(0b111 & 0b011, &properties).unwrap(), 1);
    assert!(memory_type(1, &properties).is_err());
    assert!(memory_type(0b100 & 0b011, &properties).is_err());
}

fn device() -> Rc<Device> {
    validation_logging();
    Rc::new(
        Device::new(&Options {
            validation: true,
            ..Default::default()
        })
        .unwrap(),
    )
}

fn export_image(
    owner: &Rc<Device>,
    format: wgt::TextureFormat,
    modifier: u64,
    size: [u32; 2],
) -> (Rc<DmaBufImage>, OwnedFd) {
    let usage =
        wgt::TextureUses::RESOURCE | wgt::TextureUses::COPY_SRC | wgt::TextureUses::COPY_DST;
    let (format_raw, usage_raw, _) = image_parameters(format, usage).unwrap();
    let modifiers = [modifier];
    let mut drm =
        vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&modifiers);
    let mut external = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(format_raw)
        .extent(vk::Extent3D {
            width: size[0],
            height: size[1],
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(usage_raw)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut external)
        .push_next(&mut drm);
    let raw = owner.open.device.raw_device();
    let image = unsafe { raw.create_image(&info, None) }.unwrap();
    let (device_uuid, driver_uuid) = identity(owner);
    let mut output = DmaBufImage {
        owner: owner.clone(),
        image,
        memory: vk::DeviceMemory::null(),
        states: Rc::new(vec![UsageState::new(TextureState {
            usage: wgt::TextureUses::UNINITIALIZED,
            initialized: false,
        })]),
        descriptor: DmaBufImageDescriptor {
            size,
            format,
            usage,
            modifier,
            row_pitch: 0,
            offset: 0,
            device_uuid,
            driver_uuid,
        },
    };
    unsafe {
        let requirements = raw.get_image_memory_requirements(image);
        let properties = owner
            .open
            .device
            .shared_instance()
            .raw_instance()
            .get_physical_device_memory_properties(owner.open.device.raw_physical_device());
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let mut export = vk::ExportMemoryAllocateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        output.memory = raw
            .allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(requirements.size)
                    .memory_type_index(
                        memory_type(requirements.memory_type_bits, &properties).unwrap(),
                    )
                    .push_next(&mut dedicated)
                    .push_next(&mut export),
                None,
            )
            .unwrap();
        raw.bind_image_memory(image, output.memory, 0).unwrap();
        let layout = raw.get_image_subresource_layout(
            image,
            vk::ImageSubresource::default().aspect_mask(vk::ImageAspectFlags::MEMORY_PLANE_0_EXT),
        );
        output.descriptor.offset = layout.offset;
        output.descriptor.row_pitch = layout.row_pitch;
        let extension = ash::khr::external_memory_fd::Device::new(
            owner.open.device.shared_instance().raw_instance(),
            raw,
        );
        let fd = extension
            .get_memory_fd(
                &vk::MemoryGetFdInfoKHR::default()
                    .memory(output.memory)
                    .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT),
            )
            .unwrap();
        (Rc::new(output), OwnedFd::from_raw_fd(fd))
    }
}

fn barrier(
    recording: &mut Recording<'_>,
    image: &Rc<DmaBufImage>,
    old: vk::ImageLayout,
    src_family: u32,
    dst_family: u32,
    src: vk::AccessFlags,
    dst: vk::AccessFlags,
) {
    recording.keep(image.clone());
    unsafe {
        image.owner.open.device.raw_device().cmd_pipeline_barrier(
            recording.encoder().raw_handle(),
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[vk::ImageMemoryBarrier::default()
                .image(image.image)
                .old_layout(old)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(src_family)
                .dst_queue_family_index(dst_family)
                .src_access_mask(src)
                .dst_access_mask(dst)
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .level_count(1)
                        .layer_count(1),
                )],
        );
    }
}

#[test]
#[ignore = "Requires Vulkan DMA-BUF images, shared timelines and validation"]
fn dma_buf_import_reuses_shared_pixels_and_retains_pending_images() {
    {
        let producer = device();
        let consumer = device();
        let ready = SharedTimeline::new(&producer).unwrap();
        let ready_import = SharedTimeline::import(&consumer, &ready.export().unwrap()).unwrap();
        let released = SharedTimeline::new(&consumer).unwrap();
        let release_import =
            SharedTimeline::import(&producer, &released.export().unwrap()).unwrap();
        let mut value = 0;
        for format in [
            wgt::TextureFormat::Rgba8Unorm,
            wgt::TextureFormat::Bgra8Unorm,
        ] {
            let usages = wgt::TextureUses::RESOURCE
                | wgt::TextureUses::COPY_SRC
                | wgt::TextureUses::COPY_DST;
            let formats = producer.dma_buf_formats(format, usages).unwrap();
            assert!(formats
                .iter()
                .any(|entry| entry.modifier() == 0 && entry.exportable()));
            assert!(formats
                .iter()
                .any(|entry| entry.modifier() != 0 && entry.exportable()));
            for caps in formats.iter().filter(|entry| entry.exportable()) {
                let (source, fd) = export_image(&producer, format, caps.modifier(), [17, 9]);
                let mut imported = Some(
                    unsafe { consumer.import_dma_buf(fd.as_fd(), *source.descriptor()) }.unwrap(),
                );
                assert!(fd.try_clone().is_ok());
                drop(fd);
                let weak = Rc::downgrade(imported.as_ref().unwrap());
                let size = 17 * 9 * 4;
                for channel in 0..3 {
                    value += 1;
                    let mut send = Submission::new(&producer).unwrap();
                    {
                        let mut recording = send.recording().unwrap();
                        if channel == 0 {
                            barrier(
                                &mut recording,
                                &source,
                                vk::ImageLayout::UNDEFINED,
                                vk::QUEUE_FAMILY_IGNORED,
                                vk::QUEUE_FAMILY_IGNORED,
                                vk::AccessFlags::empty(),
                                vk::AccessFlags::TRANSFER_WRITE,
                            );
                        } else {
                            recording.wait_timeline(&release_import, value - 1).unwrap();
                            barrier(
                                &mut recording,
                                &source,
                                vk::ImageLayout::GENERAL,
                                vk::QUEUE_FAMILY_EXTERNAL,
                                producer.open.device.queue_family_index(),
                                vk::AccessFlags::empty(),
                                vk::AccessFlags::TRANSFER_WRITE,
                            );
                        }
                        let mut color = [0.0, 0.0, 0.0, 1.0];
                        color[channel] = 1.0;
                        unsafe {
                            producer.open.device.raw_device().cmd_clear_color_image(
                                recording.encoder().raw_handle(),
                                source.image,
                                vk::ImageLayout::GENERAL,
                                &vk::ClearColorValue { float32: color },
                                &[vk::ImageSubresourceRange::default()
                                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                                    .level_count(1)
                                    .layer_count(1)],
                            );
                        }
                        barrier(
                            &mut recording,
                            &source,
                            vk::ImageLayout::GENERAL,
                            producer.open.device.queue_family_index(),
                            vk::QUEUE_FAMILY_EXTERNAL,
                            vk::AccessFlags::TRANSFER_WRITE,
                            vk::AccessFlags::empty(),
                        );
                        recording.signal_timeline(&ready, value).unwrap();
                    }
                    send.submit().unwrap();
                    let output = Rc::new(Owned::new(
                        &consumer,
                        unsafe {
                            consumer.open.device.create_buffer(&hal::BufferDescriptor {
                                label: Some("DMA-BUF import readback"),
                                size,
                                usage: wgt::BufferUses::COPY_DST | wgt::BufferUses::MAP_READ,
                                memory_flags: hal::MemoryFlags::PREFER_COHERENT,
                            })
                        }
                        .unwrap(),
                        hal::vulkan::Device::destroy_buffer,
                    ));
                    let mut receive = Submission::new(&consumer).unwrap();
                    {
                        let image = imported.as_ref().unwrap();
                        let mut recording = receive.recording().unwrap();
                        recording.wait_timeline(&ready_import, value).unwrap();
                        barrier(
                            &mut recording,
                            image,
                            vk::ImageLayout::GENERAL,
                            vk::QUEUE_FAMILY_EXTERNAL,
                            consumer.open.device.queue_family_index(),
                            vk::AccessFlags::empty(),
                            vk::AccessFlags::TRANSFER_READ,
                        );
                        unsafe {
                            consumer.open.device.raw_device().cmd_copy_image_to_buffer(
                                recording.encoder().raw_handle(),
                                image.image,
                                vk::ImageLayout::GENERAL,
                                output.raw_handle(),
                                &[vk::BufferImageCopy::default()
                                    .image_extent(vk::Extent3D {
                                        width: 17,
                                        height: 9,
                                        depth: 1,
                                    })
                                    .image_subresource(
                                        vk::ImageSubresourceLayers::default()
                                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                                            .layer_count(1),
                                    )],
                            );
                            recording.encoder().transition_buffers(std::iter::once(
                                hal::BufferBarrier {
                                    buffer: &**output,
                                    usage: hal::StateTransition {
                                        from: wgt::BufferUses::COPY_DST,
                                        to: wgt::BufferUses::MAP_READ,
                                    },
                                },
                            ));
                        }
                        recording.keep(output.clone());
                        barrier(
                            &mut recording,
                            image,
                            vk::ImageLayout::GENERAL,
                            consumer.open.device.queue_family_index(),
                            vk::QUEUE_FAMILY_EXTERNAL,
                            vk::AccessFlags::TRANSFER_READ,
                            vk::AccessFlags::empty(),
                        );
                        recording.signal_timeline(&released, value).unwrap();
                    }
                    receive.submit().unwrap();
                    if channel == 2 {
                        drop(imported.take());
                        assert!(weak.upgrade().is_some());
                    }
                    assert!(receive.wait(Some(Duration::from_secs(5))).unwrap());
                    assert!(send.wait(Some(Duration::from_secs(5))).unwrap());
                    let mut pixel = [0, 0, 0, 255];
                    pixel[channel] = 255;
                    if format == wgt::TextureFormat::Bgra8Unorm {
                        pixel.swap(0, 2);
                    }
                    assert_eq!(map_upload(&consumer, &output, size), pixel.repeat(17 * 9));
                }
                assert!(weak.upgrade().is_none());
            }
        }
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan DMA-BUF images and validation"]
fn dma_buf_import_rejects_bad_metadata_and_preserves_descriptors() {
    {
        let producer = device();
        let consumer = device();
        let format = wgt::TextureFormat::Rgba8Unorm;
        let usages =
            wgt::TextureUses::RESOURCE | wgt::TextureUses::COPY_SRC | wgt::TextureUses::COPY_DST;
        let caps = producer.dma_buf_formats(format, usages).unwrap();
        let tiled = caps
            .iter()
            .find(|entry| entry.modifier() != 0 && entry.exportable())
            .unwrap()
            .modifier();
        let (source, fd) = export_image(&producer, format, 0, [17, 9]);
        for which in 0..5 {
            let mut descriptor = *source.descriptor();
            match which {
                0 => descriptor.size = [0, 9],
                1 => descriptor.modifier = u64::MAX,
                2 => descriptor.device_uuid[0] ^= 1,
                3 => descriptor.driver_uuid[0] ^= 1,
                _ => descriptor.row_pitch = 1,
            }
            assert!(unsafe { consumer.import_dma_buf(fd.as_fd(), descriptor) }.is_err());
            assert!(fd.try_clone().is_ok());
        }
        let (large, large_fd) = export_image(&producer, format, tiled, [1024, 1024]);
        let error = unsafe { consumer.import_dma_buf(fd.as_fd(), *large.descriptor()) }
            .err()
            .unwrap();
        assert!(error.contains("requirements"), "{}", error);
        assert!(fd.try_clone().is_ok());
        assert!(!consumer.is_lost());
        let imported =
            unsafe { consumer.import_dma_buf(large_fd.as_fd(), *large.descriptor()) }.unwrap();
        let weak = Rc::downgrade(&consumer);
        drop(consumer);
        assert!(weak.upgrade().is_some());
        drop(imported);
        assert!(weak.upgrade().is_none());
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
