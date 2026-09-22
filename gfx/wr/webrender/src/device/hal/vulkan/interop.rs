/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use std::rc::Rc;

#[derive(Clone)]
pub struct VulkanQueueCoordinator(Arc<std::sync::Mutex<()>>);
impl VulkanQueueCoordinator {
    pub fn lock(&self) -> Result<std::sync::MutexGuard<'_, ()>> {
        self.0
            .lock()
            .map_err(|_| "HAL queue coordinator is poisoned".into())
    }
}

pub fn create_vulkan_image_device(options: &Options) -> Result<ExternalImageDevice> {
    Ok(ExternalImageDevice::new(&Rc::new(create_vulkan_device(
        options,
    )?)))
}

impl ExternalImageDevice {
    pub(super) fn vulkan_producer(&self) -> Result<&Producer<hal::api::Vulkan>> {
        let producer = self
            .0
            .as_any()
            .downcast_ref::<Producer<hal::api::Vulkan>>()
            .ok_or("Native sharing requires Vulkan")?;
        producer.ensure_healthy()?;
        Ok(producer)
    }
    pub fn vulkan_queue_coordinator(&self) -> Result<VulkanQueueCoordinator> {
        Ok(VulkanQueueCoordinator(
            self.vulkan_producer()?.owner.queue_gate.clone(),
        ))
    }

    /// Coordinates raw submissions with WR. Do not reenter WR or leave staged semaphore hooks.
    /// # Safety
    /// Raw work must obey Vulkan/HAL resource and queue synchronization contracts.
    pub unsafe fn with_vulkan_queue<T>(
        &self,
        operation: impl FnOnce(VulkanDeviceContext<'_>) -> T,
    ) -> Result<T> {
        let producer = self.vulkan_producer()?;
        let _guard = producer.owner.lock_queue()?;
        Ok(operation(self.vulkan_context().unwrap()))
    }
}
