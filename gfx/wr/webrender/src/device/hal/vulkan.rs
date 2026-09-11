/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use ash::vk;
use std::ffi::CStr;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub fn create_vulkan_device(options: &Options) -> Result<Device<hal::api::Vulkan>> {
    #[cfg(test)]
    super::validation_logging();
    if options.validation {
        let entry = unsafe { ash::Entry::load() }.map_err(|e| format!("Loading Vulkan: {e}"))?;
        let layers = unsafe { entry.enumerate_instance_layer_properties() }
            .map_err(|e| format!("Enumerating layers: {e:?}"))?;
        if !layers.iter().any(|p| {
            unsafe { CStr::from_ptr(p.layer_name.as_ptr()) }.to_bytes()
                == b"VK_LAYER_KHRONOS_validation"
        }) {
            return Err("Requested Vulkan validation layer is unavailable".into());
        }
    }
    Device::new(options)
}

struct NativeImage<'a> {
    device: &'a ash::Device,
    queue: &'a hal::vulkan::Queue,
    image: vk::Image,
    memory: vk::DeviceMemory,
    upload: vk::Buffer,
    upload_memory: vk::DeviceMemory,
    pool: vk::CommandPool,
    ready: vk::Semaphore,
    released: vk::Semaphore,
    fence: vk::Fence,
}

impl Drop for NativeImage<'_> {
    fn drop(&mut self) {
        self.queue.remove_wait_semaphore(self.ready);
        self.queue.remove_signal_semaphore(self.released);
        unsafe {
            let _ = self.queue.wait_for_idle();
            if self.pool != vk::CommandPool::null() {
                self.device.destroy_command_pool(self.pool, None);
            }
            if self.fence != vk::Fence::null() {
                self.device.destroy_fence(self.fence, None);
            }
            if self.ready != vk::Semaphore::null() {
                self.device.destroy_semaphore(self.ready, None);
            }
            if self.released != vk::Semaphore::null() {
                self.device.destroy_semaphore(self.released, None);
            }
            if self.image != vk::Image::null() {
                self.device.destroy_image(self.image, None);
            }
            if self.memory != vk::DeviceMemory::null() {
                self.device.free_memory(self.memory, None);
            }
            if self.upload != vk::Buffer::null() {
                self.device.destroy_buffer(self.upload, None);
            }
            if self.upload_memory != vk::DeviceMemory::null() {
                self.device.free_memory(self.upload_memory, None);
            }
        }
    }
}

impl Device<hal::api::Vulkan> {
    /// Exercises a borrowed, same-device native image with GPU acquire/release semaphores.
    pub fn test_native_image(&mut self, width: u32, height: u32, color: [u8; 4]) -> Result<()> {
        let layout = self.layout(width, height)?;
        let device = &self.open.device;
        let raw = device.raw_device();
        let extent = wgt::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let mut producer = NativeImage {
            device: raw,
            queue: &self.open.queue,
            image: vk::Image::null(),
            memory: vk::DeviceMemory::null(),
            upload: vk::Buffer::null(),
            upload_memory: vk::DeviceMemory::null(),
            pool: vk::CommandPool::null(),
            ready: vk::Semaphore::null(),
            released: vk::Semaphore::null(),
            fence: vk::Fence::null(),
        };
        let mut expected = Vec::with_capacity(layout.row_bytes as usize * height as usize);
        for y in 0..height {
            for x in 0..width {
                expected.extend_from_slice(&[
                    color[0].wrapping_add(x as u8),
                    color[1].wrapping_add(y as u8),
                    color[2],
                    color[3],
                ]);
            }
        }
        unsafe {
            producer.image = raw
                .create_image(
                    &vk::ImageCreateInfo::default()
                        .image_type(vk::ImageType::TYPE_2D)
                        .format(vk::Format::R8G8B8A8_UNORM)
                        .extent(vk::Extent3D {
                            width,
                            height,
                            depth: 1,
                        })
                        .mip_levels(1)
                        .array_layers(1)
                        .samples(vk::SampleCountFlags::TYPE_1)
                        .tiling(vk::ImageTiling::OPTIMAL)
                        .usage(
                            vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC,
                        )
                        .sharing_mode(vk::SharingMode::EXCLUSIVE),
                    None,
                )
                .map_err(|e| format!("Creating producer image: {e:?}"))?;
            let requirements = raw.get_image_memory_requirements(producer.image);
            let memory = device
                .shared_instance()
                .raw_instance()
                .get_physical_device_memory_properties(device.raw_physical_device());
            let memory_type = (0..memory.memory_type_count)
                .filter(|&i| requirements.memory_type_bits & (1 << i) != 0)
                .min_by_key(|&i| {
                    !memory.memory_types[i as usize]
                        .property_flags
                        .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
                })
                .ok_or("No memory type for producer image")?;
            producer.memory = raw
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(requirements.size)
                        .memory_type_index(memory_type),
                    None,
                )
                .map_err(|e| format!("Allocating producer image: {e:?}"))?;
            raw.bind_image_memory(producer.image, producer.memory, 0)
                .map_err(|e| format!("Binding producer memory: {e:?}"))?;
            producer.upload = raw
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(expected.len() as u64)
                        .usage(vk::BufferUsageFlags::TRANSFER_SRC)
                        .sharing_mode(vk::SharingMode::EXCLUSIVE),
                    None,
                )
                .map_err(|e| format!("Creating producer upload: {e:?}"))?;
            let requirements = raw.get_buffer_memory_requirements(producer.upload);
            let memory_type = (0..memory.memory_type_count)
                .find(|&i| {
                    requirements.memory_type_bits & (1 << i) != 0
                        && memory.memory_types[i as usize].property_flags.contains(
                            vk::MemoryPropertyFlags::HOST_VISIBLE
                                | vk::MemoryPropertyFlags::HOST_COHERENT,
                        )
                })
                .ok_or("No coherent host memory for producer upload")?;
            producer.upload_memory = raw
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(requirements.size)
                        .memory_type_index(memory_type),
                    None,
                )
                .map_err(|e| format!("Allocating producer upload: {e:?}"))?;
            raw.bind_buffer_memory(producer.upload, producer.upload_memory, 0)
                .map_err(|e| format!("Binding producer upload: {e:?}"))?;
            let mapped = raw
                .map_memory(
                    producer.upload_memory,
                    0,
                    expected.len() as u64,
                    vk::MemoryMapFlags::empty(),
                )
                .map_err(|e| format!("Mapping producer upload: {e:?}"))?;
            std::ptr::copy_nonoverlapping(expected.as_ptr(), mapped.cast(), expected.len());
            raw.unmap_memory(producer.upload_memory);
            producer.pool = raw
                .create_command_pool(
                    &vk::CommandPoolCreateInfo::default()
                        .queue_family_index(device.queue_family_index()),
                    None,
                )
                .map_err(|e| format!("Creating producer commands: {e:?}"))?;
            producer.ready = raw
                .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
                .map_err(|e| format!("Creating acquire semaphore: {e:?}"))?;
            producer.released = raw
                .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
                .map_err(|e| format!("Creating release semaphore: {e:?}"))?;
            producer.fence = raw
                .create_fence(&vk::FenceCreateInfo::default(), None)
                .map_err(|e| format!("Creating release fence: {e:?}"))?;
            let command = raw
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(producer.pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count(1),
                )
                .map_err(|e| format!("Allocating producer commands: {e:?}"))?[0];
            raw.begin_command_buffer(
                command,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .map_err(|e| format!("Beginning producer commands: {e:?}"))?;
            let range = vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1);
            let barrier = vk::ImageMemoryBarrier::default()
                .image(producer.image)
                .subresource_range(range)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);
            raw.cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier],
            );
            raw.cmd_copy_buffer_to_image(
                command,
                producer.upload,
                producer.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[vk::BufferImageCopy::default()
                    .image_subresource(
                        vk::ImageSubresourceLayers::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .layer_count(1),
                    )
                    .image_extent(vk::Extent3D {
                        width,
                        height,
                        depth: 1,
                    })],
            );
            let barrier = barrier
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ);
            raw.cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier],
            );
            raw.end_command_buffer(command)
                .map_err(|e| format!("Finishing producer commands: {e:?}"))?;
            raw.queue_submit(
                device.raw_queue(),
                &[vk::SubmitInfo::default()
                    .command_buffers(&[command])
                    .signal_semaphores(&[producer.ready])],
                vk::Fence::null(),
            )
            .map_err(|e| format!("Submitting producer image: {e:?}"))?;
        }
        let borrow_released = Arc::new(AtomicBool::new(false));
        let release_notice = borrow_released.clone();
        let imported = unsafe {
            device.texture_from_raw(
                producer.image,
                &texture_descriptor(
                    extent,
                    wgt::TextureFormat::Rgba8Unorm,
                    wgt::TextureUses::COPY_SRC,
                ),
                Some(Box::new(move || {
                    release_notice.store(true, Ordering::Release);
                })),
                hal::vulkan::TextureMemory::External,
            )
        };
        let imported = Resource::new(
            device,
            imported,
            <hal::vulkan::Device as hal::Device>::destroy_texture,
        );
        let readback = self.readback_buffer(&layout)?;
        let mut commands = Commands::<hal::api::Vulkan>::new(&self.open)?;
        self.open
            .queue
            .add_wait_semaphore(producer.ready, None, vk::PipelineStageFlags::TRANSFER);
        self.open
            .queue
            .add_signal_semaphore(producer.released, None);
        unsafe {
            copy_readback::<hal::api::Vulkan>(
                commands.encoder(),
                &imported,
                &readback,
                &layout,
                extent,
                hal::FormatAspects::COLOR,
            );
        }
        commands.submit_and_wait()?;
        unsafe {
            raw.queue_submit(
                device.raw_queue(),
                &[vk::SubmitInfo::default()
                    .wait_semaphores(&[producer.released])
                    .wait_dst_stage_mask(&[vk::PipelineStageFlags::TOP_OF_PIPE])],
                producer.fence,
            )
            .map_err(|e| format!("Releasing producer image: {e:?}"))?;
            raw.wait_for_fences(&[producer.fence], true, u64::MAX)
                .map_err(|e| format!("Waiting for image release: {e:?}"))?;
        }
        let pixels = self.map_readback(&readback, &layout)?;
        if pixels != expected {
            return Err("Native-image readback did not match producer contents".into());
        }
        drop(commands);
        drop(imported);
        if !borrow_released.load(Ordering::Acquire) {
            return Err("Native-image borrow was not released".into());
        }
        Ok(())
    }
}
