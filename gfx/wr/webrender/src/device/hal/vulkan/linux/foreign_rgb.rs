/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::super::super::external::{ExternalImageLease, ExternalImageRelease, ExternalImageSource};
use api::units::TexelRect;
use std::cell::Cell;
use std::rc::Weak;

#[path = "foreign_rgb_layout.rs"]
mod layout;
#[path = "foreign_rgb_lifetime.rs"]
mod lifetime;
pub use layout::{ForeignRgbFormat, ForeignRgbLayout};
pub(super) use lifetime::ForeignRgbLifetime;
#[path = "vulkan_rgb.rs"]
mod vulkan_rgb;
pub use vulkan_rgb::{VulkanDmaBufImage, WeakVulkanDmaBufImage};

impl ForeignRgbFormat {
    fn texture_format(self) -> wgt::TextureFormat {
        match self {
            Self::Rgba8 => wgt::TextureFormat::Rgba8Unorm,
            Self::Bgra8 => wgt::TextureFormat::Bgra8Unorm,
        }
    }
    fn image_format(self) -> api::ImageFormat {
        match self {
            Self::Rgba8 => api::ImageFormat::RGBA8,
            Self::Bgra8 => api::ImageFormat::BGRA8,
        }
    }
}

fn foreign_enabled(owner: &Device<V>) -> bool {
    cfg!(target_endian = "little")
        && supported(owner)
        && owner
            .open
            .device
            .enabled_device_extensions()
            .contains(&ash::ext::queue_family_foreign::NAME)
}

fn sampled_limits(
    owner: &Device<V>,
    format: ForeignRgbFormat,
) -> Result<vk::ImageFormatProperties> {
    if !foreign_enabled(owner) {
        return Err("Foreign RGB DMA-BUF ownership/sync-file support is unavailable".into());
    }
    let raw = owner.open.device.shared_instance().raw_instance();
    let physical = owner.open.device.raw_physical_device();
    let format = owner.adapter.texture_format_as_raw(format.texture_format());
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
    unsafe {
        raw.get_physical_device_format_properties2(
            physical,
            format,
            &mut vk::FormatProperties2::default().push_next(&mut list),
        );
    }
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
        );
    }
    entries.truncate(list.drm_format_modifier_count as usize);
    let needed = vk::FormatFeatureFlags::SAMPLED_IMAGE
        | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
        | vk::FormatFeatureFlags::TRANSFER_SRC;
    if !entries.iter().any(|entry| {
        entry.drm_format_modifier == 0
            && entry.drm_format_modifier_plane_count == 1
            && entry.drm_format_modifier_tiling_features.contains(needed)
    }) {
        return Err("Foreign linear RGB format cannot be sampled, filtered and captured".into());
    }
    let mut external = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let mut modifier = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
        .drm_format_modifier(0)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(format)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_SRC)
        .push_next(&mut external)
        .push_next(&mut modifier);
    let mut external_props = vk::ExternalImageFormatProperties::default();
    let mut properties = vk::ImageFormatProperties2::default().push_next(&mut external_props);
    unsafe { raw.get_physical_device_image_format_properties2(physical, &info, &mut properties) }
        .map_err(|error| format!("Querying foreign RGB sampling: {error:?}"))?;
    let limits = properties.image_format_properties;
    if !external_props
        .external_memory_properties
        .external_memory_features
        .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
    {
        return Err("Foreign sampled RGB format is not importable".into());
    }
    Ok(limits)
}

struct ForeignAccess {
    device: ExternalImageDevice,
    owner: Rc<Device<V>>,
    texture: Rc<Texture<V>>,
    lifetime: ForeignRgbLifetime,
    external_family: u32,
    releases: crate::device::hal::external::ReleaseQueue,
}

impl ForeignAccess {
    fn wait(&self) -> Result<()> {
        let producer = self.device.dmabuf_producer()?;
        let serial = producer.submissions.submit_serial()?;
        if self.external_family != vk::QUEUE_FAMILY_EXTERNAL {
            return producer.submissions.wait_for(serial);
        }
        let start = std::time::Instant::now();
        while producer.submissions.poll()? < serial {
            if start.elapsed() >= std::time::Duration::from_secs(5) {
                self.owner.lost.set(true);
                return Err("Timed out transferring Vulkan DMA-BUF ownership".into());
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Ok(())
    }

    unsafe fn acquire(&mut self, ready: &SyncFile) -> Result<()> {
        let _span = crate::device::hal::diagnostics::Span::new("acquire");
        let producer = self.device.dmabuf_producer()?;
        let sync = TransferSync::new(&self.owner, Some(ready))?;
        {
            let mut commands = producer.submissions.recording()?;
            self.lifetime.begin_acquire()?;
            commands.keep(self.texture.clone());
            commands.synchronize(sync);
            let image = self.texture.raw.raw_handle();
            image_barrier(
                &self.owner,
                &mut commands,
                image,
                vk::ImageLayout::GENERAL,
                vk::ImageLayout::GENERAL,
                self.external_family,
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
        if self.external_family == vk::QUEUE_FAMILY_EXTERNAL {
            producer.submissions.submit_serial()?;
            self.lifetime.acquire_submitted()?;
        } else {
            self.wait()?;
            self.lifetime.acquired()?;
        }
        Ok(())
    }

    fn record_release(&mut self) -> Result<()> {
        let producer = self.device.dmabuf_producer()?;
        if self.texture.current_usage() != wgt::TextureUses::RESOURCE {
            return Err("Foreign RGB image was not restored after its last use".into());
        }
        {
            let mut commands = producer.submissions.recording()?;
            self.lifetime.begin_release()?;
            commands.keep(self.texture.clone());
            let image = unsafe { self.texture.raw.raw_handle() };
            unsafe {
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
                    self.external_family,
                    vk::AccessFlags::SHADER_READ,
                    vk::AccessFlags::empty(),
                );
            }
        }
        Ok(())
    }

    fn release(&mut self) -> Result<()> {
        let _span = crate::device::hal::diagnostics::Span::new("ownershipRelease");
        self.record_release()?;
        self.wait()?;
        self.lifetime.released()?;
        Ok(())
    }

    fn release_async(&mut self, callback: Box<dyn FnOnce(ExternalImageRelease)>, status: ExternalImageRelease) -> Result<()> {
        let _span = crate::device::hal::diagnostics::Span::new("ownershipRelease");
        let mut completion = OwnershipReturn {
            callback: Some(callback), status,
            completed: Rc::new(Cell::new(false)),
            lost: self.owner.lost.clone(),
            releases: self.releases.clone(),
            lifetime: None,
        };
        self.record_release()?;
        completion.lifetime = Some(std::mem::replace(&mut self.lifetime, ForeignRgbLifetime::new()));
        let producer = self.device.dmabuf_producer()?;
        {
            let mut commands = producer.submissions.recording()?;
            let completed = completion.completed.clone();
            commands.on_complete(move || completed.set(true));
            commands.keep(completion);
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
    releases: crate::device::hal::external::ReleaseQueue,
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

struct ReleaseGuard {
    callback: Option<Box<dyn FnOnce(ExternalImageRelease)>>,
    access: Option<ForeignAccess>,
    status: Cell<ExternalImageRelease>,
}

impl ReleaseGuard {
    fn finish(&self, status: ExternalImageRelease) {
        match status {
            ExternalImageRelease::Abandoned => self.status.set(status),
            ExternalImageRelease::Complete if self.status.get() == ExternalImageRelease::Unused => {
                self.status.set(status);
            }
            _ => {}
        }
    }
}

impl Drop for ReleaseGuard {
    fn drop(&mut self) {
        if let Some(access) = &mut self.access {
            if access.external_family == vk::QUEUE_FAMILY_EXTERNAL
                && self.status.get() != ExternalImageRelease::Abandoned
                && access.lifetime.needs_release() {
                if let Some(callback) = self.callback.take() {
                    if let Err(error) = access.release_async(callback, self.status.get()) {
                        log::error!("Queueing Vulkan RGB ownership return: {error}");
                        access.owner.lost.set(true);
                        if let Ok(producer) = access.device.dmabuf_producer() {
                            producer.submissions.discard_recording();
                        }
                    }
                }
                return;
            }
            if self.status.get() != ExternalImageRelease::Abandoned
                && access.lifetime.needs_release()
            {
                if let Err(error) = access.release() {
                    log::error!("Releasing foreign RGB ownership: {error}");
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

struct ForeignPublication {
    image: ExternalNativeImage,
    generation: u64,
    release: ReleaseGuard,
}

#[derive(Clone)]
pub struct ForeignRgbImage(Rc<ForeignPublication>);

pub struct WeakForeignRgbImage(Weak<ForeignPublication>);

impl WeakForeignRgbImage {
    pub fn upgrade(&self) -> Option<ForeignRgbImage> {
        self.0.upgrade().map(ForeignRgbImage)
    }
}

impl ForeignRgbImage {
    pub fn downgrade(&self) -> WeakForeignRgbImage {
        WeakForeignRgbImage(Rc::downgrade(&self.0))
    }

    pub fn lease(&self, uv: TexelRect) -> Result<ExternalImageLease> {
        let publication = self.0.clone();
        ExternalImageLease::new(
            self.0.image.descriptor(),
            uv,
            self.0.generation,
            ExternalImageSource::Native(self.0.image.clone()),
            move |status| publication.release.finish(status),
        )
    }
}

impl ExternalImageDevice {
    pub fn foreign_rgb_drm_node(&self) -> Result<Option<[u64; 2]>> {
        let owner = &self.dmabuf_producer()?.owner;
        if !owner
            .adapter
            .physical_device_capabilities()
            .supports_extension(ash::ext::physical_device_drm::NAME)
        {
            return Ok(None);
        }
        let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
        unsafe {
            owner
                .open
                .device
                .shared_instance()
                .raw_instance()
                .get_physical_device_properties2(
                    owner.open.device.raw_physical_device(),
                    &mut vk::PhysicalDeviceProperties2::default().push_next(&mut drm),
                );
        }
        Ok(
            if drm.has_render != 0 && drm.render_major >= 0 && drm.render_minor >= 0 {
                Some([drm.render_major as u64, drm.render_minor as u64])
            } else {
                None
            },
        )
    }

    pub fn foreign_rgb_formats(&self) -> Result<Vec<ForeignRgbFormat>> {
        let owner = &self.dmabuf_producer()?.owner;
        Ok([ForeignRgbFormat::Rgba8, ForeignRgbFormat::Bgra8]
            .iter()
            .copied()
            .filter(|format| sampled_limits(owner, *format).is_ok())
            .collect())
    }

    /// Imports a foreign RGB publication for direct sampling; clones share its ownership.
    ///
    /// # Safety
    /// `fd` and `layout` must describe the negotiated single-plane allocation, acquirable
    /// from FOREIGN_EXT in GENERAL layout. `ready` must cover submitted producer writes;
    /// an already-signaled fence requires proven completion. The caller must retain the
    /// producer publication and prohibit writes until `release` reports Unused or Complete.
    /// Abandoned publications must not be recycled. Reuse this handle for repeated leases
    /// of the same live publication; do not import its allocation twice concurrently.
    /// The callback runs after all image handles and renderer leases are dropped.
    pub unsafe fn import_foreign_rgb_dmabuf(
        &self,
        fd: BorrowedFd<'_>,
        layout: ForeignRgbLayout,
        ready: &SyncFile,
        generation: u64,
        release: impl FnOnce(ExternalImageRelease) + 'static,
    ) -> Result<ForeignRgbImage> {
        let mut guard = ReleaseGuard {
            callback: Some(Box::new(release)),
            access: None,
            status: Cell::new(ExternalImageRelease::Unused),
        };
        let producer = self.dmabuf_producer()?;
        let owner = &producer.owner;
        if generation == 0 {
            return Err("Foreign RGB publication needs a generation".into());
        }
        let size = layout.size();
        let limits = sampled_limits(owner, layout.format())?;
        owner.layout(size[0], size[1])?;
        if size[0] > limits.max_extent.width
            || size[1] > limits.max_extent.height
            || !limits.sample_counts.contains(vk::SampleCountFlags::TYPE_1)
            || limits.max_mip_levels == 0
            || limits.max_array_layers == 0
            || u64::from(size[0]) * u64::from(size[1]) * 4 > limits.max_resource_size
        {
            return Err("Foreign RGB dimensions exceed sampled-image limits".into());
        }
        let bytes = File::from(fd.try_clone_to_owned().map_err(|error| error.to_string())?)
            .metadata()
            .map_err(|error| error.to_string())?
            .len();
        layout.validate_allocation(bytes)?;
        let desc = texture_descriptor(
            wgt::Extent3d {
                width: size[0],
                height: size[1],
                depth_or_array_layers: 1,
            },
            layout.format().texture_format(),
            wgt::TextureUses::RESOURCE | wgt::TextureUses::COPY_SRC,
        );
        let raw = owner
            .open
            .device
            .texture_from_dmabuf_fd(
                fd.try_clone_to_owned().map_err(|error| error.to_string())?,
                &desc,
                0,
                layout.stride(),
                layout.offset(),
            )
            .map_err(|error| format!("Importing sampled foreign RGB DMA-BUF: {error:?}"))?;
        let texture = Texture::from_raw(
            owner,
            raw,
            &desc,
            crate::device::TextureFilter::Linear,
            false,
            wgt::TextureUses::RESOURCE,
        )?;
        guard.access = Some(ForeignAccess {
            device: self.clone(),
            owner: owner.clone(),
            texture: texture.clone(),
            lifetime: ForeignRgbLifetime::new(),
            external_family: vk::QUEUE_FAMILY_FOREIGN_EXT,
            releases: producer.releases.clone(),
        });
        guard.access.as_mut().unwrap().acquire(ready)?;
        let descriptor = api::ImageDescriptor::new(
            size[0] as i32,
            size[1] as i32,
            layout.format().image_format(),
            api::ImageDescriptorFlags::empty(),
        );
        let image = ExternalNativeImage::new(texture, descriptor);
        Ok(ForeignRgbImage(Rc::new(ForeignPublication {
            image,
            generation,
            release: guard,
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abandoned_lease_cannot_be_overwritten_by_later_success() {
        for order in [
            [
                ExternalImageRelease::Abandoned,
                ExternalImageRelease::Complete,
            ],
            [
                ExternalImageRelease::Complete,
                ExternalImageRelease::Abandoned,
            ],
        ] {
            let observed = Rc::new(Cell::new(None));
            let result = observed.clone();
            let guard = ReleaseGuard {
                callback: Some(Box::new(move |status| result.set(Some(status)))),
                access: None,
                status: Cell::new(ExternalImageRelease::Unused),
            };
            for status in order {
                guard.finish(status);
            }
            guard.finish(ExternalImageRelease::Unused);
            drop(guard);
            assert_eq!(observed.get(), Some(ExternalImageRelease::Abandoned));
        }
    }
}

#[cfg(test)]
#[path = "foreign_rgb_gpu.rs"]
mod gpu_tests;
