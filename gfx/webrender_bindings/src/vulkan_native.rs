/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::ptr;
use webrender::api::{units::DeviceIntRect, ExternalTextureHandle, ImageFormat};

use crate::vulkan_external::WrVulkanExternalImages;

#[cfg(all(feature = "vulkan", target_os = "linux"))]
use self::enabled::*;

#[repr(C)]
pub struct WrVulkanNativeImageInfo {
    pub width: u32,
    pub height: u32,
    pub format: ImageFormat,
}

/// Immutable, process-local publication; contains no thread-local WR objects.
pub struct WrVulkanNativePublication {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    owner: Arc<ProcessOwner>,
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    epoch: Arc<ImageEpoch>,
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    view_format: TextureFormat,
}

/// Registration and deletion remain on the creating renderer thread.
pub struct WrVulkanNativeImage {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    registry: Rc<ExternalTextureRegistry>,
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    epoch: Arc<ImageEpoch>,
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    _owner: Arc<ProcessOwner>,
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    view_format: TextureFormat,
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    handles: [Option<ExternalTextureHandle>; 2],
}

#[no_mangle]
pub extern "C" fn wr_vulkan_native_publication_info(
    publication: &WrVulkanNativePublication,
    output: &mut WrVulkanNativeImageInfo,
) -> bool {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    {
        let descriptor = publication.epoch.descriptor();
        *output = WrVulkanNativeImageInfo {
            width: descriptor.size.width,
            height: descriptor.size.height,
            format: publication.format(),
        };
        return true;
    }
    #[cfg(not(all(feature = "vulkan", target_os = "linux")))]
    {
        let _ = (publication, output);
        false
    }
}

/// Clone immutable publication data; renderer registrations remain independent.
#[no_mangle]
pub extern "C" fn wr_vulkan_native_publication_clone(
    publication: &WrVulkanNativePublication,
) -> *mut WrVulkanNativePublication {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    return Box::into_raw(Box::new(WrVulkanNativePublication {
        owner: publication.owner.clone(),
        epoch: publication.epoch.clone(),
        view_format: publication.view_format,
    }));
    #[cfg(not(all(feature = "vulkan", target_os = "linux")))]
    {
        let _ = publication;
        ptr::null_mut()
    }
}

#[no_mangle]
pub unsafe extern "C" fn wr_vulkan_native_publication_delete(
    publication: *mut WrVulkanNativePublication,
) {
    if !publication.is_null() {
        drop(Box::from_raw(publication));
    }
}

/// The destination is borrowed only for this call; padding is zeroed on success.
///
/// # Safety
/// Output pointers must describe live, writable, disjoint storage for this call.
#[no_mangle]
pub unsafe extern "C" fn wr_vulkan_native_publication_snapshot(
    publication: &WrVulkanNativePublication,
    rect: DeviceIntRect,
    stride: usize,
    destination: *mut u8,
    destination_length: usize,
    output_format: *mut ImageFormat,
) -> bool {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    {
        return checked((|| {
            if destination.is_null() || output_format.is_null() {
                return Err("Null native snapshot destination".into());
            }
            let (row_bytes, length, height) = snapshot_layout(
                publication.epoch.descriptor().size.width,
                publication.epoch.descriptor().size.height,
                rect,
                stride,
                destination_length,
            )?;
            let pixels = publication.snapshot(rect)?;
            if pixels.len()
                != row_bytes
                    .checked_mul(height)
                    .ok_or("Snapshot size overflow")?
            {
                return Err("Native snapshot returned an inconsistent pixel length".into());
            }
            let destination = std::slice::from_raw_parts_mut(destination, length);
            destination.fill(0);
            for (source, target) in pixels
                .chunks_exact(row_bytes)
                .zip(destination.chunks_exact_mut(stride))
            {
                target[..row_bytes].copy_from_slice(source);
            }
            *output_format = publication.format();
            Ok(())
        })())
        .is_some();
    }
    #[cfg(not(all(feature = "vulkan", target_os = "linux")))]
    {
        let _ = (
            publication,
            rect,
            stride,
            destination,
            destination_length,
            output_format,
        );
        false
    }
}

#[no_mangle]
pub extern "C" fn wr_vulkan_native_image_new(
    images: &WrVulkanExternalImages,
    publication: &WrVulkanNativePublication,
) -> *mut WrVulkanNativeImage {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    {
        let result = (|| {
            publication.validate()?;
            let handle = unsafe {
                images.registry.register_shared_image(
                    publication.epoch.clone(),
                    publication.view_format,
                    TextureFilter::Linear,
                    false,
                )
            }?;
            Ok(WrVulkanNativeImage {
                registry: images.registry.clone(),
                epoch: publication.epoch.clone(),
                _owner: publication.owner.clone(),
                view_format: publication.view_format,
                handles: [Some(handle), None],
            })
        })();
        return checked(result).map_or(ptr::null_mut(), |image| Box::into_raw(Box::new(image)));
    }
    #[cfg(not(all(feature = "vulkan", target_os = "linux")))]
    {
        let _ = (images, publication);
        ptr::null_mut()
    }
}

#[no_mangle]
pub extern "C" fn wr_vulkan_native_image_matches_context(
    image: &WrVulkanNativeImage,
    images: &WrVulkanExternalImages,
) -> bool {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    return Rc::ptr_eq(&image.registry, &images.registry);
    #[cfg(not(all(feature = "vulkan", target_os = "linux")))]
    {
        let _ = (image, images);
        false
    }
}

#[no_mangle]
pub extern "C" fn wr_vulkan_native_image_lock(
    image: &mut WrVulkanNativeImage,
    images: &WrVulkanExternalImages,
    channel: u8,
    output: &mut ExternalTextureHandle,
) -> bool {
    #[cfg(all(feature = "vulkan", target_os = "linux"))]
    {
        return checked((|| {
            if channel > 1
                || !Rc::ptr_eq(&image.registry, &images.registry)
                || image.epoch.is_closed()
            {
                return Err("Invalid native image sampling admission".into());
            }
            let owner = image.registry.device().hal_owner();
            image
                .epoch
                .validate_owner(&*owner.device, &*owner.queue)
                .map_err(|error| error.to_string())?;
            let slot = &mut image.handles[usize::from(channel)];
            let handle = match *slot {
                Some(handle) => handle,
                None => {
                    let handle = unsafe {
                        image.registry.register_shared_image(
                            image.epoch.clone(),
                            image.view_format,
                            TextureFilter::Linear,
                            true,
                        )
                    }?;
                    *slot = Some(handle);
                    handle
                }
            };
            *output = handle;
            Ok(())
        })())
        .is_some();
    }
    #[cfg(not(all(feature = "vulkan", target_os = "linux")))]
    {
        let _ = (image, images, channel, output);
        false
    }
}

#[no_mangle]
pub unsafe extern "C" fn wr_vulkan_native_image_delete(image: *mut WrVulkanNativeImage) {
    if !image.is_null() {
        drop(Box::from_raw(image));
    }
}

#[cfg(all(feature = "vulkan", target_os = "linux"))]
mod enabled {
    use super::*;
    pub(super) use std::{rc::Rc, sync::Arc};
    pub(super) use webrender::vulkan::{
        Device, ExternalTextureRegistry, ImageEpoch, SharedDeviceOptions, Texture, TextureFilter,
        TextureFormat,
    };
    pub(super) use wgpu_bindings::unified::ProcessOwner;

    pub(super) fn checked<T>(result: Result<T, String>) -> Option<T> {
        result
            .map_err(|error| log::error!("Native canvas image: {error}"))
            .ok()
    }

    impl WrVulkanNativePublication {
        /// # Safety
        /// Retain producer submission polling. Keep this publication open and
        /// immutable; no close or reuse is admitted. Actor routing is external.
        pub unsafe fn new(
            owner: Arc<ProcessOwner>,
            finalized: wgpu_bindings::unified::FinalizedCanvas,
            view_format: TextureFormat,
        ) -> Result<Self, String> {
            let publication = Self {
                owner,
                epoch: finalized.publish_epoch(),
                view_format,
            };
            let descriptor = publication.epoch.descriptor();
            match descriptor.format {
                TextureFormat::Rgba8Unorm | TextureFormat::Bgra8Unorm => {}
                _ => return Err("Native canvas storage requires RGBA8 or BGRA8".into()),
            }
            if view_format != descriptor.format && !descriptor.view_formats.contains(&view_format) {
                return Err("Native canvas sampled view was not declared".into());
            }
            publication.validate()?;
            Ok(publication)
        }

        pub(super) fn validate(&self) -> Result<(), String> {
            if self.epoch.is_closed() {
                return Err("Native canvas publication is closed".into());
            }
            let graphics = self.owner.graphics();
            self.epoch
                .validate_owner(&*graphics.device, &*graphics.queue)
                .map_err(|error| error.to_string())
        }

        pub(super) fn format(&self) -> ImageFormat {
            match self.epoch.descriptor().format {
                TextureFormat::Rgba8Unorm => ImageFormat::RGBA8,
                TextureFormat::Bgra8Unorm => ImageFormat::BGRA8,
                _ => unreachable!(),
            }
        }

        pub(super) fn snapshot(&self, rect: DeviceIntRect) -> Result<Vec<u8>, String> {
            self.validate()?;
            let context = self.owner.context();
            let (_, instance) = context.clone_instance();
            let mut adapters = unsafe { instance.enumerate_adapters(None) };
            if adapters.len() != 1 {
                return Err("Native snapshot selected adapter is unavailable".into());
            }
            let recover = self.owner.clone();
            let device = Rc::new(unsafe {
                Device::from_shared_endpoint(
                    self.owner.graphics(),
                    instance,
                    adapters.pop().unwrap(),
                    SharedDeviceOptions {
                        context,
                        recover: Rc::new(move |retirement| {
                            recover.recover_native_failure(retirement)
                        }),
                        fatal_process: self.owner.fatal_process(),
                    },
                )
            }?);
            let source = unsafe {
                Texture::from_shared_image(
                    &device,
                    self.epoch.clone(),
                    self.epoch.descriptor().format,
                    TextureFilter::Nearest,
                    false,
                )
            }?;
            source.readback(rect)?.wait()
        }
    }

    impl Drop for WrVulkanNativeImage {
        fn drop(&mut self) {
            for handle in self.handles.iter().flatten() {
                let _ = self.registry.unregister(*handle);
            }
        }
    }
}

#[cfg(any(test, all(feature = "vulkan", target_os = "linux")))]
fn snapshot_layout(
    width: u32,
    height: u32,
    rect: DeviceIntRect,
    stride: usize,
    destination_length: usize,
) -> Result<(usize, usize, usize), String> {
    if rect.min.x < 0
        || rect.min.y < 0
        || rect.max.x <= rect.min.x
        || rect.max.y <= rect.min.y
        || rect.max.x as u32 > width
        || rect.max.y as u32 > height
    {
        return Err("Invalid native snapshot rectangle".into());
    }
    let row_bytes = (rect.width() as usize)
        .checked_mul(4)
        .ok_or("Snapshot row size overflow")?;
    let height = rect.height() as usize;
    let length = stride
        .checked_mul(height)
        .ok_or("Snapshot destination size overflow")?;
    if stride < row_bytes || length > destination_length || length > isize::MAX as usize {
        return Err("Invalid native snapshot stride or destination length".into());
    }
    Ok((row_bytes, length, height))
}

#[cfg(test)]
mod tests {
    use super::*;
    use webrender::api::units::{DeviceIntPoint, DeviceIntSize};

    #[test]
    fn snapshot_layout_checks_bounds_stride_length_and_overflow() {
        let rect = DeviceIntRect::from_origin_and_size(
            DeviceIntPoint::new(1, 0),
            DeviceIntSize::new(2, 2),
        );
        assert_eq!(snapshot_layout(3, 2, rect, 12, 24).unwrap(), (8, 24, 2));
        assert!(snapshot_layout(2, 2, rect, 12, 24).is_err());
        assert!(snapshot_layout(3, 2, rect, 7, 24).is_err());
        assert!(snapshot_layout(3, 2, rect, 12, 23).is_err());
        assert!(snapshot_layout(3, 2, rect, usize::MAX, usize::MAX).is_err());
        assert!(snapshot_layout(3, 2, DeviceIntRect::zero(), 12, 24).is_err());
    }
}
