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
#[derive(Clone, Copy)]
pub struct WrHalDmaBuf {
    pub fd: i32,
    pub ready_fd: i32,
    pub access_lock_fd: i32,
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

/// cbindgen:derive-ostream=false
#[repr(C)]
#[derive(Clone, Copy)]
pub struct WrHalVideo {
    pub fd: i32,
    pub access_lock_fd: i32,
    pub fourcc: u32,
    pub width: u32,
    pub height: u32,
    pub allocation_width: u32,
    pub allocation_height: u32,
    pub allocation_size: u64,
    pub modifier: u64,
    pub strides: [u64; 2],
    pub offsets: [u64; 2],
    pub allocation_id: u64,
    pub producer_epoch: u64,
    pub drm_node: [u64; 2],
}

/// cbindgen:derive-ostream=false
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct WrHalVideoFormat {
    pub fourcc: u32,
    pub modifier: u64,
    pub max_width: u32,
    pub max_height: u32,
    pub max_allocation_size: u64,
}

/// cbindgen:derive-eq=false
/// cbindgen:derive-ostream=false
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct WrHalVideoCapabilities {
    pub drm_node: [u64; 2],
    pub device_uuid: [u8; 16],
    pub driver_uuid: [u8; 16],
    pub formats: [WrHalVideoFormat; 4],
    pub format_count: usize,
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
    /// cbindgen:derive-eq=false
    /// cbindgen:derive-ostream=false
    Video(WrHalVideo),
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
    /// cbindgen:ignore
    pub fn wr_renderer_lock_vaapi_image(lease: *mut WrHalImageLease) -> bool;
    /// cbindgen:ignore
    pub fn wr_renderer_lock_vulkan_dmabuf(lease: *mut WrHalImageLease) -> bool;
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
        device: u64,
        generation: u64,
        layout: hal::ForeignRgbLayout,
        image: hal::WeakForeignRgbImage,
        pending: std::rc::Rc<std::cell::Cell<bool>>,
    }

    #[derive(Clone, Copy, Eq, PartialEq)]
    struct VideoIdentity {
        allocation: u64,
        generation: u64,
        producer_epoch: u64,
        drm_node: [u64; 2],
        access_lock: (u64, u64),
    }

    struct VideoEntry {
        identity: VideoIdentity,
        layout: hal::VideoDmaBufLayout,
        image: hal::WeakForeignYuvImage,
        device: u64,
        pending: std::rc::Rc<std::cell::Cell<bool>>,
        progress: std::rc::Rc<dyn Fn() -> Result<bool, String>>,
    }

    struct VulkanEntry {
        generation: u64,
        access_lock: (u64, u64),
        layout: hal::DmaBufLayout,
        image: hal::WeakVulkanDmaBufImage,
        consumer: u64,
        pending: std::rc::Rc<std::cell::Cell<bool>>,
    }

    thread_local! {
        static FOREIGN_IMAGES: RefCell<HashMap<(u64, u64), ForeignEntry>> = RefCell::new(HashMap::new());
        static VIDEO_IMAGES: RefCell<HashMap<(u64, u64), VideoEntry>> = RefCell::new(HashMap::new());
        static VULKAN_IMAGES: RefCell<HashMap<(u64, u64), VulkanEntry>> = RefCell::new(HashMap::new());
    }

    pub fn finish_video_images(
        device: &ExternalImageDevice,
        mut poll: impl FnMut() -> Result<bool, String>,
    ) -> Result<(), String> {
        if !hal::diagnostics::force_video_sync() {
            return Ok(());
        }
        let live = || {
            VIDEO_IMAGES.with(|images| {
                images
                    .borrow()
                    .values()
                    .any(|entry| entry.image.upgrade().map_or(false, |image| image.belongs_to(device)))
            })
        };
        if !live() {
            return Ok(());
        }
        let _span = hal::diagnostics::Span::new("videoFrameCompletion");
        let start = std::time::Instant::now();
        while !poll()? {
            if start.elapsed() >= std::time::Duration::from_secs(5) {
                return Err("Timed out completing native video reads".into());
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        device.poll()?;
        if live() {
            return Err("Native video publication outlived its frame".into());
        }
        Ok(())
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

    #[no_mangle]
    pub extern "C" fn wr_vulkan_query_video(major: u64, minor: u64, output: &mut WrHalVideoCapabilities) -> bool {
        *output = WrHalVideoCapabilities::default();
        let result = (|| -> Result<WrHalVideoCapabilities, String> {
            let device = hal::create_vulkan_image_device(&hal::Options {
                validation: std::env::var_os("MOZ_WR_VULKAN_VALIDATION").is_some(),
                adapter_name: std::env::var("MOZ_WR_VULKAN_ADAPTER").ok(),
            })?;
            if device.foreign_rgb_drm_node()? != Some([major, minor]) {
                return Err("Video decoder and renderer DRM devices differ".into());
            }
            video_capabilities(&device)
        })();
        match result {
            Ok(capabilities) => {
                *output = capabilities;
                true
            },
            Err(error) => {
                log::info!("Native video capability unavailable: {error}");
                false
            },
        }
    }

    pub(super) fn video_capabilities(device: &ExternalImageDevice) -> Result<WrHalVideoCapabilities, String> {
        let node = device.foreign_rgb_drm_node()?.ok_or("Video DRM identity unavailable")?;
        let mut formats = device.vaapi_video_capabilities(hal::VideoDmaBufFormat::Nv12)?;
        formats.extend(
            device
                .vaapi_video_capabilities(hal::VideoDmaBufFormat::P010)?
                .into_iter()
                .filter(|format| format.modifier == 0x0100000000000002),
        );
        let mut capabilities = WrHalVideoCapabilities::default();
        if formats.is_empty() || formats.len() > capabilities.formats.len() {
            return Err("No supported native video sampling formats".into());
        }
        let identity = device.dmabuf_capabilities()?;
        capabilities.drm_node = node;
        capabilities.device_uuid = identity.device_uuid();
        capabilities.driver_uuid = identity.driver_uuid();
        capabilities.format_count = formats.len();
        for (destination, source) in capabilities.formats.iter_mut().zip(formats) {
            *destination = WrHalVideoFormat {
                fourcc: source.format.fourcc(),
                modifier: source.modifier,
                max_width: source.max_size[0],
                max_height: source.max_size[1],
                max_allocation_size: source.max_allocation_size,
            };
        }
        Ok(capabilities)
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

    const SNAPSHOT_CACHE_BYTES: usize = 16 * 1024 * 1024;
    const SNAPSHOT_CACHE_SLOTS: usize = 4;

    #[derive(Default)]
    struct BufferSnapshots {
        buffers: Vec<Arc<Vec<u8>>>,
    }

    fn buffer_layout(data: &WrHalBuffer) -> Result<(ImageDescriptor, usize), String> {
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
        if row > data.stride as usize || needed > data.length || needed > isize::MAX as usize {
            return Err("HAL external buffer layout exceeds its allocation".into());
        }
        let mut descriptor = ImageDescriptor::new(data.width, data.height, data.format, ImageDescriptorFlags::empty());
        descriptor.stride = Some(data.stride);
        Ok((descriptor, needed))
    }

    impl BufferSnapshots {
        fn copy(&mut self, data: WrHalBuffer) -> Result<(ImageDescriptor, Arc<Vec<u8>>), String> {
            let (descriptor, needed) = buffer_layout(&data)?;
            let source = unsafe { std::slice::from_raw_parts(data.data, needed) };
            let reusable = self
                .buffers
                .iter_mut()
                .enumerate()
                .filter_map(|(index, buffer)| {
                    Arc::get_mut(buffer)
                        .filter(|bytes| bytes.capacity() >= source.len())
                        .map(|bytes| (index, bytes.capacity()))
                })
                .min_by_key(|&(_, capacity)| capacity);
            let mut snapshot = match reusable {
                Some((index, _)) => self.buffers.swap_remove(index),
                None => Arc::new(Vec::with_capacity(source.len())),
            };
            let bytes = Arc::get_mut(&mut snapshot).expect("CPU snapshot is exclusively owned");
            bytes.clear();
            bytes.extend_from_slice(source);
            if data.opaque && matches!(data.format, ImageFormat::BGRA8 | ImageFormat::RGBA8) {
                for y in 0..data.height as usize {
                    for x in 0..data.width as usize {
                        bytes[y * data.stride as usize + x * 4 + 3] = 255;
                    }
                }
            }
            Ok((descriptor, snapshot))
        }

        fn retain(&mut self, snapshot: &Arc<Vec<u8>>) {
            let capacity = snapshot.capacity();
            if capacity > SNAPSHOT_CACHE_BYTES {
                return;
            }
            let mut retained: usize = self.buffers.iter().map(|bytes| bytes.capacity()).sum();
            while self.buffers.len() >= SNAPSHOT_CACHE_SLOTS || retained + capacity > SNAPSHOT_CACHE_BYTES {
                retained -= self.buffers.pop().unwrap().capacity();
            }
            self.buffers.push(snapshot.clone());
        }
    }

    #[cfg(test)]
    mod snapshot_tests {
        use super::*;

        fn buffer(
            bytes: &[u8],
            width: i32,
            height: i32,
            stride: i32,
            format: ImageFormat,
            opaque: bool,
        ) -> WrHalBuffer {
            WrHalBuffer {
                data: bytes.as_ptr(),
                length: bytes.len(),
                width,
                height,
                stride,
                format,
                opaque,
            }
        }

        fn retained_bytes(snapshots: &BufferSnapshots) -> usize {
            snapshots.buffers.iter().map(|bytes| bytes.capacity()).sum()
        }

        #[test]
        fn snapshot_reuse_preserves_layout_and_alpha() {
            let mut snapshots = BufferSnapshots::default();
            let mut source = (0..24).collect::<Vec<u8>>();
            let (descriptor, first) = snapshots
                .copy(buffer(&source, 2, 2, 12, ImageFormat::RGBA8, true))
                .unwrap();
            let mut expected = source[..20].to_vec();
            for index in [3, 7, 15, 19] {
                expected[index] = 255;
            }
            assert_eq!(first.as_slice(), expected);
            assert_eq!(descriptor.stride, Some(12));
            source.fill(91);
            assert_eq!(first.as_slice(), expected);
            let pointer = first.as_ptr();
            let capacity = first.capacity();
            snapshots.retain(&first);
            drop(first);

            let (_, small) = snapshots
                .copy(buffer(&source[..3], 3, 1, 3, ImageFormat::R8, true))
                .unwrap();
            assert_eq!(small.as_slice(), [91; 3]);
            assert_eq!(small.as_ptr(), pointer);
            assert_eq!(small.capacity(), capacity);
            snapshots.retain(&small);
            assert_eq!(retained_bytes(&snapshots), capacity);
            drop(small);

            let (descriptor, next) = snapshots
                .copy(buffer(&source, 2, 2, 12, ImageFormat::BGRA8, false))
                .unwrap();
            assert_eq!(next.as_slice(), [91; 20]);
            assert_eq!(next.as_ptr(), pointer);
            assert_eq!(descriptor.format, ImageFormat::BGRA8);
        }

        #[test]
        fn snapshot_held_leases_survive_cache_eviction() {
            let mut snapshots = BufferSnapshots::default();
            let mut held = Vec::new();
            for value in 0..6 {
                let source = [value; 8];
                let (descriptor, bytes) = snapshots
                    .copy(buffer(&source, 2, 1, 8, ImageFormat::RGBA8, false))
                    .unwrap();
                let lease = ExternalImageLease::new(
                    descriptor,
                    TexelRect::new(0.0, 0.0, 2.0, 1.0),
                    1,
                    ExternalImageSource::Buffer(bytes.clone()),
                    |_| {},
                )
                .unwrap();
                snapshots.retain(&bytes);
                held.push((lease, bytes));
                assert!(snapshots.buffers.len() <= SNAPSHOT_CACHE_SLOTS);
                assert!(retained_bytes(&snapshots) <= SNAPSHOT_CACHE_BYTES);
                for (index, (_, bytes)) in held.iter().enumerate() {
                    assert_eq!(bytes.as_slice(), [index as u8; 8]);
                }
            }
            drop(snapshots);
            for (index, (_, bytes)) in held.iter().enumerate() {
                assert_eq!(bytes.as_slice(), [index as u8; 8]);
            }
        }

        #[test]
        fn snapshot_weak_observers_prevent_reuse() {
            let mut snapshots = BufferSnapshots::default();
            let (_, first) = snapshots
                .copy(buffer(&[17; 8], 2, 1, 8, ImageFormat::RGBA8, false))
                .unwrap();
            let weak = Arc::downgrade(&first);
            snapshots.retain(&first);
            drop(first);
            let (_, next) = snapshots
                .copy(buffer(&[23; 8], 2, 1, 8, ImageFormat::RGBA8, false))
                .unwrap();
            assert_eq!(weak.upgrade().unwrap().as_slice(), [17; 8]);
            assert_eq!(next.as_slice(), [23; 8]);
        }

        #[test]
        fn snapshot_cache_bounds_capacity_and_bypasses_oversized_storage() {
            let mut snapshots = BufferSnapshots::default();
            let size = SNAPSHOT_CACHE_BYTES / 2 + 1;
            let mut source = vec![17; size];
            let (_, first) = snapshots
                .copy(buffer(&source, size as i32, 1, size as i32, ImageFormat::R8, false))
                .unwrap();
            snapshots.retain(&first);
            source.fill(23);
            let (_, second) = snapshots
                .copy(buffer(&source, size as i32, 1, size as i32, ImageFormat::R8, false))
                .unwrap();
            snapshots.retain(&second);
            assert!(retained_bytes(&snapshots) <= SNAPSHOT_CACHE_BYTES);
            assert_eq!(first.as_slice(), vec![17; size]);
            drop(first);
            drop(second);
            let (_, small) = snapshots.copy(buffer(&[31], 1, 1, 1, ImageFormat::R8, false)).unwrap();
            assert_eq!(small.as_slice(), [31]);
            assert!(small.capacity() >= size);
            snapshots.retain(&small);
            assert_eq!(retained_bytes(&snapshots), small.capacity());
            drop(small);

            let before = retained_bytes(&snapshots);
            source.resize(SNAPSHOT_CACHE_BYTES + 1, 47);
            let (_, oversized) = snapshots
                .copy(buffer(
                    &source,
                    source.len() as i32,
                    1,
                    source.len() as i32,
                    ImageFormat::R8,
                    false,
                ))
                .unwrap();
            snapshots.retain(&oversized);
            assert_eq!(oversized.as_slice(), source);
            assert_eq!(Arc::strong_count(&oversized), 1);
            assert_eq!(retained_bytes(&snapshots), before);
            let mut spare = Vec::with_capacity(SNAPSHOT_CACHE_BYTES + 1);
            spare.push(59);
            snapshots.retain(&Arc::new(spare));
            assert_eq!(retained_bytes(&snapshots), before);
        }

        #[test]
        fn snapshot_invalid_layout_preserves_cached_storage() {
            let mut snapshots = BufferSnapshots::default();
            let source = [17; 16];
            let (_, first) = snapshots
                .copy(buffer(&source, 2, 2, 8, ImageFormat::RGBA8, false))
                .unwrap();
            snapshots.retain(&first);
            let pointer = first.as_ptr();
            drop(first);
            for (width, height, stride, length) in [(0, 2, 8, 16), (2, -1, 8, 16), (2, 2, 4, 16), (2, 2, 8, 15)] {
                assert!(snapshots
                    .copy(buffer(
                        &source[..length],
                        width,
                        height,
                        stride,
                        ImageFormat::RGBA8,
                        false
                    ))
                    .is_err());
                assert_eq!(snapshots.buffers[0].as_slice(), source);
                assert_eq!(snapshots.buffers[0].as_ptr(), pointer);
            }
            let mut null = buffer(&source, 2, 2, 8, ImageFormat::RGBA8, false);
            null.data = std::ptr::null();
            assert!(snapshots.copy(null).is_err());
            assert_eq!(snapshots.buffers[0].as_ptr(), pointer);
        }
    }

    pub struct ExternalImages {
        handler: WrExternalImageHandler,
        device: ExternalImageDevice,
        snapshots: BufferSnapshots,
    }

    impl ExternalImages {
        fn acquire_image(&self, id: ExternalImageId, channel: u8) -> Result<(WrHalImage, Lease), String> {
            let mut image = std::mem::MaybeUninit::<WrHalImage>::uninit();
            let raw = unsafe { wr_renderer_acquire_hal_image(self.handler.object(), id, channel, image.as_mut_ptr()) };
            let lease = Lease {
                raw: NonNull::new(raw).ok_or_else(|| format!("Unsupported HAL external image {id:?}/{channel}"))?,
                status: WrHalImageRelease::Unused,
            };
            Ok((unsafe { image.assume_init() }, lease))
        }

        pub fn new(handler: WrExternalImageHandler, device: ExternalImageDevice) -> Self {
            Self {
                handler,
                device,
                snapshots: BufferSnapshots::default(),
            }
        }

        fn buffer(
            &mut self,
            data: WrHalBuffer,
            generation: u64,
            mut lease: Lease,
        ) -> Result<ExternalImageLease, String> {
            let (desc, snapshot) = self.snapshots.copy(data)?;
            lease.status = WrHalImageRelease::Complete;
            drop(lease);
            let image = ExternalImageLease::new(
                desc,
                TexelRect::new(0.0, 0.0, desc.size.width as f32, desc.size.height as f32),
                generation,
                ExternalImageSource::Buffer(snapshot.clone()),
                |_| {},
            )?;
            self.snapshots.retain(&snapshot);
            Ok(image)
        }

        fn foreign_rgb(
            &self,
            data: WrHalForeignRGB,
            generation: u64,
            mut lease: Lease,
        ) -> Result<ExternalImageLease, String> {
            let _span = hal::diagnostics::Span::new("webglImport");
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
            let device = self.device.device_id();
            let pending = FOREIGN_IMAGES.with(|images| {
                let images = images.borrow();
                let Some(entry) = images.get(&key) else {
                    return Ok(None);
                };
                if entry.pending.get()
                    && (entry.consumer != consumer
                        || entry.device != device
                        || entry.generation != generation
                        || entry.layout != layout)
                {
                    return Err("Foreign WebGL allocation has a different live consumer/publication".to_owned());
                }
                Ok(if entry.pending.get() && entry.image.upgrade().is_none() {
                    Some(entry.pending.clone())
                } else {
                    None
                })
            })?;
            if let Some(pending) = pending {
                let _span = hal::diagnostics::Span::new("webglPublicationReuseWait");
                let start = std::time::Instant::now();
                while pending.get() {
                    self.device.poll()?;
                    if !pending.get() {
                        break;
                    }
                    if start.elapsed() >= std::time::Duration::from_secs(5) {
                        return Err("Timed out returning foreign WebGL publication".into());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
            let uv = TexelRect::new(0.0, 0.0, data.width as f32, data.height as f32);
            FOREIGN_IMAGES.with(|images| {
                let mut images = images.borrow_mut();
                images.retain(|_, entry| entry.pending.get());
                if let Some(entry) = images.get(&key) {
                    if let Some(image) = entry.image.upgrade() {
                        if entry.consumer != consumer
                            || entry.device != device
                            || entry.generation != generation
                            || entry.layout != layout
                        {
                            return Err(
                                "Foreign WebGL allocation already has a different live consumer/publication".into(),
                            );
                        }
                        hal::diagnostics::webgl_transport();
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
                let pending = std::rc::Rc::new(std::cell::Cell::new(true));
                let completed = pending.clone();
                let image = unsafe {
                    self.device
                        .import_foreign_rgb_dmabuf(fd, layout, &ready, generation, move |status| {
                            completed.set(false);
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
                        device,
                        generation,
                        layout,
                        image: image.downgrade(),
                        pending,
                    },
                );
                hal::diagnostics::webgl_transport();
                if !hal::diagnostics::quiet() {
                    log::info!("WebGL canvas transport: direct Vulkan DMA-BUF sampling, generation={generation}");
                }
                image.lease(uv)
            })
        }

        fn dmabuf(&self, data: WrHalDmaBuf, generation: u64, mut lease: Lease) -> Result<ExternalImageLease, String> {
            let _span = hal::diagnostics::Span::new("dmabufTotal");
            if data.fd < 0 || data.ready_fd < -1 || data.access_lock_fd < 0 || generation == 0 {
                return Err("Invalid Vulkan DMA-BUF handles or generation".into());
            }
            // The C++ lease owns the borrowed descriptors until release.
            let (plane, ready) = dmabuf_plane(&data)?;
            let layout = plane.layout();
            let sampled = !hal::diagnostics::force_dmabuf_copy() && {
                let _span = hal::diagnostics::Span::new("samplingQuery");
                self.device.supports_dmabuf_sampling(layout)
            };
            if sampled {
                let _span = hal::diagnostics::Span::new("directImport");
                let metadata = |fd: i32| -> Result<_, String> {
                    let file = File::from(
                        unsafe { BorrowedFd::borrow_raw(fd) }
                            .try_clone_to_owned()
                            .map_err(|error| error.to_string())?,
                    );
                    let stat = file.metadata().map_err(|error| error.to_string())?;
                    Ok((stat.dev(), stat.ino()))
                };
                let key = metadata(data.fd)?;
                let access_lock = metadata(data.access_lock_fd)?;
                let pending = VULKAN_IMAGES.with(|images| {
                    let images = images.borrow();
                    let entry = match images.get(&key) {
                        Some(entry) => entry,
                        None => return Ok(None),
                    };
                    if entry.pending.get()
                        && (entry.generation != generation
                            || entry.access_lock != access_lock
                            || entry.layout != layout
                            || entry.consumer != self.device.device_id())
                    {
                        return Err("Vulkan DMA-BUF has a different live publication or device".to_owned());
                    }
                    Ok(if entry.pending.get() && entry.image.upgrade().is_none() {
                        Some(entry.pending.clone())
                    } else {
                        None
                    })
                })?;
                if let Some(pending) = pending {
                    let _span = hal::diagnostics::Span::new("publicationReuseWait");
                    let start = std::time::Instant::now();
                    while pending.get() {
                        self.device.poll()?;
                        if !pending.get() {
                            break;
                        }
                        if start.elapsed() >= std::time::Duration::from_secs(5) {
                            return Err("Timed out returning Vulkan DMA-BUF publication".into());
                        }
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                }
                let uv = TexelRect::new(0.0, 0.0, data.width as f32, data.height as f32);
                return VULKAN_IMAGES.with(|images| {
                    let mut images = images.borrow_mut();
                    images.retain(|_, entry| entry.pending.get());
                    if let Some(entry) = images.get(&key) {
                        if let Some(image) = entry.image.upgrade() {
                            if entry.generation != generation || entry.access_lock != access_lock
                                || entry.layout != layout || !image.belongs_to(&self.device) {
                                return Err("Vulkan DMA-BUF has a different live publication or device".into());
                            }
                            hal::diagnostics::transport(true);
                            return image.lease(uv);
                        }
                    }
                    if !unsafe { wr_renderer_lock_vulkan_dmabuf(lease.raw.as_ptr()) } {
                        return Err("Vulkan DMA-BUF publication is unavailable for sampling".into());
                    }
                    let pending = std::rc::Rc::new(std::cell::Cell::new(true));
                    let completed = pending.clone();
                    let image = unsafe { self.device.import_vulkan_dmabuf(&plane, &ready, generation, move |status| {
                        completed.set(false);
                        lease.status = match status {
                            hal::ExternalImageRelease::Unused => WrHalImageRelease::Unused,
                            hal::ExternalImageRelease::Complete => WrHalImageRelease::Complete,
                            hal::ExternalImageRelease::Abandoned => WrHalImageRelease::Abandoned,
                        };
                        drop(lease);
                    }) }?;
                    images.insert(key, VulkanEntry { generation, access_lock, layout, image: image.downgrade(),
                        consumer: self.device.device_id(), pending });
                    hal::diagnostics::transport(true);
                    log::info!("WebRender Vulkan DMA-BUF directly sampled: generation={generation}, format={:?}, modifier={:#x}", data.format, data.modifier);
                    image.lease(uv)
                });
            }
            if !unsafe { wr_renderer_lock_vulkan_dmabuf(lease.raw.as_ptr()) } {
                return Err("Vulkan DMA-BUF publication is unavailable for copying".into());
            }
            lease.status = WrHalImageRelease::Abandoned;
            // VulkanDmaBuf denotes an immutable single-plane image released in GENERAL
            // layout to QUEUE_FAMILY_EXTERNAL, on the identified device and driver.
            let copied = {
                let _span = hal::diagnostics::Span::new("copySubmit");
                unsafe { self.device.copy_dmabuf_planes(&[plane], &ready) }?
            };
            let (mut images, release) = copied.into_parts();
            {
                let _span = hal::diagnostics::Span::new("copyReleaseWait");
                self.device.wait_dmabuf_release(&release)?;
            }
            hal::diagnostics::transport(false);
            log::info!(
                "WebRender Vulkan DMA-BUF materialized: generation={generation}, format={:?}, modifier={:#x}, stride={}, offset={}",
                data.format, data.modifier, data.stride, data.offset,
            );
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

        fn video(
            &self,
            data: WrHalVideo,
            generation: u64,
            channel: u8,
            mut lease: Lease,
        ) -> Result<ExternalImageLease, String> {
            let _span = hal::diagnostics::Span::new("videoImport");
            if data.fd < 0
                || data.access_lock_fd < 0
                || generation == 0
                || data.allocation_id == 0
                || data.producer_epoch == 0
                || channel > 1
            {
                return Err("Invalid video publication metadata".into());
            }
            let format = hal::VideoDmaBufFormat::from_fourcc(data.fourcc)?;
            let p010 = format == hal::VideoDmaBufFormat::P010;
            let layout = hal::VideoDmaBufLayout::new(
                format,
                [data.allocation_width, data.allocation_height],
                [data.width, data.height],
                data.modifier,
                data.strides,
                data.offsets,
                data.allocation_size,
            )?;
            let metadata = |fd: i32| -> Result<_, String> {
                let fd = unsafe { BorrowedFd::borrow_raw(fd) };
                File::from(fd.try_clone_to_owned().map_err(|error| error.to_string())?)
                    .metadata()
                    .map_err(|error| error.to_string())
            };
            let allocation = metadata(data.fd)?;
            let lock = metadata(data.access_lock_fd)?;
            let key = (allocation.dev(), allocation.ino());
            let identity = VideoIdentity {
                allocation: data.allocation_id,
                generation,
                producer_epoch: data.producer_epoch,
                drm_node: data.drm_node,
                access_lock: (lock.dev(), lock.ino()),
            };
            let device = self.device.device_id();
            let pending = VIDEO_IMAGES.with(|images| {
                let images = images.borrow();
                let Some(entry) = images.get(&key) else {
                    return Ok(None);
                };
                if entry.pending.get() && (entry.identity != identity || entry.layout != layout) {
                    return Err("Video allocation has a different live publication or Vulkan device".to_owned());
                }
                let live = entry.image.upgrade().is_some();
                Ok(if entry.pending.get() && (!live || entry.device != device) {
                    Some((
                        entry.pending.clone(),
                        entry.progress.clone(),
                        live,
                        entry.device != device,
                    ))
                } else {
                    None
                })
            })?;
            if let Some((pending, progress, live, other_device)) = pending {
                let _span = hal::diagnostics::Span::new("videoPublicationReuseWait");
                let start = std::time::Instant::now();
                while pending.get() {
                    let attached = if other_device {
                        progress()?
                    } else {
                        self.device.poll()?;
                        false
                    };
                    if !pending.get() {
                        break;
                    }
                    if live && !attached {
                        return Err("Video publication is held by another consumer".into());
                    }
                    if start.elapsed() >= std::time::Duration::from_secs(5) {
                        return Err("Timed out returning video publication".into());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
            let uv = TexelRect::new(
                0.0,
                0.0,
                (data.width >> channel) as f32,
                (data.height >> channel) as f32,
            );
            VIDEO_IMAGES.with(|images| {
                let mut images = images.borrow_mut();
                images.retain(|_, entry| entry.pending.get());
                if let Some(entry) = images.get(&key) {
                    if let Some(image) = entry.image.upgrade() {
                        if entry.identity != identity || entry.layout != layout || !image.belongs_to(&self.device) {
                            return Err("Video allocation has a different live publication or Vulkan device".into());
                        }
                        hal::diagnostics::video_transport(p010);
                        return image.lease(channel, uv);
                    }
                }
                if !unsafe { wr_renderer_lock_vaapi_image(lease.raw.as_ptr()) } {
                    return Err("Video publication is busy or abandoned".into());
                }
                let pending = std::rc::Rc::new(std::cell::Cell::new(true));
                let returned = pending.clone();
                let image = unsafe {
                    self.device.import_vaapi_video(
                        BorrowedFd::borrow_raw(data.fd),
                        layout,
                        data.drm_node,
                        generation,
                        move |status| {
                            returned.set(false);
                            lease.status = match status {
                                hal::ExternalImageRelease::Unused => WrHalImageRelease::Unused,
                                hal::ExternalImageRelease::Complete => WrHalImageRelease::Complete,
                                hal::ExternalImageRelease::Abandoned => WrHalImageRelease::Abandoned,
                            };
                            drop(lease);
                        },
                    )
                }?;
                images.insert(
                    key,
                    VideoEntry {
                        identity,
                        layout,
                        image: image.downgrade(),
                        device,
                        pending,
                        progress: self.device.consumer_poller(),
                    },
                );
                hal::diagnostics::video_transport(p010);
                if !hal::diagnostics::quiet() {
                    log::info!(
                        "Video transport: direct Vulkan {} sampling, generation={generation}",
                        if p010 { "P010" } else { "NV12" }
                    );
                }
                image.lease(channel, uv)
            })
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
        fn with_buffer(
            &mut self,
            id: ExternalImageId,
            channel: u8,
            upload: &mut dyn FnMut(hal::ExternalImageBuffer<'_>) -> Result<(), String>,
        ) -> Result<(), String> {
            let (image, mut lease) = self.acquire_image(id, channel)?;
            let data = match image.source {
                WrHalImageSource::Buffer(data) => data,
                _ => return Err("External buffer update requires CPU bytes".into()),
            };
            let (descriptor, needed) = buffer_layout(&data)?;
            let source = unsafe { std::slice::from_raw_parts(data.data, needed) };
            let opaque = data.opaque && matches!(data.format, ImageFormat::RGBA8 | ImageFormat::BGRA8);
            upload(hal::ExternalImageBuffer::new(descriptor, source, opaque))?;
            lease.status = WrHalImageRelease::Complete;
            Ok(())
        }

        fn acquire(&mut self, id: ExternalImageId, channel: u8, _: bool) -> Result<ExternalImageLease, String> {
            let (image, lease) = self.acquire_image(id, channel)?;
            match image.source {
                WrHalImageSource::Buffer(data) => self.buffer(data, image.generation, lease),
                WrHalImageSource::VulkanDmaBuf(data) => self.dmabuf(data, image.generation, lease),
                WrHalImageSource::ForeignRGB(data) => self.foreign_rgb(data, image.generation, lease),
                WrHalImageSource::Video(data) => self.video(data, image.generation, channel, lease),
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub use self::linux::{finish_video_images, ExternalImages};

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
                video: &WrHalVideoCapabilities,
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
        let video = linux::video_capabilities(device).unwrap_or_default();
        Ok(Self(std::ptr::NonNull::new(unsafe {
            wr_vulkan_register_dmabuf_device(
                caps.device_uuid().as_ptr(),
                caps.driver_uuid().as_ptr(),
                rgba.as_ptr(),
                rgba.len(),
                bgra.as_ptr(),
                bgra.len(),
                &video,
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
