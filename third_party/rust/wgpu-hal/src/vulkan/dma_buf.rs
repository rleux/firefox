use crate::dma_buf::{DmaBufFormat, DmaBufImageDescriptor, ExternalDomain};
use crate::Device as _;
use alloc::{boxed::Box, format, string::String, sync::Arc, vec, vec::Vec};
use ash::vk;
use core::sync::atomic::Ordering;
use std::{
    fs::File,
    os::fd::{AsRawFd, BorrowedFd, IntoRawFd},
};

pub struct Capabilities {
    owner: Arc<crate::DynOpenDevice>,
    device_uuid: [u8; 16],
    driver_uuid: [u8; 16],
    foreign_drm_node: Option<[u64; 2]>,
    formats: Arc<[DmaBufFormat]>,
    max_dimension: u32,
}
impl core::fmt::Debug for Capabilities {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DmaBufCapabilities")
            .field("formats", &self.formats)
            .finish_non_exhaustive()
    }
}
pub fn supports_imports(device: &dyn crate::DynDevice) -> bool {
    device.as_any().downcast_ref::<super::Device>()
        .is_some_and(|device| super::renderer_imports::enabled(&device.shared))
}

pub fn query_capabilities(
    owner: Arc<crate::DynOpenDevice>,
    adapter: &dyn crate::DynAdapter,
    max_dimension: u32,
) -> Result<Arc<Capabilities>, String> {
    let device = owner
        .device
        .as_any()
        .downcast_ref::<super::Device>()
        .ok_or("DMA-BUF requires Vulkan")?;
    let queue = owner
        .queue
        .as_any()
        .downcast_ref::<super::Queue>()
        .ok_or("DMA-BUF requires a graphics queue")?;
    let adapter = adapter
        .as_any()
        .downcast_ref::<super::Adapter>()
        .ok_or("DMA-BUF requires a Vulkan adapter")?;
    if !Arc::ptr_eq(&device.shared, &queue.device)
        || !Arc::ptr_eq(&device.shared.instance, &adapter.instance)
        || device.shared.physical_device != adapter.raw
        || device.shared.queue_index != 0
    {
        return Err("DMA-BUF graphics endpoint or adapter mismatch".into());
    }
    let max_dimension = unsafe {
        device
            .shared
            .instance
            .raw
            .get_physical_device_properties(device.shared.physical_device)
    }
    .limits
    .max_image_dimension2_d
    .min(max_dimension)
    .min(i32::MAX as u32);
    let mut caps = Capabilities {
        owner: owner.clone(),
        device_uuid: [0; 16],
        driver_uuid: [0; 16],
        foreign_drm_node: None,
        formats: Arc::from([]),
        max_dimension,
    };
    caps.foreign_drm_node = caps.query_foreign_rgb_drm_node(adapter);
    if !super::renderer_imports::enabled(&device.shared) {
        return Ok(Arc::new(caps));
    }
    let ids = super::external_sync::properties(device)
        .map_err(|e| format!("Querying DMA-BUF identity: {e}"))?;
    caps.device_uuid = ids.device_uuid;
    caps.driver_uuid = ids.driver_uuid;
    let mut formats = Vec::new();
    let mut modifiers = naga::FastHashMap::default();
    for format in [
        wgt::TextureFormat::Rgba8Unorm,
        wgt::TextureFormat::Bgra8Unorm,
    ] {
        for bits in 0..8 {
            let mut usage = wgt::TextureUses::RESOURCE;
            for (bit, flag) in [
                (1, wgt::TextureUses::COPY_SRC),
                (2, wgt::TextureUses::COPY_DST),
                (4, wgt::TextureUses::COLOR_TARGET),
            ] {
                if bits & bit != 0 {
                    usage |= flag;
                }
            }
            formats.extend(caps.query_dma_buf_formats(format, usage, &mut modifiers)?);
        }
    }
    caps.formats = formats.into();
    Ok(Arc::new(caps))
}
impl Capabilities {
    fn raw_device(&self) -> &super::Device {
        self.owner.device.as_any().downcast_ref().unwrap()
    }
    pub fn identity(&self) -> ([u8; 16], [u8; 16]) {
        (self.device_uuid, self.driver_uuid)
    }
    pub fn foreign_drm_node(&self) -> Option<[u64; 2]> {
        self.foreign_drm_node
    }
    pub fn formats(&self) -> &Arc<[DmaBufFormat]> {
        &self.formats
    }
    pub fn formats_for(
        &self,
        format: wgt::TextureFormat,
        usage: wgt::TextureUses,
    ) -> Result<Vec<DmaBufFormat>, String> {
        image_parameters(format, usage)?;
        Ok(self
            .formats
            .iter()
            .filter(|f| f.format == format && f.usage == usage)
            .cloned()
            .collect())
    }
}
fn image_parameters(
    format: wgt::TextureFormat,
    usage: wgt::TextureUses,
) -> Result<(vk::Format, vk::ImageUsageFlags, vk::FormatFeatureFlags), String> {
    let format = match format {
        wgt::TextureFormat::Rgba8Unorm => vk::Format::R8G8B8A8_UNORM,
        wgt::TextureFormat::Bgra8Unorm => vk::Format::B8G8R8A8_UNORM,
        _ => return Err("DMA-BUF sampling requires RGBA8 or BGRA8".into()),
    };
    let allowed = wgt::TextureUses::RESOURCE
        | wgt::TextureUses::COPY_SRC
        | wgt::TextureUses::COPY_DST
        | wgt::TextureUses::COLOR_TARGET;
    if !usage.contains(wgt::TextureUses::RESOURCE) || !allowed.contains(usage) {
        return Err("Unsupported sampled DMA-BUF usage".into());
    }
    let mut vk_usage = vk::ImageUsageFlags::SAMPLED;
    let mut required =
        vk::FormatFeatureFlags::SAMPLED_IMAGE | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR;
    for (uses, image, features) in [
        (
            wgt::TextureUses::COPY_SRC,
            vk::ImageUsageFlags::TRANSFER_SRC,
            vk::FormatFeatureFlags::TRANSFER_SRC,
        ),
        (
            wgt::TextureUses::COPY_DST,
            vk::ImageUsageFlags::TRANSFER_DST,
            vk::FormatFeatureFlags::TRANSFER_DST,
        ),
        (
            wgt::TextureUses::COLOR_TARGET,
            vk::ImageUsageFlags::COLOR_ATTACHMENT,
            vk::FormatFeatureFlags::COLOR_ATTACHMENT,
        ),
    ] {
        if usage.contains(uses) {
            vk_usage |= image;
            required |= features;
        }
    }
    Ok((format, vk_usage, required))
}

fn eligible_modifier(
    entry: &vk::DrmFormatModifierPropertiesEXT,
    required: vk::FormatFeatureFlags,
) -> bool {
    entry.drm_format_modifier_plane_count == 1
        && entry.drm_format_modifier_tiling_features.contains(required)
}

impl Capabilities {
    fn query_foreign_rgb_drm_node(&self, adapter: &super::Adapter) -> Option<[u64; 2]> {
        if !self
            .raw_device()
            .enabled_device_extensions()
            .contains(&ash::ext::queue_family_foreign::NAME)
            || !adapter
                .physical_device_capabilities()
                .supports_extension(ash::ext::physical_device_drm::NAME)
            || !super::external_sync::supports_sync_file_import(self.raw_device())
        {
            return None;
        }
        let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
        unsafe {
            self.raw_device()
                .shared_instance()
                .raw_instance()
                .get_physical_device_properties2(
                    self.raw_device().raw_physical_device(),
                    &mut vk::PhysicalDeviceProperties2::default().push_next(&mut drm),
                );
        }
        (drm.has_render != 0 && drm.render_major >= 0 && drm.render_minor >= 0)
            .then_some([drm.render_major as u64, drm.render_minor as u64])
    }

    fn query_dma_buf_formats(
        &self,
        format: wgt::TextureFormat,
        usage: wgt::TextureUses,
        modifiers: &mut naga::FastHashMap<
            wgt::TextureFormat,
            Arc<[vk::DrmFormatModifierPropertiesEXT]>,
        >,
    ) -> Result<Vec<DmaBufFormat>, String> {
        let (raw_format, raw_usage, required) = image_parameters(format, usage)?;
        if !super::renderer_imports::enabled(&self.raw_device().shared) {
            return Ok(Vec::new());
        }
        let raw = self.raw_device().shared_instance().raw_instance();
        let physical = self.raw_device().raw_physical_device();
        let entries = modifiers
            .entry(format)
            .or_insert_with(|| {
                let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
                unsafe {
                    raw.get_physical_device_format_properties2(
                        physical,
                        raw_format,
                        &mut vk::FormatProperties2::default().push_next(&mut list),
                    );
                }
                let mut entries = vec![
                    vk::DrmFormatModifierPropertiesEXT::default();
                    list.drm_format_modifier_count as usize
                ];
                if entries.is_empty() {
                    return Vec::new().into();
                }
                list.p_drm_format_modifier_properties = entries.as_mut_ptr();
                unsafe {
                    raw.get_physical_device_format_properties2(
                        physical,
                        raw_format,
                        &mut vk::FormatProperties2::default().push_next(&mut list),
                    );
                }
                entries.truncate(list.drm_format_modifier_count as usize);
                entries.into()
            })
            .clone();
        let mut formats = Vec::new();
        for entry in entries
            .iter()
            .filter(|entry| eligible_modifier(entry, required))
        {
            let mut external = vk::PhysicalDeviceExternalImageFormatInfo::default()
                .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let mut modifier = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
                .drm_format_modifier(entry.drm_format_modifier)
                .sharing_mode(vk::SharingMode::EXCLUSIVE);
            let info = vk::PhysicalDeviceImageFormatInfo2::default()
                .format(raw_format)
                .ty(vk::ImageType::TYPE_2D)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(raw_usage)
                .push_next(&mut external)
                .push_next(&mut modifier);
            let mut external_properties = vk::ExternalImageFormatProperties::default();
            let mut properties =
                vk::ImageFormatProperties2::default().push_next(&mut external_properties);
            match unsafe {
                raw.get_physical_device_image_format_properties2(physical, &info, &mut properties)
            } {
                Ok(()) => {}
                Err(vk::Result::ERROR_FORMAT_NOT_SUPPORTED) => continue,
                Err(error) => return Err(format!("Querying DMA-BUF format: {error:?}")),
            }
            let limits = properties.image_format_properties;
            let memory = external_properties.external_memory_properties;
            if !memory
                .external_memory_features
                .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
                || !memory
                    .compatible_handle_types
                    .contains(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
                || !limits.sample_counts.contains(vk::SampleCountFlags::TYPE_1)
                || limits.max_mip_levels == 0
                || limits.max_array_layers == 0
                || limits.max_extent.depth == 0
                || limits.max_resource_size == 0
            {
                continue;
            }
            let maximum = self.max_dimension.min(i32::MAX as u32);
            let max_size = [
                limits.max_extent.width.min(maximum),
                limits.max_extent.height.min(maximum),
            ];
            if max_size.contains(&0) {
                continue;
            }
            formats.push(DmaBufFormat {
                format,
                usage,
                modifier: entry.drm_format_modifier,
                max_size,
                max_resource_size: limits.max_resource_size,
                exportable: memory
                    .external_memory_features
                    .contains(vk::ExternalMemoryFeatureFlags::EXPORTABLE),
                dedicated_only: memory
                    .external_memory_features
                    .contains(vk::ExternalMemoryFeatureFlags::DEDICATED_ONLY),
            });
        }
        Ok(formats)
    }
}
fn memory_type(bits: u32, properties: &vk::PhysicalDeviceMemoryProperties) -> Result<u32, String> {
    (0..properties.memory_type_count)
        .find(|&index| {
            bits & (1 << index) != 0
                && !properties.memory_types[index as usize]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::PROTECTED)
        })
        .ok_or_else(|| "No compatible unprotected DMA-BUF memory type".into())
}

impl Capabilities {
    /// # Safety
    /// The FD and descriptor must name a compatible unprotected Vulkan export;
    /// GPU use requires externally synchronized ownership acquisition.
    pub unsafe fn import_dma_buf(
        self: &Arc<Self>,
        fd: BorrowedFd<'_>,
        descriptor: DmaBufImageDescriptor,
    ) -> Result<Arc<Image>, String> {
        unsafe { self.import_impl(fd, descriptor, true) }
    }
    /// # Safety
    /// The FD must name the negotiated unprotected linear RGB allocation.
    pub unsafe fn import_foreign_rgb(
        self: &Arc<Self>,
        fd: BorrowedFd<'_>,
        descriptor: DmaBufImageDescriptor,
    ) -> Result<Arc<Image>, String> {
        if self.foreign_drm_node.is_none()
            || descriptor.modifier != 0
            || descriptor.usage != wgt::TextureUses::RESOURCE
        {
            return Err("Foreign queue ownership is unavailable".into());
        }
        unsafe { self.import_impl(fd, descriptor, false) }
    }
    unsafe fn import_impl(
        self: &Arc<Self>,
        fd: BorrowedFd<'_>,
        descriptor: DmaBufImageDescriptor,
        check_vulkan_identity: bool,
    ) -> Result<Arc<Image>, String> {
        if self
            .raw_device()
            .shared
            .shared_queue_group
            .as_ref()
            .is_some_and(|g| g.closed.load(Ordering::Acquire))
        {
            return Err("Vulkan device requires recreation".into());
        }
        let group = self.raw_device().shared.shared_queue_group.as_ref();
        let _host = group.map(|g| g.host[0].lock());
        if group.is_some_and(|g| g.closed.load(Ordering::Acquire)) {
            return Err("Shared native context is closed".into());
        }
        let supported = self
            .formats_for(descriptor.format, descriptor.usage)?
            .iter()
            .find(|entry| entry.modifier() == descriptor.modifier)
            .cloned()
            .ok_or("Unsupported DMA-BUF format, modifier or usage")?;
        if !supported.supports_extent(descriptor.size) {
            return Err("DMA-BUF dimensions exceed device limits".into());
        }
        if check_vulkan_identity
            && self.identity() != (descriptor.device_uuid, descriptor.driver_uuid)
        {
            return Err("DMA-BUF device or driver identity mismatch".into());
        }
        let file = File::from(
            fd.try_clone_to_owned()
                .map_err(|error| format!("Duplicating DMA-BUF: {error}"))?,
        );
        let bytes = file
            .metadata()
            .map_err(|error| format!("Querying DMA-BUF size: {error}"))?
            .len();
        descriptor.validate_plane(bytes)?;
        let (format, usage, _) = image_parameters(descriptor.format, descriptor.usage)?;
        let plane = [vk::SubresourceLayout::default()
            .offset(descriptor.offset)
            .row_pitch(descriptor.row_pitch)];
        let mut modifier = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(descriptor.modifier)
            .plane_layouts(&plane);
        let mut external = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: descriptor.size[0],
                height: descriptor.size[1],
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external)
            .push_next(&mut modifier);
        let raw = self.raw_device().raw_device();
        let image = unsafe { raw.create_image(&info, None) }
            .map_err(|error| format!("Creating imported DMA-BUF image: {error:?}"))?;
        let mut imported = Image {
            owner: self.owner.clone(),
            image,
            memory: vk::DeviceMemory::null(),
            descriptor,
        };
        let requirements = unsafe { raw.get_image_memory_requirements(image) };
        if requirements.size > bytes || requirements.size > supported.max_resource_size() {
            return Err("DMA-BUF image requirements exceed allocation or format limits".into());
        }
        let extension = ash::khr::external_memory_fd::Device::new(
            self.raw_device().shared_instance().raw_instance(),
            raw,
        );
        let mut fd_properties = vk::MemoryFdPropertiesKHR::default();
        unsafe {
            extension.get_memory_fd_properties(
                vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                file.as_raw_fd(),
                &mut fd_properties,
            )
        }
        .map_err(|error| format!("Querying DMA-BUF memory types: {error:?}"))?;
        let properties = unsafe {
            self.raw_device()
                .shared_instance()
                .raw_instance()
                .get_physical_device_memory_properties(self.raw_device().raw_physical_device())
        };
        let index = memory_type(
            requirements.memory_type_bits & fd_properties.memory_type_bits,
            &properties,
        )?;
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let mut import = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(file.as_raw_fd());
        imported.memory = unsafe {
            raw.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(requirements.size)
                    .memory_type_index(index)
                    .push_next(&mut dedicated)
                    .push_next(&mut import),
                None,
            )
        }
        .map_err(|error| format!("Importing DMA-BUF memory: {error:?}"))?;
        let _ = file.into_raw_fd();
        unsafe { raw.bind_image_memory(image, imported.memory, 0) }
            .map_err(|error| format!("Binding DMA-BUF image memory: {error:?}"))?;
        Ok(Arc::new(imported))
    }
}

pub struct Image {
    owner: Arc<crate::DynOpenDevice>,
    image: vk::Image,
    memory: vk::DeviceMemory,
    descriptor: DmaBufImageDescriptor,
}
impl core::fmt::Debug for Image {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ImportedDmaBuf")
            .field("descriptor", &self.descriptor)
            .finish_non_exhaustive()
    }
}
impl Drop for Image {
    fn drop(&mut self) {
        unsafe {
            let device = self.device();
            device.shared.raw.destroy_image(self.image, None);
            if self.memory != vk::DeviceMemory::null() {
                device.shared.raw.free_memory(self.memory, None);
            }
        }
    }
}
impl Image {
    fn device(&self) -> &super::Device {
        self.owner.device.as_any().downcast_ref().unwrap()
    }
    pub fn descriptor(&self) -> &DmaBufImageDescriptor {
        &self.descriptor
    }
    pub fn create_alias(
        self: &Arc<Self>,
        target: &dyn crate::DynDevice,
    ) -> Result<Box<dyn crate::DynTexture>, String> {
        let target = target
            .as_any()
            .downcast_ref::<super::Device>()
            .ok_or("DMA-BUF alias requires Vulkan")?;
        if !Arc::ptr_eq(&target.shared, &self.device().shared) {
            return Err("DMA-BUF alias device mismatch".into());
        }
        let group = target.shared.shared_queue_group.as_ref();
        let _host = group.map(|g| g.host[0].lock());
        if group.is_some_and(|g| g.closed.load(Ordering::Acquire)) {
            return Err("Shared native context is closed".into());
        }
        let d = &self.descriptor;
        let lease = self.clone();
        let alias = unsafe {
            target.texture_from_raw(
                self.image,
                &crate::TextureDescriptor {
                    label: None,
                    size: wgt::Extent3d {
                        width: d.size[0],
                        height: d.size[1],
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgt::TextureDimension::D2,
                    format: d.format,
                    usage: d.usage,
                    memory_flags: crate::MemoryFlags::empty(),
                    view_formats: Vec::new(),
                },
                Some(Box::new(move || drop(lease))),
                super::TextureMemory::External,
            )
        };
        unsafe { target.add_raw_texture(&alias) };
        Ok(Box::new(alias))
    }
    pub fn validate_access(
        &self,
        encoder: &dyn crate::DynCommandEncoder,
        domain: ExternalDomain,
    ) -> Result<(), String> {
        let encoder = encoder
            .as_any()
            .downcast_ref::<super::CommandEncoder>()
            .ok_or("DMA-BUF access requires Vulkan")?;
        if !Arc::ptr_eq(&encoder.device, &self.device().shared)
            || encoder.active == vk::CommandBuffer::null()
        {
            return Err("DMA-BUF encoder owner mismatch".into());
        }
        if domain == ExternalDomain::Foreign
            && (!self
                .device()
                .shared
                .enabled_extensions
                .contains(&ash::ext::queue_family_foreign::NAME)
                || !super::external_sync::supports_sync_file_import(self.device()))
        {
            return Err("Foreign queue ownership is unavailable".into());
        }
        Ok(())
    }
    /// # Safety
    /// validate_access must have succeeded for this recording. External
    /// synchronization and allocation lifetime must cover accepted work.
    pub unsafe fn record_access(
        &self,
        encoder: &mut dyn crate::DynCommandEncoder,
        acquire: bool,
        domain: ExternalDomain,
    ) {
        let encoder = encoder
            .as_any_mut()
            .downcast_mut::<super::CommandEncoder>()
            .unwrap();
        let external = match domain {
            ExternalDomain::External => vk::QUEUE_FAMILY_EXTERNAL,
            ExternalDomain::Foreign => vk::QUEUE_FAMILY_FOREIGN_EXT,
        };
        let family = self.device().shared.family_index;
        unsafe {
            if acquire {
                self.barrier(
                    encoder,
                    vk::ImageLayout::GENERAL,
                    vk::ImageLayout::GENERAL,
                    external,
                    family,
                    vk::AccessFlags::empty(),
                    vk::AccessFlags::SHADER_READ,
                );
                self.barrier(
                    encoder,
                    vk::ImageLayout::GENERAL,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::QUEUE_FAMILY_IGNORED,
                    vk::QUEUE_FAMILY_IGNORED,
                    vk::AccessFlags::empty(),
                    vk::AccessFlags::SHADER_READ,
                );
            } else {
                self.barrier(
                    encoder,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::ImageLayout::GENERAL,
                    vk::QUEUE_FAMILY_IGNORED,
                    vk::QUEUE_FAMILY_IGNORED,
                    vk::AccessFlags::SHADER_READ,
                    vk::AccessFlags::empty(),
                );
                self.barrier(
                    encoder,
                    vk::ImageLayout::GENERAL,
                    vk::ImageLayout::GENERAL,
                    family,
                    external,
                    vk::AccessFlags::SHADER_READ,
                    vk::AccessFlags::empty(),
                );
            }
        }
    }
    unsafe fn barrier(
        &self,
        encoder: &super::CommandEncoder,
        old: vk::ImageLayout,
        new: vk::ImageLayout,
        src: u32,
        dst: u32,
        src_access: vk::AccessFlags,
        dst_access: vk::AccessFlags,
    ) {
        unsafe {
            self.device().shared.raw.cmd_pipeline_barrier(
                encoder.active,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[vk::ImageMemoryBarrier::default()
                    .image(self.image)
                    .old_layout(old)
                    .new_layout(new)
                    .src_queue_family_index(src)
                    .dst_queue_family_index(dst)
                    .src_access_mask(src_access)
                    .dst_access_mask(dst_access)
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .level_count(1)
                            .layer_count(1),
                    )],
            );
        }
    }
}

#[cfg(feature = "validation_canary")]
pub mod testing;
#[cfg(test)]
mod tests;
