/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Native image calls and receipt polling belong to the render thread. Capability
//! snapshots are immutable and hold no device. Input FDs remain owned by callers.

use std::ptr;
use webrender::{
    api::{ExternalTextureHandle, ImageFormat},
    Renderer,
};

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
    pub copy_src: bool,
    pub copy_dst: bool,
    pub color_target: bool,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WrVulkanReleaseStatus {
    Pending,
    Submitted,
    Complete,
    Abandoned,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WrVulkanAcquireStatus {
    Pending,
    Ready,
    Failed,
}

#[cfg(all(feature = "vulkan", target_os = "linux"))]
use self::enabled::*;
pub struct WrVulkanExternalImages {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    registry: Rc<ExternalTextureRegistry>,
}
pub struct WrVulkanTimeline {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    timeline: Rc<SharedTimeline>,
}
pub struct WrVulkanDmaBufCapabilities {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    capabilities: DmaBufCapabilities,
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
pub extern "C" fn wr_vulkan_process_native_work(images: Option<&WrVulkanExternalImages>) -> bool {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    {
        let mut pending = webrender::wgpu::poll_retired_native_images();
        if let Some(images) = images {
            let _ = images.registry.flush_idle();
            let _ = images.registry.poll();
            pending |= images.registry.pending_work();
        }
        return pending;
    }
    #[cfg(not(all(feature = "vulkan", target_os = "linux")))]
    {
        let _ = images;
        false
    }
}
#[no_mangle]
pub extern "C" fn wr_vulkan_shutdown_native_work() {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    webrender::wgpu::shutdown_native_images();
}
#[no_mangle]
pub extern "C" fn wr_vulkan_dmabuf_capabilities_new(
    images: &WrVulkanExternalImages,
) -> *mut WrVulkanDmaBufCapabilities {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    return boxed(
        images
            .registry
            .device()
            .dma_buf_capabilities()
            .map(|capabilities| WrVulkanDmaBufCapabilities { capabilities }),
    );
    #[cfg(not(all(feature = "vulkan", target_os = "linux")))]
    {
        let _ = images;
        ptr::null_mut()
    }
}
#[no_mangle]
pub extern "C" fn wr_vulkan_dmabuf_capabilities_supports(
    caps: &WrVulkanDmaBufCapabilities,
    descriptor: &WrVulkanDmaBufDescriptor,
) -> bool {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    return image_descriptor(descriptor)
        .map(|image| caps.capabilities.supports(&image))
        .unwrap_or(false);
    #[cfg(not(all(feature = "vulkan", target_os = "linux")))]
    {
        let _ = (caps, descriptor);
        false
    }
}
#[no_mangle]
pub extern "C" fn wr_vulkan_dmabuf_capabilities_supports_opaque(caps: &WrVulkanDmaBufCapabilities) -> bool {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    return caps.capabilities.supports_opaque();
    #[cfg(not(all(feature = "vulkan", target_os = "linux")))]
    { let _ = caps; false }
}
#[no_mangle]
pub unsafe extern "C" fn wr_vulkan_dmabuf_readback(image: &WrVulkanDmaBufImage, output: *mut u8, length: usize) -> bool {
    if output.is_null() { return false; }
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    return image.image.registry.readback(&image.image.raw, unsafe { std::slice::from_raw_parts_mut(output, length) }).is_ok();
    #[cfg(not(all(feature = "vulkan", target_os = "linux")))]
    { let _ = (image, output, length); false }
}
#[no_mangle]
pub unsafe extern "C" fn wr_vulkan_dmabuf_capabilities_delete(object: *mut WrVulkanDmaBufCapabilities) {
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

/// The descriptor must name a genuine compatible unprotected allocation. Later
/// acquire additionally requires full producer initialization, not merely a ready fence.
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
#[no_mangle]
pub extern "C" fn wr_vulkan_dmabuf_matches_context(
    image: &WrVulkanDmaBufImage,
    images: &WrVulkanExternalImages,
) -> bool {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    return Rc::ptr_eq(&image.image.registry, &images.registry);
    #[cfg(not(all(feature = "vulkan", target_os = "linux")))]
    {
        let _ = (image, images);
        false
    }
}
/// The producer must fully initialize and release GENERAL/EXTERNAL before ready;
/// all aliases remain immutable until this consumer's returned signal completes.
#[no_mangle]
pub unsafe extern "C" fn wr_vulkan_dmabuf_acquire(
    image: &WrVulkanDmaBufImage,
    ready: &WrVulkanTimeline,
    value: u64,
    output: &mut ExternalTextureHandle,
) -> WrVulkanAcquireStatus {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    return match image.image.registry.acquire(&image.image.raw, &ready.timeline, value) {
        Ok(true) => {
            *output = image.image.handle;
            WrVulkanAcquireStatus::Ready
        },
        Ok(false) => WrVulkanAcquireStatus::Pending,
        Err(error) => {
            log::error!("Native image acquire: {}", error);
            WrVulkanAcquireStatus::Failed
        },
    };
    #[cfg(not(all(feature = "vulkan", target_os = "linux")))]
    {
        let _ = (image, ready, value, output);
        WrVulkanAcquireStatus::Failed
    }
}
#[no_mangle]
pub extern "C" fn wr_vulkan_dmabuf_opaque_view(
    image: &WrVulkanDmaBufImage,
    output: &mut ExternalTextureHandle,
) -> bool {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    if let Some(handle) = checked(image.image.opaque()) {
        *output = handle;
        return true;
    }
    let _ = (image, output);
    false
}
#[no_mangle]
pub extern "C" fn wr_vulkan_dmabuf_has_access(image: &WrVulkanDmaBufImage) -> bool {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    return image.image.raw.has_access();
    #[cfg(not(all(feature = "vulkan", target_os = "linux")))]
    {
        let _ = image;
        false
    }
}
#[no_mangle]
pub extern "C" fn wr_vulkan_dmabuf_return_timeline(
    image: &WrVulkanDmaBufImage,
    output: &mut WrVulkanTimelineDescriptor,
) -> bool {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    if let Some(handle) = checked(image.image.raw.return_timeline().export()) {
        let (fd, device_uuid, driver_uuid) = handle.into_parts();
        *output = WrVulkanTimelineDescriptor {
            fd: fd.into_raw_fd(),
            device_uuid,
            driver_uuid,
        };
        return true;
    }
    let _ = (image, output);
    false
}
#[no_mangle]
pub extern "C" fn wr_vulkan_dmabuf_release(image: &WrVulkanDmaBufImage) -> *mut WrVulkanRelease {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    return boxed(image.image.registry.release(&image.image.raw).map(|receipt| {
        let _ = image.image.registry.flush_idle();
        WrVulkanRelease { receipt }
    }));
    #[cfg(not(all(feature = "vulkan", target_os = "linux")))]
    {
        let _ = image;
        ptr::null_mut()
    }
}
#[no_mangle]
pub extern "C" fn wr_vulkan_dmabuf_last_release(image: &WrVulkanDmaBufImage) -> *mut WrVulkanRelease {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    if let Some(receipt) = image.image.raw.last_release() {
        return Box::into_raw(Box::new(WrVulkanRelease { receipt }));
    }
    let _ = image;
    ptr::null_mut()
}
#[no_mangle]
pub extern "C" fn wr_vulkan_release_value(receipt: Option<&WrVulkanRelease>) -> u64 {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    if let Some(receipt) = receipt {
        return receipt.receipt.value();
    }
    let _ = receipt;
    0
}
#[no_mangle]
pub extern "C" fn wr_vulkan_release_status(receipt: Option<&WrVulkanRelease>) -> WrVulkanReleaseStatus {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    if let Some(receipt) = receipt {
        return match receipt.receipt.status() {
            ExternalReleaseStatus::Pending => WrVulkanReleaseStatus::Pending,
            ExternalReleaseStatus::Submitted(_) => WrVulkanReleaseStatus::Submitted,
            ExternalReleaseStatus::Complete => WrVulkanReleaseStatus::Complete,
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


#[repr(C)]
#[derive(Clone,Copy)]
pub struct WrVulkanForeignRgbDescriptor {
    pub fd:i32,pub render_node_fd:i32,pub width:u32,pub height:u32,pub fourcc:u32,
    pub modifier:u64,pub offset:u64,pub stride:u64,
}
pub struct WrVulkanForeignRgbImage {
    #[cfg(all(feature="vulkan",target_os="linux"))]
    image:ImportedImage,
}
#[no_mangle]
pub extern "C" fn wr_vulkan_dmabuf_capabilities_supports_foreign_rgb(caps:&WrVulkanDmaBufCapabilities,descriptor:&WrVulkanForeignRgbDescriptor,drm_major:u64,drm_minor:u64)->bool {
    #[cfg(all(feature="vulkan",target_os="linux"))]
    return foreign_layout(descriptor).map(|layout|caps.capabilities.supports_foreign_rgb([drm_major,drm_minor],layout)).unwrap_or(false);
    #[cfg(not(all(feature="vulkan",target_os="linux")))]
    {let _=(caps,descriptor,drm_major,drm_minor);false}
}
/// The borrowed descriptor names a fully initialized unprotected allocation on the
/// declared render node. Its aliases stay immutable until the Complete return.
#[no_mangle]
pub unsafe extern "C" fn wr_vulkan_foreign_rgb_import(images:&WrVulkanExternalImages,descriptor:&WrVulkanForeignRgbDescriptor)->*mut WrVulkanForeignRgbImage {
    #[cfg(all(feature="vulkan",target_os="linux"))]
    return boxed(import_foreign_image(images,descriptor).map(|image|WrVulkanForeignRgbImage{image}));
    #[cfg(not(all(feature="vulkan",target_os="linux")))]
    {let _=(images,descriptor);ptr::null_mut()}
}
#[no_mangle]
pub unsafe extern "C" fn wr_vulkan_foreign_rgb_delete(image:*mut WrVulkanForeignRgbImage){if !image.is_null(){drop(Box::from_raw(image));}}
#[no_mangle]
pub extern "C" fn wr_vulkan_foreign_rgb_matches_context(image:&WrVulkanForeignRgbImage,images:&WrVulkanExternalImages)->bool {
    #[cfg(all(feature="vulkan",target_os="linux"))]
    return Rc::ptr_eq(&image.image.registry,&images.registry);
    #[cfg(not(all(feature="vulkan",target_os="linux")))]
    {let _=(image,images);false}
}
#[no_mangle]
pub extern "C" fn wr_vulkan_foreign_rgb_has_access(image:&WrVulkanForeignRgbImage)->bool {
    #[cfg(all(feature="vulkan",target_os="linux"))]
    return image.image.raw.has_access();
    #[cfg(not(all(feature="vulkan",target_os="linux")))]
    {let _=image;false}
}
/// ready_fd is a genuine EGL SYNC_FD covering complete producer initialization.
#[no_mangle]
pub unsafe extern "C" fn wr_vulkan_foreign_rgb_acquire(image:&WrVulkanForeignRgbImage,ready_fd:i32,output:&mut ExternalTextureHandle)->WrVulkanAcquireStatus {
    #[cfg(all(feature="vulkan",target_os="linux"))]
    return match fd(&ready_fd).and_then(|ready|image.image.registry.acquire_foreign(&image.image.raw,ready)) {
        Ok(true)=>{*output=image.image.handle;WrVulkanAcquireStatus::Ready},
        Ok(false)=>WrVulkanAcquireStatus::Pending,
        Err(error)=>{log::error!("Foreign image acquire: {}",error);WrVulkanAcquireStatus::Failed},
    };
    #[cfg(not(all(feature="vulkan",target_os="linux")))]
    {let _=(image,ready_fd,output);WrVulkanAcquireStatus::Failed}
}
#[no_mangle]
pub extern "C" fn wr_vulkan_foreign_rgb_opaque_view(image:&WrVulkanForeignRgbImage,output:&mut ExternalTextureHandle)->bool {
    #[cfg(all(feature="vulkan",target_os="linux"))]
    if let Some(handle)=checked(image.image.opaque()){*output=handle;return true;}
    let _=(image,output);false
}
#[no_mangle]
pub extern "C" fn wr_vulkan_foreign_rgb_release(image:&WrVulkanForeignRgbImage)->*mut WrVulkanRelease {
    #[cfg(all(feature="vulkan",target_os="linux"))]
    return boxed(image.image.registry.release(&image.image.raw).map(|receipt|{let _=image.image.registry.flush_idle();WrVulkanRelease{receipt}}));
    #[cfg(not(all(feature="vulkan",target_os="linux")))]
    {let _=image;ptr::null_mut()}
}
#[no_mangle]
pub extern "C" fn wr_vulkan_foreign_rgb_last_release(image:&WrVulkanForeignRgbImage)->*mut WrVulkanRelease {
    #[cfg(all(feature="vulkan",target_os="linux"))]
    if let Some(receipt)=image.image.raw.last_release(){return Box::into_raw(Box::new(WrVulkanRelease{receipt}));}
    let _=image;ptr::null_mut()
}

#[cfg(all(feature = "vulkan", target_os = "linux"))]
mod enabled {
    use super::*;
    use std::os::fd::BorrowedFd;
    pub(super) use std::{cell::Cell, os::fd::IntoRawFd, rc::Rc};
    pub(super) use webrender::wgpu::*;
    pub(super) fn checked<T>(value: Result<T, String>) -> Option<T> {
        value.map_err(|error| log::error!("Native image: {}", error)).ok()
    }
    pub(super) fn boxed<T>(value: Result<T, String>) -> *mut T {
        checked(value).map_or(ptr::null_mut(), |value| Box::into_raw(Box::new(value)))
    }
    pub(super) unsafe fn fd(value: &i32) -> Result<BorrowedFd<'_>, String> {
        if *value < 0 {
            Err("Missing native FD".into())
        } else {
            Ok(BorrowedFd::borrow_raw(*value))
        }
    }
    pub(super) unsafe fn import_timeline(
        images: &WrVulkanExternalImages,
        d: &WrVulkanTimelineDescriptor,
    ) -> Result<Rc<SharedTimeline>, String> {
        let handle = TimelineHandle::from_fd(
            fd(&d.fd)?.try_clone_to_owned().map_err(|e| e.to_string())?,
            d.device_uuid,
            d.driver_uuid,
        );
        SharedTimeline::import(images.registry.device(), &handle)
    }
    pub(super) struct ImportedImage {
        pub registry: Rc<ExternalTextureRegistry>,
        pub raw: Rc<NativeImage>,
        pub handle: ExternalTextureHandle,
        opaque: Cell<Option<ExternalTextureHandle>>,
    }
    impl Drop for ImportedImage {
        fn drop(&mut self) {
            if let Some(handle) = self.opaque.get() {
                let _ = self.registry.unregister(handle);
            }
            let _ = self.registry.unregister(self.handle);
        }
    }
    impl ImportedImage {
        pub fn opaque(&self) -> Result<ExternalTextureHandle, String> {
            if let Some(handle) = self.opaque.get() {
                return Ok(handle);
            }
            let handle = self.registry.register(&self.raw, true)?;
            self.opaque.set(Some(handle));
            Ok(handle)
        }
    }
    pub(super) fn image_descriptor(d: &WrVulkanDmaBufDescriptor) -> Result<DmaBufImageDescriptor, String> {
        let format = match d.format {
            ImageFormat::RGBA8 => webrender::wgpu::TextureFormat::Rgba8Unorm,
            ImageFormat::BGRA8 => webrender::wgpu::TextureFormat::Bgra8Unorm,
            _ => return Err("Native DMA-BUF requires RGBA8/BGRA8".into()),
        };
        let mut usage = webrender::wgpu::TextureUses::RESOURCE;
        for (present, flag) in [
            (d.copy_src, webrender::wgpu::TextureUses::COPY_SRC),
            (d.copy_dst, webrender::wgpu::TextureUses::COPY_DST),
            (d.color_target, webrender::wgpu::TextureUses::COLOR_TARGET),
        ] {
            if present {
                usage |= flag;
            }
        }
        Ok(DmaBufImageDescriptor {
            size: [d.width, d.height],
            format,
            usage,
            modifier: d.modifier,
            offset: d.offset,
            row_pitch: d.stride,
            device_uuid: d.device_uuid,
            driver_uuid: d.driver_uuid,
        })
    }
    pub(super) unsafe fn import_image(
        images: &WrVulkanExternalImages,
        d: &WrVulkanDmaBufDescriptor,
    ) -> Result<ImportedImage, String> {
        let allocation = images
            .registry
            .device()
            .import_dma_buf(fd(&d.fd)?, image_descriptor(d)?)?;
        let raw = images.registry.import(allocation)?;
        let handle = images.registry.register(&raw, false)?;
        Ok(ImportedImage {
            registry: images.registry.clone(),
            raw,
            handle,
            opaque: Cell::new(None),
        })
    }
    pub(super) fn foreign_layout(d:&WrVulkanForeignRgbDescriptor)->Result<ForeignRgbLayout,String>{ForeignRgbLayout::new([d.width,d.height],d.fourcc,d.modifier,d.stride,d.offset)}
    pub(super) unsafe fn import_foreign_image(images:&WrVulkanExternalImages,d:&WrVulkanForeignRgbDescriptor)->Result<ImportedImage,String>{
        let allocation=images.registry.device().import_foreign_rgb(fd(&d.fd)?,fd(&d.render_node_fd)?,foreign_layout(d)?)?;
        let raw=images.registry.import_foreign(allocation)?;let handle=images.registry.register(&raw,false)?;
        Ok(ImportedImage{registry:images.registry.clone(),raw,handle,opaque:Cell::new(None)})
    }

}
