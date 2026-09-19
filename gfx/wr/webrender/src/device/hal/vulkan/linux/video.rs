/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::foreign_rgb::ForeignRgbLifetime;
use super::super::super::external::{ExternalImageLease, ExternalImageRelease, ExternalImageSource, ReleaseQueue};
use api::units::TexelRect;
use std::cell::Cell;
use std::os::fd::{AsRawFd, IntoRawFd};
use std::rc::Weak;
use std::time::{Duration, Instant};

const INTEL_Y_TILED: u64 = 0x0100000000000002;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoDmaBufFormat {
    Nv12,
    P010,
}

impl VideoDmaBufFormat {
    fn bytes_per_sample(self) -> u64 {
        match self { Self::Nv12 => 1, Self::P010 => 2 }
    }

    fn texture_format(self) -> wgt::TextureFormat {
        match self { Self::Nv12 => wgt::TextureFormat::NV12, Self::P010 => wgt::TextureFormat::P010 }
    }

    fn plane_formats(self) -> [wgt::TextureFormat; 2] {
        match self {
            Self::Nv12 => [wgt::TextureFormat::R8Unorm, wgt::TextureFormat::Rg8Unorm],
            Self::P010 => [wgt::TextureFormat::R16Unorm, wgt::TextureFormat::Rg16Unorm],
        }
    }

    fn view_formats(self) -> [vk::Format; 3] {
        match self {
            Self::Nv12 => [vk::Format::G8_B8R8_2PLANE_420_UNORM, vk::Format::R8_UNORM, vk::Format::R8G8_UNORM],
            Self::P010 => [vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16, vk::Format::R16_UNORM, vk::Format::R16G16_UNORM],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoDmaBufLayout {
    format: VideoDmaBufFormat,
    allocation: [u32; 2],
    visible: [u32; 2],
    modifier: u64,
    strides: [u64; 2],
    offsets: [u64; 2],
    bytes: u64,
}

impl VideoDmaBufLayout {
    pub fn new(
        format: VideoDmaBufFormat,
        allocation: [u32; 2],
        visible: [u32; 2],
        modifier: u64,
        strides: [u64; 2],
        offsets: [u64; 2],
        bytes: u64,
    ) -> Result<Self> {
        if !matches!(modifier, 0 | INTEL_Y_TILED) || bytes == 0 {
            return Err("Unsupported video modifier or empty allocation".into());
        }
        for i in 0..2 {
            if allocation[i] == 0
                || allocation[i] > i32::MAX as u32
                || allocation[i] % 2 != 0
                || visible[i] == 0
                || visible[i] > allocation[i]
                || visible[i] % 2 != 0
            {
                return Err("Invalid video allocation/visible dimensions".into());
            }
        }
        let sample_bytes = format.bytes_per_sample();
        let row_bytes = u64::from(allocation[0]) * sample_bytes;
        let mut ends = [0; 2];
        for i in 0..2 {
            if strides[i] < row_bytes || strides[i] % sample_bytes != 0 || offsets[i] % sample_bytes != 0 {
                return Err("Video plane pitch or sample alignment is invalid".into());
            }
            let rows = u64::from(allocation[1] >> i);
            let length = if modifier == INTEL_Y_TILED {
                if strides[i] % 128 != 0 || offsets[i] % 4096 != 0 {
                    return Err("Invalid Intel Y-tiled plane alignment".into());
                }
                strides[i].checked_mul((rows + 31) & !31)
            } else {
                strides[i]
                    .checked_mul(rows - 1)
                    .and_then(|length| length.checked_add(row_bytes))
            };
            ends[i] = length
                .and_then(|length| offsets[i].checked_add(length))
                .filter(|&end| end <= bytes)
                .ok_or("Video plane exceeds its allocation")?;
        }
        if !(ends[0] <= offsets[1] || ends[1] <= offsets[0]) {
            return Err("Video planes overlap".into());
        }
        Ok(Self {
            format,
            allocation,
            visible,
            modifier,
            strides,
            offsets,
            bytes,
        })
    }

    fn descriptor(&self, plane: usize) -> api::ImageDescriptor {
        api::ImageDescriptor::new(
            (self.visible[0] >> plane) as i32,
            (self.visible[1] >> plane) as i32,
            match (self.format, plane) {
                (VideoDmaBufFormat::Nv12, 0) => api::ImageFormat::R8,
                (VideoDmaBufFormat::Nv12, _) => api::ImageFormat::RG8,
                (VideoDmaBufFormat::P010, 0) => api::ImageFormat::R16,
                (VideoDmaBufFormat::P010, _) => api::ImageFormat::RG16,
            },
            api::ImageDescriptorFlags::empty(),
        )
    }
}

#[derive(Clone, Copy, Debug)]
pub struct VideoDmaBufCapabilities {
    pub format: VideoDmaBufFormat,
    pub modifier: u64,
    pub max_size: [u32; 2],
    pub max_allocation_size: u64,
}

impl VideoDmaBufCapabilities {
    pub fn supports(&self, layout: &VideoDmaBufLayout) -> bool {
        layout.format == self.format && layout.modifier == self.modifier
            && layout.allocation[0] <= self.max_size[0]
            && layout.allocation[1] <= self.max_size[1]
            && layout.bytes <= self.max_allocation_size
    }
}

fn sampled_limits(owner: &Device<V>, format: VideoDmaBufFormat, drm_modifier: u64) -> Result<VideoDmaBufCapabilities> {
    if !supported(owner)
        || !owner
            .open
            .device
            .enabled_device_extensions()
            .contains(&ash::ext::queue_family_foreign::NAME)
    {
        return Err("Foreign video import is unavailable".into());
    }
    let instance = owner.open.device.shared_instance().raw_instance();
    let physical = owner.open.device.raw_physical_device();
    let view_formats = format.view_formats();
    for (format, count) in view_formats.iter().copied().zip([2, 1, 1].iter().copied()) {
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        unsafe {
            instance.get_physical_device_format_properties2(
                physical,
                format,
                &mut vk::FormatProperties2::default().push_next(&mut list),
            );
        }
        let mut properties = vec![
            vk::DrmFormatModifierPropertiesEXT::default();
            list.drm_format_modifier_count as usize
        ];
        list.p_drm_format_modifier_properties = properties.as_mut_ptr();
        unsafe {
            instance.get_physical_device_format_properties2(
                physical,
                format,
                &mut vk::FormatProperties2::default().push_next(&mut list),
            );
        }
        if list.drm_format_modifier_count as usize > properties.len() {
            return Err("Video modifier list changed".into());
        }
        properties.truncate(list.drm_format_modifier_count as usize);
        let features = vk::FormatFeatureFlags::SAMPLED_IMAGE
            | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
            | vk::FormatFeatureFlags::TRANSFER_SRC;
        if !properties.iter().any(|property| {
            property.drm_format_modifier == drm_modifier
                && property.drm_format_modifier_plane_count == count
                && property
                    .drm_format_modifier_tiling_features
                    .contains(features)
        }) {
            return Err(
                "Video image or plane format lacks sampling/filtering/readback support".into(),
            );
        }
    }
    let mut modifier = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
        .drm_format_modifier(drm_modifier)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let mut external = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let mut views = vk::ImageFormatListCreateInfo::default().view_formats(&view_formats);
    let info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(view_formats[0])
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .flags(vk::ImageCreateFlags::MUTABLE_FORMAT)
        .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_SRC)
        .push_next(&mut modifier)
        .push_next(&mut views)
        .push_next(&mut external);
    let mut memory = vk::ExternalImageFormatProperties::default();
    let mut properties = vk::ImageFormatProperties2::default().push_next(&mut memory);
    unsafe {
        instance.get_physical_device_image_format_properties2(physical, &info, &mut properties)
    }
    .map_err(|error| format!("Querying sampled video import: {error:?}"))?;
    let limits = properties.image_format_properties;
    if !memory
        .external_memory_properties
        .external_memory_features
        .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
        || !memory
            .external_memory_properties
            .compatible_handle_types
            .contains(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
        || limits.max_extent.width < 2
        || limits.max_extent.height < 2
        || limits.max_extent.depth == 0
        || limits.max_mip_levels == 0
        || limits.max_array_layers == 0
        || !limits.sample_counts.contains(vk::SampleCountFlags::TYPE_1)
        || limits.max_resource_size == 0
    {
        return Err("Video allocation exceeds import capabilities".into());
    }
    Ok(VideoDmaBufCapabilities {
        format,
        modifier: drm_modifier,
        max_size: [limits.max_extent.width, limits.max_extent.height],
        max_allocation_size: limits.max_resource_size,
    })
}

fn import(
    owner: &Rc<Device<V>>,
    fd: BorrowedFd<'_>,
    layout: &VideoDmaBufLayout,
) -> Result<[Rc<Texture<V>>; 2]> {
    if !sampled_limits(owner, layout.format, layout.modifier)?.supports(layout) {
        return Err("Video allocation exceeds import capabilities".into());
    }
    let file = File::from(fd.try_clone_to_owned().map_err(|error| error.to_string())?);
    if file.metadata().map_err(|error| error.to_string())?.len() < layout.bytes {
        return Err("Video object size exceeds its handle".into());
    }
    let device = &owner.open.device;
    let raw = device.raw_device();
    let planes = [0, 1].map(|i| vk::SubresourceLayout {
        offset: layout.offsets[i],
        row_pitch: layout.strides[i],
        ..Default::default()
    });
    let mut modifier = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
        .drm_format_modifier(layout.modifier)
        .plane_layouts(&planes);
    let mut external = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let view_formats = layout.format.view_formats();
    let mut views = vk::ImageFormatListCreateInfo::default().view_formats(&view_formats);
    let info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(view_formats[0])
        .extent(vk::Extent3D {
            width: layout.allocation[0],
            height: layout.allocation[1],
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .flags(vk::ImageCreateFlags::MUTABLE_FORMAT)
        .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_SRC)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut modifier)
        .push_next(&mut views)
        .push_next(&mut external);
    let image = unsafe { raw.create_image(&info, None) }
        .map_err(|error| format!("Creating video image: {error:?}"))?;
    let mut image = Owned::new(owner, image, |device, image| unsafe {
        device.raw_device().destroy_image(image, None)
    });
    let requirements = unsafe { raw.get_image_memory_requirements(*image) };
    if requirements.size > layout.bytes {
        return Err("Video Vulkan memory requirements exceed exported allocation".into());
    }
    for i in 0..2 {
        let aspect = if i == 0 {
            vk::ImageAspectFlags::MEMORY_PLANE_0_EXT
        } else {
            vk::ImageAspectFlags::MEMORY_PLANE_1_EXT
        };
        let actual = unsafe {
            raw.get_image_subresource_layout(
                *image,
                vk::ImageSubresource::default().aspect_mask(aspect),
            )
        };
        if actual.offset != layout.offsets[i]
            || actual.row_pitch != layout.strides[i]
            || actual
                .offset
                .checked_add(actual.size)
                .map_or(true, |end| end > layout.bytes)
        {
            return Err("Video Vulkan plane layout differs from export".into());
        }
    }
    let extension =
        ash::khr::external_memory_fd::Device::new(device.shared_instance().raw_instance(), raw);
    let fd = fd.try_clone_to_owned().map_err(|error| error.to_string())?;
    let mut fd_properties = vk::MemoryFdPropertiesKHR::default();
    unsafe {
        extension.get_memory_fd_properties(
            vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
            fd.as_raw_fd(),
            &mut fd_properties,
        )
    }
    .map_err(|error| format!("Querying video memory types: {error:?}"))?;
    let types = requirements.memory_type_bits & fd_properties.memory_type_bits;
    if types == 0 {
        return Err("No compatible video memory type".into());
    }
    let mut import = vk::ImportMemoryFdInfoKHR::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
        .fd(fd.as_raw_fd());
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(*image);
    let memory = unsafe {
        raw.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(layout.bytes)
                .memory_type_index(types.trailing_zeros())
                .push_next(&mut import)
                .push_next(&mut dedicated),
            None,
        )
    }
    .map_err(|error| format!("Importing video memory: {error:?}"))?;
    let _ = fd.into_raw_fd();
    let mut memory = Owned::new(owner, memory, |device, memory| unsafe {
        device.raw_device().free_memory(memory, None)
    });
    unsafe { raw.bind_image_memory(*image, *memory, 0) }
        .map_err(|error| format!("Binding video memory: {error:?}"))?;
    let descriptor = texture_descriptor(
        wgt::Extent3d {
            width: layout.allocation[0],
            height: layout.allocation[1],
            depth_or_array_layers: 1,
        },
        layout.format.texture_format(),
        wgt::TextureUses::RESOURCE | wgt::TextureUses::COPY_SRC,
    );
    let texture = unsafe {
        device.texture_from_raw(
            image.take(),
            &descriptor,
            None,
            hal::vulkan::TextureMemory::Dedicated(memory.take()),
        )
    };
    Texture::from_yuv(owner, texture, layout.allocation, layout.bytes, layout.format.plane_formats())
}

struct Access {
    device: ExternalImageDevice,
    owner: Rc<Device<V>>,
    planes: [Rc<Texture<V>>; 2],
    lifetime: ForeignRgbLifetime,
    releases: ReleaseQueue,
}

fn wait(device: &ExternalImageDevice) -> Result<()> {
    let producer = device.dmabuf_producer()?;
    let serial = producer.submissions.submit_serial()?;
    let start = Instant::now();
    loop {
        if producer.submissions.poll()? >= serial {
            return Ok(());
        }
        if start.elapsed() >= Duration::from_secs(5) {
            producer.owner.lost.set(true);
            return Err("Timed out waiting for NV12 ownership transfer".into());
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

impl Access {
    unsafe fn acquire(&mut self) -> Result<()> {
        let _span = crate::device::hal::diagnostics::Span::new("videoAcquire");
        let producer = self.device.dmabuf_producer()?;
        {
            let mut commands = producer.submissions.recording()?;
            self.lifetime.begin_acquire()?;
            commands.keep(self.planes.clone());
            let image = self.planes[0].raw.raw_handle();
            image_barrier(
                &self.owner,
                &mut commands,
                image,
                vk::ImageLayout::GENERAL,
                vk::ImageLayout::GENERAL,
                vk::QUEUE_FAMILY_FOREIGN_EXT,
                self.owner.open.device.queue_family_index(),
                vk::AccessFlags::empty(),
                vk::AccessFlags::SHADER_READ,
            );
            image_barrier(
                &self.owner,
                &mut commands,
                image,
                vk::ImageLayout::GENERAL,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::QUEUE_FAMILY_IGNORED,
                vk::QUEUE_FAMILY_IGNORED,
                vk::AccessFlags::empty(),
                vk::AccessFlags::SHADER_READ,
            );
        }
        if crate::device::hal::diagnostics::force_video_sync() {
            wait(&self.device)?;
            self.lifetime.acquired()?;
        } else {
            producer.submissions.submit_serial()?;
            self.lifetime.acquire_submitted()?;
        }
        Ok(())
    }
    fn record_release(&mut self) -> Result<()> {
        let producer = self.device.dmabuf_producer()?;
        if self.planes[0].current_usage() != wgt::TextureUses::RESOURCE {
            return Err("Video was not restored after its last use".into());
        }
        {
            let mut commands = producer.submissions.recording()?;
            self.lifetime.begin_release()?;
            commands.keep(self.planes.clone());
            unsafe {
                let image = self.planes[0].raw.raw_handle();
                image_barrier(
                    &self.owner,
                    &mut commands,
                    image,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::ImageLayout::GENERAL,
                    vk::QUEUE_FAMILY_IGNORED,
                    vk::QUEUE_FAMILY_IGNORED,
                    vk::AccessFlags::SHADER_READ,
                    vk::AccessFlags::empty(),
                );
                image_barrier(
                    &self.owner,
                    &mut commands,
                    image,
                    vk::ImageLayout::GENERAL,
                    vk::ImageLayout::GENERAL,
                    self.owner.open.device.queue_family_index(),
                    vk::QUEUE_FAMILY_FOREIGN_EXT,
                    vk::AccessFlags::SHADER_READ,
                    vk::AccessFlags::empty(),
                );
            }
        }
        Ok(())
    }
    fn release(&mut self) -> Result<()> {
        let _span = crate::device::hal::diagnostics::Span::new("videoOwnershipReturn");
        self.record_release()?;
        wait(&self.device)?;
        self.lifetime.released()?;
        Ok(())
    }

    fn release_async(&mut self, callback: Box<dyn FnOnce(ExternalImageRelease)>, status: ExternalImageRelease) -> Result<()> {
        let _span = crate::device::hal::diagnostics::Span::new("videoOwnershipReturn");
        let mut returned = OwnershipReturn {
            callback: Some(callback), status,
            completed: Rc::new(Cell::new(false)),
            lost: self.owner.lost.clone(),
            releases: self.releases.clone(),
            lifetime: None,
        };
        self.record_release()?;
        returned.lifetime = Some(std::mem::replace(&mut self.lifetime, ForeignRgbLifetime::new()));
        let producer = self.device.dmabuf_producer()?;
        {
            let mut commands = producer.submissions.recording()?;
            let completed = returned.completed.clone();
            commands.on_complete(move || completed.set(true));
            commands.keep(returned);
        }
        producer.submissions.submit_serial()?;
        Ok(())
    }
}

struct OwnershipReturn {
    callback: Option<Box<dyn FnOnce(ExternalImageRelease)>>,
    status: ExternalImageRelease,
    completed: Rc<Cell<bool>>,
    lost: crate::device::hal::DeviceLost,
    releases: ReleaseQueue,
    lifetime: Option<ForeignRgbLifetime>,
}

impl Drop for OwnershipReturn {
    fn drop(&mut self) {
        if !self.completed.get() || self.lost.get()
            || self.lifetime.as_mut().map_or(true, |lifetime| lifetime.released().is_err()) {
            self.status = ExternalImageRelease::Abandoned;
            self.lost.set(true);
        }
        if let Some(callback) = self.callback.take() {
            self.releases.borrow_mut().push((callback, self.status));
        }
    }
}

struct Release {
    callback: Option<Box<dyn FnOnce(ExternalImageRelease)>>,
    access: Option<Access>,
    status: Cell<ExternalImageRelease>,
}
impl Release {
    fn finish(&self, status: ExternalImageRelease) {
        if status == ExternalImageRelease::Abandoned
            || self.status.get() == ExternalImageRelease::Unused
        {
            self.status.set(status);
        }
    }
}
impl Drop for Release {
    fn drop(&mut self) {
        if let Some(access) = &mut self.access {
            if !crate::device::hal::diagnostics::force_video_sync()
                && self.status.get() != ExternalImageRelease::Abandoned
                && access.lifetime.needs_release() {
                if let Some(callback) = self.callback.take() {
                    if let Err(error) = access.release_async(callback, self.status.get()) {
                        log::error!("Queueing video ownership return: {error}");
                        if let Some(producer) = access.device.0.as_any().downcast_ref::<Producer<V>>() {
                            producer.submissions.discard_recording();
                        }
                        access.owner.lost.set(true);
                    }
                }
                return;
            }
            if self.status.get() != ExternalImageRelease::Abandoned
                && access.lifetime.needs_release()
            {
                if let Err(error) = access.release() {
                    log::error!("Returning foreign NV12 ownership: {error}");
                    self.status.set(ExternalImageRelease::Abandoned);
                }
            }
            if !access.lifetime.producer_reusable()
                || self.status.get() == ExternalImageRelease::Abandoned
            {
                self.status.set(ExternalImageRelease::Abandoned);
                if let Ok(producer) = access.device.dmabuf_producer() {
                    producer.submissions.discard_recording();
                }
                access.owner.lost.set(true);
            }
        }
        if let Some(callback) = self.callback.take() {
            callback(self.status.get());
        }
    }
}

struct Publication {
    planes: [ExternalNativeImage; 2],
    generation: u64,
    release: Release,
}

#[derive(Clone)]
pub struct ForeignYuvImage(Rc<Publication>);
pub struct WeakForeignYuvImage(Weak<Publication>);
impl WeakForeignYuvImage {
    pub fn upgrade(&self) -> Option<ForeignYuvImage> {
        self.0.upgrade().map(ForeignYuvImage)
    }
}
impl ForeignYuvImage {
    pub fn belongs_to(&self, device: &ExternalImageDevice) -> bool {
        device.dmabuf_producer().map_or(false, |producer| {
            self.0
                .release
                .access
                .as_ref()
                .map_or(false, |access| Rc::ptr_eq(&access.owner, &producer.owner))
        })
    }
    pub fn downgrade(&self) -> WeakForeignYuvImage {
        WeakForeignYuvImage(Rc::downgrade(&self.0))
    }
    pub fn lease(&self, channel: u8, uv: TexelRect) -> Result<ExternalImageLease> {
        if self.0.release.status.get() == ExternalImageRelease::Abandoned {
            return Err("Video publication was abandoned".into());
        }
        let plane = self
            .0
            .planes
            .get(usize::from(channel))
            .ok_or("Invalid video channel")?;
        let publication = self.0.clone();
        ExternalImageLease::new(
            plane.descriptor(),
            uv,
            self.0.generation,
            ExternalImageSource::Native(plane.clone()),
            move |status| publication.release.finish(status),
        )
    }
}

impl ExternalImageDevice {
    pub fn vaapi_video_capabilities(&self, format: VideoDmaBufFormat) -> Result<Vec<VideoDmaBufCapabilities>> {
        let owner = &self.dmabuf_producer()?.owner;
        Ok([0, INTEL_Y_TILED]
            .iter()
            .copied()
            .filter_map(|modifier| sampled_limits(owner, format, modifier).ok())
            .collect())
    }

    /// Imports one completed VA-API NV12 or P010 allocation for direct Y/UV sampling.
    ///
    /// # Safety
    /// The fd/layout and render node must identify the same supported allocation.
    /// Producer completion must be established (for example, successful vaSyncSurface),
    /// and the image must be acquirable from FOREIGN_EXT in GENERAL layout.
    /// Retain the producer frame and exclude every other ownership transfer/write until
    /// the callback reports Unused or Complete. Never recycle an Abandoned allocation.
    /// Reuse this publication for all channel leases rather than importing it twice.
    /// Drive queued ownership returns with `poll` or `finish` before reusing storage.
    pub unsafe fn import_vaapi_video(
        &self,
        fd: BorrowedFd<'_>,
        layout: VideoDmaBufLayout,
        render_node: [u64; 2],
        generation: u64,
        release: impl FnOnce(ExternalImageRelease) + 'static,
    ) -> Result<ForeignYuvImage> {
        let mut guard = Release {
            callback: Some(Box::new(release)),
            access: None,
            status: Cell::new(ExternalImageRelease::Unused),
        };
        let producer = self.dmabuf_producer()?;
        let owner = &producer.owner;
        if generation == 0 || self.foreign_rgb_drm_node()? != Some(render_node) {
            return Err("Video publication has invalid generation or a different DRM device".into());
        }
        #[cfg(any(test, feature = "hal-testing"))]
        owner.check_fault(FailurePoint::Import)?;
        let planes = import(owner, fd, &layout)?;
        guard.access = Some(Access {
            device: self.clone(),
            owner: owner.clone(),
            planes: planes.clone(),
            lifetime: ForeignRgbLifetime::new(),
            releases: producer.releases.clone(),
        });
        guard.access.as_mut().unwrap().acquire()?;
        let [y, uv] = planes;
        Ok(ForeignYuvImage(Rc::new(Publication {
            planes: [
                ExternalNativeImage::new(y, layout.descriptor(0)),
                ExternalNativeImage::new(uv, layout.descriptor(1)),
            ],
            generation,
            release: guard,
        })))
    }
}

#[cfg(test)]
#[path = "video_tests.rs"]
mod tests;
