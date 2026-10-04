/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#![cfg(all(feature = "vulkan", target_os = "linux"))]

extern crate ash;
extern crate log;
extern crate webrender;
extern crate wgpu_hal;

#[path = "../../../webrender_bindings/src/vulkan_external.rs"]
mod ffi;

use self::ffi::*;
use ash::vk;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use webrender::api::*;
use webrender::vulkan::{Device, Options, SharedTimeline, Submission};
use wgpu_hal::CommandEncoder;

struct ValidationLog;
static ERRORS: AtomicUsize = AtomicUsize::new(0);
impl log::Log for ValidationLog {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, entry: &log::Record) {
        if entry.target().starts_with("wgpu_hal") && entry.level() == log::Level::Error {
            ERRORS.fetch_add(1, Ordering::Relaxed);
        }
        eprintln!("{}", entry.args());
    }
    fn flush(&self) {}
}

struct ExportedImage {
    owner: Rc<Device>,
    image: vk::Image,
    memory: vk::DeviceMemory,
    fd: OwnedFd,
    descriptor: WrVulkanDmaBufDescriptor,
}

impl Drop for ExportedImage {
    fn drop(&mut self) {
        unsafe {
            let raw = self.owner.raw_device().raw_device();
            raw.destroy_image(self.image, None);
            raw.free_memory(self.memory, None);
        }
    }
}

fn export_image(owner: &Rc<Device>, identity: &WrVulkanTimelineDescriptor) -> Rc<ExportedImage> {
    let device = owner.raw_device();
    let raw = device.raw_device();
    let modifiers = [0];
    let mut drm =
        vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&modifiers);
    let mut external = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let info = vk::ImageCreateInfo::default()
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
        .push_next(&mut external)
        .push_next(&mut drm);
    unsafe {
        let image = raw.create_image(&info, None).unwrap();
        let requirements = raw.get_image_memory_requirements(image);
        let properties = device
            .shared_instance()
            .raw_instance()
            .get_physical_device_memory_properties(device.raw_physical_device());
        let index = (0..properties.memory_type_count)
            .find(|&i| {
                requirements.memory_type_bits & (1 << i) != 0
                    && !properties.memory_types[i as usize]
                        .property_flags
                        .contains(vk::MemoryPropertyFlags::PROTECTED)
            })
            .unwrap();
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let mut export = vk::ExportMemoryAllocateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
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
        let layout = raw.get_image_subresource_layout(
            image,
            vk::ImageSubresource::default().aspect_mask(vk::ImageAspectFlags::MEMORY_PLANE_0_EXT),
        );
        let extension =
            ash::khr::external_memory_fd::Device::new(device.shared_instance().raw_instance(), raw);
        let fd = OwnedFd::from_raw_fd(
            extension
                .get_memory_fd(
                    &vk::MemoryGetFdInfoKHR::default()
                        .memory(memory)
                        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT),
                )
                .unwrap(),
        );
        Rc::new(ExportedImage {
            owner: owner.clone(),
            image,
            memory,
            descriptor: WrVulkanDmaBufDescriptor {
                fd: fd.as_raw_fd(),
                width: 2,
                height: 2,
                format: ImageFormat::RGBA8,
                modifier: 0,
                offset: layout.offset,
                stride: layout.row_pitch,
                device_uuid: identity.device_uuid,
                driver_uuid: identity.driver_uuid,
                copy_src: false,
                copy_dst: true,
                color_target: false,
            },
            fd,
        })
    }
}

#[test]
#[ignore = "Requires Vulkan DMA-BUF, timelines and validation"]
fn dmabuf_bindings_retain_images_until_renderer_submission() {
    let _ = log::set_logger(&ValidationLog);
    log::set_max_level(log::LevelFilter::Warn);
    struct Notice;
    impl RenderNotifier for Notice {
        fn clone(&self) -> Box<dyn RenderNotifier> {
            Box::new(Self)
        }
        fn wake_up(&self, _: bool) {}
        fn new_frame_ready(&self, _: DocumentId, _: FramePublishId, _: &FrameReadyParams) {}
    }
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
    let context = wr_vulkan_external_images_new(&renderer);
    assert!(!context.is_null());
    let context = unsafe { Box::from_raw(context) };
    let producer = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Default::default()
        })
        .unwrap(),
    );
    let ready = SharedTimeline::new(&producer).unwrap();
    let (fd, device_uuid, driver_uuid) = ready.export().unwrap().into_parts();
    let ready_descriptor = WrVulkanTimelineDescriptor {
        fd: fd.as_raw_fd(),
        device_uuid,
        driver_uuid,
    };
    let ready_import = unsafe { wr_vulkan_timeline_import(&context, &ready_descriptor) };
    assert!(!ready_import.is_null());
    let ready_import = unsafe { Box::from_raw(ready_import) };
    drop(fd);
    let source = export_image(&producer, &ready_descriptor);
    let mut send = Submission::new(&producer).unwrap();
    {
        let mut commands = send.recording().unwrap();
        commands.keep(source.clone());
        let raw = producer.raw_device().raw_device();
        let family = producer.raw_device().queue_family_index();
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        unsafe {
            let buffer = commands.encoder().raw_handle();
            raw.cmd_pipeline_barrier(
                buffer,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[vk::ImageMemoryBarrier::default()
                    .image(source.image)
                    .subresource_range(range)
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)],
            );
            raw.cmd_clear_color_image(
                buffer,
                source.image,
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
                    .image(source.image)
                    .subresource_range(range)
                    .old_layout(vk::ImageLayout::GENERAL)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .src_queue_family_index(family)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)],
            );
        }
        commands.signal_timeline(&ready, 1).unwrap();
    }
    send.submit().unwrap();
    let imported = unsafe { wr_vulkan_dmabuf_import(&context, &source.descriptor) };
    assert!(!imported.is_null());
    let imported = unsafe { Box::from_raw(imported) };
    assert_eq!(source.fd.as_raw_fd(), source.descriptor.fd);
    let released = wr_vulkan_timeline_new(&context);
    assert!(!released.is_null());
    let released = unsafe { Box::from_raw(released) };
    let mut handle = ExternalTextureHandle(0);
    assert!(unsafe { wr_vulkan_dmabuf_acquire(&imported, &ready_import, 1, &mut handle) });
    assert_ne!(handle.0, 0);
    let receipt = wr_vulkan_dmabuf_release(&imported, &released, 1);
    assert!(!receipt.is_null());
    let receipt = unsafe { Box::from_raw(receipt) };
    assert_eq!(
        wr_vulkan_release_status(Some(&receipt)),
        WrVulkanReleaseStatus::Pending
    );
    assert!(wr_vulkan_dmabuf_release(&imported, &released, 2).is_null());
    unsafe { wr_vulkan_dmabuf_delete(Box::into_raw(imported)) };
    unsafe { wr_vulkan_timeline_delete(Box::into_raw(ready_import)) };
    unsafe { wr_vulkan_timeline_delete(Box::into_raw(released)) };
    unsafe { wr_vulkan_external_images_delete(Box::into_raw(context)) };
    renderer.deinit();
    assert_eq!(
        wr_vulkan_release_status(Some(&receipt)),
        WrVulkanReleaseStatus::Submitted
    );
    unsafe { wr_vulkan_release_delete(Box::into_raw(receipt)) };
    assert!(send.wait(Some(Duration::from_secs(5))).unwrap());
    drop(send);
    drop(source);
    drop(ready);
    drop(producer);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
