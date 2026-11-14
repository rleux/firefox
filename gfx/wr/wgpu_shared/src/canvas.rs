/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::{core, hal, wgt, NativeContext};
use hal::Device as _;
use std::{
    borrow::Cow,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex, OnceLock,
    },
    time::Duration,
};

const MAX_IMAGES: usize = 6;
const MAX_BYTES: u64 = 256 * 1024 * 1024;
static BUDGET: Mutex<(usize, u64)> = Mutex::new((0, 0));
static NONCE: AtomicU64 = AtomicU64::new(1);

struct Permit(u64);
impl Permit {
    fn reserve(bytes: u64) -> Option<Self> {
        let mut budget = BUDGET.lock().unwrap();
        let total = budget.1.checked_add(bytes)?;
        if bytes == 0 || budget.0 >= MAX_IMAGES || total > MAX_BYTES {
            return None;
        }
        *budget = (budget.0 + 1, total);
        Some(Self(bytes))
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        let mut budget = BUDGET.lock().unwrap();
        budget.0 -= 1;
        budget.1 -= self.0;
    }
}

struct ImageKeeper {
    original: Option<Arc<core::resource::DestroyedTexture>>,
    _context: Arc<NativeContext>,
    _permit: Permit,
}
impl Drop for ImageKeeper {
    fn drop(&mut self) {
        // Producer retirement is drained before Ready; consumer callbacks own only this keeper.
        let original = self.original.take().unwrap();
        assert_eq!(Arc::strong_count(&original), 1);
        drop(original);
    }
}

pub struct CanvasPublication {
    context: Arc<NativeContext>,
    generation: u64,
    owner: u64,
    publication: u64,
    nonce: u64,
    descriptor: wgt::TextureDescriptor<String, Vec<wgt::TextureFormat>>,
    keeper: Arc<ImageKeeper>,
    producer: Arc<core::device::Device>,
    submission: core::SubmissionIndex,
    core_done: Arc<AtomicBool>,
    ready: AtomicBool,
    quarantined: AtomicBool,
}

pub fn descriptor_supported(d: &wgt::TextureDescriptor<String, Vec<wgt::TextureFormat>>) -> bool {
    matches!(
        d.format,
        wgt::TextureFormat::Rgba8Unorm | wgt::TextureFormat::Bgra8Unorm
    ) && d.dimension == wgt::TextureDimension::D2
        && d.mip_level_count == 1
        && d.sample_count == 1
        && d.size.width > 0
        && d.size.height > 0
        && d.size.depth_or_array_layers == 1
        && d.view_formats.is_empty()
        && d.usage.contains(wgt::TextureUsages::RENDER_ATTACHMENT)
        && (wgt::TextureUsages::RENDER_ATTACHMENT
            | wgt::TextureUsages::COPY_SRC
            | wgt::TextureUsages::COPY_DST
            | wgt::TextureUsages::TEXTURE_BINDING)
            .contains(d.usage)
}

impl CanvasPublication {
    /// The caller serializes every producer command, write, and seal on its owning actor thread.
    pub unsafe fn seal(
        context: &Arc<NativeContext>,
        source: &Arc<core::resource::Texture>,
        queue: &Arc<core::device::queue::Queue>,
        owner: u64,
        publication: u64,
    ) -> Option<Arc<Self>> {
        use core::resource::ParentDevice;
        let producer = source.device().clone();
        let descriptor = source.descriptor().clone();
        if owner == 0
            || publication == 0
            || !context.is_valid()
            || !context.matches_device(&producer)
            || queue.same_device(&producer).is_err()
            || !descriptor_supported(&descriptor)
        {
            return None;
        }
        let bytes = {
            let texture = unsafe { source.clone().as_hal::<hal::api::Vulkan>() }?;
            match unsafe { texture.memory() } {
                hal::vulkan::TextureMemory::Allocation(allocation) => allocation.size(),
                _ => return None,
            }
        };
        // This accounts admitted shared leases, not ordinary application texture allocations.
        let permit = Permit::reserve(bytes)?;
        for filter in [
            wgt::error::ErrorFilter::Validation,
            wgt::error::ErrorFilter::OutOfMemory,
            wgt::error::ErrorFilter::Internal,
        ] {
            producer.push_error_scope(filter);
        }
        let encoder = producer.create_command_encoder(&wgt::CommandEncoderDescriptor {
            label: Some("WebGPU shared canvas seal".into()),
        });
        let mut pass = encoder.begin_render_pass(core::command::ResolvedRenderPassDescriptor {
            color_attachments: Cow::Owned(vec![Some(core::command::RenderPassColorAttachment {
                view: source.create_view(&Default::default()),
                depth_slice: None,
                resolve_target: None,
                load_op: wgt::LoadOp::Load,
                store_op: wgt::StoreOp::Store,
            })]),
            ..Default::default()
        });
        pass.end();
        encoder.transition_resources(
            std::iter::empty(),
            std::iter::once(wgt::TextureTransition {
                texture: source.clone(),
                selector: None,
                state: wgt::TextureUses::RESOURCE,
            }),
        );
        let commands = [encoder.finish(&Default::default())];
        let mut valid = producer.is_valid();
        for _ in 0..3 {
            valid &= producer
                .pop_error_scope()
                .is_ok_and(|error| error.is_none());
        }
        if !valid {
            return None;
        }
        for filter in [
            wgt::error::ErrorFilter::Validation,
            wgt::error::ErrorFilter::OutOfMemory,
            wgt::error::ErrorFilter::Internal,
        ] {
            producer.push_error_scope(filter);
        }
        let submission = queue.submit(&commands);
        valid = submission.is_some() && producer.is_valid();
        for _ in 0..3 {
            valid &= producer
                .pop_error_scope()
                .is_ok_and(|error| error.is_none());
        }
        if !valid {
            return None;
        }
        let original = source.destroy_and_retain()?;
        let core_done = Arc::new(AtomicBool::new(false));
        let completed = core_done.clone();
        queue.on_submitted_work_done(Box::new(move || completed.store(true, Ordering::Release)));
        let lease = Arc::new(Self {
            context: context.clone(),
            generation: context.generation(),
            owner,
            publication,
            nonce: NONCE.fetch_add(1, Ordering::Relaxed),
            descriptor,
            keeper: Arc::new(ImageKeeper {
                original: Some(original),
                _context: context.clone(),
                _permit: permit,
            }),
            producer,
            submission: submission.unwrap(),
            core_done,
            ready: AtomicBool::new(false),
            quarantined: AtomicBool::new(false),
        });
        retain_pending(lease.clone());
        Some(lease)
    }

    fn producer_fence_value(&self) -> Option<Result<u64, hal::DeviceError>> {
        let device = unsafe { self.producer.clone().as_hal::<hal::api::Vulkan>() }?;
        let fence = unsafe { self.producer.clone().fence_as_hal::<hal::api::Vulkan>() }?;
        Some(unsafe { device.get_fence_value(&fence) })
    }

    fn complete_producer(&self, completed: Result<u64, hal::DeviceError>) -> bool {
        if self.quarantined.load(Ordering::Acquire) {
            return false;
        }
        match producer_complete(
            completed,
            self.submission,
            self.core_done.load(Ordering::Acquire),
            Arc::strong_count(self.keeper.original.as_ref().unwrap()),
        ) {
            Ok(ready) => ready,
            Err(()) => {
                self.context.mark_failed();
                self.quarantined.store(true, Ordering::Release);
                false
            }
        }
    }

    /// The caller must be the producer's existing callback/actor thread.
    pub unsafe fn poll_producer_on_owner_thread(self: &Arc<Self>) -> bool {
        if self.ready.load(Ordering::Acquire) || self.quarantined.load(Ordering::Acquire) {
            return false;
        }
        let completed = self.producer_fence_value();
        if matches!(completed, Some(Ok(value)) if value < self.submission) {
            return true;
        }
        if !self.core_done.load(Ordering::Acquire)
            || Arc::strong_count(self.keeper.original.as_ref().unwrap()) != 1
            || !matches!(completed, Some(Ok(_)))
        {
            let _ = self.producer.poll(wgt::PollType::Poll);
        }
        if completed.is_some_and(|value| self.complete_producer(value))
            && !self.ready.swap(true, Ordering::AcqRel)
        {
            retain_pending(self.clone());
        }
        !self.ready.load(Ordering::Acquire) && !self.quarantined.load(Ordering::Acquire)
    }
    pub fn descriptor(&self) -> &wgt::TextureDescriptor<String, Vec<wgt::TextureFormat>> {
        &self.descriptor
    }
    pub fn owner(&self) -> u64 {
        self.owner
    }
    pub fn publication(&self) -> u64 {
        self.publication
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn matches_texture(&self, texture: &Arc<core::resource::Texture>) -> bool {
        let original = unsafe {
            self.keeper
                .original
                .as_ref()
                .unwrap()
                .as_hal::<hal::api::Vulkan>()
        };
        let consumer = unsafe { texture.clone().as_hal::<hal::api::Vulkan>() };
        match (original, consumer) {
            (Some(original), Some(consumer)) => unsafe {
                original.raw_handle() == consumer.raw_handle()
            },
            _ => false,
        }
    }
    pub fn nonce(&self) -> u64 {
        self.nonce
    }
    pub fn matches(&self, context: &Arc<NativeContext>, owner: u64, publication: u64) -> bool {
        Arc::ptr_eq(&self.context, context)
            && self.generation == context.generation()
            && self.owner == owner
            && self.publication == publication
            && context.is_valid()
    }
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire) && self.context.is_valid()
    }
    pub fn is_failed(&self) -> bool {
        !self.context.is_valid()
    }

    pub fn wrap(
        self: &Arc<Self>,
        device: &Arc<core::device::Device>,
    ) -> Result<Option<Arc<core::resource::Texture>>, String> {
        if !self.context.is_valid() || !self.context.matches_device(device) {
            return Err("Shared canvas context is retired or incompatible".into());
        }
        if !self.is_ready() {
            return Ok(None);
        }
        let original = self.keeper.original.as_ref().unwrap();
        let raw = unsafe { original.as_hal::<hal::api::Vulkan>() }
            .ok_or("Shared canvas is not Vulkan")?;
        let hal_device = unsafe { device.clone().as_hal::<hal::api::Vulkan>() }
            .ok_or("Consumer is not Vulkan")?;
        let keeper = self.keeper.clone();
        let desc = self
            .descriptor
            .map_label(|_| Some("WR shared immutable canvas".into()));
        let mut desc = desc;
        desc.usage = wgt::TextureUsages::TEXTURE_BINDING;
        let wrapper = unsafe {
            hal_device.texture_from_raw(
                raw.raw_handle(),
                &hal::TextureDescriptor {
                    label: Some("WR shared immutable canvas"),
                    size: desc.size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: desc.dimension,
                    format: desc.format,
                    usage: wgt::TextureUses::RESOURCE,
                    memory_flags: hal::MemoryFlags::empty(),
                    view_formats: vec![],
                },
                Some(Box::new(move || drop(keeper))),
                hal::vulkan::TextureMemory::External,
            )
        };
        let (texture, error) = unsafe {
            device.create_texture_from_hal(
                Box::new(wrapper),
                &desc,
                wgt::TextureUses::RESOURCE,
                true,
            )
        };
        match error {
            Some(error) => Err(error.to_string()),
            None => Ok(Some(texture)),
        }
    }
}

fn producer_complete(
    native: Result<u64, hal::DeviceError>,
    submission: u64,
    core_done: bool,
    raw_owners: usize,
) -> Result<bool, ()> {
    native
        .map(|value| value >= submission && core_done && raw_owners == 1)
        .map_err(|_| ())
}

fn poll_interval(attempt: u32) -> Duration {
    Duration::from_millis(if attempt < 16 {
        2
    } else if attempt < 30 {
        16
    } else {
        100
    })
}

fn retain_pending(lease: Arc<CanvasPublication>) {
    static WORKER: OnceLock<Option<mpsc::Sender<Arc<CanvasPublication>>>> = OnceLock::new();
    let sender = WORKER.get_or_init(|| {
        let (sender, receiver) = mpsc::channel::<Arc<CanvasPublication>>();
        std::thread::Builder::new()
            .name("WebGPU shared images".into())
            .spawn(move || {
                let mut pending = Vec::new();
                let mut attempt = 0;
                loop {
                    if pending.is_empty() {
                        match receiver.recv() {
                            Ok(lease) => {
                                pending.push(lease);
                                attempt = 0;
                            }
                            Err(_) => return,
                        }
                    } else if let Ok(lease) = receiver.recv_timeout(poll_interval(attempt)) {
                        pending.push(lease);
                        attempt = 0;
                    }
                    let before = pending.len();
                    pending.retain(|lease| {
                        if lease.ready.load(Ordering::Acquire) {
                            return false;
                        }
                        if lease
                            .producer_fence_value()
                            .is_some_and(|value| lease.complete_producer(value))
                        {
                            lease.ready.store(true, Ordering::Release);
                            return false;
                        }
                        if lease.quarantined.load(Ordering::Acquire) {
                            std::mem::forget(lease.clone());
                            return false;
                        }
                        true
                    });
                    attempt = if pending.len() < before {
                        0
                    } else {
                        (attempt + 1).min(30)
                    };
                }
            })
            .ok()
            .map(|_| sender)
    });
    if let Some(sender) = sender {
        if let Err(error) = sender.send(lease) {
            std::mem::forget(error.0);
        }
    } else {
        std::mem::forget(lease);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn completion_requires_native_core_and_exclusive_ownership() {
        assert_eq!(producer_complete(Ok(4), 5, true, 1), Ok(false));
        assert_eq!(producer_complete(Ok(5), 5, false, 1), Ok(false));
        assert_eq!(producer_complete(Ok(5), 5, true, 2), Ok(false));
        assert_eq!(producer_complete(Ok(5), 5, true, 1), Ok(true));
        assert_eq!(
            producer_complete(Err(hal::DeviceError::Lost), 5, true, 1),
            Err(())
        );
        assert_eq!(
            producer_complete(Err(hal::DeviceError::Unexpected), 5, true, 1),
            Err(())
        );
    }
    #[test]
    fn delayed_producers_eventually_use_ordinary_idle_polling() {
        assert_eq!(poll_interval(0), Duration::from_millis(2));
        assert_eq!(poll_interval(15), Duration::from_millis(2));
        assert_eq!(poll_interval(16), Duration::from_millis(16));
        assert_eq!(poll_interval(29), Duration::from_millis(16));
        assert_eq!(poll_interval(30), Duration::from_millis(100));
        assert_eq!(poll_interval(u32::MAX), Duration::from_millis(100));
    }
    #[test]
    fn shared_budget_is_exact_and_releases_failed_admission() {
        let a = Permit::reserve(MAX_BYTES - 1).unwrap();
        assert!(Permit::reserve(2).is_none());
        let b = Permit::reserve(1).unwrap();
        assert!(Permit::reserve(1).is_none());
        drop((a, b));
        let permits = (0..MAX_IMAGES)
            .map(|_| Permit::reserve(1).unwrap())
            .collect::<Vec<_>>();
        assert!(Permit::reserve(1).is_none());
        drop(permits);
        assert_eq!(*BUDGET.lock().unwrap(), (0, 0));
    }
}
