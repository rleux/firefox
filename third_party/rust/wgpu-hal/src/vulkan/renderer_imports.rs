use alloc::{boxed::Box, vec::Vec};
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

fn eligible(adapter: &super::Adapter) -> bool {
    let caps = adapter.physical_device_capabilities();
    let memory_supported = [
        ash::khr::external_memory_fd::NAME,
        ash::ext::external_memory_dma_buf::NAME,
        ash::ext::image_drm_format_modifier::NAME,
    ]
    .iter()
    .all(|name| caps.supports_extension(name));
    let instance = adapter.shared_instance();
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
                adapter.raw_physical_device(),
                &mut vk::PhysicalDeviceFeatures2::default().push_next(&mut timeline_features),
            );
        }
        if timeline_features.timeline_semaphore == vk::TRUE {
            let mut timeline =
                vk::SemaphoreTypeCreateInfo::default().semaphore_type(vk::SemaphoreType::TIMELINE);
            unsafe {
                instance
                    .raw_instance()
                    .get_physical_device_external_semaphore_properties(
                        adapter.raw_physical_device(),
                        &vk::PhysicalDeviceExternalSemaphoreInfo::default()
                            .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD)
                            .push_next(&mut timeline),
                        &mut semaphore,
                    );
            }
        }
    }
    supports_dma_buf(
        memory_supported,
        semaphore.external_semaphore_features,
    )
}

pub(super) fn enabled(device: &super::DeviceShared) -> bool {
    let memory_supported = [
        ash::khr::external_memory_fd::NAME,
        ash::ext::external_memory_dma_buf::NAME,
        ash::ext::image_drm_format_modifier::NAME,
    ]
    .iter()
    .all(|name| device.enabled_extensions.contains(name));
    memory_supported
        && device.instance.instance_api_version >= vk::API_VERSION_1_1
        && device.private_caps.timeline_semaphores
        && unsafe {
            device.instance.raw.get_physical_device_properties(device.physical_device)
        }.api_version >= vk::API_VERSION_1_1
        && super::external_sync::fd_capability(
            device,
            vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD,
            true,
            vk::ExternalSemaphoreFeatureFlags::IMPORTABLE
                | vk::ExternalSemaphoreFeatureFlags::EXPORTABLE,
        )
}

pub(crate) fn configure(
    adapter: &super::Adapter,
    features: wgt::Features,
    extensions: &mut Vec<&'static core::ffi::CStr>,
) -> wgt::Features {
    let supported = eligible(adapter);
    if supported {
        for extension in [
            ash::khr::external_semaphore_fd::NAME,
            ash::ext::queue_family_foreign::NAME,
        ] {
            if adapter
                .physical_device_capabilities()
                .supports_extension(extension)
                && !extensions.contains(&extension)
            {
                extensions.push(extension);
            }
        }
    }
    features
}
pub(crate) unsafe fn open_adapter(
    adapter: &super::Adapter,
    features: wgt::Features,
    limits: &wgt::Limits,
    hints: &wgt::MemoryHints,
) -> Result<(crate::DynOpenDevice, wgt::Features), crate::DeviceError> {
    let mut extra = Vec::new();
    let features = configure(adapter, features, &mut extra);
    let callback: Box<super::CreateDeviceCallback<'_>> = Box::new(move |args| {
        for extension in &extra {
            if !args.extensions.contains(extension) {
                args.extensions.push(extension);
            }
        }
    });
    let open = unsafe { adapter.open_with_callback(features, limits, hints, Some(callback)) }?;
    Ok((open.into(), features))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dma_buf_requires_bidirectional_timeline_support() {
        let import = vk::ExternalSemaphoreFeatureFlags::IMPORTABLE;
        let export = vk::ExternalSemaphoreFeatureFlags::EXPORTABLE;
        assert!(!supports_dma_buf(
            true,
            vk::ExternalSemaphoreFeatureFlags::empty()
        ));
        assert!(!supports_dma_buf(true, import));
        assert!(!supports_dma_buf(true, export));
        assert!(!supports_dma_buf(false, import | export));
        assert!(supports_dma_buf(true, import | export));
    }

}
