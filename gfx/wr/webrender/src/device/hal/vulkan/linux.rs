/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::super::resources::{Owned, Texture, texture_format, bytes_per_pixel};
use super::super::submission::Submission;
#[cfg(test)]
use super::super::submission::SubmissionSync;
use std::{collections::HashSet, fs::File, rc::Rc};
use std::os::fd::{AsFd, BorrowedFd, FromRawFd, OwnedFd};
use super::sync_file::{SyncFile, TransferSync};
use std::os::unix::fs::MetadataExt;

type V = hal::api::Vulkan;

#[derive(Clone, Copy, Debug)]
pub struct DmaBufLayout {
    size: [u32; 2],
    format: api::ImageFormat,
    modifier: u64,
    stride: u64,
    offset: u64,
    device_uuid: [u8; 16],
    driver_uuid: [u8; 16],
}

impl DmaBufLayout {
    pub fn new(
        size: [u32; 2],
        format: api::ImageFormat,
        modifier: u64,
        stride: u64,
        offset: u64,
        device_uuid: [u8; 16],
        driver_uuid: [u8; 16],
    ) -> Result<Self> {
        let layout = Self {
            size,
            format,
            modifier,
            stride,
            offset,
            device_uuid,
            driver_uuid,
        };
        layout.validate()?;
        Ok(layout)
    }
    pub fn size(&self) -> [u32; 2] {
        self.size
    }
    pub fn format(&self) -> api::ImageFormat {
        self.format
    }
    pub fn modifier(&self) -> u64 {
        self.modifier
    }
    pub fn stride(&self) -> u64 {
        self.stride
    }
    pub fn offset(&self) -> u64 {
        self.offset
    }
    pub fn device_uuid(&self) -> [u8; 16] {
        self.device_uuid
    }
    pub fn driver_uuid(&self) -> [u8; 16] {
        self.driver_uuid
    }
    fn validate(&self) -> Result<()> {
        if self.size.contains(&0) || self.size.iter().any(|&v| v > i32::MAX as u32) {
            return Err("Invalid DMA-BUF dimensions".into());
        }
        let format = plane_format(self.format)?;
        let row = u64::from(self.size[0]) * bytes_per_pixel(format) as u64;
        if self.stride < row {
            return Err("DMA-BUF stride is smaller than a pixel row".into());
        }
        self.stride
            .checked_mul(u64::from(self.size[1] - 1))
            .and_then(|v| v.checked_add(row))
            .and_then(|v| v.checked_add(self.offset))
            .ok_or("DMA-BUF layout overflow")?;
        Ok(())
    }
    fn descriptor(&self) -> api::ImageDescriptor {
        api::ImageDescriptor::new(
            self.size[0] as i32,
            self.size[1] as i32,
            self.format,
            api::ImageDescriptorFlags::empty(),
        )
    }
}

pub struct DmaBufPlane {
    fd: OwnedFd,
    layout: DmaBufLayout,
}
impl DmaBufPlane {
    pub fn new(fd: OwnedFd, layout: DmaBufLayout) -> Self {
        Self { fd, layout }
    }
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
    pub fn layout(&self) -> DmaBufLayout {
        self.layout
    }
    pub fn into_parts(self) -> (OwnedFd, DmaBufLayout) {
        (self.fd, self.layout)
    }
}

pub struct DmaBufExport {
    plane: DmaBufPlane,
    ready: SyncFile,
}
impl DmaBufExport {
    pub fn plane(&self) -> &DmaBufPlane {
        &self.plane
    }
    pub fn ready(&self) -> &SyncFile {
        &self.ready
    }
    pub fn into_parts(self) -> (DmaBufPlane, SyncFile) {
        (self.plane, self.ready)
    }
}

pub struct DmaBufCopy {
    images: Vec<ExternalNativeImage>,
    release: SyncFile,
}
impl DmaBufCopy {
    pub fn images(&self) -> &[ExternalNativeImage] {
        &self.images
    }
    pub fn release(&self) -> &SyncFile {
        &self.release
    }
    pub fn into_parts(self) -> (Vec<ExternalNativeImage>, SyncFile) {
        (self.images, self.release)
    }
}

#[derive(Debug)]
pub struct DmaBufCapabilities {
    supported: bool,
    formats: Vec<(api::ImageFormat, Vec<u64>)>,
    device_uuid: [u8; 16],
    driver_uuid: [u8; 16],
}
impl DmaBufCapabilities {
    pub fn supported(&self) -> bool {
        self.supported
    }
    pub fn formats(&self) -> &[(api::ImageFormat, Vec<u64>)] {
        &self.formats
    }
    pub fn device_uuid(&self) -> [u8; 16] {
        self.device_uuid
    }
    pub fn driver_uuid(&self) -> [u8; 16] {
        self.driver_uuid
    }
}

pub(super) fn open_adapter(
    adapter: &hal::ExposedAdapter<V>,
    mut features: wgt::Features,
) -> Result<(hal::OpenDevice<V>, wgt::Features)> {
    let caps = adapter.adapter.physical_device_capabilities();
    let mut semaphore = vk::ExternalSemaphoreProperties::default();
    unsafe {
        adapter
            .adapter
            .shared_instance()
            .raw_instance()
            .get_physical_device_external_semaphore_properties(
                adapter.adapter.raw_physical_device(),
                &vk::PhysicalDeviceExternalSemaphoreInfo::default()
                    .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD),
                &mut semaphore,
            )
    };
    let supported = adapter
        .features
        .contains(wgt::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF)
        && caps.supports_extension(ash::khr::external_semaphore_fd::NAME)
        && semaphore.external_semaphore_features.contains(
            vk::ExternalSemaphoreFeatureFlags::IMPORTABLE
                | vk::ExternalSemaphoreFeatureFlags::EXPORTABLE,
        );
    if supported {
        features |= wgt::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF;
    }
    let callback: Option<Box<hal::vulkan::CreateDeviceCallback<'_>>> = if supported {
        Some(Box::new(|args| {
            args.extensions.push(ash::khr::external_semaphore_fd::NAME);
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
    .map_err(|e| format!("Opening DMA-BUF-capable Vulkan device: {e:?}"))?;
    Ok((open, features))
}

fn plane_format(format: api::ImageFormat) -> Result<wgt::TextureFormat> {
    if !matches!(
        format,
        api::ImageFormat::RGBA8
            | api::ImageFormat::BGRA8
            | api::ImageFormat::R8
            | api::ImageFormat::RG8
            | api::ImageFormat::R16
            | api::ImageFormat::RG16
    ) {
        return Err("DMA-BUF copy requires a supported single-plane color format".into());
    }
    texture_format(format)
}

fn ids(owner: &Device<V>) -> ([u8; 16], [u8; 16]) {
    let mut id = vk::PhysicalDeviceIDProperties::default();
    let mut props = vk::PhysicalDeviceProperties2::default().push_next(&mut id);
    unsafe {
        owner
            .open
            .device
            .shared_instance()
            .raw_instance()
            .get_physical_device_properties2(owner.open.device.raw_physical_device(), &mut props)
    };
    (id.device_uuid, id.driver_uuid)
}

fn supported(owner: &Device<V>) -> bool {
    owner
        .features
        .contains(wgt::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF)
        && owner
            .open
            .device
            .enabled_device_extensions()
            .contains(&ash::khr::external_semaphore_fd::NAME)
}

fn modifiers(owner: &Device<V>, format: wgt::TextureFormat) -> Vec<u64> {
    if !supported(owner) {
        return Vec::new();
    }
    let raw = owner.open.device.shared_instance().raw_instance();
    let physical = owner.open.device.raw_physical_device();
    let format = owner.adapter.texture_format_as_raw(format);
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
    unsafe {
        raw.get_physical_device_format_properties2(
            physical,
            format,
            &mut vk::FormatProperties2::default().push_next(&mut list),
        )
    };
    let mut entries = vec![
        vk::DrmFormatModifierPropertiesEXT::default();
        list.drm_format_modifier_count as usize
    ];
    list.p_drm_format_modifier_properties = entries.as_mut_ptr();
    unsafe {
        raw.get_physical_device_format_properties2(
            physical,
            format,
            &mut vk::FormatProperties2::default().push_next(&mut list),
        )
    };
    entries.truncate(list.drm_format_modifier_count as usize);
    entries
        .into_iter()
        .filter(|entry| {
            entry.drm_format_modifier_plane_count == 1
                && entry.drm_format_modifier_tiling_features.contains(
                    vk::FormatFeatureFlags::TRANSFER_SRC | vk::FormatFeatureFlags::TRANSFER_DST,
                )
        })
        .filter_map(|entry| {
            modifier_limits(owner, format, entry.drm_format_modifier)
                .ok()
                .map(|_| entry.drm_format_modifier)
        })
        .collect()
}

fn modifier_limits(
    owner: &Device<V>,
    format: vk::Format,
    modifier: u64,
) -> Result<vk::ImageFormatProperties> {
    let mut external = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let mut modifier = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
        .drm_format_modifier(modifier)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(format)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST)
        .push_next(&mut external)
        .push_next(&mut modifier);
    let mut external_props = vk::ExternalImageFormatProperties::default();
    let mut props = vk::ImageFormatProperties2::default().push_next(&mut external_props);
    unsafe {
        owner
            .open
            .device
            .shared_instance()
            .raw_instance()
            .get_physical_device_image_format_properties2(
                owner.open.device.raw_physical_device(),
                &info,
                &mut props,
            )
    }
    .map_err(|e| format!("DMA-BUF format query: {e:?}"))?;
    let limits = props.image_format_properties;
    if !external_props
        .external_memory_properties
        .external_memory_features
        .contains(
            vk::ExternalMemoryFeatureFlags::IMPORTABLE | vk::ExternalMemoryFeatureFlags::EXPORTABLE,
        )
    {
        return Err("DMA-BUF format is not importable and exportable".into());
    }
    Ok(limits)
}

fn check_extent(
    owner: &Device<V>,
    format: wgt::TextureFormat,
    modifier: u64,
    size: [u32; 2],
) -> Result<()> {
    let limits = modifier_limits(owner, owner.adapter.texture_format_as_raw(format), modifier)?;
    if size.contains(&0)
        || size[0] > limits.max_extent.width
        || size[1] > limits.max_extent.height
        || !limits.sample_counts.contains(vk::SampleCountFlags::TYPE_1)
        || limits.max_mip_levels == 0
        || limits.max_array_layers == 0
        || u64::from(size[0]) * u64::from(size[1]) * bytes_per_pixel(format) as u64
            > limits.max_resource_size
    {
        return Err("DMA-BUF dimensions exceed format/modifier limits".into());
    }
    Ok(())
}

unsafe fn image_barrier(
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

fn copy_region(size: [u32; 2]) -> hal::TextureCopy {
    let base = hal::TextureCopyBase {
        mip_level: 0,
        array_layer: 0,
        origin: wgt::Origin3d::ZERO,
        aspect: hal::FormatAspects::COLOR,
    };
    hal::TextureCopy {
        src_base: base.clone(),
        dst_base: base,
        size: wgt::Extent3d {
            width: size[0],
            height: size[1],
            depth_or_array_layers: 1,
        }
        .into(),
    }
}

impl ExternalImageDevice {
    fn dmabuf_producer(&self) -> Result<&Producer<V>> {
        let producer = self
            .0
            .as_any()
            .downcast_ref::<Producer<V>>()
            .ok_or("DMA-BUF requires Vulkan")?;
        producer.ensure_healthy()?;
        Ok(producer)
    }
    pub fn dmabuf_capabilities(&self) -> Result<DmaBufCapabilities> {
        let owner = &self.dmabuf_producer()?.owner;
        let mut formats = Vec::new();
        for format in [
            api::ImageFormat::RGBA8,
            api::ImageFormat::BGRA8,
            api::ImageFormat::R8,
            api::ImageFormat::RG8,
            api::ImageFormat::R16,
            api::ImageFormat::RG16,
        ] {
            let values = modifiers(owner, plane_format(format)?);
            if !values.is_empty() {
                formats.push((format, values));
            }
        }
        Ok(DmaBufCapabilities {
            supported: supported(owner),
            formats,
            device_uuid: ids(owner).0,
            driver_uuid: ids(owner).1,
        })
    }

    pub fn export_dmabuf_image(
        &self,
        image: &ExternalNativeImage,
        modifier: u64,
    ) -> Result<DmaBufExport> {
        let producer = self.dmabuf_producer()?;
        let owner = &producer.owner;
        if !supported(owner) {
            return Err("DMA-BUF modifier and sync-file sharing are unavailable".into());
        }
        image.ensure_idle()?;
        let descriptor = image.descriptor();
        let format = plane_format(descriptor.format)?;
        let source = image.texture(owner)?;
        if source.mip_count != 1 || !source.sample_initialized() {
            return Err("DMA-BUF export needs an initialized single mip".into());
        }
        if !modifiers(owner, format).contains(&modifier) {
            return Err("Unsupported export format/modifier or memory-plane count".into());
        }
        let size = [descriptor.size.width as u32, descriptor.size.height as u32];
        check_extent(owner, format, modifier, size)?;
        let (target, plane) = export_allocation(owner, size, descriptor.format, modifier)?;
        let sync = TransferSync::new(owner, None)?;
        {
            let mut commands = producer.submissions.recording()?;
            commands.keep(target.clone());
            commands.synchronize(sync.clone());
            let previous = source.current_usage();
            source.transition(&mut commands, wgt::TextureUses::COPY_SRC);
            unsafe {
                image_barrier(
                    owner,
                    &mut commands,
                    target.raw_handle(),
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::QUEUE_FAMILY_IGNORED,
                    vk::QUEUE_FAMILY_IGNORED,
                    vk::AccessFlags::empty(),
                    vk::AccessFlags::TRANSFER_WRITE,
                );
                commands.encoder().copy_texture_to_texture(
                    &source.raw,
                    wgt::TextureUses::COPY_SRC,
                    &target,
                    std::iter::once(copy_region(size)),
                );
                image_barrier(
                    owner,
                    &mut commands,
                    target.raw_handle(),
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::ImageLayout::GENERAL,
                    vk::QUEUE_FAMILY_IGNORED,
                    vk::QUEUE_FAMILY_IGNORED,
                    vk::AccessFlags::TRANSFER_WRITE,
                    vk::AccessFlags::empty(),
                );
                image_barrier(
                    owner,
                    &mut commands,
                    target.raw_handle(),
                    vk::ImageLayout::GENERAL,
                    vk::ImageLayout::GENERAL,
                    owner.open.device.queue_family_index(),
                    vk::QUEUE_FAMILY_EXTERNAL,
                    vk::AccessFlags::TRANSFER_WRITE,
                    vk::AccessFlags::empty(),
                );
            }
            source.transition(&mut commands, previous);
        }
        producer.submissions.submit()?;
        Ok(DmaBufExport {
            plane,
            ready: sync.receipt(owner)?,
        })
    }

    pub fn wait_dmabuf_release(&self, release: &SyncFile) -> Result<()> {
        let producer = self.dmabuf_producer()?;
        if !supported(&producer.owner) {
            return Err("DMA-BUF sync-file sharing is unavailable".into());
        }
        let sync = TransferSync::new(&producer.owner, Some(release))?;
        producer.submissions.recording()?.synchronize(sync);
        producer.submissions.wait()
    }

    /// Copies independently allocated single-memory-plane Vulkan DMA-BUFs into owned WR images.
    /// # Safety
    /// FDs/layouts must describe live, disjoint allocations on this physical device and driver. The producer
    /// must release GENERAL-layout images to QUEUE_FAMILY_EXTERNAL before `ready` signals and must
    /// not access them until the returned release sync-file signals. Foreign/non-Vulkan producers,
    /// aliases and reinterpreted multi-planar images are outside this contract. Images must be
    /// transfer-source/destination compatible, with one mip and one array layer.
    pub unsafe fn copy_dmabuf_planes(
        &self,
        planes: &[DmaBufPlane],
        ready: &SyncFile,
    ) -> Result<DmaBufCopy> {
        let producer = self.dmabuf_producer()?;
        let owner = &producer.owner;
        if !supported(owner) {
            return Err("DMA-BUF modifier and sync-file sharing are unavailable".into());
        }
        if planes.is_empty() || planes.len() > 3 {
            return Err("DMA-BUF copy needs one to three independent planes".into());
        }
        let mut identities = HashSet::new();
        for plane in planes {
            plane.layout.validate()?;
            if plane.layout.driver_uuid != ids(owner).1 {
                return Err("DMA-BUF driver identity differs".into());
            }
            if plane.layout.device_uuid != ids(owner).0 {
                return Err("DMA-BUF belongs to another physical device".into());
            }
            owner.layout(plane.layout.size[0], plane.layout.size[1])?;
            if !modifiers(owner, plane_format(plane.layout.format)?)
                .contains(&plane.layout.modifier)
            {
                return Err(
                    "Unsupported DMA-BUF format/modifier or multi-memory-plane layout".into(),
                );
            }
            check_extent(
                owner,
                plane_format(plane.layout.format)?,
                plane.layout.modifier,
                plane.layout.size,
            )?;
            let stat = File::from(plane.fd.try_clone().map_err(|e| e.to_string())?)
                .metadata()
                .map_err(|e| e.to_string())?;
            let end = plane.layout.offset
                + plane.layout.stride * u64::from(plane.layout.size[1] - 1)
                + u64::from(plane.layout.size[0])
                    * bytes_per_pixel(plane_format(plane.layout.format)?) as u64;
            if end > stat.len() {
                return Err("DMA-BUF layout exceeds the allocation".into());
            }
            if !identities.insert((stat.dev(), stat.ino())) {
                return Err("Aliased DMA-BUF planes are unsupported".into());
            }
        }
        #[cfg(any(test, feature = "hal-testing"))]
        owner.check_fault(FailurePoint::Import)?;
        let mut imported = Vec::new();
        let mut images = Vec::new();
        for plane in planes {
            let layout = plane.layout;
            let format = plane_format(layout.format)?;
            let desc = texture_descriptor(
                wgt::Extent3d {
                    width: layout.size[0],
                    height: layout.size[1],
                    depth_or_array_layers: 1,
                },
                format,
                wgt::TextureUses::COPY_SRC | wgt::TextureUses::COPY_DST,
            );
            let raw = owner
                .open
                .device
                .texture_from_dmabuf_fd(
                    plane.fd.try_clone().map_err(|e| e.to_string())?,
                    &desc,
                    layout.modifier,
                    layout.stride,
                    layout.offset,
                )
                .map_err(|e| format!("Importing DMA-BUF: {e:?}"))?;
            imported.push(Rc::new(Owned::new(
                owner,
                raw,
                <hal::vulkan::Device as hal::Device>::destroy_texture,
            )));
            images.push(Texture::new(
                owner,
                layout.size[0],
                layout.size[1],
                format,
                crate::device::TextureFilter::Linear,
                false,
            )?);
        }
        let sync = TransferSync::new(owner, Some(ready))?;
        {
            let mut commands = producer.submissions.recording()?;
            commands.synchronize(sync.clone());
            for ((plane, source), target) in planes.iter().zip(&imported).zip(&images) {
                commands.keep(source.clone());
                let image = source.raw_handle();
                let family = owner.open.device.queue_family_index();
                image_barrier(
                    owner,
                    &mut commands,
                    image,
                    vk::ImageLayout::GENERAL,
                    vk::ImageLayout::GENERAL,
                    vk::QUEUE_FAMILY_EXTERNAL,
                    family,
                    vk::AccessFlags::empty(),
                    vk::AccessFlags::TRANSFER_READ,
                );
                image_barrier(
                    owner,
                    &mut commands,
                    image,
                    vk::ImageLayout::GENERAL,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    vk::QUEUE_FAMILY_IGNORED,
                    vk::QUEUE_FAMILY_IGNORED,
                    vk::AccessFlags::empty(),
                    vk::AccessFlags::TRANSFER_READ,
                );
                target.transition(&mut commands, wgt::TextureUses::COPY_DST);
                commands.encoder().copy_texture_to_texture(
                    source,
                    wgt::TextureUses::COPY_SRC,
                    &target.raw,
                    std::iter::once(copy_region(plane.layout.size)),
                );
                target.initialize(&mut commands);
                image_barrier(
                    owner,
                    &mut commands,
                    image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    vk::ImageLayout::GENERAL,
                    vk::QUEUE_FAMILY_IGNORED,
                    vk::QUEUE_FAMILY_IGNORED,
                    vk::AccessFlags::TRANSFER_READ,
                    vk::AccessFlags::empty(),
                );
                image_barrier(
                    owner,
                    &mut commands,
                    image,
                    vk::ImageLayout::GENERAL,
                    vk::ImageLayout::GENERAL,
                    family,
                    vk::QUEUE_FAMILY_EXTERNAL,
                    vk::AccessFlags::TRANSFER_READ,
                    vk::AccessFlags::empty(),
                );
            }
        }
        producer.submissions.submit()?;
        let release = sync.receipt(owner)?;
        Ok(DmaBufCopy {
            images: images
                .into_iter()
                .zip(planes)
                .map(|(image, plane)| ExternalNativeImage::new(image, plane.layout.descriptor()))
                .collect(),
            release,
        })
    }
}

fn export_allocation(
    owner: &Rc<Device<V>>,
    size: [u32; 2],
    format: api::ImageFormat,
    modifier: u64,
) -> Result<(Rc<Owned<V, hal::vulkan::Texture>>, DmaBufPlane)> {
    let device = &owner.open.device;
    let raw = device.raw_device();
    let mut external_image = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let modifiers = [modifier];
    let mut modifier_info =
        vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&modifiers);
    let info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(owner.adapter.texture_format_as_raw(plane_format(format)?))
        .extent(vk::Extent3D {
            width: size[0],
            height: size[1],
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut external_image)
        .push_next(&mut modifier_info);
    let image = unsafe { raw.create_image(&info, None) }
        .map_err(|e| format!("Creating DMA-BUF export image: {e:?}"))?;
    let mut image = Owned::new(owner, image, |device, image| unsafe {
        device.raw_device().destroy_image(image, None)
    });
    let requirements = unsafe { raw.get_image_memory_requirements(*image) };
    let properties = unsafe {
        device
            .shared_instance()
            .raw_instance()
            .get_physical_device_memory_properties(device.raw_physical_device())
    };
    let memory_type = (0..properties.memory_type_count)
        .find(|index| requirements.memory_type_bits & (1 << index) != 0)
        .ok_or("No DMA-BUF export memory type")?;
    let mut export = vk::ExportMemoryAllocateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(*image);
    let memory = unsafe {
        raw.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(requirements.size)
                .memory_type_index(memory_type)
                .push_next(&mut export)
                .push_next(&mut dedicated),
            None,
        )
    }
    .map_err(|e| format!("Allocating DMA-BUF export memory: {e:?}"))?;
    let mut memory = Owned::new(owner, memory, |device, memory| unsafe {
        device.raw_device().free_memory(memory, None)
    });
    unsafe { raw.bind_image_memory(*image, *memory, 0) }
        .map_err(|e| format!("Binding DMA-BUF export memory: {e:?}"))?;
    let extension =
        ash::khr::external_memory_fd::Device::new(device.shared_instance().raw_instance(), raw);
    let fd = unsafe {
        extension.get_memory_fd(
            &vk::MemoryGetFdInfoKHR::default()
                .memory(*memory)
                .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT),
        )
    }
    .map_err(|e| format!("Exporting DMA-BUF fd: {e:?}"))?;
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let fd = fd
        .try_clone()
        .map_err(|e| format!("Duplicating export fd: {e}"))?;
    let layout = unsafe {
        raw.get_image_subresource_layout(
            *image,
            vk::ImageSubresource::default().aspect_mask(vk::ImageAspectFlags::MEMORY_PLANE_0_EXT),
        )
    };
    let desc = texture_descriptor(
        wgt::Extent3d {
            width: size[0],
            height: size[1],
            depth_or_array_layers: 1,
        },
        plane_format(format)?,
        wgt::TextureUses::COPY_SRC | wgt::TextureUses::COPY_DST,
    );
    let raw_texture = unsafe {
        device.texture_from_raw(
            image.take(),
            &desc,
            None,
            hal::vulkan::TextureMemory::Dedicated(memory.take()),
        )
    };
    let texture = Rc::new(Owned::new(
        owner,
        raw_texture,
        <hal::vulkan::Device as hal::Device>::destroy_texture,
    ));
    let layout = DmaBufLayout::new(
        size,
        format,
        modifier,
        layout.row_pitch,
        layout.offset,
        ids(owner).0,
        ids(owner).1,
    )?;
    Ok((texture, DmaBufPlane::new(fd, layout)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::super::resources::Buffer;

    fn image_device() -> ExternalImageDevice {
        ExternalImageDevice::new(&Rc::new(
            create_vulkan_device(&Options {
                validation: true,
                ..Default::default()
            })
            .unwrap(),
        ))
    }

    fn read(device: &ExternalImageDevice, image: &ExternalNativeImage) -> Vec<u8> {
        let producer = device.dmabuf_producer().unwrap();
        let owner = &producer.owner;
        let texture = image.texture(owner).unwrap();
        let layout = ReadbackLayout::with_pixel_size(
            texture.size.width,
            texture.size.height,
            owner.capabilities.alignments.buffer_copy_pitch.get(),
            bytes_per_pixel(texture.format) as u32,
        )
        .unwrap();
        let buffer = Buffer::readback(owner, &layout).unwrap();
        {
            let mut commands = producer.submissions.recording().unwrap();
            texture.transition(&mut commands, wgt::TextureUses::COPY_SRC);
            buffer.transition(&mut commands, wgt::BufferUses::COPY_DST);
            unsafe {
                copy_readback::<V>(
                    commands.encoder(),
                    &texture.raw,
                    &buffer.raw,
                    &layout,
                    texture.size,
                    hal::FormatAspects::COLOR,
                )
            };
            buffer.transition(&mut commands, wgt::BufferUses::MAP_READ);
        }
        producer.submissions.wait().unwrap();
        owner.map_readback(&buffer.raw, &layout).unwrap()
    }

    #[test]
    fn dmabuf_layout_rejects_invalid_planes() {
        assert!(
            DmaBufLayout::new([0, 1], api::ImageFormat::RGBA8, 0, 4, 0, [0; 16], [0; 16]).is_err()
        );
        assert!(
            DmaBufLayout::new([7, 5], api::ImageFormat::RGBA8, 0, 27, 0, [0; 16], [0; 16]).is_err()
        );
        assert!(DmaBufLayout::new(
            [7, 5],
            api::ImageFormat::RGBA8,
            0,
            32,
            u64::MAX,
            [0; 16],
            [0; 16]
        )
        .is_err());
        assert!(DmaBufLayout::new(
            [7, 5],
            api::ImageFormat::RGBAF32,
            0,
            128,
            0,
            [0; 16],
            [0; 16]
        )
        .is_err());
    }

    #[test]
    #[ignore = "Requires Vulkan and validation"]
    fn dmabuf_cross_device_copy() {
        let producer = image_device();
        let consumer = image_device();
        let caps = producer.dmabuf_capabilities().unwrap();
        println!("DMA-BUF capabilities: {caps:?}");
        if let Ok(expected) = std::env::var("WR_DMABUF_EXPECT_SUPPORTED") {
            assert_eq!(caps.supported(), expected == "1");
        }
        if !caps.supported() {
            assert!(
                unsafe { consumer.copy_dmabuf_planes(&[], &SyncFile::already_signaled()) }.is_err()
            );
            println!("DMA-BUF unavailable: negative capability gate passed");
            return;
        }
        assert_ne!(
            producer.vulkan_context().unwrap().queue,
            consumer.vulkan_context().unwrap().queue
        );
        assert_ne!(
            producer.vulkan_context().unwrap().device.handle(),
            consumer.vulkan_context().unwrap().device.handle()
        );
        let mut count = 0;
        let mut linear = 0;
        let mut tiled = 0;
        for (format, modifiers) in caps.formats() {
            for &modifier in modifiers {
                let descriptor =
                    api::ImageDescriptor::new(17, 13, *format, api::ImageDescriptorFlags::empty());
                let size = 17 * 13 * bytes_per_pixel(plane_format(*format).unwrap());
                let expected: Vec<u8> = (0..size).map(|i| (i * 19 + 7) as u8).collect();
                let original = producer.create_image(descriptor, &expected).unwrap();
                let export = producer.export_dmabuf_image(&original, modifier).unwrap();
                producer
                    .update_image(&original, descriptor, &vec![0; size])
                    .unwrap();
                let copied = unsafe {
                    consumer
                        .copy_dmabuf_planes(std::slice::from_ref(export.plane()), export.ready())
                }
                .unwrap();
                producer.wait_dmabuf_release(copied.release()).unwrap();
                assert_eq!(
                    read(&consumer, &copied.images()[0]),
                    expected,
                    "{:?} modifier={:#x}",
                    format,
                    modifier
                );
                assert_ne!(copied.images()[0].generation(), 0);
                count += 1;
                if modifier == 0 {
                    linear += 1;
                } else {
                    tiled += 1;
                }
            }
        }
        assert!(count > 0 && linear > 0);
        println!("DMA-BUF positive copies: {count}; linear={linear}, tiled={tiled}; distinct logical-device queues");
    }

    #[test]
    #[ignore = "Requires Vulkan and validation"]
    fn dmabuf_queue_coordinator_is_shared() {
        let device = image_device();
        let coordinator = device.vulkan_queue_coordinator().unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _guard = coordinator.lock().unwrap();
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        entered_rx.recv().unwrap();
        assert!(device
            .dmabuf_producer()
            .unwrap()
            .owner
            .queue_gate
            .try_lock()
            .is_err());
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        assert!(device
            .dmabuf_producer()
            .unwrap()
            .owner
            .queue_gate
            .try_lock()
            .is_ok());
    }

    #[test]
    #[ignore = "Requires Vulkan and validation"]
    fn dmabuf_submission_staging_is_scoped_to_the_queue_lock() {
        use std::cell::Cell;
        struct Check {
            gate: Arc<std::sync::Mutex<()>>,
            calls: Rc<Cell<u32>>,
        }
        impl SubmissionSync<V> for Check {
            fn stage(&self, _: &hal::vulkan::Queue) {
                assert!(self.gate.try_lock().is_err());
                self.calls.set(self.calls.get() + 1);
            }
            fn unstage(&self, _: &hal::vulkan::Queue) {
                assert!(self.gate.try_lock().is_err());
                self.calls.set(self.calls.get() + 1);
            }
        }
        let device = image_device();
        let producer = device.dmabuf_producer().unwrap();
        let calls = Rc::new(Cell::new(0));
        let gate = producer.owner.queue_gate.clone();
        {
            let mut recording = producer.submissions.recording().unwrap();
            recording.synchronize(Rc::new(Check {
                gate: gate.clone(),
                calls: calls.clone(),
            }));
            recording.on_complete(move || {
                assert!(gate.try_lock().is_ok());
            });
        }
        producer.submissions.wait().unwrap();
        assert_eq!(calls.get(), 2);
    }

    #[test]
    #[ignore = "Requires Vulkan and validation"]
    fn dmabuf_groups_rejections_and_failed_submission() {
        let producer = image_device();
        let consumer = image_device();
        if !producer.dmabuf_capabilities().unwrap().supported() {
            return;
        }
        for formats in [
            vec![api::ImageFormat::R8, api::ImageFormat::RG8],
            vec![
                api::ImageFormat::R8,
                api::ImageFormat::R8,
                api::ImageFormat::R8,
            ],
            vec![api::ImageFormat::R16, api::ImageFormat::RG16],
        ] {
            for epoch in 0..3 {
                let mut exported = Vec::new();
                let mut expected = Vec::new();
                for (plane, format) in formats.iter().enumerate() {
                    let size = if plane == 0 { [17, 13] } else { [9, 7] };
                    let data: Vec<u8> = (0..size[0]
                        * size[1]
                        * bytes_per_pixel(plane_format(*format).unwrap()) as u32)
                        .map(|i| (i * 13 + epoch * 23 + plane as u32) as u8)
                        .collect();
                    let descriptor = api::ImageDescriptor::new(
                        size[0] as i32,
                        size[1] as i32,
                        *format,
                        api::ImageDescriptorFlags::empty(),
                    );
                    let source = producer.create_image(descriptor, &data).unwrap();
                    exported.push(producer.export_dmabuf_image(&source, 0).unwrap());
                    expected.push(data);
                }
                let planes: Vec<_> = exported
                    .iter()
                    .map(|export| {
                        DmaBufPlane::new(export.plane.fd.try_clone().unwrap(), export.plane.layout)
                    })
                    .collect();
                let ready = exported.last().unwrap().ready();
                let copied = unsafe { consumer.copy_dmabuf_planes(&planes, ready) }.unwrap();
                producer.wait_dmabuf_release(copied.release()).unwrap();
                for (image, data) in copied.images().iter().zip(expected) {
                    assert_eq!(read(&consumer, image), data);
                }
                let alias = DmaBufPlane::new(planes[0].fd.try_clone().unwrap(), planes[0].layout);
                assert!(unsafe {
                    consumer.copy_dmabuf_planes(
                        &[
                            DmaBufPlane::new(planes[0].fd.try_clone().unwrap(), planes[0].layout),
                            alias,
                        ],
                        ready,
                    )
                }
                .err()
                .unwrap()
                .contains("Aliased"));
                let mut invalid = planes[0].layout;
                invalid.device_uuid = [0; 16];
                let wrong_device = DmaBufPlane::new(planes[0].fd.try_clone().unwrap(), invalid);
                assert!(
                    unsafe { consumer.copy_dmabuf_planes(&[wrong_device], ready) }
                        .err()
                        .unwrap()
                        .contains("physical device")
                );
                invalid = planes[0].layout;
                invalid.driver_uuid = [0; 16];
                assert!(unsafe {
                    consumer.copy_dmabuf_planes(
                        &[DmaBufPlane::new(planes[0].fd.try_clone().unwrap(), invalid)],
                        ready,
                    )
                }
                .err()
                .unwrap()
                .contains("driver identity"));
                invalid = planes[0].layout;
                invalid.offset = File::from(planes[0].fd.try_clone().unwrap())
                    .metadata()
                    .unwrap()
                    .len();
                let overflow = DmaBufPlane::new(planes[0].fd.try_clone().unwrap(), invalid);
                assert!(unsafe { consumer.copy_dmabuf_planes(&[overflow], ready) }
                    .err()
                    .unwrap()
                    .contains("exceeds"));
                invalid = planes[0].layout;
                invalid.modifier = u64::MAX;
                assert!(unsafe {
                    consumer.copy_dmabuf_planes(
                        &[DmaBufPlane::new(planes[0].fd.try_clone().unwrap(), invalid)],
                        ready,
                    )
                }
                .is_err());
                assert!(planes[0].fd.as_fd().try_clone_to_owned().is_ok());
            }
        }
        let descriptor = api::ImageDescriptor::new(
            7,
            5,
            api::ImageFormat::RGBA8,
            api::ImageDescriptorFlags::empty(),
        );
        let source = producer.create_image(descriptor, &[71; 7 * 5 * 4]).unwrap();
        let export = producer.export_dmabuf_image(&source, 0).unwrap();
        consumer
            .dmabuf_producer()
            .unwrap()
            .owner
            .fault
            .set(Some(FailurePoint::Import));
        assert!(unsafe {
            consumer.copy_dmabuf_planes(std::slice::from_ref(export.plane()), export.ready())
        }
        .is_err());
        assert!(!consumer.dmabuf_producer().unwrap().owner.lost.get());
        consumer
            .dmabuf_producer()
            .unwrap()
            .owner
            .fault
            .set(Some(FailurePoint::Submit));
        assert!(unsafe {
            consumer.copy_dmabuf_planes(std::slice::from_ref(export.plane()), export.ready())
        }
        .is_err());
        assert!(consumer.dmabuf_capabilities().is_err());
        assert!(export.plane.fd.as_fd().try_clone_to_owned().is_ok());
        println!("DMA-BUF grouped Y/UV and Y/U/V, replacement, alias/layout/device rejection and failed submit checks passed");
    }
}
