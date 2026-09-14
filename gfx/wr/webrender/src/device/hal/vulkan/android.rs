/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::super::{resources::Texture, submission::Submission};
use super::*;
use super::{
    conversion::Conversion,
    sync_file::{SyncFile, TransferSync},
};
use ndk::hardware_buffer::{HardwareBuffer, HardwareBufferRef, HardwareBufferUsage};
use std::rc::Rc;
type V = hal::api::Vulkan;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AndroidBufferColor {
    SdrRgb,
    Bt601 { full_range: bool },
    Bt709 { full_range: bool },
}
#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(u32)]
pub enum AndroidBufferAlpha {
    Opaque = 0,
    Premultiplied = 1,
    Straight = 2,
}

pub struct HardwareBufferCopy {
    image: ExternalNativeImage,
    release: SyncFile,
}
impl HardwareBufferCopy {
    pub fn image(&self) -> &ExternalNativeImage {
        &self.image
    }
    pub fn release(&self) -> &SyncFile {
        &self.release
    }
    pub fn into_parts(self) -> (ExternalNativeImage, SyncFile) {
        (self.image, self.release)
    }
}

pub(super) fn open_adapter(
    adapter: &hal::ExposedAdapter<V>,
    features: wgt::Features,
) -> Result<(hal::OpenDevice<V>, wgt::Features)> {
    let caps = adapter.adapter.physical_device_capabilities();
    let instance = adapter.adapter.shared_instance().raw_instance();
    let physical = adapter.adapter.raw_physical_device();
    let mut ycbcr = vk::PhysicalDeviceSamplerYcbcrConversionFeatures::default();
    let mut semaphore = vk::ExternalSemaphoreProperties::default();
    unsafe {
        instance.get_physical_device_features2(
            physical,
            &mut vk::PhysicalDeviceFeatures2::default().push_next(&mut ycbcr),
        );
        instance.get_physical_device_external_semaphore_properties(
            physical,
            &vk::PhysicalDeviceExternalSemaphoreInfo::default()
                .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD),
            &mut semaphore,
        );
    }
    let extensions = [
        ash::android::external_memory_android_hardware_buffer::NAME,
        ash::khr::external_semaphore_fd::NAME,
        ash::ext::queue_family_foreign::NAME,
    ];
    let supported = extensions
        .iter()
        .all(|extension| caps.supports_extension(extension))
        && ycbcr.sampler_ycbcr_conversion != vk::FALSE
        && semaphore.external_semaphore_features.contains(
            vk::ExternalSemaphoreFeatureFlags::IMPORTABLE
                | vk::ExternalSemaphoreFeatureFlags::EXPORTABLE,
        );
    let callback: Option<Box<hal::vulkan::CreateDeviceCallback<'_>>> = if supported {
        Some(Box::new(move |args| {
            for extension in extensions {
                if !args.extensions.contains(&extension) {
                    args.extensions.push(extension);
                }
            }
            args.device_features.enable_sampler_ycbcr_conversion();
        }))
    } else {
        None
    };
    let open = unsafe {
        adapter.adapter.open_with_callback(
            features,
            &adapter.capabilities.limits,
            &wgt::MemoryHints::default(),
            callback,
        )
    }
    .map_err(|error| format!("Opening Android Vulkan device: {error:?}"))?;
    Ok((open, features))
}

fn supported(owner: &Device<V>) -> bool {
    owner
        .open
        .device
        .enabled_device_extensions()
        .contains(&ash::android::external_memory_android_hardware_buffer::NAME)
}

struct Imported {
    owner: Rc<Device<V>>,
    image: vk::Image,
    memory: vk::DeviceMemory,
    _buffer: HardwareBufferRef,
}
impl Drop for Imported {
    fn drop(&mut self) {
        unsafe {
            let raw = self.owner.open.device.raw_device();
            raw.destroy_image(self.image, None);
            raw.free_memory(self.memory, None);
        }
    }
}

unsafe fn barrier(
    owner: &Device<V>,
    commands: &mut Submission<V>,
    image: vk::Image,
    old: vk::ImageLayout,
    new: vk::ImageLayout,
    src_family: u32,
    dst_family: u32,
    src: vk::AccessFlags,
    dst: vk::AccessFlags,
) {
    owner.open.device.raw_device().cmd_pipeline_barrier(
        commands.encoder().raw_handle(),
        vk::PipelineStageFlags::ALL_COMMANDS,
        vk::PipelineStageFlags::ALL_COMMANDS,
        vk::DependencyFlags::empty(),
        &[],
        &[],
        &[vk::ImageMemoryBarrier::default()
            .image(image)
            .old_layout(old)
            .new_layout(new)
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

impl ExternalImageDevice {
    pub fn android_hardware_buffer_supported(&self) -> Result<bool> {
        let producer = self.sync_file_producer()?;
        Ok(supported(&producer.owner))
    }

    /// Materializes an SDR hardware buffer into a premultiplied RGBA8 WR image on the GPU.
    /// Matrix/range conversion preserves the source nonlinear RGB encoding; no gamut or HDR conversion is applied.
    /// # Safety
    /// The buffer must contain initialized SDR pixels matching `color` and `alpha`, with no concurrent accesses.
    /// The producer must release FOREIGN ownership in GENERAL and submit `ready` before this call.
    /// Do not reuse the buffer until the returned fence signals. On failure, retire the device before reuse.
    pub unsafe fn copy_android_hardware_buffer(
        &self,
        buffer: &HardwareBuffer,
        ready: &SyncFile,
        color: AndroidBufferColor,
        alpha: AndroidBufferAlpha,
    ) -> Result<HardwareBufferCopy> {
        let producer = self.sync_file_producer()?;
        let owner = &producer.owner;
        if !supported(owner) {
            return Err("Android hardware-buffer import requires AHB, YCbCr, FOREIGN ownership and sync-file extensions".into());
        }
        #[cfg(any(test, feature = "hal-testing"))]
        owner.check_fault(FailurePoint::Import)?;
        let desc = buffer.describe();
        if desc.layers != 1
            || !desc.usage.contains(HardwareBufferUsage::GPU_SAMPLED_IMAGE)
            || desc.usage.intersects(
                HardwareBufferUsage::PROTECTED_CONTENT
                    | HardwareBufferUsage::GPU_CUBE_MAP
                    | HardwareBufferUsage::GPU_MIPMAP_COMPLETE,
            )
        {
            return Err("Hardware-buffer copy requires an unprotected, sampled, single-layer image without cube or mipmap usage".into());
        }
        owner.layout(desc.width, desc.height)?;
        let device = &owner.open.device;
        let raw = device.raw_device();
        let extension = ash::android::external_memory_android_hardware_buffer::Device::new(
            device.shared_instance().raw_instance(),
            raw,
        );
        let mut format = vk::AndroidHardwareBufferFormatPropertiesANDROID::default();
        let mut properties =
            vk::AndroidHardwareBufferPropertiesANDROID::default().push_next(&mut format);
        extension
            .get_android_hardware_buffer_properties(buffer.as_ptr().cast(), &mut properties)
            .map_err(|error| format!("Querying hardware-buffer allocation: {error:?}"))?;
        let allocation_size = properties.allocation_size;
        let memory_bits = properties.memory_type_bits;
        if !format
            .format_features
            .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE)
        {
            return Err("Hardware-buffer format does not support sampling".into());
        }
        let video = !matches!(color, AndroidBufferColor::SdrRgb);
        if video && alpha != AndroidBufferAlpha::Opaque {
            return Err("YCbCr hardware buffers require opaque alpha".into());
        }
        let (image_format, external_format) = if video {
            if format.external_format == 0 {
                return Err("Video hardware buffer has no external sampling format".into());
            }
            (vk::Format::UNDEFINED, format.external_format)
        } else {
            if !matches!(
                format.format,
                vk::Format::R8G8B8A8_UNORM
                    | vk::Format::R8G8B8_UNORM
                    | vk::Format::R5G6B5_UNORM_PACK16
            ) {
                return Err(
                    "SDR RGB hardware-buffer copy supports only RGBA8/RGB8/RGB565 native formats"
                        .into(),
                );
            }
            (format.format, 0)
        };
        if !video {
            if desc.format != ndk::hardware_buffer_format::HardwareBufferFormat::R8G8B8A8_UNORM
                && alpha != AndroidBufferAlpha::Opaque
            {
                return Err("RGB/RGBX hardware buffers require opaque alpha".into());
            }
            let instance = device.shared_instance().raw_instance();
            let physical = device.raw_physical_device();
            let regular = instance.get_physical_device_format_properties(physical, image_format);
            if !regular
                .optimal_tiling_features
                .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE)
            {
                return Err("Native RGB hardware-buffer format is not sampleable".into());
            }
            let mut external_query = vk::PhysicalDeviceExternalImageFormatInfo::default()
                .handle_type(vk::ExternalMemoryHandleTypeFlags::ANDROID_HARDWARE_BUFFER_ANDROID);
            let query = vk::PhysicalDeviceImageFormatInfo2::default()
                .format(image_format)
                .ty(vk::ImageType::TYPE_2D)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(vk::ImageUsageFlags::SAMPLED)
                .push_next(&mut external_query);
            let mut external_limits = vk::ExternalImageFormatProperties::default();
            let mut limits = vk::ImageFormatProperties2::default().push_next(&mut external_limits);
            instance
                .get_physical_device_image_format_properties2(physical, &query, &mut limits)
                .map_err(|error| {
                    format!("Querying native RGB hardware-buffer import: {error:?}")
                })?;
            let limits = limits.image_format_properties;
            if !external_limits
                .external_memory_properties
                .external_memory_features
                .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
                || desc.width > limits.max_extent.width
                || desc.height > limits.max_extent.height
                || limits.max_mip_levels == 0
                || limits.max_array_layers == 0
                || !limits.sample_counts.contains(vk::SampleCountFlags::TYPE_1)
            {
                return Err("Native RGB hardware-buffer format/extent cannot be imported".into());
            }
        }
        let mut external = vk::ExternalFormatANDROID::default().external_format(external_format);
        let mut conversion = vk::SamplerYcbcrConversionCreateInfo::default().format(image_format);
        if video {
            let (model, full_range) = match color {
                AndroidBufferColor::Bt601 { full_range } => {
                    (vk::SamplerYcbcrModelConversion::YCBCR_601, full_range)
                }
                AndroidBufferColor::Bt709 { full_range } => {
                    (vk::SamplerYcbcrModelConversion::YCBCR_709, full_range)
                }
                AndroidBufferColor::SdrRgb => unreachable!(),
            };
            let range = if full_range {
                vk::SamplerYcbcrRange::ITU_FULL
            } else {
                vk::SamplerYcbcrRange::ITU_NARROW
            };
            if model != format.suggested_ycbcr_model || range != format.suggested_ycbcr_range {
                return Err("Hardware-buffer matrix/range metadata differs from the driver's external conversion; this override is unsupported".into());
            }
            for location in [
                format.suggested_x_chroma_offset,
                format.suggested_y_chroma_offset,
            ] {
                let needed = match location {
                    vk::ChromaLocation::MIDPOINT => vk::FormatFeatureFlags::MIDPOINT_CHROMA_SAMPLES,
                    vk::ChromaLocation::COSITED_EVEN => {
                        vk::FormatFeatureFlags::COSITED_CHROMA_SAMPLES
                    }
                    _ => return Err("Unsupported hardware-buffer chroma location".into()),
                };
                if !format.format_features.contains(needed) {
                    return Err("Hardware-buffer chroma location is not sampleable".into());
                }
            }
            conversion = conversion
                .ycbcr_model(model)
                .ycbcr_range(range)
                .components(format.sampler_ycbcr_conversion_components)
                .x_chroma_offset(format.suggested_x_chroma_offset)
                .y_chroma_offset(format.suggested_y_chroma_offset)
                .chroma_filter(vk::Filter::NEAREST)
                .push_next(&mut external);
        }
        let mut imported = Imported {
            owner: owner.clone(),
            image: vk::Image::null(),
            memory: vk::DeviceMemory::null(),
            _buffer: buffer.acquire(),
        };
        let mut external_image =
            vk::ExternalFormatANDROID::default().external_format(external_format);
        let mut external_memory = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::ANDROID_HARDWARE_BUFFER_ANDROID);
        let mut image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(image_format)
            .extent(vk::Extent3D {
                width: desc.width,
                height: desc.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .push_next(&mut external_memory);
        if video {
            image_info = image_info.push_next(&mut external_image);
        }
        imported.image = raw
            .create_image(&image_info, None)
            .map_err(|error| format!("Creating hardware-buffer image: {error:?}"))?;
        let memory = device
            .shared_instance()
            .raw_instance()
            .get_physical_device_memory_properties(device.raw_physical_device());
        let memory_type = (0..memory.memory_type_count)
            .filter(|index| memory_bits & (1 << index) != 0)
            .min_by_key(|index| {
                !memory.memory_types[*index as usize]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
            })
            .ok_or("Hardware buffer has no compatible memory type")?;
        let mut ahb =
            vk::ImportAndroidHardwareBufferInfoANDROID::default().buffer(buffer.as_ptr().cast());
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(imported.image);
        imported.memory = raw
            .allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(allocation_size)
                    .memory_type_index(memory_type)
                    .push_next(&mut ahb)
                    .push_next(&mut dedicated),
                None,
            )
            .map_err(|error| format!("Importing dedicated hardware-buffer memory: {error:?}"))?;
        raw.bind_image_memory(imported.image, imported.memory, 0)
            .map_err(|error| format!("Binding hardware buffer: {error:?}"))?;
        let imported = Rc::new(imported);
        let target = Texture::new(
            owner,
            desc.width,
            desc.height,
            wgt::TextureFormat::Rgba8Unorm,
            crate::device::TextureFilter::Linear,
            true,
        )?;
        let materialize = Conversion::new(
            owner,
            imported.image,
            image_format,
            if video { Some(&conversion) } else { None },
            imported.clone(),
            target.clone(),
        )?;
        let sync = TransferSync::new(owner, Some(ready))?;
        {
            let mut commands = producer.submissions.recording()?;
            commands.synchronize(sync.clone());
            let family = device.queue_family_index();
            barrier(
                owner,
                &mut commands,
                imported.image,
                vk::ImageLayout::GENERAL,
                vk::ImageLayout::GENERAL,
                vk::QUEUE_FAMILY_FOREIGN_EXT,
                family,
                vk::AccessFlags::empty(),
                vk::AccessFlags::SHADER_READ,
            );
            barrier(
                owner,
                &mut commands,
                imported.image,
                vk::ImageLayout::GENERAL,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::QUEUE_FAMILY_IGNORED,
                vk::QUEUE_FAMILY_IGNORED,
                vk::AccessFlags::empty(),
                vk::AccessFlags::SHADER_READ,
            );
            materialize.record(&mut commands, alpha as u32);
            barrier(
                owner,
                &mut commands,
                imported.image,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::ImageLayout::GENERAL,
                vk::QUEUE_FAMILY_IGNORED,
                vk::QUEUE_FAMILY_IGNORED,
                vk::AccessFlags::SHADER_READ,
                vk::AccessFlags::empty(),
            );
            barrier(
                owner,
                &mut commands,
                imported.image,
                vk::ImageLayout::GENERAL,
                vk::ImageLayout::GENERAL,
                family,
                vk::QUEUE_FAMILY_FOREIGN_EXT,
                vk::AccessFlags::SHADER_READ,
                vk::AccessFlags::empty(),
            );
        }
        producer.submissions.submit()?;
        let release = sync.receipt(owner)?;
        let flags = if alpha == AndroidBufferAlpha::Opaque {
            api::ImageDescriptorFlags::IS_OPAQUE
        } else {
            api::ImageDescriptorFlags::empty()
        };
        let descriptor = api::ImageDescriptor::new(
            desc.width as i32,
            desc.height as i32,
            api::ImageFormat::RGBA8,
            flags,
        );
        Ok(HardwareBufferCopy {
            image: ExternalNativeImage::new(target, descriptor),
            release,
        })
    }
}
