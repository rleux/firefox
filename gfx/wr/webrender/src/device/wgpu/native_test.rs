/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use ash::vk;
use std::{
    cell::Cell,
    os::fd::{FromRawFd, OwnedFd},
    rc::Rc,
    sync::Arc,
};

#[cfg(feature = "capture")]
pub(crate) fn queue_capture_write(owner: &Device) -> Arc<core::resource::Buffer> {
    let buffer = owner
        .create_buffer(&core::resource::BufferDescriptor {
            label: Some("Native capture pending write".into()),
            size: 4,
            usage: wgt::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
        .unwrap();
    owner.queue.write_buffer(buffer.clone(), 0, &[1, 2, 3, 4]);
    owner.check().unwrap();
    buffer
}

pub(crate) struct Producer {
    owner: Arc<Device>,
    image: vk::Image,
    memory: vk::DeviceMemory,
    fd: OwnedFd,
    descriptor: DmaBufImageDescriptor,
    ready: Rc<SharedTimeline>,
    retirement: Rc<SharedTimeline>,
    last: Cell<u64>,
}
impl Producer {
    pub(crate) fn import_ready(
        &self,
        registry: &Rc<ExternalTextureRegistry>,
    ) -> Rc<SharedTimeline> {
        SharedTimeline::import(registry.device(), &self.ready.export().unwrap()).unwrap()
    }
    pub(crate) fn ready_value(&self) -> u64 {
        self.ready.current_value().unwrap()
    }

    pub(crate) fn open_ready(&self, value: u64) {
        let outcome = unsafe {
            self.owner.submit_native(
                &[],
                &[],
                &[native_submit::NativeSignal {
                    semaphore: self.ready.semaphore,
                    value,
                }],
                native_submit::NativeSignal {
                    semaphore: self.retirement.semaphore,
                    value,
                },
            )
        };
        self.ready.signal_submitted(value);
        self.retirement.signal_submitted(value);
        let index = outcome.result.unwrap();
        self.last.set(index);
        self.owner.wait_for(index).unwrap();
    }
    pub(crate) fn block_consumer(
        &self,
        registry: &Rc<ExternalTextureRegistry>,
        ready: &Rc<SharedTimeline>,
        value: u64,
    ) -> (Rc<SharedTimeline>, Rc<SharedTimeline>) {
        let retirement = SharedTimeline::new(registry.device()).unwrap();
        let outcome = unsafe {
            registry.device().submit_native(
                &[],
                &[native_submit::NativeWait {
                    semaphore: ready.semaphore,
                    value: Some(value),
                }],
                &[],
                native_submit::NativeSignal {
                    semaphore: retirement.semaphore,
                    value: 1,
                },
            )
        };
        outcome.result.unwrap();
        (retirement, ready.clone())
    }
    pub(crate) fn new() -> Self {
        let owner = Arc::new(
            Device::new(&Options {
                validation: true,
                ..Default::default()
            })
            .unwrap(),
        );
        let hal = owner.raw_device();
        let raw = hal.raw_device();
        let modifiers = [0];
        let mut modifier =
            vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&modifiers);
        let mut external = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let image = unsafe {
            raw.create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(vk::Format::R8G8B8A8_UNORM)
                    .extent(vk::Extent3D {
                        width: 4,
                        height: 4,
                        depth: 1,
                    })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                    .usage(
                        vk::ImageUsageFlags::SAMPLED
                            | vk::ImageUsageFlags::TRANSFER_SRC
                            | vk::ImageUsageFlags::TRANSFER_DST,
                    )
                    .sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .push_next(&mut modifier)
                    .push_next(&mut external),
                None,
            )
        }
        .unwrap();
        let req = unsafe { raw.get_image_memory_requirements(image) };
        let props = unsafe {
            hal.shared_instance()
                .raw_instance()
                .get_physical_device_memory_properties(hal.raw_physical_device())
        };
        let index = (0..props.memory_type_count)
            .find(|&i| {
                req.memory_type_bits & (1 << i) != 0
                    && !props.memory_types[i as usize]
                        .property_flags
                        .contains(vk::MemoryPropertyFlags::PROTECTED)
            })
            .unwrap();
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let mut export = vk::ExportMemoryAllocateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let memory = unsafe {
            raw.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(req.size)
                    .memory_type_index(index)
                    .push_next(&mut dedicated)
                    .push_next(&mut export),
                None,
            )
        }
        .unwrap();
        unsafe { raw.bind_image_memory(image, memory, 0) }.unwrap();
        let layout = unsafe {
            raw.get_image_subresource_layout(
                image,
                vk::ImageSubresource::default()
                    .aspect_mask(vk::ImageAspectFlags::MEMORY_PLANE_0_EXT),
            )
        };
        let ext =
            ash::khr::external_memory_fd::Device::new(hal.shared_instance().raw_instance(), raw);
        let fd = unsafe {
            OwnedFd::from_raw_fd(
                ext.get_memory_fd(
                    &vk::MemoryGetFdInfoKHR::default()
                        .memory(memory)
                        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT),
                )
                .unwrap(),
            )
        };
        let (device_uuid, driver_uuid, _) = vulkan::timeline::properties(&owner);
        let ready = SharedTimeline::new(&owner).unwrap();
        let retirement = SharedTimeline::new(&owner).unwrap();
        drop(hal);
        Self {
            owner,
            image,
            memory,
            fd,
            descriptor: DmaBufImageDescriptor {
                size: [4, 4],
                format: wgt::TextureFormat::Rgba8Unorm,
                usage: wgt::TextureUses::RESOURCE
                    | wgt::TextureUses::COPY_SRC
                    | wgt::TextureUses::COPY_DST,
                modifier: 0,
                offset: layout.offset,
                row_pitch: layout.row_pitch,
                device_uuid,
                driver_uuid,
            },
            ready,
            retirement,
            last: Cell::new(0),
        }
    }
    pub(crate) fn import(
        &self,
        registry: &Rc<ExternalTextureRegistry>,
    ) -> (Rc<NativeImage>, Rc<SharedTimeline>) {
        use std::os::fd::AsFd;
        let allocation = unsafe {
            registry
                .device()
                .import_dma_buf(self.fd.as_fd(), self.descriptor)
        }
        .unwrap();
        let image = registry.import(allocation).unwrap();
        let ready =
            SharedTimeline::import(registry.device(), &self.ready.export().unwrap()).unwrap();
        (image, ready)
    }
    pub(crate) fn publish(
        &self,
        value: u64,
        pixel: [u8; 4],
        released: Option<(&Rc<SharedTimeline>, u64)>,
    ) {
        let returned = released.map(|(timeline, point)| {
            (
                SharedTimeline::import(&self.owner, &timeline.export().unwrap()).unwrap(),
                point,
            )
        });
        let encoder = self
            .owner
            .create_command_encoder("native producer publish")
            .unwrap();
        let hal = self.owner.raw_device();
        let raw = hal.raw_device();
        let family = hal.queue_family_index();
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        unsafe {
            encoder.as_hal_mut::<hal::api::Vulkan, _, _>(|hal| {
                let command = hal.unwrap().raw_handle();
                let (old, src, dst) = if returned.is_some() {
                    (vk::ImageLayout::GENERAL, vk::QUEUE_FAMILY_EXTERNAL, family)
                } else {
                    (
                        vk::ImageLayout::UNDEFINED,
                        vk::QUEUE_FAMILY_IGNORED,
                        vk::QUEUE_FAMILY_IGNORED,
                    )
                };
                raw.cmd_pipeline_barrier(
                    command,
                    vk::PipelineStageFlags::ALL_COMMANDS,
                    vk::PipelineStageFlags::ALL_COMMANDS,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[vk::ImageMemoryBarrier::default()
                        .image(self.image)
                        .subresource_range(range)
                        .old_layout(old)
                        .new_layout(vk::ImageLayout::GENERAL)
                        .src_queue_family_index(src)
                        .dst_queue_family_index(dst)
                        .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)],
                );
                raw.cmd_clear_color_image(
                    command,
                    self.image,
                    vk::ImageLayout::GENERAL,
                    &vk::ClearColorValue {
                        float32: pixel.map(|v| v as f32 / 255.),
                    },
                    &[range],
                );
                raw.cmd_pipeline_barrier(
                    command,
                    vk::PipelineStageFlags::ALL_COMMANDS,
                    vk::PipelineStageFlags::ALL_COMMANDS,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[vk::ImageMemoryBarrier::default()
                        .image(self.image)
                        .subresource_range(range)
                        .old_layout(vk::ImageLayout::GENERAL)
                        .new_layout(vk::ImageLayout::GENERAL)
                        .src_queue_family_index(family)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
                        .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)],
                );
            });
        }
        let waits = returned
            .as_ref()
            .map(|(timeline, point)| native_submit::NativeWait {
                semaphore: timeline.semaphore,
                value: Some(*point),
            })
            .into_iter()
            .collect::<Vec<_>>();
        let commands = encoder.finish(&wgt::CommandBufferDescriptor::default());
        let result = unsafe {
            self.owner.submit_native(
                &[commands],
                &waits,
                &[native_submit::NativeSignal {
                    semaphore: self.ready.semaphore,
                    value,
                }],
                native_submit::NativeSignal {
                    semaphore: self.retirement.semaphore,
                    value,
                },
            )
        };
        let index = result.result.unwrap();
        self.ready.signal_submitted(value);
        self.retirement.signal_submitted(value);
        self.last.set(index);
        self.owner.wait_for(index).unwrap();
    }
}
impl Drop for Producer {
    fn drop(&mut self) {
        self.owner.wait_for(self.last.get()).unwrap();
        unsafe {
            let hal = self.owner.raw_device();
            hal.raw_device().destroy_image(self.image, None);
            hal.raw_device().free_memory(self.memory, None);
        }
    }
}
