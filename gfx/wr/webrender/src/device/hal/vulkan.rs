/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::external::{ExternalImageDevice, NativeImage as ExternalNativeImage, Producer, validate_descriptor};

#[cfg(all(target_os = "linux", feature = "hal-linux-dmabuf"))]
mod linux;
#[cfg(all(target_os = "linux", feature = "hal-linux-dmabuf"))]
pub use linux::{DmaBufLayout, DmaBufPlane, DmaBufExport, DmaBufCopy, DmaBufCapabilities, ForeignRgbFormat, ForeignRgbLayout, ForeignRgbImage, WeakForeignRgbImage};
#[cfg(all(target_os = "linux", feature = "hal-linux-dmabuf"))]
pub use linux::{VideoDmaBufCapabilities, VideoDmaBufFormat, VideoDmaBufLayout, ForeignYuvImage, WeakForeignYuvImage};
#[cfg(all(target_os = "linux", feature = "hal-linux-dmabuf"))]
pub use linux::{VulkanDmaBufImage, WeakVulkanDmaBufImage};

#[cfg(any(all(target_os = "linux", feature = "hal-linux-dmabuf"), all(target_os = "android", feature = "hal-android-ahb")))]
mod sync_file;
#[cfg(any(all(target_os = "linux", feature = "hal-linux-dmabuf"), all(target_os = "android", feature = "hal-android-ahb")))]
pub use sync_file::SyncFile;
mod interop;
pub use interop::{VulkanQueueCoordinator, create_vulkan_image_device};
#[cfg(all(target_os = "windows", feature = "hal-win32"))]
mod win32;
#[cfg(all(target_os = "windows", feature = "hal-win32"))]
pub use win32::{Win32Image, Win32Export, Win32Copy, Win32Semaphore};
#[cfg(feature = "hal-win32")]
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
mod win32_layout;
#[cfg(all(target_os = "windows", feature = "hal-win32"))]
pub use win32_layout::Win32ImageLayout;
#[cfg(all(target_os = "android", feature = "hal-android-ahb"))]
mod android;
#[cfg(all(target_os = "android", feature = "hal-android-ahb"))]
pub use android::{AndroidBufferColor, AndroidBufferAlpha, HardwareBufferCopy};
#[cfg(feature = "hal-android-ahb")]
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
mod conversion;

pub struct VulkanDeviceContext<'a> {
    pub device: &'a ash::Device,
    pub instance: &'a ash::Instance,
    pub physical_device: vk::PhysicalDevice,
    pub queue: vk::Queue,
    pub queue_family: u32,
}

pub struct VulkanImageDescriptor {
    pub image: vk::Image,
    pub device: vk::Device,
    pub queue: vk::Queue,
    pub queue_family: u32,
    pub descriptor: api::ImageDescriptor,
    pub initial_usage: wgt::TextureUses,
    pub renderable: bool,
}

struct ImportLifetime(Option<hal::DropCallback>);
impl Drop for ImportLifetime {
    fn drop(&mut self) { if let Some(release) = self.0.take() { release(); } }
}

impl ExternalImageDevice {
    pub fn vulkan_context(&self) -> Option<VulkanDeviceContext<'_>> {
        let producer = self.0.as_any().downcast_ref::<Producer<hal::api::Vulkan>>()?;
        let device = &producer.owner.open.device;
        Some(VulkanDeviceContext {
            device: device.raw_device(), instance: device.shared_instance().raw_instance(),
            physical_device: device.raw_physical_device(), queue: device.raw_queue(),
            queue_family: device.queue_family_index(),
        })
    }

    /// Imports a borrowed image; `release` runs on rejection or after the last HAL reference.
    ///
    /// # Safety
    /// The image and memory must match the descriptor and remain valid until `release`.
    /// Contents must be initialized unless `initial_usage` is `UNINITIALIZED`.
    /// Producer work must already be submitted to the specified graphics queue, in
    /// `initial_usage`. Writes must stop while a renderer lease is active.
    /// The image must support sampling and copies, plus color attachment use if renderable.
    pub unsafe fn import_vulkan_image(&self, source: VulkanImageDescriptor, release: hal::DropCallback) -> Result<ExternalNativeImage> {
        let mut lifetime = ImportLifetime(Some(release));
        let producer = self.0.as_any().downcast_ref::<Producer<hal::api::Vulkan>>()
            .ok_or("External image device is not Vulkan")?;
        let owner = &producer.owner;
        #[cfg(any(test, feature = "hal-testing"))]
        owner.check_fault(FailurePoint::Import)?;
        let device = &owner.open.device;
        if source.image == vk::Image::null() || source.device != device.raw_device().handle()
            || source.queue != device.raw_queue() || source.queue_family != device.queue_family_index() {
            return Err("Vulkan image must belong to this device and graphics queue".into());
        }
        validate_descriptor(source.descriptor)?;
        if source.descriptor.flags.contains(api::ImageDescriptorFlags::ALLOW_MIPMAPS) {
            return Err("Native Vulkan image import currently requires one mip level".into());
        }
        if ![wgt::TextureUses::UNINITIALIZED, wgt::TextureUses::RESOURCE, wgt::TextureUses::COPY_SRC,
             wgt::TextureUses::COPY_DST, wgt::TextureUses::COLOR_TARGET].contains(&source.initial_usage) {
            return Err("Unsupported native Vulkan image initial usage".into());
        }
        let width = source.descriptor.size.width as u32;
        let height = source.descriptor.size.height as u32;
        owner.layout(width, height)?;
        let format = super::resources::texture_format(source.descriptor.format)?;
        if !owner.features.contains(format.required_features()) { return Err("Native image format feature is unavailable".into()); }
        let caps = owner.formats.iter().find(|(candidate, _)| *candidate == format)
            .ok_or("Unknown native image format")?.1;
        let mut required = hal::TextureFormatCapabilities::SAMPLED | hal::TextureFormatCapabilities::SAMPLED_LINEAR
            | hal::TextureFormatCapabilities::COPY_SRC | hal::TextureFormatCapabilities::COPY_DST;
        if source.renderable {
            required |= hal::TextureFormatCapabilities::COLOR_ATTACHMENT | hal::TextureFormatCapabilities::COLOR_ATTACHMENT_BLEND;
        }
        if !caps.contains(required) { return Err("Unsupported native image format usages".into()); }
        let mut usage = wgt::TextureUses::RESOURCE | wgt::TextureUses::COPY_SRC | wgt::TextureUses::COPY_DST;
        if source.renderable { usage |= wgt::TextureUses::COLOR_TARGET; }
        let descriptor = texture_descriptor(wgt::Extent3d { width, height, depth_or_array_layers: 1 }, format, usage);
        let raw = device.texture_from_raw(source.image, &descriptor, lifetime.0.take(), hal::vulkan::TextureMemory::External);
        let texture = super::resources::Texture::from_raw(owner, raw, &descriptor,
            crate::device::TextureFilter::Linear, source.renderable, source.initial_usage)?;
        Ok(ExternalNativeImage::new(texture, source.descriptor))
    }
}
use ash::vk;
use std::ffi::CStr;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

impl super::backend::sealed::Sealed for hal::api::Vulkan {}

impl super::backend::BackendApi for hal::api::Vulkan {
    fn create_device(options: &Options, window: Option<std::rc::Rc<dyn SurfaceWindow>>)
        -> Result<(Device<Self>, Option<super::surface::SurfaceSetup<Self>>)> {
        validate_options(options)?;
        Device::new_with_window(options, window)
    }

    #[cfg(all(target_os = "linux", feature = "hal-linux-dmabuf"))]
    fn open_adapter(adapter: &hal::ExposedAdapter<Self>, features: wgt::Features)
        -> Result<(hal::OpenDevice<Self>, wgt::Features)> { linux::open_adapter(adapter, features) }

    #[cfg(all(target_os = "android", feature = "hal-android-ahb"))]
    fn open_adapter(adapter: &hal::ExposedAdapter<Self>, features: wgt::Features)
        -> Result<(hal::OpenDevice<Self>, wgt::Features)> { android::open_adapter(adapter, features) }

    #[cfg(all(target_os = "windows", feature = "hal-win32"))]
    fn open_adapter(adapter: &hal::ExposedAdapter<Self>, features: wgt::Features)
        -> Result<(hal::OpenDevice<Self>, wgt::Features)> { win32::open_adapter(adapter, features) }

    fn timestamp_valid_bits(device: &Device<Self>) -> u32 {
        let device = &device.open.device;
        let properties = unsafe { device.shared_instance().raw_instance()
            .get_physical_device_queue_family_properties(device.raw_physical_device()) };
        properties[device.queue_family_index() as usize].timestamp_valid_bits
    }

    fn shader_input() -> Result<super::backend::ShaderInputMode> {
        super::backend::ShaderInputMode::from_env()
    }

    fn create_shader_module(device: &Self::Device, artifact: &webrender_build::hal::ShaderArtifact,
        fragment: bool, mode: super::backend::ShaderInputMode, cache: &mut super::backend::ShaderCache)
        -> Result<Self::ShaderModule> {
        cache.create_module::<Self>(device, artifact, fragment, mode)
    }

    fn request_surface_preservation(surface: &Self::Surface) -> bool {
        surface.set_native_swapchain_clipped(false)
    }

    fn has_native_swapchain(surface: &Self::Surface) -> bool {
        surface.raw_native_swapchain().is_some()
    }

    fn surface_image_id(texture: &Self::Texture) -> Option<u64> {
        Some(ash::vk::Handle::as_raw(unsafe { texture.raw_handle() }))
    }

    fn supports_presentation_blit(device: &Device<Self>, format: wgt::TextureFormat) -> bool {
        let instance = device.open.device.shared_instance().raw_instance();
        let format = match format {
            wgt::TextureFormat::Rgba8Unorm => vk::Format::R8G8B8A8_UNORM,
            wgt::TextureFormat::Bgra8Unorm => vk::Format::B8G8R8A8_UNORM,
            _ => return false,
        };
        for (format, usage) in [(vk::Format::R8G8B8A8_UNORM, vk::FormatFeatureFlags::BLIT_SRC), (format, vk::FormatFeatureFlags::BLIT_DST)] {
            let caps = unsafe { instance.get_physical_device_format_properties(device.open.device.raw_physical_device(), format) };
            if !caps.optimal_tiling_features.contains(usage) { return false; }
        }
        true
    }

    unsafe fn record_presentation_blit(device: &Self::Device, encoder: &mut Self::CommandEncoder,
        source: &Self::Texture, target: &Self::Texture, source_size: [u32; 2], target_size: [u32; 2], region: Option<[u32; 4]>) -> Result<()> {
        let raw = device.raw_device();
        let layers = vk::ImageSubresourceLayers::default().aspect_mask(vk::ImageAspectFlags::COLOR).layer_count(1);
        if region.is_some() && source_size != target_size {
            return Err("Partial Vulkan presentation requires matching extents".into());
        }
        let offsets = |rect: [u32; 4]| [
            vk::Offset3D { x: rect[0] as i32, y: rect[1] as i32, z: 0 },
            vk::Offset3D { x: (rect[0] + rect[2]) as i32, y: (rect[1] + rect[3]) as i32, z: 1 },
        ];
        let source_rect = region.unwrap_or([0, 0, source_size[0], source_size[1]]);
        let target_rect = region.unwrap_or([0, 0, target_size[0], target_size[1]]);
        let blit = vk::ImageBlit::default().src_subresource(layers).dst_subresource(layers)
            .src_offsets(offsets(source_rect)).dst_offsets(offsets(target_rect));
        raw.cmd_blit_image(encoder.raw_handle(), source.raw_handle(), vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            target.raw_handle(), vk::ImageLayout::TRANSFER_DST_OPTIMAL, &[blit], vk::Filter::NEAREST);
        Ok(())
    }
}

pub fn create_vulkan_device(options: &Options) -> Result<Device<hal::api::Vulkan>> {
    validate_options(options)?;
    Device::new(options)
}

pub(crate) fn create_vulkan_device_for_window(options: &Options, window: std::rc::Rc<dyn SurfaceWindow>)
    -> Result<(Device<hal::api::Vulkan>, super::surface::SurfaceSetup<hal::api::Vulkan>)>
{
    validate_options(options)?;
    let (device, surface) = Device::new_with_window(options, Some(window))?;
    Ok((device, surface.unwrap()))
}

fn validate_options(options: &Options) -> Result<()> {
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
    Ok(())
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
