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

/// cbindgen:derive-eq=false
/// cbindgen:derive-ostream=false
#[repr(C)]
pub enum WrHalImageSource {
    /// cbindgen:derive-ostream=false
    Buffer(WrHalBuffer),
    /// cbindgen:derive-eq=false
    /// cbindgen:derive-ostream=false
    VulkanDmaBuf(WrHalDmaBuf),
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

        fn dmabuf(&self, data: WrHalDmaBuf, generation: u64, mut lease: Lease) -> Result<ExternalImageLease, String> {
            if data.fd < 0 || data.ready_fd < -1 || generation == 0 {
                return Err("Invalid Vulkan DMA-BUF handles or generation".into());
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
            // The C++ lease owns the borrowed descriptors until release.
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
            lease.status = WrHalImageRelease::Abandoned;
            // VulkanDmaBuf denotes an immutable single-plane image released in GENERAL
            // layout to QUEUE_FAMILY_EXTERNAL, on the identified device and driver.
            let copied = unsafe { self.device.copy_dmabuf_planes(&[plane], &ready) }?;
            let (mut images, release) = copied.into_parts();
            self.device.wait_dmabuf_release(&release)?;
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
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub use self::linux::ExternalImages;
