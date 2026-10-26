use super::*;
use crate::vulkan::{Queue, ResourceIdentity};
use core::{
    mem::ManuallyDrop,
    sync::atomic::{AtomicBool, Ordering},
};

struct Fence {
    raw: vk::Fence,
    device: Arc<DeviceShared>,
}
impl Fence {
    fn new(device: &Arc<DeviceShared>) -> Result<Self, crate::DeviceError> {
        let raw = unsafe {
            device
                .raw
                .create_fence(&vk::FenceCreateInfo::default(), None)
        }
        .map_err(map_host_device_oom_and_lost_err)?;
        Ok(Self {
            raw,
            device: device.clone(),
        })
    }
}
impl Drop for Fence {
    fn drop(&mut self) {
        unsafe { self.device.raw.destroy_fence(self.raw, None) };
    }
}

struct AcquireFence {
    fence: Fence,
    armed: AtomicBool,
}
pub(super) struct PresentFence {
    fence: Fence,
    uncertain: bool,
}
impl PresentFence {
    pub(super) fn raw(&self) -> vk::Fence {
        self.fence.raw
    }
}

struct Payload {
    raw: vk::SwapchainKHR,
    functor: khr::swapchain::Device,
    device: Arc<DeviceShared>,
    _surface: Arc<NativeSurfaceOwner>,
    acquire_semaphores: Vec<Arc<Mutex<SwapchainAcquireSemaphore>>>,
    present_semaphores: Vec<Arc<Mutex<SwapchainPresentSemaphores>>>,
    acquire_fences: Vec<AcquireFence>,
    present_fences: Mutex<Vec<PresentFence>>,
    images: Mutex<Vec<Option<ResourceIdentity<vk::Image>>>>,
    execution: Mutex<Option<(Arc<crate::vulkan::shared::QueueProgress>, u64)>>,
    public_fence: Mutex<Option<vk::Semaphore>>,
}

pub(crate) struct Generation {
    payload: ManuallyDrop<Payload>,
    closed: AtomicBool,
    retired: Arc<AtomicBool>,
    submission_uncertain: AtomicBool,
}
impl core::fmt::Debug for Generation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SwapchainGeneration")
            .field("closed", &self.closed.load(Ordering::Acquire))
            .field("retired", &self.retired.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl Generation {
    pub(super) fn new(
        raw: vk::SwapchainKHR,
        functor: khr::swapchain::Device,
        device: Arc<DeviceShared>,
        surface: Arc<NativeSurfaceOwner>,
        count: usize,
    ) -> Result<Arc<Self>, crate::DeviceError> {
        let mut generation = Self {
            payload: ManuallyDrop::new(Payload {
                raw,
                functor,
                device,
                _surface: surface,
                acquire_semaphores: Vec::new(),
                present_semaphores: Vec::new(),
                acquire_fences: Vec::new(),
                present_fences: Mutex::new(Vec::new()),
                images: Mutex::new(vec![None; count]),
                execution: Mutex::new(None),
                public_fence: Mutex::new(None),
            }),
            closed: AtomicBool::new(false),
            retired: Arc::new(AtomicBool::new(true)),
            submission_uncertain: AtomicBool::new(false),
        };
        for index in 0..count {
            let acquire = SwapchainAcquireSemaphore::new(&generation.payload.device, index)?;
            generation
                .payload
                .acquire_semaphores
                .push(Arc::new(Mutex::new(acquire)));
            generation
                .payload
                .present_semaphores
                .push(Arc::new(Mutex::new(SwapchainPresentSemaphores::new(index))));
            let fence = Fence::new(&generation.payload.device)?;
            generation.payload.acquire_fences.push(AcquireFence {
                fence,
                armed: AtomicBool::new(false),
            });
        }
        let generation = Arc::new(generation);
        generation
            .payload
            .device
            .shared_queue_group
            .as_ref()
            .unwrap()
            .presentations
            .lock()
            .push((Arc::downgrade(&generation), generation.retired.clone()));
        Ok(generation)
    }

    pub(super) fn acquire_semaphores(&self) -> Vec<Arc<Mutex<SwapchainAcquireSemaphore>>> {
        self.payload.acquire_semaphores.clone()
    }

    pub(super) fn present_semaphores(&self) -> Vec<Arc<Mutex<SwapchainPresentSemaphores>>> {
        self.payload.present_semaphores.clone()
    }

    pub(crate) fn group(&self) -> &Arc<crate::vulkan::shared::QueueGroup> {
        self.payload.device.shared_queue_group.as_ref().unwrap()
    }

    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    pub(crate) fn check_open(&self) -> Result<(), crate::SurfaceError> {
        if self.closed.load(Ordering::Acquire) || self.group().closed.load(Ordering::Acquire) {
            return Err(crate::SurfaceError::Outdated);
        }
        Ok(())
    }

    pub(super) fn bind_public_fence(
        &self,
        public: &crate::vulkan::Fence,
    ) -> Result<(), crate::SurfaceError> {
        self.check_open()?;
        let crate::vulkan::Fence::TimelineSemaphore(raw) = public else {
            return Err(crate::SurfaceError::Other(
                "Shared surfaces require a timeline fence",
            ));
        };
        let mut bound = self.payload.public_fence.lock();
        if bound.is_some_and(|previous| previous != *raw) {
            return Err(crate::SurfaceError::Other("Shared surface fence changed"));
        }
        *bound = Some(*raw);
        Ok(())
    }

    pub(super) fn prepare_acquire(&self, slot: usize) -> Result<vk::Fence, crate::SurfaceError> {
        self.check_open()?;
        let fence = &self.payload.acquire_fences[slot];
        if fence.armed.load(Ordering::Acquire) {
            unsafe {
                self.payload
                    .device
                    .raw
                    .wait_for_fences(&[fence.fence.raw], true, u64::MAX)
                    .map_err(map_host_device_oom_and_lost_err)?;
                fence.armed.store(false, Ordering::Release);
                self.payload
                    .device
                    .raw
                    .reset_fences(&[fence.fence.raw])
                    .map_err(|error| {
                        self.close();
                        map_host_device_oom_and_lost_err(error)
                    })?;
            }
        }
        Ok(fence.fence.raw)
    }

    pub(super) fn wait_acquire(
        &self,
        slot: usize,
        timeout: u64,
    ) -> Result<(), crate::SurfaceError> {
        unsafe {
            self.payload.device.raw.wait_for_fences(
                &[self.payload.acquire_fences[slot].fence.raw],
                true,
                timeout,
            )
        }
        .map_err(|error| {
            self.close();
            if error == vk::Result::TIMEOUT {
                crate::SurfaceError::Timeout
            } else {
                map_host_device_oom_and_lost_err(error).into()
            }
        })
    }

    pub(super) fn acquired(&self, slot: usize) {
        self.retired.store(false, Ordering::Release);
        self.payload.acquire_fences[slot]
            .armed
            .store(true, Ordering::Release);
    }

    pub(super) fn begin_image(
        &self,
        index: u32,
        identity: ResourceIdentity<vk::Image>,
    ) -> Result<(), crate::SurfaceError> {
        let mut images = self.payload.images.lock();
        let Some(image) = images.get_mut(index as usize) else {
            self.close();
            return Err(crate::SurfaceError::Outdated);
        };
        *image = Some(identity);
        Ok(())
    }

    pub(super) fn validate_image(
        &self,
        queue: &Queue,
        index: u32,
        identity: ResourceIdentity<vk::Image>,
    ) -> Result<(), crate::DeviceError> {
        if self.check_open().is_err()
            || queue.device.queue_index != 0
            || queue.device.raw.handle() != self.payload.device.raw.handle()
            || !queue
                .device
                .shared_queue_group
                .as_ref()
                .is_some_and(|group| Arc::ptr_eq(group, self.group()))
            || self.payload.images.lock().get(index as usize) != Some(&Some(identity))
        {
            return Err(crate::DeviceError::Unexpected);
        }
        Ok(())
    }

    pub(super) fn validate_fence(
        &self,
        fence: &crate::vulkan::Fence,
    ) -> Result<(), crate::DeviceError> {
        if !matches!(fence, crate::vulkan::Fence::TimelineSemaphore(raw)
            if *self.payload.public_fence.lock() == Some(*raw))
        {
            return Err(crate::DeviceError::Unexpected);
        }
        Ok(())
    }

    pub(super) fn finish_image(&self, index: u32) {
        self.payload.images.lock()[index as usize] = None;
    }

    pub(super) fn prepare_present(&self) -> Result<PresentFence, crate::DeviceError> {
        let mut fences = self.payload.present_fences.lock();
        let mut index = 0;
        while index < fences.len() {
            if unsafe {
                self.payload
                    .device
                    .raw
                    .get_fence_status(fences[index].fence.raw)
            }
            .map_err(map_host_device_oom_and_lost_err)?
            {
                fences.swap_remove(index);
            } else {
                index += 1;
            }
        }
        fences
            .try_reserve(1)
            .map_err(|_| crate::DeviceError::OutOfMemory)?;
        Ok(PresentFence {
            fence: Fence::new(&self.payload.device)?,
            uncertain: false,
        })
    }

    pub(super) fn presented(&self, mut fence: PresentFence, uncertain: bool) {
        fence.uncertain = uncertain;
        self.payload.present_fences.lock().push(fence);
    }

    pub(crate) fn wait_presentation(&self) -> Result<(), crate::DeviceError> {
        #[cfg(test)]
        if self
            .group()
            .presentation_wait_error
            .swap(false, Ordering::AcqRel)
        {
            return Err(crate::DeviceError::OutOfMemory);
        }
        let presents = self.payload.present_fences.lock();
        let mut fences = Vec::with_capacity(presents.len() + self.payload.acquire_fences.len());
        for present in presents.iter() {
            if present.uncertain
                && !unsafe { self.payload.device.raw.get_fence_status(present.fence.raw) }
                    .map_err(map_host_device_oom_and_lost_err)?
            {
                return Err(crate::DeviceError::Unexpected);
            }
            fences.push(present.fence.raw);
        }
        fences.extend(
            self.payload
                .acquire_fences
                .iter()
                .filter(|fence| fence.armed.load(Ordering::Acquire))
                .map(|fence| fence.fence.raw),
        );
        if !fences.is_empty() {
            unsafe {
                self.payload
                    .device
                    .raw
                    .wait_for_fences(&fences, true, u64::MAX)
            }
            .map_err(map_host_device_oom_and_lost_err)?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn retirement_confirmed(&self) -> bool {
        self.retired.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn pending_present_count(&self) -> usize {
        self.payload.present_fences.lock().len()
    }

    #[cfg(test)]
    pub(crate) fn wait_present_fences_for_test(&self) -> Result<(), crate::DeviceError> {
        let records = self.payload.present_fences.lock();
        let fences: Vec<_> = records.iter().map(|record| record.fence.raw).collect();
        if fences.is_empty() {
            return Ok(());
        }
        unsafe {
            self.payload
                .device
                .raw
                .wait_for_fences(&fences, true, u64::MAX)
        }
        .map_err(map_host_device_oom_and_lost_err)
    }

    pub(crate) fn mark_retired(&self) {
        self.close();
        let mut registry = self.group().presentations.lock();
        self.retired.store(true, Ordering::Release);
        registry
            .retain(|(weak, retired)| weak.strong_count() != 0 || !retired.load(Ordering::Acquire));
    }

    pub(crate) fn submission_pending(&self) {
        self.submission_uncertain.store(true, Ordering::Release);
        self.retired.store(false, Ordering::Release);
    }

    pub(crate) fn submission_accepted(
        &self,
        progress: &Arc<crate::vulkan::shared::QueueProgress>,
        value: u64,
    ) {
        *self.payload.execution.lock() = Some((progress.clone(), value));
        self.submission_uncertain.store(false, Ordering::Release);
    }

    pub(super) fn retire_closed(&self) -> Result<(), crate::DeviceError> {
        if !self.closed.load(Ordering::Acquire) {
            return Err(crate::DeviceError::Unexpected);
        }
        if self.retired.load(Ordering::Acquire) {
            self.mark_retired();
            return Ok(());
        }
        if self.submission_uncertain.load(Ordering::Acquire) {
            return Err(crate::DeviceError::Unexpected);
        }
        #[cfg(test)]
        if self
            .group()
            .generation_execution_error
            .swap(false, Ordering::AcqRel)
        {
            self.group().close_uncertain();
            return Err(crate::DeviceError::OutOfMemory);
        }
        let execution = self.payload.execution.lock().clone();
        let result = match execution {
            Some((progress, value)) => progress.wait(value, None).and_then(|done| {
                if done {
                    Ok(())
                } else {
                    Err(crate::DeviceError::Unexpected)
                }
            }),
            None => Ok(()),
        }
        .and_then(|()| self.wait_presentation());
        if matches!(result, Ok(()) | Err(crate::DeviceError::Lost)) {
            self.mark_retired();
        } else {
            self.group().close_uncertain();
        }
        result
    }

    pub(crate) fn retire(&self) -> Result<(), crate::DeviceError> {
        {
            let _host = self.group().host[0].lock();
            self.close();
        }
        self.retire_closed()
    }
}

impl Drop for Generation {
    fn drop(&mut self) {
        if !self.retired.load(Ordering::Acquire) {
            self.group()
                .orphaned_presentation
                .store(true, Ordering::Release);
            self.group().close_uncertain();
            return;
        }
        let data = &mut *self.payload;
        unsafe {
            for semaphore in data.acquire_semaphores.drain(..) {
                let semaphore = Arc::into_inner(semaphore)
                    .expect("Acquire lease without its generation")
                    .into_inner();
                semaphore.destroy(&data.device.raw);
            }
            for semaphore in data.present_semaphores.drain(..) {
                let semaphore = Arc::into_inner(semaphore)
                    .expect("Present lease without its generation")
                    .into_inner();
                semaphore.destroy(&data.device.raw);
            }
            data.functor.destroy_swapchain(data.raw, None);
            ManuallyDrop::drop(&mut self.payload);
        }
    }
}
