/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! All objects and calls are confined to the creating Renderer's render thread.
//! Input FDs are borrowed; exported FDs belong to the caller. Non-null pointers
//! must refer to live objects of the corresponding type, and deletion is unique.

use std::ptr;
use webrender::api::{ExternalTextureHandle, ImageFormat};
use webrender::Renderer;

#[cfg(all(feature = "vulkan", target_os = "linux"))]
use self::enabled::*;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct WrVulkanTimelineDescriptor {
    pub fd: i32,
    pub device_uuid: [u8; 16],
    pub driver_uuid: [u8; 16],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct WrVulkanDmaBufDescriptor {
    pub fd: i32,
    pub width: u32,
    pub height: u32,
    pub format: ImageFormat,
    pub modifier: u64,
    pub offset: u64,
    pub stride: u64,
    pub device_uuid: [u8; 16],
    pub driver_uuid: [u8; 16],
    // Match the producer's image usage, in addition to mandatory sampling.
    pub copy_src: bool,
    pub copy_dst: bool,
    pub color_target: bool,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WrVulkanReleaseStatus {
    Pending,
    Submitted,
    Abandoned,
}

/// Render-thread-only registry; retain this before entering Renderer image callbacks.
pub struct WrVulkanExternalImages {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    registry: Rc<ExternalTextureRegistry>,
}

pub struct WrVulkanTimeline {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    timeline: Rc<SharedTimeline>,
}

pub struct WrVulkanDmaBufImage {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    image: ImportedImage,
}

pub struct WrVulkanRelease {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    receipt: PendingExternalRelease,
}

#[no_mangle]
pub extern "C" fn wr_vulkan_external_images_new(renderer: &Renderer) -> *mut WrVulkanExternalImages {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    if let Some(registry) = renderer.wgpu_external_textures() {
        return Box::into_raw(Box::new(WrVulkanExternalImages { registry }));
    }
    let _ = renderer;
    ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn wr_vulkan_external_images_delete(object: *mut WrVulkanExternalImages) {
    if !object.is_null() {
        drop(Box::from_raw(object));
    }
}

#[no_mangle]
pub extern "C" fn wr_vulkan_timeline_new(images: &WrVulkanExternalImages) -> *mut WrVulkanTimeline {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    return boxed(SharedTimeline::new(images.registry.device()).map(|timeline| WrVulkanTimeline { timeline }));
    #[cfg(not(all(feature = "vulkan", target_os = "linux")))]
    {
        let _ = images;
        ptr::null_mut()
    }
}

/// Borrows an OPAQUE_FD timeline export with zero creation flags and matching
/// device/driver UUIDs. The original producer must remain its sole signaller.
#[no_mangle]
pub unsafe extern "C" fn wr_vulkan_timeline_import(
    images: &WrVulkanExternalImages,
    descriptor: &WrVulkanTimelineDescriptor,
) -> *mut WrVulkanTimeline {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    return boxed(import_timeline(images, descriptor).map(|timeline| WrVulkanTimeline { timeline }));
    #[cfg(not(all(feature = "vulkan", target_os = "linux")))]
    {
        let _ = (images, descriptor);
        ptr::null_mut()
    }
}

/// On success the caller owns the exported FD; on failure output is unchanged.
#[no_mangle]
pub extern "C" fn wr_vulkan_timeline_export(
    timeline: &WrVulkanTimeline,
    output: &mut WrVulkanTimelineDescriptor,
) -> bool {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    if let Some(handle) = checked(timeline.timeline.export()) {
        let (fd, device_uuid, driver_uuid) = handle.into_parts();
        *output = WrVulkanTimelineDescriptor {
            fd: fd.into_raw_fd(),
            device_uuid,
            driver_uuid,
        };
        return true;
    }
    let _ = (timeline, output);
    false
}

#[no_mangle]
pub unsafe extern "C" fn wr_vulkan_timeline_delete(object: *mut WrVulkanTimeline) {
    if !object.is_null() {
        drop(Box::from_raw(object));
    }
}

/// Borrows an unprotected, single-plane DMA-BUF exported from a compatible Vulkan
/// image with zero flags, one mip/layer/sample and memory bound at offset zero.
/// Descriptor layout, usage and UUIDs must match the producer's image.
#[no_mangle]
pub unsafe extern "C" fn wr_vulkan_dmabuf_import(
    images: &WrVulkanExternalImages,
    descriptor: &WrVulkanDmaBufDescriptor,
) -> *mut WrVulkanDmaBufImage {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    return boxed(import_image(images, descriptor).map(|image| WrVulkanDmaBufImage { image }));
    #[cfg(not(all(feature = "vulkan", target_os = "linux")))]
    {
        let _ = (images, descriptor);
        ptr::null_mut()
    }
}

#[no_mangle]
pub unsafe extern "C" fn wr_vulkan_dmabuf_delete(object: *mut WrVulkanDmaBufImage) {
    if !object.is_null() {
        drop(Box::from_raw(object));
    }
}

/// The producer must release the initialized image in GENERAL layout to EXTERNAL
/// ownership before ready reaches value. It and any aliases must not access the
/// allocation again until the consumer's release signal completes.
/// On success output can be returned from the image lock callback; on failure it is unchanged.
#[no_mangle]
pub unsafe extern "C" fn wr_vulkan_dmabuf_acquire(
    image: &WrVulkanDmaBufImage,
    ready: &WrVulkanTimeline,
    value: u64,
    output: &mut ExternalTextureHandle,
) -> bool {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    if checked(
        image
            .image
            .registry
            .acquire_dma_buf(&image.image.raw, &ready.timeline, value),
    )
    .is_some()
    {
        *output = image.image.handle;
        return true;
    }
    let _ = (image, ready, value, output);
    false
}

/// Records release without submitting. Publish value only after the receipt reports Submitted.
#[no_mangle]
pub extern "C" fn wr_vulkan_dmabuf_release(
    image: &WrVulkanDmaBufImage,
    released: &WrVulkanTimeline,
    value: u64,
) -> *mut WrVulkanRelease {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    return boxed(
        image
            .image
            .registry
            .release_dma_buf(&image.image.raw, &released.timeline, value)
            .map(|receipt| WrVulkanRelease { receipt }),
    );
    #[cfg(not(all(feature = "vulkan", target_os = "linux")))]
    {
        let _ = (image, released, value);
        ptr::null_mut()
    }
}

/// Submitted permits a GPU wait, not immediate CPU reuse. Abandoned requires discarding
/// the allocation. A null receipt also reports Abandoned; polling never submits work.
#[no_mangle]
pub extern "C" fn wr_vulkan_release_status(receipt: Option<&WrVulkanRelease>) -> WrVulkanReleaseStatus {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    if let Some(receipt) = receipt {
        return match receipt.receipt.status() {
            ExternalReleaseStatus::Pending => WrVulkanReleaseStatus::Pending,
            ExternalReleaseStatus::Submitted(_) => WrVulkanReleaseStatus::Submitted,
            ExternalReleaseStatus::Abandoned => WrVulkanReleaseStatus::Abandoned,
        };
    }
    let _ = receipt;
    WrVulkanReleaseStatus::Abandoned
}

#[no_mangle]
pub unsafe extern "C" fn wr_vulkan_release_delete(object: *mut WrVulkanRelease) {
    if !object.is_null() {
        drop(Box::from_raw(object));
    }
}

#[cfg(all(feature = "vulkan", target_os = "linux"))]
mod enabled {
    use super::*;
    use std::os::fd::BorrowedFd;
    pub(super) use std::os::fd::IntoRawFd;
    pub(super) use std::rc::Rc;
    pub(super) use webrender::vulkan::*;

    pub(super) fn checked<T>(result: Result<T, String>) -> Option<T> {
        result
            .map_err(|error| log::error!("Vulkan external image: {}", error))
            .ok()
    }

    pub(super) fn boxed<T>(result: Result<T, String>) -> *mut T {
        checked(result).map_or(ptr::null_mut(), |value| Box::into_raw(Box::new(value)))
    }

    unsafe fn fd(raw: &i32) -> Result<BorrowedFd<'_>, String> {
        if *raw < 0 {
            return Err("Missing Vulkan external FD".into());
        }
        Ok(BorrowedFd::borrow_raw(*raw))
    }

    pub(super) unsafe fn import_timeline(
        images: &WrVulkanExternalImages,
        descriptor: &WrVulkanTimelineDescriptor,
    ) -> Result<Rc<SharedTimeline>, String> {
        let fd = fd(&descriptor.fd)?
            .try_clone_to_owned()
            .map_err(|error| error.to_string())?;
        let handle = TimelineHandle::from_fd(fd, descriptor.device_uuid, descriptor.driver_uuid);
        SharedTimeline::import(images.registry.device(), &handle)
    }

    pub(super) struct ImportedImage {
        pub registry: Rc<ExternalTextureRegistry>,
        pub raw: Rc<DmaBufImage>,
        pub handle: ExternalTextureHandle,
    }

    impl Drop for ImportedImage {
        fn drop(&mut self) {
            let _ = self.registry.unregister(self.handle);
        }
    }

    pub(super) fn image_descriptor(descriptor: &WrVulkanDmaBufDescriptor) -> Result<DmaBufImageDescriptor, String> {
        let format = match descriptor.format {
            ImageFormat::RGBA8 => TextureFormat::Rgba8Unorm,
            ImageFormat::BGRA8 => TextureFormat::Bgra8Unorm,
            _ => return Err("Vulkan DMA-BUF requires RGBA8 or BGRA8".into()),
        };
        let mut usage = TextureUses::RESOURCE;
        for (present, flag) in [
            (descriptor.copy_src, TextureUses::COPY_SRC),
            (descriptor.copy_dst, TextureUses::COPY_DST),
            (descriptor.color_target, TextureUses::COLOR_TARGET),
        ] {
            if present {
                usage |= flag;
            }
        }
        Ok(DmaBufImageDescriptor {
            size: [descriptor.width, descriptor.height],
            format,
            usage,
            modifier: descriptor.modifier,
            offset: descriptor.offset,
            row_pitch: descriptor.stride,
            device_uuid: descriptor.device_uuid,
            driver_uuid: descriptor.driver_uuid,
        })
    }

    pub(super) unsafe fn import_image(
        images: &WrVulkanExternalImages,
        descriptor: &WrVulkanDmaBufDescriptor,
    ) -> Result<ImportedImage, String> {
        let raw = images
            .registry
            .device()
            .import_dma_buf(fd(&descriptor.fd)?, image_descriptor(descriptor)?)?;
        let texture = Texture::from_dma_buf(&raw, TextureFilter::Linear)?;
        let handle = images.registry.register(&texture)?;
        Ok(ImportedImage {
            registry: images.registry.clone(),
            raw,
            handle,
        })
    }
}

#[cfg(test)]
#[path = "vulkan_external_tests.rs"]
mod tests;
