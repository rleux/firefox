/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::resources::{Buffer, Texture, texture_format};
use super::submission::SubmissionQueue;
use api::{ExternalImageId, ImageDescriptor};
use api::units::{DeviceIntRect, TexelRect};
use std::{any::Any, cell::{Cell, RefCell}, rc::Rc, sync::Arc};

#[derive(Clone)]
pub struct NativeImage {
    storage: Rc<dyn Any>,
    descriptor: ImageDescriptor,
    leases: Rc<Cell<usize>>,
    generation: u64,
    failed: Rc<Cell<bool>>,
}

impl NativeImage {
    pub(super) fn new<A: hal::Api>(texture: Rc<Texture<A>>, mut descriptor: ImageDescriptor) -> Self {
        descriptor.offset = 0;
        descriptor.stride = None;
        texture.transient.set(true);
        Self { generation: texture.allocation_id, storage: texture, descriptor, leases: Rc::new(Cell::new(0)), failed: Rc::new(Cell::new(false)) }
    }

    pub fn descriptor(&self) -> ImageDescriptor { self.descriptor }
    pub fn generation(&self) -> u64 { self.generation }

    pub(super) fn ensure_idle(&self) -> Result<()> {
        if self.leases.get() != 0 { return Err("Native image is acquired by the renderer".into()); }
        if self.failed.get() { return Err("Native image requires recreation".into()); }
        Ok(())
    }

    pub(super) fn texture<A: hal::Api>(&self, owner: &Rc<Device<A>>) -> Result<Rc<Texture<A>>> {
        if self.failed.get() { return Err("Native image requires recreation after abandoned GPU use".into()); }
        let texture = self.storage.clone().downcast::<Texture<A>>()
            .map_err(|_| "Native image belongs to another HAL backend")?;
        if !texture.belongs_to(owner) { return Err("Native image belongs to another device".into()); }
        Ok(texture)
    }
}

pub(super) trait ImageDevice: Any {
    fn create(&self, descriptor: ImageDescriptor, bytes: &[u8]) -> Result<NativeImage>;
    fn update(&self, image: &NativeImage, descriptor: ImageDescriptor, bytes: &[u8]) -> Result<()>;
    fn poll(&self) -> Result<()>;
    fn poll_consumer(&self) -> Result<bool>;
    fn submitted(&self) -> u64;
    fn device_id(&self) -> u64;
    fn poll_complete(&self, serial: u64) -> Result<bool>;
    fn finish(&self) -> Result<()>;
    fn target(&self, descriptor: ImageDescriptor) -> Result<NativeImage>;
    fn read(&self, image: &NativeImage) -> Result<Vec<u8>>;
    fn as_any(&self) -> &dyn Any;
}

pub(super) struct Producer<A: hal::Api> {
    pub owner: Rc<Device<A>>,
    pub(super) submissions: SubmissionQueue<A>,
    pub(super) releases: ReleaseQueue,
    failed: Cell<bool>,
    consumer: Option<Consumer<A>>,
}

struct Consumer<A: hal::Api> {
    submissions: std::rc::Weak<SubmissionQueue<A>>,
    releases: std::rc::Weak<RefCell<Vec<(ReleaseCallback, ExternalImageRelease)>>>,
}

impl<A: hal::Api> Producer<A> {
    fn progress(&self) -> Result<u64> {
        let result = self.ensure_healthy().and_then(|_| self.submissions.poll());
        if result.is_err() { self.failed.set(true); self.owner.lost.set(true); }
        dispatch_releases(&self.releases);
        result
    }
    pub(super) fn ensure_healthy(&self) -> Result<()> {
        if self.failed.get() || self.owner.lost.get() { return Err("Native image producer requires recreation".into()); }
        Ok(())
    }
    fn upload(&self, image: &NativeImage, descriptor: ImageDescriptor, bytes: &[u8]) -> Result<()> {
        if self.failed.get() || self.owner.lost.get() { return Err("Native image producer requires recreation".into()); }
        if image.leases.get() != 0 { return Err("Native image is acquired by the renderer".into()); }
        validate_buffer(descriptor, bytes)?;
        if image.descriptor.size != descriptor.size || image.descriptor.format != descriptor.format {
            return Err("Native image update requires matching size and format".into());
        }
        let texture = image.texture(&self.owner)?;
        self.failed.set(true);
        texture.upload_recorded(&self.owner, &self.submissions, DeviceIntRect::from_size(descriptor.size),
                                bytes, descriptor.stride, descriptor.offset, None)?;
        self.submissions.submit()?;
        self.failed.set(false);
        Ok(())
    }
}

impl<A: hal::Api> ImageDevice for Producer<A> {
    fn read(&self, image: &NativeImage) -> Result<Vec<u8>> {
        self.ensure_healthy()?;
        image.ensure_idle()?;
        if !matches!(image.descriptor.format, api::ImageFormat::RGBA8 | api::ImageFormat::BGRA8) {
            return Err("Native image readback requires RGBA8 or BGRA8".into());
        }
        let texture = image.texture(&self.owner)?;
        if !texture.initialized() {
            return Err("Cannot read an uninitialized native image".into());
        }
        let layout = self.owner.layout(texture.size.width, texture.size.height)?;
        let buffer = Buffer::readback(&self.owner, &layout)?;
        self.failed.set(true);
        let mut commands = self.submissions.recording()?;
        let previous = texture.current_usage();
        texture.transition(&mut commands, wgt::TextureUses::COPY_SRC);
        buffer.transition(&mut commands, wgt::BufferUses::COPY_DST);
        unsafe {
            copy_readback::<A>(commands.encoder(), &texture.raw, &buffer.raw,
                &layout, texture.size, hal::FormatAspects::COLOR);
        }
        texture.transition(&mut commands, previous);
        buffer.transition(&mut commands, wgt::BufferUses::MAP_READ);
        drop(commands);
        self.submissions.submit()?;
        self.submissions.wait()?;
        let pixels = self.owner.map_readback(&buffer.raw, &layout)?;
        self.failed.set(false);
        Ok(pixels)
    }

    fn target(&self, descriptor: ImageDescriptor) -> Result<NativeImage> {
        if self.failed.get() || self.owner.lost.get() { return Err("Native image producer requires recreation".into()); }
        validate_descriptor(descriptor)?;
        let texture = Texture::new(&self.owner, descriptor.size.width as u32, descriptor.size.height as u32,
                                  texture_format(descriptor.format)?, crate::device::TextureFilter::Linear, true)?;
        Ok(NativeImage::new(texture, descriptor))
    }
    fn create(&self, descriptor: ImageDescriptor, bytes: &[u8]) -> Result<NativeImage> {
        if self.failed.get() || self.owner.lost.get() { return Err("Native image producer requires recreation".into()); }
        validate_buffer(descriptor, bytes)?;
        let texture = Texture::new(&self.owner, descriptor.size.width as u32, descriptor.size.height as u32,
                                  texture_format(descriptor.format)?, crate::device::TextureFilter::Linear, false)?;
        let image = NativeImage::new(texture, descriptor);
        self.upload(&image, descriptor, bytes)?;
        Ok(image)
    }

    fn update(&self, image: &NativeImage, descriptor: ImageDescriptor, bytes: &[u8]) -> Result<()> {
        self.upload(image, descriptor, bytes)
    }

    fn poll(&self) -> Result<()> {
        self.progress().map(|_| ())
    }
    fn poll_consumer(&self) -> Result<bool> {
        let mut attached = false;
        let result = self.ensure_healthy().and_then(|_| {
            if let Some(queue) = self.consumer.as_ref().and_then(|consumer| consumer.submissions.upgrade()) {
                attached = true;
                queue.poll()?;
            }
            Ok(())
        });
        if result.is_err() { self.failed.set(true); self.owner.lost.set(true); }
        if let Some(releases) = self.consumer.as_ref().and_then(|consumer| consumer.releases.upgrade()) {
            dispatch_releases(&releases);
        }
        let returned = self.progress();
        result.and(returned.map(|_| attached))
    }
    fn submitted(&self) -> u64 { self.submissions.submitted() }
    fn device_id(&self) -> u64 { self.owner.cache_id }
    fn poll_complete(&self, serial: u64) -> Result<bool> {
        self.progress().map(|completed| completed >= serial)
    }
    fn finish(&self) -> Result<()> {
        let result = self.ensure_healthy().and_then(|_| self.submissions.wait());
        if result.is_err() {
            self.failed.set(true);
            self.owner.lost.set(true);
            self.submissions.shutdown();
        }
        dispatch_releases(&self.releases);
        result
    }
    fn as_any(&self) -> &dyn Any { self }
}

#[derive(Clone)]
pub struct ExternalImageDevice(pub(super) Rc<dyn ImageDevice>);

impl ExternalImageDevice {
    pub(super) fn new<A: hal::Api>(owner: &Rc<Device<A>>) -> Self {
        Self::new_inner(owner, None)
    }
    pub(super) fn for_renderer<A: hal::Api>(owner: &Rc<Device<A>>, submissions: &Rc<SubmissionQueue<A>>, releases: &ReleaseQueue) -> Self {
        Self::new_inner(owner, Some(Consumer {
            submissions: Rc::downgrade(submissions), releases: Rc::downgrade(releases),
        }))
    }
    fn new_inner<A: hal::Api>(owner: &Rc<Device<A>>, consumer: Option<Consumer<A>>) -> Self {
        let mut submissions = SubmissionQueue::new(owner, 3, false);
        if owner.info.backend == wgt::Backend::Vulkan {
            submissions = submissions.with_wait_timeout(std::time::Duration::from_secs(5));
        }
        Self(Rc::new(Producer { owner: owner.clone(), submissions, failed: Cell::new(false),
            releases: Rc::new(RefCell::new(Vec::new())), consumer }))
    }
    pub fn create_image(&self, descriptor: ImageDescriptor, bytes: &[u8]) -> Result<NativeImage> {
        self.0.create(descriptor, bytes)
    }
    pub fn update_image(&self, image: &NativeImage, descriptor: ImageDescriptor, bytes: &[u8]) -> Result<()> {
        self.0.update(image, descriptor, bytes)
    }
    pub fn poll(&self) -> Result<()> { self.0.poll() }
    /// Progress another renderer outside its command recording without retaining it.
    /// Returns whether that renderer's submission queue still exists.
    pub fn consumer_poller(&self) -> Rc<dyn Fn() -> Result<bool>> {
        let device = Rc::downgrade(&self.0);
        Rc::new(move || device.upgrade().ok_or("Native image consumer no longer exists")?.poll_consumer())
    }
    pub fn submitted(&self) -> u64 { self.0.submitted() }
    pub fn device_id(&self) -> u64 { self.0.device_id() }
    pub fn poll_complete(&self, serial: u64) -> Result<bool> { self.0.poll_complete(serial) }
    pub fn finish(&self) -> Result<()> { self.0.finish() }
    pub fn create_target(&self, descriptor: ImageDescriptor) -> Result<NativeImage> { self.0.target(descriptor) }
    pub fn read_image(&self, image: &NativeImage) -> Result<Vec<u8>> { self.0.read(image) }
}

impl<A: hal::Api> Drop for Producer<A> {
    fn drop(&mut self) {
        self.submissions.shutdown();
        dispatch_releases(&self.releases);
    }
}

#[derive(Clone)]
pub enum ExternalImageSource {
    Buffer(Arc<Vec<u8>>),
    Native(NativeImage),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExternalImageRelease {
    Unused,
    Complete,
    Abandoned,
}

type ReleaseCallback = Box<dyn FnOnce(ExternalImageRelease)>;
pub(super) type ReleaseQueue = Rc<RefCell<Vec<(ReleaseCallback, ExternalImageRelease)>>>;

pub(super) fn dispatch_releases(queue: &ReleaseQueue) {
    let callbacks: Vec<_> = queue.borrow_mut().drain(..).collect();
    for (callback, status) in callbacks { callback(status); }
}

pub(super) struct LeaseState {
    uses: Cell<usize>,
    completed: Cell<usize>,
    release: Option<ReleaseCallback>,
    native: Option<(Rc<Cell<usize>>, Rc<Cell<bool>>)>,
    queue: RefCell<Option<ReleaseQueue>>,
    metrics: RefCell<Option<Arc<diagnostics::RenderMetrics>>>,
}

impl LeaseState {
    pub fn track(self: &Rc<Self>, commands: &mut super::submission::Submission<impl hal::Api>) {
        self.uses.set(self.uses.get() + 1);
        let lease = self.clone();
        commands.on_complete(move || lease.completed.set(lease.completed.get() + 1));
    }
}

impl Drop for LeaseState {
    fn drop(&mut self) {
        let status = if self.uses.get() == 0 { ExternalImageRelease::Unused }
            else if self.uses.get() == self.completed.get() { ExternalImageRelease::Complete }
            else { ExternalImageRelease::Abandoned };
        if let Some((count, failed)) = &self.native {
            count.set(count.get() - 1);
            if status == ExternalImageRelease::Abandoned { failed.set(true); }
        }
        if let Some(mut release) = self.release.take() {
            if let Some(metrics) = self.metrics.get_mut().take() {
                release = Box::new(move |status| {
                    metrics.add(diagnostics::RenderCounter::ExternalLeaseReleases, 1);
                    metrics.release(diagnostics::RenderGauge::ExternalLeases);
                    release(status);
                });
            }
            if let Some(queue) = self.queue.get_mut().as_ref() { queue.borrow_mut().push((release, status)); }
            else { release(status); }
        }
    }
}

pub struct ExternalImageLease {
    pub(crate) descriptor: ImageDescriptor,
    pub(crate) uv: TexelRect,
    pub(crate) generation: u64,
    pub(crate) source: ExternalImageSource,
    pub(super) state: Rc<LeaseState>,
}

impl ExternalImageLease {
    pub fn new(descriptor: ImageDescriptor, uv: TexelRect, generation: u64, source: ExternalImageSource,
               release: impl FnOnce(ExternalImageRelease) + 'static) -> Result<Self> {
        let mut lease = Self { descriptor, uv, generation, source, state: Rc::new(LeaseState {
            uses: Cell::new(0), completed: Cell::new(0), release: Some(Box::new(release)), native: None, queue: RefCell::new(None),
            metrics: RefCell::new(None),
        }) };
        validate_descriptor(descriptor)?;
        if !uv.to_array().iter().all(|value| value.is_finite())
            || uv.uv0.x == uv.uv1.x || uv.uv0.y == uv.uv1.y
            || [uv.uv0.x, uv.uv1.x].iter().any(|&v| v < 0.0 || v > descriptor.size.width as f32)
            || [uv.uv0.y, uv.uv1.y].iter().any(|&v| v < 0.0 || v > descriptor.size.height as f32) {
            return Err("Invalid native image UV rectangle".into());
        }
        match &lease.source {
            ExternalImageSource::Buffer(bytes) => validate_buffer(descriptor, bytes)?,
            ExternalImageSource::Native(image) => {
                if descriptor.size != image.descriptor.size || descriptor.format != image.descriptor.format {
                    return Err("Native image descriptor does not match its allocation".into());
                }
                if image.failed.get() { return Err("Native image requires recreation".into()); }
                image.leases.set(image.leases.get() + 1);
                Rc::get_mut(&mut lease.state).unwrap().native = Some((image.leases.clone(), image.failed.clone()));
            }
        }
        Ok(lease)
    }

    pub fn descriptor(&self) -> ImageDescriptor { self.descriptor }
    pub fn uv(&self) -> TexelRect { self.uv }
    pub fn generation(&self) -> u64 { self.generation }

    pub(super) fn attach_releases(&self, queue: &ReleaseQueue) {
        *self.state.queue.borrow_mut() = Some(queue.clone());
    }

    pub(super) fn attach_metrics(&self, metrics: Option<&Arc<diagnostics::RenderMetrics>>) {
        if let Some(metrics) = metrics {
            let mut attached = self.state.metrics.borrow_mut();
            if attached.is_none() {
                metrics.add(diagnostics::RenderCounter::ExternalLeaseAcquires, 1);
                metrics.retain(diagnostics::RenderGauge::ExternalLeases);
                *attached = Some(metrics.clone());
            }
        }
    }

    pub(super) fn complete_cpu_copy(&self) {
        self.state.uses.set(self.state.uses.get() + 1);
        self.state.completed.set(self.state.completed.get() + 1);
    }
}

pub trait ExternalImageProvider {
    fn acquire(&mut self, id: ExternalImageId, channel: u8, is_composited: bool) -> Result<ExternalImageLease>;
}

pub(super) fn validate_descriptor(descriptor: ImageDescriptor) -> Result<()> {
    if descriptor.size.width <= 0 || descriptor.size.height <= 0 || descriptor.offset < 0 {
        return Err("Invalid external image dimensions or offset".into());
    }
    let row = descriptor.size.width.checked_mul(descriptor.format.bytes_per_pixel()).ok_or("External image row overflow")?;
    if descriptor.stride.unwrap_or(row) < row { return Err("Invalid external image stride".into()); }
    Ok(())
}

fn validate_buffer(descriptor: ImageDescriptor, bytes: &[u8]) -> Result<()> {
    validate_descriptor(descriptor)?;
    let row = descriptor.size.width as usize * descriptor.format.bytes_per_pixel() as usize;
    let stride = descriptor.stride.map_or(row, |stride| stride as usize);
    let end = stride.checked_mul(descriptor.size.height as usize - 1)
        .and_then(|offset| offset.checked_add(descriptor.offset as usize))
        .and_then(|offset| offset.checked_add(row)).ok_or("External image buffer overflow")?;
    if end > bytes.len() { return Err("External image buffer is too short".into()); }
    Ok(())
}

#[cfg(all(test, wr_hal_vulkan))]
mod tests {
    use super::*;
    use api::{ImageFormat, ImageDescriptorFlags};
    use std::cell::RefCell;

    #[test]
    fn lease_metrics_follow_queued_callback_for_each_terminal_status() {
        use diagnostics::{RenderCounter, RenderGauge, RenderMetrics};
        for expected in [ExternalImageRelease::Unused, ExternalImageRelease::Complete, ExternalImageRelease::Abandoned] {
            let metrics = RenderMetrics::for_test(1, 0);
            let queue = Rc::new(RefCell::new(Vec::new()));
            let released = Rc::new(Cell::new(None));
            let callback = released.clone();
            let descriptor = ImageDescriptor::new(1, 1, ImageFormat::RGBA8, ImageDescriptorFlags::empty());
            let lease = ExternalImageLease::new(descriptor, TexelRect::new(0.0, 0.0, 1.0, 1.0), 1,
                ExternalImageSource::Buffer(Arc::new(vec![0; 4])), move |status| callback.set(Some(status))).unwrap();
            lease.attach_releases(&queue);
            lease.attach_metrics(Some(&metrics));
            lease.attach_metrics(Some(&metrics));
            match expected {
                ExternalImageRelease::Unused => {},
                ExternalImageRelease::Complete => lease.complete_cpu_copy(),
                ExternalImageRelease::Abandoned => lease.state.uses.set(1),
            }
            drop(lease);
            assert_eq!(released.get(), None);
            assert_eq!(metrics.snapshot().count(RenderCounter::ExternalLeaseAcquires), 1);
            assert_eq!(metrics.snapshot().count(RenderCounter::ExternalLeaseReleases), 0);
            assert_eq!(metrics.snapshot().gauge(RenderGauge::ExternalLeases), 1);
            dispatch_releases(&queue);
            assert_eq!(released.get(), Some(expected));
            assert_eq!(metrics.snapshot().count(RenderCounter::ExternalLeaseReleases), 1);
            assert_eq!(metrics.snapshot().gauge(RenderGauge::ExternalLeases), 0);
        }
    }

    #[test]
    fn rejects_invalid_cpu_images_and_releases_unused_lease() {
        let releases = Rc::new(RefCell::new(Vec::new()));
        let descriptor = ImageDescriptor::new(7, 5, ImageFormat::RGBA8, ImageDescriptorFlags::empty());
        for (desc, uv, len) in [
            (descriptor, TexelRect::new(0.0, 0.0, 7.0, 5.0), 139),
            (descriptor, TexelRect::new(0.0, 0.0, f32::NAN, 5.0), 140),
            (ImageDescriptor { stride: Some(27), ..descriptor }, TexelRect::new(0.0, 0.0, 7.0, 5.0), 140),
        ] {
            let log = releases.clone();
            assert!(ExternalImageLease::new(desc, uv, 1, ExternalImageSource::Buffer(Arc::new(vec![0; len])),
                move |status| log.borrow_mut().push(status)).is_err());
        }
        assert_eq!(*releases.borrow(), vec![ExternalImageRelease::Unused; 3]);
    }

    #[test]
    #[ignore = "Requires Vulkan and validation"]
    fn native_image_identity_and_acquisition() {
        let options = Options { validation: true, ..Options::default() };
        let first = Rc::new(create_vulkan_device(&options).unwrap());
        let second = Rc::new(create_vulkan_device(&options).unwrap());
        let producer = ExternalImageDevice::new(&first);
        let foreign = ExternalImageDevice::new(&second);
        let descriptor = ImageDescriptor::new(7, 5, ImageFormat::RGBA8, ImageDescriptorFlags::IS_OPAQUE);
        let pixels = [17, 31, 199, 255].repeat(35);
        let image = producer.create_image(descriptor, &pixels).unwrap();
        assert!(foreign.update_image(&image, descriptor, &pixels).unwrap_err().contains("another device"));
        let releases = Rc::new(RefCell::new(Vec::new()));
        let log = releases.clone();
        let lease = ExternalImageLease::new(descriptor, TexelRect::new(7.0, 5.0, 0.0, 0.0), 9,
            ExternalImageSource::Native(image.clone()), move |status| log.borrow_mut().push(status)).unwrap();
        assert!(producer.update_image(&image, descriptor, &pixels).unwrap_err().contains("acquired"));
        let invalid = ImageDescriptor::new(8, 5, ImageFormat::RGBA8, ImageDescriptorFlags::IS_OPAQUE);
        assert!(ExternalImageLease::new(invalid, TexelRect::new(0.0, 0.0, 7.0, 5.0), 10,
            ExternalImageSource::Native(image.clone()), |_| {}).is_err());
        assert!(producer.update_image(&image, descriptor, &pixels).is_err());
        drop(lease);
        assert_eq!(*releases.borrow(), [ExternalImageRelease::Unused]);
        producer.update_image(&image, descriptor, &pixels).unwrap();
        let replacement = producer.create_image(descriptor, &pixels).unwrap();
        assert_ne!(replacement.generation(), image.generation());
        assert!(image.texture(&second).is_err());
        assert!(image.texture(&first).is_ok());
        let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done = released.clone();
        let context = producer.vulkan_context().unwrap();
        assert!(unsafe {
            producer.import_vulkan_image(VulkanImageDescriptor {
                image: ash::vk::Image::null(), device: context.device.handle(), queue: context.queue,
                queue_family: context.queue_family, descriptor, initial_usage: wgt::TextureUses::RESOURCE, renderable: false,
            }, Box::new(move || done.store(true, std::sync::atomic::Ordering::SeqCst)))
        }.is_err());
        assert!(released.load(std::sync::atomic::Ordering::SeqCst));
        let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done = released.clone();
        let texture = image.texture(&first).unwrap();
        first.fault.set(Some(FailurePoint::Import));
        let rejected = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notice = rejected.clone();
        assert!(unsafe { producer.import_vulkan_image(VulkanImageDescriptor {
            image: texture.raw.raw_handle(), device: context.device.handle(), queue: context.queue,
            queue_family: context.queue_family, descriptor, initial_usage: texture.current_usage(), renderable: false,
        }, Box::new(move || notice.store(true, std::sync::atomic::Ordering::SeqCst))) }.is_err());
        assert!(rejected.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!first.lost.get());
        let imported = unsafe {
            producer.import_vulkan_image(VulkanImageDescriptor {
                image: texture.raw.raw_handle(), device: context.device.handle(), queue: context.queue,
                queue_family: context.queue_family, descriptor, initial_usage: wgt::TextureUses::RESOURCE, renderable: false,
            }, Box::new(move || done.store(true, std::sync::atomic::Ordering::SeqCst)))
        }.unwrap();
        assert!(imported.texture(&first).is_ok());
        assert!(!released.load(std::sync::atomic::Ordering::SeqCst));
        drop(imported);
        assert!(released.load(std::sync::atomic::Ordering::SeqCst));
        producer.update_image(&image, descriptor, &pixels).unwrap();
    }

    #[test]
    #[ignore = "Requires Vulkan and validation"]
    fn abandoned_image_use_requires_recreation() {
        let owner = Rc::new(create_vulkan_device(&Options { validation: true, ..Options::default() }).unwrap());
        let producer = ExternalImageDevice::new(&owner);
        let descriptor = ImageDescriptor::new(7, 5, ImageFormat::RGBA8, ImageDescriptorFlags::IS_OPAQUE);
        let pixels = [17, 31, 199, 255].repeat(35);
        let image = producer.create_image(descriptor, &pixels).unwrap();
        let released = Rc::new(RefCell::new(Vec::new()));
        let log = released.clone();
        let lease = ExternalImageLease::new(descriptor, TexelRect::new(0.0, 0.0, 7.0, 5.0), 1,
            ExternalImageSource::Native(image.clone()), move |status| log.borrow_mut().push(status)).unwrap();
        let releases = Rc::new(RefCell::new(Vec::new()));
        lease.attach_releases(&releases);
        let view = image.texture(&owner).unwrap().with_lease(lease.state.clone(), crate::device::TextureFilter::Linear, false).unwrap();
        let queue = SubmissionQueue::new(&owner, 2, false);
        {
            let mut commands = queue.recording().unwrap();
            view.transition(&mut commands, wgt::TextureUses::RESOURCE);
        }
        drop(view);
        drop(lease);
        assert!(released.borrow().is_empty());
        queue.discard_recording();
        dispatch_releases(&releases);
        assert_eq!(*released.borrow(), [ExternalImageRelease::Abandoned]);
        assert!(producer.update_image(&image, descriptor, &pixels).unwrap_err().contains("recreation"));
    }
}
