/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{hal, wgt};
use ash::vk;

fn supports_dma_buf(
    memory_supported: bool,
    semaphore_features: vk::ExternalSemaphoreFeatureFlags,
) -> bool {
    memory_supported
        && semaphore_features.contains(
            vk::ExternalSemaphoreFeatureFlags::IMPORTABLE
                | vk::ExternalSemaphoreFeatureFlags::EXPORTABLE,
        )
}

impl super::Device {
    pub(super) fn supports_dma_buf(&self) -> bool {
        let extensions = self.raw_device().enabled_device_extensions();
        [
            ash::khr::external_memory_fd::NAME,
            ash::ext::external_memory_dma_buf::NAME,
            ash::ext::image_drm_format_modifier::NAME,
            ash::khr::external_semaphore_fd::NAME,
        ].iter().all(|extension| extensions.contains(extension))
    }
}

pub(super) fn open_adapter(
    adapter: &hal::ExposedAdapter<hal::api::Vulkan>,
    features: wgt::Features,
) -> Result<(hal::OpenDevice<hal::api::Vulkan>, wgt::Features), hal::DeviceError> {
    let caps = adapter.adapter.physical_device_capabilities();
    let instance = adapter.adapter.shared_instance();
    let memory_supported = [
        ash::khr::external_memory_fd::NAME,
        ash::ext::external_memory_dma_buf::NAME,
        ash::ext::image_drm_format_modifier::NAME,
    ].iter().all(|extension| caps.supports_extension(extension));
    let mut semaphore = vk::ExternalSemaphoreProperties::default();
    if instance.instance_api_version() >= vk::API_VERSION_1_1
        && caps.properties().api_version >= vk::API_VERSION_1_1
        && caps.supports_extension(ash::khr::external_semaphore_fd::NAME)
        && (caps.properties().api_version >= vk::API_VERSION_1_2
            || caps.supports_extension(ash::khr::timeline_semaphore::NAME))
        && memory_supported
    {
        let mut timeline_features = vk::PhysicalDeviceTimelineSemaphoreFeatures::default();
        unsafe {
            instance.raw_instance().get_physical_device_features2(
                adapter.adapter.raw_physical_device(),
                &mut vk::PhysicalDeviceFeatures2::default().push_next(&mut timeline_features),
            );
        }
        if timeline_features.timeline_semaphore == vk::TRUE {
            let mut timeline = vk::SemaphoreTypeCreateInfo::default()
                .semaphore_type(vk::SemaphoreType::TIMELINE);
            unsafe {
                instance.raw_instance().get_physical_device_external_semaphore_properties(
                    adapter.adapter.raw_physical_device(),
                    &vk::PhysicalDeviceExternalSemaphoreInfo::default()
                        .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD)
                        .push_next(&mut timeline),
                    &mut semaphore,
                );
            }
        }
    }
    let supported = supports_dma_buf(memory_supported, semaphore.external_semaphore_features);
    let callback: Box<hal::vulkan::CreateDeviceCallback<'_>> = Box::new(|args| {
        if supported {
            for extension in [
                ash::khr::external_semaphore_fd::NAME,
                ash::ext::queue_family_foreign::NAME,
            ] {
                if caps.supports_extension(extension) && !args.extensions.contains(&extension) {
                    args.extensions.push(extension);
                }
            }
        }
    });
    let open = unsafe {
        adapter.adapter.open_with_callback(
            features,
            &adapter.capabilities.limits,
            &wgt::MemoryHints::default(),
            Some(callback),
        )
    }?;
    Ok((open, features))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dma_buf_requires_bidirectional_timeline_support() {
        let import = vk::ExternalSemaphoreFeatureFlags::IMPORTABLE;
        let export = vk::ExternalSemaphoreFeatureFlags::EXPORTABLE;
        assert!(!supports_dma_buf(true, vk::ExternalSemaphoreFeatureFlags::empty()));
        assert!(!supports_dma_buf(true, import));
        assert!(!supports_dma_buf(true, export));
        assert!(!supports_dma_buf(false, import | export));
        assert!(supports_dma_buf(true, import | export));
    }

    #[test]
    #[ignore = "Requires Vulkan DMA-BUF and external timeline support"]
    fn dma_buf_device_enables_memory_and_sync_extensions() {
        let device = super::super::Device::new(&super::super::Options {
            validation: true,
            ..Default::default()
        }).unwrap();
        assert!(device.supports_dma_buf());
        let extensions = device.raw_device().enabled_device_extensions();
        for required in [
            ash::khr::external_memory_fd::NAME,
            ash::ext::external_memory_dma_buf::NAME,
            ash::ext::image_drm_format_modifier::NAME,
            ash::khr::external_semaphore_fd::NAME,
        ] {
            assert!(extensions.contains(&required), "Missing {:?}", required);
        }
        if device.raw_adapter().physical_device_capabilities()
            .supports_extension(ash::ext::queue_family_foreign::NAME)
        {
            assert!(extensions.contains(&ash::ext::queue_family_foreign::NAME));
        }
    }
}
