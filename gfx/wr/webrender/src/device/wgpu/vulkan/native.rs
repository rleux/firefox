/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{hal, wgt, Device};
use ash::vk;
use std::{
    collections::HashMap,
    ops::Deref,
    sync::{Arc, Mutex, OnceLock},
};

#[derive(Default)]
pub(in crate::device::wgpu) struct NativeCache {
    pub(super) capabilities: OnceLock<super::DmaBufCapabilities>,
    pub(super) formats:
        Mutex<HashMap<(wgt::TextureFormat, wgt::TextureUses), Arc<[super::DmaBufFormat]>>>,
    pub(super) modifiers:
        Mutex<HashMap<wgt::TextureFormat, Arc<[vk::DrmFormatModifierPropertiesEXT]>>>,
    pub(super) properties: OnceLock<([u8; 16], [u8; 16], u64)>,
    pub(super) device_lost: std::sync::atomic::AtomicBool,
    pub(super) sync_file: OnceLock<bool>,
    pub(super) drm_node: OnceLock<Option<[u64; 2]>>,
    semaphore_fd: OnceLock<ash::khr::external_semaphore_fd::Device>,
}

impl Device {
    pub(in crate::device::wgpu) fn native_error(
        &self,
        operation: &str,
        error: vk::Result,
    ) -> String {
        let message = format!("{operation}: {error:?}");
        let kind = match error {
            vk::Result::ERROR_DEVICE_LOST => {
                #[cfg(feature = "shared-webgpu")]
                if let Some(context) = &self.shared {
                    context.mark_failed();
                }
                self.native_cache
                    .device_lost
                    .store(true, std::sync::atomic::Ordering::Release);
                self.device.destroy();
                Some(wgt::error::ErrorType::DeviceLost)
            }
            vk::Result::ERROR_OUT_OF_HOST_MEMORY | vk::Result::ERROR_OUT_OF_DEVICE_MEMORY => {
                Some(wgt::error::ErrorType::OutOfMemory)
            }
            _ => None,
        };
        if let Some(kind) = kind {
            self.errors.record(kind, message.clone());
        }
        message
    }
    pub(in crate::device::wgpu) fn native_idle(&self) -> Result<(), String> {
        let _queue = self.queue_access();
        // WR and shared WebGPU clients use this one queue and its HAL mutex.
        let result = {
            let queue = unsafe { self.queue.clone().as_hal::<hal::api::Vulkan>() }
                .expect("Native image retirement requires Vulkan");
            unsafe { hal::Queue::wait_for_idle(&*queue) }
        };
        match result {
            Ok(()) => Ok(()),
            Err(hal::DeviceError::Lost) => {
                self.native_error("Draining native device", vk::Result::ERROR_DEVICE_LOST);
                Ok(())
            }
            Err(error) => {
                let message = format!("Draining native device: {error}");
                if error == hal::DeviceError::OutOfMemory {
                    self.errors
                        .record(wgt::error::ErrorType::OutOfMemory, message.clone());
                }
                Err(message)
            }
        }
    }
    pub(in crate::device::wgpu) fn native_device_lost(&self) -> bool {
        self.native_cache
            .device_lost
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn raw_device(&self) -> impl Deref<Target = hal::vulkan::Device> {
        unsafe { self.device.clone().as_hal::<hal::api::Vulkan>() }.expect("Vulkan device")
    }
    pub(super) fn is_lost(&self) -> bool {
        self.failure().is_some()
    }
    pub(super) fn supports_dma_buf(&self) -> bool {
        let hal = self.raw_device();
        [
            ash::khr::external_memory_fd::NAME,
            ash::ext::external_memory_dma_buf::NAME,
            ash::ext::image_drm_format_modifier::NAME,
            ash::khr::external_semaphore_fd::NAME,
        ]
        .iter()
        .all(|name| hal.enabled_device_extensions().contains(name))
    }
    pub(super) fn external_semaphore_fd(&self) -> &ash::khr::external_semaphore_fd::Device {
        self.native_cache.semaphore_fd.get_or_init(|| {
            let hal = self.raw_device();
            ash::khr::external_semaphore_fd::Device::new(
                hal.shared_instance().raw_instance(),
                hal.raw_device(),
            )
        })
    }
}
