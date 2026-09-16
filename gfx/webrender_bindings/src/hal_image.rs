/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::os::raw::c_void;
use webrender::api::ImageFormat;

/// cbindgen:derive-ostream=false
#[repr(C)]
pub struct WrHalBuffer {
    pub data: *const u8,
    pub length: usize,
    pub width: i32,
    pub height: i32,
    pub stride: i32,
    pub format: ImageFormat,
    pub opaque: bool,
}

/// cbindgen:derive-ostream=false
#[repr(C)]
pub struct WrHalDmaBuf {
    pub fd: i32,
    pub ready_fd: i32,
    pub width: u32,
    pub height: u32,
    pub format: ImageFormat,
    pub modifier: u64,
    pub stride: u64,
    pub offset: u64,
    pub device_uuid: [u8; 16],
    pub driver_uuid: [u8; 16],
}

/// cbindgen:derive-ostream=false
#[repr(C)]
pub struct WrHalForeignRGB {
    pub fd: i32,
    pub ready_fd: i32,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub stride: u64,
    pub offset: u64,
}

/// cbindgen:derive-eq=false
/// cbindgen:derive-ostream=false
#[repr(C)]
pub enum WrHalImageSource {
    /// cbindgen:derive-ostream=false
    Buffer(WrHalBuffer),
    /// cbindgen:derive-eq=false
    /// cbindgen:derive-ostream=false
    VulkanDmaBuf(WrHalDmaBuf),
    /// cbindgen:derive-ostream=false
    ForeignRGB(WrHalForeignRGB),
}

/// cbindgen:derive-eq=false
/// cbindgen:derive-ostream=false
#[repr(C)]
pub struct WrHalImage {
    pub generation: u64,
    pub source: WrHalImageSource,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub enum WrHalImageRelease {
    Unused,
    Complete,
    Abandoned,
}

pub struct WrHalImageLease {
    _private: (),
}

#[allow(improper_ctypes)]
extern "C" {
    pub fn wr_renderer_acquire_hal_image(
        obj: *mut c_void,
        id: webrender::api::ExternalImageId,
        channel: u8,
        image: *mut WrHalImage,
    ) -> *mut WrHalImageLease;
    /// cbindgen:ignore
    pub fn wr_renderer_lock_foreign_rgb(lease: *mut WrHalImageLease) -> bool;
    pub fn wr_renderer_release_hal_image(lease: *mut WrHalImageLease, status: WrHalImageRelease);
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use crate::bindings::WrExternalImageHandler;
    use std::os::fd::BorrowedFd;
    use std::ptr::NonNull;
    use std::sync::Arc;
    use webrender::api::{units::TexelRect, ExternalImageId, ImageDescriptor, ImageDescriptorFlags};
    use webrender::hal::{self, ExternalImageDevice, ExternalImageLease, ExternalImageSource};

    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::fs::File;
    use std::os::unix::fs::MetadataExt;

    struct ForeignEntry {
        consumer: usize,
        generation: u64,
        layout: hal::ForeignRgbLayout,
        image: hal::WeakForeignRgbImage,
    }

    thread_local! {
        static FOREIGN_IMAGES: RefCell<HashMap<(u64, u64), ForeignEntry>> = RefCell::new(HashMap::new());
    }

    /// cbindgen:ignore
    #[no_mangle]
    pub extern "C" fn wr_vulkan_supports_foreign_webgl(major: u64, minor: u64) -> bool {
        let result = (|| -> Result<bool, String> {
            let device = hal::create_vulkan_image_device(&hal::Options {
                validation: std::env::var_os("MOZ_WR_VULKAN_VALIDATION").is_some(),
                adapter_name: std::env::var("MOZ_WR_VULKAN_ADAPTER").ok(),
            })?;
            Ok(device.foreign_rgb_drm_node()? == Some([major, minor])
                && device.foreign_rgb_formats()?.contains(&hal::ForeignRgbFormat::Bgra8))
        })();
        match result {
            Ok(supported) => supported,
            Err(error) => {
                log::info!("Native WebGL capability unavailable: {error}");
                false
            },
        }
    }

    struct Lease {
        raw: NonNull<WrHalImageLease>,
        status: WrHalImageRelease,
    }

    impl Drop for Lease {
        fn drop(&mut self) {
            unsafe {
                wr_renderer_release_hal_image(self.raw.as_ptr(), self.status);
            }
        }
    }

    pub struct ExternalImages {
        handler: WrExternalImageHandler,
        device: ExternalImageDevice,
    }

    impl ExternalImages {
        pub fn new(handler: WrExternalImageHandler, device: ExternalImageDevice) -> Self {
            Self { handler, device }
        }

        fn buffer(data: WrHalBuffer, generation: u64, mut lease: Lease) -> Result<ExternalImageLease, String> {
            if data.data.is_null() || data.width <= 0 || data.height <= 0 || data.stride <= 0 {
                return Err("Invalid HAL external buffer".into());
            }
            let row = (data.width as usize)
                .checked_mul(data.format.bytes_per_pixel() as usize)
                .ok_or("HAL external buffer row overflow")?;
            let needed = (data.stride as usize)
                .checked_mul(data.height as usize - 1)
                .and_then(|n| n.checked_add(row))
                .ok_or("HAL external buffer size overflow")?;
            if row > data.stride as usize || needed > data.length {
                return Err("HAL external buffer layout exceeds its allocation".into());
            }
            let mut bytes = unsafe { std::slice::from_raw_parts(data.data, needed) }.to_vec();
            if data.opaque && matches!(data.format, ImageFormat::BGRA8 | ImageFormat::RGBA8) {
                for y in 0..data.height as usize {
                    for x in 0..data.width as usize {
                        bytes[y * data.stride as usize + x * 4 + 3] = 255;
                    }
                }
            }
            lease.status = WrHalImageRelease::Complete;
            drop(lease);
            let mut desc = ImageDescriptor::new(data.width, data.height, data.format, ImageDescriptorFlags::empty());
            desc.stride = Some(data.stride);
            ExternalImageLease::new(
                desc,
                TexelRect::new(0.0, 0.0, data.width as f32, data.height as f32),
                generation,
                ExternalImageSource::Buffer(Arc::new(bytes)),
                |_| {},
            )
        }

        fn foreign_rgb(
            &self,
            data: WrHalForeignRGB,
            generation: u64,
            mut lease: Lease,
        ) -> Result<ExternalImageLease, String> {
            if data.fd < 0 || data.ready_fd < 0 || generation == 0 {
                return Err("Invalid foreign WebGL handles or generation".into());
            }
            let layout =
                hal::ForeignRgbLayout::new([data.width, data.height], data.fourcc, 0, data.stride, data.offset)?;
            let fd = unsafe { BorrowedFd::borrow_raw(data.fd) };
            let metadata = File::from(fd.try_clone_to_owned().map_err(|e| e.to_string())?)
                .metadata()
                .map_err(|e| e.to_string())?;
            let key = (metadata.dev(), metadata.ino());
            let consumer = self.handler.object() as usize;
            let uv = TexelRect::new(0.0, 0.0, data.width as f32, data.height as f32);
            FOREIGN_IMAGES.with(|images| {
                let mut images = images.borrow_mut();
                images.retain(|_, entry| entry.image.upgrade().is_some());
                if let Some(entry) = images.get(&key) {
                    if let Some(image) = entry.image.upgrade() {
                        if entry.consumer != consumer || entry.generation != generation || entry.layout != layout {
                            return Err(
                                "Foreign WebGL allocation already has a different live consumer/publication".into(),
                            );
                        }
                        return image.lease(uv);
                    }
                }
                if !unsafe { wr_renderer_lock_foreign_rgb(lease.raw.as_ptr()) } {
                    return Err("Foreign WebGL publication is unavailable for sampling".into());
                }
                let ready = hal::SyncFile::from_fd(
                    unsafe { BorrowedFd::borrow_raw(data.ready_fd) }
                        .try_clone_to_owned()
                        .map_err(|e| e.to_string())?,
                );
                let image = unsafe {
                    self.device
                        .import_foreign_rgb_dmabuf(fd, layout, &ready, generation, move |status| {
                            lease.status = match status {
                                hal::ExternalImageRelease::Unused => WrHalImageRelease::Unused,
                                hal::ExternalImageRelease::Complete => WrHalImageRelease::Complete,
                                hal::ExternalImageRelease::Abandoned => WrHalImageRelease::Abandoned,
                            };
                            drop(lease);
                        })
                }?;
                images.insert(
                    key,
                    ForeignEntry {
                        consumer,
                        generation,
                        layout,
                        image: image.downgrade(),
                    },
                );
                log::info!("WebGL canvas transport: direct Vulkan DMA-BUF sampling, generation={generation}");
                image.lease(uv)
            })
        }

        fn dmabuf(&self, data: WrHalDmaBuf, generation: u64, mut lease: Lease) -> Result<ExternalImageLease, String> {
            if data.fd < 0 || data.ready_fd < -1 || generation == 0 {
                return Err("Invalid Vulkan DMA-BUF handles or generation".into());
            }
            // The C++ lease owns the borrowed descriptors until release.
            let (plane, ready) = dmabuf_plane(&data)?;
            lease.status = WrHalImageRelease::Abandoned;
            // VulkanDmaBuf denotes an immutable single-plane image released in GENERAL
            // layout to QUEUE_FAMILY_EXTERNAL, on the identified device and driver.
            let copied = unsafe { self.device.copy_dmabuf_planes(&[plane], &ready) }?;
            let (mut images, release) = copied.into_parts();
            self.device.wait_dmabuf_release(&release)?;
            log::info!("WebRender Vulkan DMA-BUF materialized: generation={generation}");
            lease.status = WrHalImageRelease::Complete;
            drop(lease);
            let image = images.pop().ok_or("DMA-BUF copy returned no image")?;
            ExternalImageLease::new(
                image.descriptor(),
                TexelRect::new(0.0, 0.0, data.width as f32, data.height as f32),
                generation,
                ExternalImageSource::Native(image),
                |_| {},
            )
        }
    }

    fn dmabuf_plane(data: &WrHalDmaBuf) -> Result<(hal::DmaBufPlane, hal::SyncFile), String> {
        if data.fd < 0 || data.ready_fd < -1 {
            return Err("Invalid Vulkan DMA-BUF handles".into());
        }
        let layout = hal::DmaBufLayout::new(
            [data.width, data.height],
            data.format,
            data.modifier,
            data.stride,
            data.offset,
            data.device_uuid,
            data.driver_uuid,
        )?;
        let fd = unsafe { BorrowedFd::borrow_raw(data.fd) }
            .try_clone_to_owned()
            .map_err(|e| e.to_string())?;
        let ready = if data.ready_fd == -1 {
            hal::SyncFile::already_signaled()
        } else {
            hal::SyncFile::from_fd(
                unsafe { BorrowedFd::borrow_raw(data.ready_fd) }
                    .try_clone_to_owned()
                    .map_err(|e| e.to_string())?,
            )
        };
        let plane = hal::DmaBufPlane::new(fd, layout);
        Ok((plane, ready))
    }

    thread_local! {
        static SNAPSHOT_DEVICE: std::cell::RefCell<Option<ExternalImageDevice>> = const { std::cell::RefCell::new(None) };
    }

    #[no_mangle]
    pub unsafe extern "C" fn wr_snapshot_vulkan_dmabuf(
        data: &WrHalDmaBuf,
        destination: *mut u8,
        length: usize,
        stride: usize,
    ) -> bool {
        let result = SNAPSHOT_DEVICE.with(|slot| -> Result<(), String> {
            let row = (data.width as usize).checked_mul(4).ok_or("Snapshot row overflow")?;
            let needed = stride
                .checked_mul(data.height as usize)
                .ok_or("Snapshot size overflow")?;
            if destination.is_null() || data.height == 0 || row == 0 || stride < row || length < needed {
                return Err("Invalid Vulkan snapshot destination".into());
            }
            let (plane, ready) = dmabuf_plane(data)?;
            let mut slot = slot.borrow_mut();
            if slot.is_none() {
                *slot = Some(hal::create_vulkan_image_device(&hal::Options {
                    validation: std::env::var_os("MOZ_WR_VULKAN_VALIDATION").is_some(),
                    adapter_name: std::env::var("MOZ_WR_VULKAN_ADAPTER").ok(),
                })?);
            }
            let device = slot.as_ref().unwrap();
            // The caller retains an immutable image released in GENERAL to QUEUE_FAMILY_EXTERNAL.
            let copied = unsafe { device.copy_dmabuf_planes(&[plane], &ready) }?;
            let (mut images, release) = copied.into_parts();
            device.wait_dmabuf_release(&release)?;
            let image = images.pop().ok_or("DMA-BUF copy returned no image")?;
            let pixels = device.read_image(&image)?;
            let destination = unsafe { std::slice::from_raw_parts_mut(destination, needed) };
            for (src, dst) in pixels.chunks_exact(row).zip(destination.chunks_exact_mut(stride)) {
                dst[..row].copy_from_slice(src);
                dst[row..].fill(0);
            }
            log::info!("WebGPU snapshot: Vulkan DMA-BUF readback");
            Ok(())
        });
        if let Err(error) = result {
            SNAPSHOT_DEVICE.with(|slot| *slot.borrow_mut() = None);
            log::warn!("Vulkan DMA-BUF snapshot failed: {error}");
            return false;
        }
        true
    }

    impl hal::ExternalImageProvider for ExternalImages {
        fn acquire(&mut self, id: ExternalImageId, channel: u8, _: bool) -> Result<ExternalImageLease, String> {
            let mut image = std::mem::MaybeUninit::<WrHalImage>::uninit();
            let raw = unsafe { wr_renderer_acquire_hal_image(self.handler.object(), id, channel, image.as_mut_ptr()) };
            let lease = Lease {
                raw: NonNull::new(raw).ok_or_else(|| format!("Unsupported HAL external image {id:?}/{channel}"))?,
                status: WrHalImageRelease::Unused,
            };
            let image = unsafe { image.assume_init() };
            match image.source {
                WrHalImageSource::Buffer(data) => Self::buffer(data, image.generation, lease),
                WrHalImageSource::VulkanDmaBuf(data) => self.dmabuf(data, image.generation, lease),
                WrHalImageSource::ForeignRGB(data) => self.foreign_rgb(data, image.generation, lease),
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub use self::linux::ExternalImages;

#[cfg(target_os = "linux")]
pub struct DeviceRegistration(Option<std::ptr::NonNull<c_void>>);

#[cfg(target_os = "linux")]
impl DeviceRegistration {
    pub fn new(device: &webrender::hal::ExternalImageDevice) -> Result<Self, String> {
        extern "C" {
            /// cbindgen:ignore
            fn wr_vulkan_register_dmabuf_device(
                device: *const u8,
                driver: *const u8,
                rgba: *const u64,
                rgba_len: usize,
                bgra: *const u64,
                bgra_len: usize,
            ) -> *mut c_void;
        }
        let caps = device.dmabuf_capabilities()?;
        if !caps.supported() {
            return Ok(Self(None));
        }
        let modifiers = |format| {
            caps.formats()
                .iter()
                .find(|entry| entry.0 == format)
                .map_or(&[][..], |entry| entry.1.as_slice())
        };
        let rgba = modifiers(ImageFormat::RGBA8);
        let bgra = modifiers(ImageFormat::BGRA8);
        Ok(Self(std::ptr::NonNull::new(unsafe {
            wr_vulkan_register_dmabuf_device(
                caps.device_uuid().as_ptr(),
                caps.driver_uuid().as_ptr(),
                rgba.as_ptr(),
                rgba.len(),
                bgra.as_ptr(),
                bgra.len(),
            )
        })))
    }
}

#[cfg(target_os = "linux")]
impl Drop for DeviceRegistration {
    fn drop(&mut self) {
        extern "C" {
            /// cbindgen:ignore
            fn wr_vulkan_unregister_dmabuf_device(registration: *mut c_void);
        }
        if let Some(registration) = self.0 {
            unsafe {
                wr_vulkan_unregister_dmabuf_device(registration.as_ptr());
            }
        }
    }
}
