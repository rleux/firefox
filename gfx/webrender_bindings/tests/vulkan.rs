/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use ash::vk;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::rc::Rc;
use std::time::Duration;
use webrender::api::*;
use webrender::vulkan::{Device, Options, SharedTimeline, Submission, TimelineHandle};

#[repr(C)]
pub struct TestVulkanImage {
    pub(super) memory_fd: i32,
    pub(super) ready_fd: i32,
    pub(super) offset: u64,
    pub(super) stride: u64,
    pub(super) device_uuid: [u8; 16],
    pub(super) driver_uuid: [u8; 16],
}

pub struct Fixture {
    renderer: Option<webrender::Renderer>,
    producer: Rc<Device>,
    submission: Submission,
    image: vk::Image,
    memory: vk::DeviceMemory,
    _memory_fd: OwnedFd,
    _ready_fd: OwnedFd,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(renderer) = self.renderer.take() {
            renderer.deinit();
        }
        assert!(self.submission.wait(Some(Duration::from_secs(5))).unwrap());
        unsafe {
            self.producer.raw_device().raw_device().destroy_image(self.image, None);
            self.producer.raw_device().raw_device().free_memory(self.memory, None);
        }
    }
}

struct Notice;
impl RenderNotifier for Notice {
    fn clone(&self) -> Box<dyn RenderNotifier> {
        Box::new(Self)
    }
    fn wake_up(&self, _: bool) {}
    fn new_frame_ready(&self, _: DocumentId, _: FramePublishId, _: &FrameReadyParams) {}
}

pub(super) fn renderer() -> webrender::Renderer {
    let (renderer, _) = webrender::create_webrender_instance(
        webrender::GpuBackendConfig::Vulkan(Options {
            validation: true,
            ..Default::default()
        }),
        Box::new(Notice),
        webrender::WebRenderOptions {
            enable_debugger: false,
            ..Default::default()
        },
        None,
    )
    .unwrap();
    renderer
}

#[no_mangle]
pub extern "C" fn wr_test_vulkan_image_new(output: &mut TestVulkanImage) -> *mut Fixture {
    let renderer = renderer();
    let producer = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Default::default()
        })
        .unwrap(),
    );
    let ready = SharedTimeline::new(&producer).unwrap();
    let (ready_fd, device_uuid, driver_uuid) = ready.export().unwrap().into_parts();
    let device = producer.raw_device();
    let raw = device.raw_device();
    unsafe {
        let modifiers = [0];
        let mut drm = vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&modifiers);
        let mut external =
            vk::ExternalMemoryImageCreateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let image = raw
            .create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(vk::Format::R8G8B8A8_UNORM)
                    .extent(vk::Extent3D {
                        width: 2,
                        height: 2,
                        depth: 1,
                    })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                    .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .push_next(&mut drm)
                    .push_next(&mut external),
                None,
            )
            .unwrap();
        let requirements = raw.get_image_memory_requirements(image);
        let properties = device
            .shared_instance()
            .raw_instance()
            .get_physical_device_memory_properties(device.raw_physical_device());
        let index = (0..properties.memory_type_count)
            .find(|&index| {
                requirements.memory_type_bits & (1 << index) != 0
                    && !properties.memory_types[index as usize]
                        .property_flags
                        .contains(vk::MemoryPropertyFlags::PROTECTED)
            })
            .unwrap();
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let mut export =
            vk::ExportMemoryAllocateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let memory = raw
            .allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(requirements.size)
                    .memory_type_index(index)
                    .push_next(&mut dedicated)
                    .push_next(&mut export),
                None,
            )
            .unwrap();
        raw.bind_image_memory(image, memory, 0).unwrap();
        let extension = ash::khr::external_memory_fd::Device::new(device.shared_instance().raw_instance(), raw);
        let memory_fd = OwnedFd::from_raw_fd(
            extension
                .get_memory_fd(
                    &vk::MemoryGetFdInfoKHR::default()
                        .memory(memory)
                        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT),
                )
                .unwrap(),
        );
        let layout = raw.get_image_subresource_layout(
            image,
            vk::ImageSubresource::default().aspect_mask(vk::ImageAspectFlags::MEMORY_PLANE_0_EXT),
        );
        let mut submission = Submission::new(&producer).unwrap();
        {
            let mut commands = submission.recording().unwrap();
            let buffer = commands.vulkan_encoder().unwrap().raw_handle();
            let range = vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1);
            raw.cmd_pipeline_barrier(
                buffer,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[vk::ImageMemoryBarrier::default()
                    .image(image)
                    .subresource_range(range)
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)],
            );
            raw.cmd_clear_color_image(
                buffer,
                image,
                vk::ImageLayout::GENERAL,
                &vk::ClearColorValue {
                    float32: [1., 0., 0., 1.],
                },
                &[range],
            );
            raw.cmd_pipeline_barrier(
                buffer,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[vk::ImageMemoryBarrier::default()
                    .image(image)
                    .subresource_range(range)
                    .old_layout(vk::ImageLayout::GENERAL)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .src_queue_family_index(device.queue_family_index())
                    .dst_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)],
            );
            commands.signal_timeline(&ready, 1).unwrap();
        }
        submission.submit().unwrap();
        *output = TestVulkanImage {
            memory_fd: memory_fd.as_raw_fd(),
            ready_fd: ready_fd.as_raw_fd(),
            offset: layout.offset,
            stride: layout.row_pitch,
            device_uuid,
            driver_uuid,
        };
        Box::into_raw(Box::new(Fixture {
            renderer: Some(renderer),
            producer,
            submission,
            image,
            memory,
            _memory_fd: memory_fd,
            _ready_fd: ready_fd,
        }))
    }
}

#[no_mangle]
pub extern "C" fn wr_test_vulkan_image_renderer(fixture: &mut Fixture) -> &mut webrender::Renderer {
    fixture.renderer.as_mut().unwrap()
}

#[no_mangle]
pub extern "C" fn wr_test_vulkan_image_submit(fixture: &mut Fixture) {
    fixture.renderer.take().unwrap().deinit();
}

#[no_mangle]
pub unsafe extern "C" fn wr_test_vulkan_image_wait(
    fixture: &Fixture,
    fd: i32,
    device_uuid: &[u8; 16],
    driver_uuid: &[u8; 16],
    value: u64,
) -> bool {
    let fd = BorrowedFd::borrow_raw(fd).try_clone_to_owned().unwrap();
    let handle = TimelineHandle::from_fd(fd, *device_uuid, *driver_uuid);
    let timeline = SharedTimeline::import(&fixture.producer, &handle).unwrap();
    let mut wait = Submission::new(&fixture.producer).unwrap();
    wait.recording().unwrap().wait_timeline(&timeline, value).unwrap();
    wait.submit().unwrap();
    wait.wait(Some(Duration::from_secs(5))).unwrap()
}

#[no_mangle]
pub unsafe extern "C" fn wr_test_vulkan_image_delete(fixture: *mut Fixture) {
    drop(Box::from_raw(fixture));
}
