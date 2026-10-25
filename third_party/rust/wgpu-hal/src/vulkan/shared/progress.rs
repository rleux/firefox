use alloc::sync::Arc;
use ash::vk;
use core::{fmt, time::Duration};
use wgpu_sync::Mutex;

pub(in crate::vulkan) struct QueueProgress {
    pub(super) device: Arc<super::super::DeviceShared>,
    pub(super) semaphore: vk::Semaphore,
    accepted: Mutex<u64>,
}

impl fmt::Debug for QueueProgress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedQueueProgress")
            .field("accepted", &self.accepted())
            .finish()
    }
}

impl QueueProgress {
    pub(in crate::vulkan) fn new(
        device: Arc<super::super::DeviceShared>,
    ) -> Result<Self, crate::DeviceError> {
        let mut kind =
            vk::SemaphoreTypeCreateInfo::default().semaphore_type(vk::SemaphoreType::TIMELINE);
        let info = vk::SemaphoreCreateInfo::default().push_next(&mut kind);
        let semaphore = unsafe { device.raw.create_semaphore(&info, None) }
            .map_err(super::super::map_host_device_oom_err)?;
        Ok(Self {
            device,
            semaphore,
            accepted: Mutex::new(0),
        })
    }

    pub(super) fn accepted(&self) -> u64 {
        *self.accepted.lock()
    }

    pub(super) fn next(&self) -> Result<u64, crate::DeviceError> {
        self.accepted()
            .checked_add(1)
            .ok_or(crate::DeviceError::Unexpected)
    }

    pub(super) fn commit(&self, value: u64) {
        let mut accepted = self.accepted.lock();
        debug_assert!(value > *accepted);
        *accepted = value;
    }

    pub(super) fn wait(
        &self,
        value: u64,
        timeout: Option<Duration>,
    ) -> Result<bool, crate::DeviceError> {
        let timeout = timeout
            .unwrap_or(Duration::MAX)
            .as_nanos()
            .min(u64::MAX as u128) as u64;
        self.device.wait_for_fence(
            &super::super::Fence::TimelineSemaphore(self.semaphore),
            value,
            timeout,
        )
    }
}

impl Drop for QueueProgress {
    fn drop(&mut self) {
        unsafe { self.device.raw.destroy_semaphore(self.semaphore, None) };
    }
}

pub(in crate::vulkan) fn wait_for_idle(
    queue: &super::super::Queue,
) -> Result<(), crate::DeviceError> {
    use core::sync::atomic::Ordering;
    let group = queue.device.shared_queue_group.as_ref().unwrap();
    if group.closed.load(Ordering::Acquire) || group.uncertain.load(Ordering::Acquire) {
        return group.wait_for_idle(&queue.device.raw);
    }
    if queue.device.queue_index == 0 {
        #[cfg(test)]
        group.graphics_idle_waiters.fetch_add(1, Ordering::SeqCst);
        let host = group.host[0].lock();
        #[cfg(test)]
        group.graphics_idle_waiters.fetch_sub(1, Ordering::SeqCst);
        if group.closed.load(Ordering::Acquire) || group.uncertain.load(Ordering::Acquire) {
            drop(host);
            return group.wait_for_idle(&queue.device.raw);
        }
        let result = unsafe { queue.device.raw.queue_wait_idle(queue.raw) }
            .map_err(super::super::map_host_device_oom_and_lost_err);
        if result.is_err() {
            group.close_uncertain();
            return result;
        }
        drop(host);
        if group.closed.load(Ordering::Acquire) || group.uncertain.load(Ordering::Acquire) {
            return group.wait_for_idle(&queue.device.raw);
        }
        return Ok(());
    }
    let value = {
        let _host = group.host[1].lock();
        if group.closed.load(Ordering::Acquire) || group.uncertain.load(Ordering::Acquire) {
            drop(_host);
            return group.wait_for_idle(&queue.device.raw);
        }
        queue.progress.as_ref().unwrap().accepted()
    };
    let waited = queue.progress.as_ref().unwrap().wait(value, None);
    if !matches!(waited, Ok(true)) {
        group.close_uncertain();
        return waited.and_then(|_| Err(crate::DeviceError::Unexpected));
    }
    if group.closed.load(Ordering::Acquire) || group.uncertain.load(Ordering::Acquire) {
        return group.wait_for_idle(&queue.device.raw);
    }
    Ok(())
}
