use super::*;
#[test]
fn dma_buf_format_and_usage_are_explicit() {
    for (format, raw) in [
        (wgt::TextureFormat::Rgba8Unorm, vk::Format::R8G8B8A8_UNORM),
        (wgt::TextureFormat::Bgra8Unorm, vk::Format::B8G8R8A8_UNORM),
    ] {
        assert_eq!(
            image_parameters(format, wgt::TextureUses::RESOURCE)
                .unwrap()
                .0,
            raw
        );
    }
    assert!(image_parameters(wgt::TextureFormat::R8Unorm, wgt::TextureUses::RESOURCE).is_err());
    assert!(image_parameters(
        wgt::TextureFormat::Rgba8UnormSrgb,
        wgt::TextureUses::RESOURCE
    )
    .is_err());
    for usage in [
        wgt::TextureUses::empty(),
        wgt::TextureUses::COPY_SRC,
        wgt::TextureUses::RESOURCE | wgt::TextureUses::STORAGE_READ_WRITE,
    ] {
        assert!(image_parameters(wgt::TextureFormat::Rgba8Unorm, usage).is_err());
    }
    let (_, usage, required) = image_parameters(
        wgt::TextureFormat::Rgba8Unorm,
        wgt::TextureUses::RESOURCE
            | wgt::TextureUses::COPY_SRC
            | wgt::TextureUses::COPY_DST
            | wgt::TextureUses::COLOR_TARGET,
    )
    .unwrap();
    assert_eq!(
        usage,
        vk::ImageUsageFlags::SAMPLED
            | vk::ImageUsageFlags::TRANSFER_SRC
            | vk::ImageUsageFlags::TRANSFER_DST
            | vk::ImageUsageFlags::COLOR_ATTACHMENT
    );
    assert!(required.contains(
        vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
            | vk::FormatFeatureFlags::COLOR_ATTACHMENT
    ));
}

#[test]
fn dma_buf_modifiers_require_one_memory_plane_and_filtering() {
    let (_, _, required) =
        image_parameters(wgt::TextureFormat::Rgba8Unorm, wgt::TextureUses::RESOURCE).unwrap();
    let mut entry = vk::DrmFormatModifierPropertiesEXT::default()
        .drm_format_modifier_plane_count(1)
        .drm_format_modifier_tiling_features(required);
    assert!(eligible_modifier(&entry, required));
    for count in [0, 2, 3, 4] {
        entry.drm_format_modifier_plane_count = count;
        assert!(!eligible_modifier(&entry, required));
    }
    entry.drm_format_modifier_plane_count = 1;
    entry.drm_format_modifier_tiling_features = vk::FormatFeatureFlags::SAMPLED_IMAGE;
    assert!(!eligible_modifier(&entry, required));
}

#[test]
fn dma_buf_size_checks_both_dimensions() {
    let format = DmaBufFormat {
        format: wgt::TextureFormat::Rgba8Unorm,
        usage: wgt::TextureUses::RESOURCE,
        modifier: 0,
        max_size: [32, 16],
        max_resource_size: 4096,
        exportable: true,
        dedicated_only: false,
    };
    for size in [[1, 1], [32, 16]] {
        assert!(format.supports_extent(size));
    }
    for size in [[0, 1], [1, 0], [33, 1], [1, 17], [u32::MAX, u32::MAX]] {
        assert!(!format.supports_extent(size));
    }
}

#[test]
fn dma_buf_memory_types_intersect_and_exclude_protected_memory() {
    let mut properties = vk::PhysicalDeviceMemoryProperties::default();
    properties.memory_type_count = 3;
    properties.memory_types[0].property_flags = vk::MemoryPropertyFlags::PROTECTED;
    properties.memory_types[1].property_flags = vk::MemoryPropertyFlags::DEVICE_LOCAL;
    assert_eq!(memory_type(0b111 & 0b011, &properties).unwrap(), 1);
    assert!(memory_type(1, &properties).is_err());
    assert!(memory_type(0b100 & 0b011, &properties).is_err());
}

fn test_capabilities(
    shared: bool,
) -> (
    Arc<Capabilities>,
    super::super::Instance,
    Option<Arc<dyn crate::shared::SharedDeviceContext>>,
) {
    use crate::Instance as _;
    let instance = unsafe {
        super::super::Instance::init(&crate::InstanceDescriptor {
            name: "DMA-BUF HAL test",
            flags: wgt::InstanceFlags::DEBUG | wgt::InstanceFlags::VALIDATION,
            memory_budget_thresholds: Default::default(),
            backend_options: Default::default(),
            telemetry: None,
            display: None,
        })
    }
    .unwrap();
    let adapter = unsafe { instance.enumerate_adapters(None) }.pop().unwrap();
    let (owner, context) = if shared {
        let pair = unsafe {
            crate::shared::open_shared_device(
                &adapter.adapter,
                wgt::Features::empty(),
                &adapter.capabilities.limits,
                &wgt::MemoryHints::default(),
            )
        }
        .unwrap();
        drop(pair.compute);
        (Arc::new(pair.graphics), Some(pair.context))
    } else {
        let (open, _) = unsafe {
            super::super::renderer_imports::open_adapter(
                &adapter.adapter,
                wgt::Features::empty(),
                &adapter.capabilities.limits,
                &wgt::MemoryHints::default(),
            )
        }
        .unwrap();
        (Arc::new(open), None)
    };

    let reported =
        crate::renderer::graphics_capabilities(&instance, &adapter.adapter, &*owner.device)
            .unwrap();
    let device = owner
        .device
        .as_any()
        .downcast_ref::<super::super::Device>()
        .unwrap();
    assert_eq!(reported.features, device.shared.features);
    assert!(crate::dma_buf::supports_imports(&*owner.device));
    for extension in [
        ash::khr::external_memory_fd::NAME,
        ash::ext::external_memory_dma_buf::NAME,
        ash::ext::image_drm_format_modifier::NAME,
        ash::khr::external_semaphore_fd::NAME,
    ] {
        assert!(device.shared.enabled_extensions.contains(&extension));
    }
    if adapter
        .adapter
        .physical_device_capabilities()
        .supports_extension(ash::ext::queue_family_foreign::NAME)
    {
        assert!(device
            .shared
            .enabled_extensions
            .contains(&ash::ext::queue_family_foreign::NAME));
    }
    let caps = query_capabilities(
        owner,
        &adapter.adapter,
        adapter.capabilities.limits.max_texture_dimension_2d,
    )
    .unwrap();
    (caps, instance, context)
}
#[test]
#[ignore = "Requires Vulkan DMA-BUF modifiers, two graphics queues and validation"]
fn dma_buf_queries_match_native_image_creation() {
    let mut created_images = 0;
    assert!(cfg!(all(feature = "validation_canary", debug_assertions)));
    for shared in [false, true] {
        let (caps, _instance, _context) = test_capabilities(shared);
        let before = created_images;
        let full_usage = wgt::TextureUses::RESOURCE
            | wgt::TextureUses::COPY_SRC
            | wgt::TextureUses::COPY_DST
            | wgt::TextureUses::COLOR_TARGET;
        for format in [
            wgt::TextureFormat::Rgba8Unorm,
            wgt::TextureFormat::Bgra8Unorm,
        ] {
            for usage in [wgt::TextureUses::RESOURCE, full_usage] {
                let formats = caps.formats_for(format, usage).unwrap();
                let (raw_format, raw_usage, _) = image_parameters(format, usage).unwrap();
                for supported in formats {
                    assert!(supported.supports_extent([17, 13]));
                    assert_eq!(supported.format(), format);
                    assert_eq!(supported.usage(), usage);
                    let modifiers = [supported.modifier()];
                    let mut modifier = vk::ImageDrmFormatModifierListCreateInfoEXT::default()
                        .drm_format_modifiers(&modifiers);
                    let mut external = vk::ExternalMemoryImageCreateInfo::default()
                        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
                    let info = vk::ImageCreateInfo::default()
                        .image_type(vk::ImageType::TYPE_2D)
                        .format(raw_format)
                        .extent(vk::Extent3D {
                            width: 17,
                            height: 13,
                            depth: 1,
                        })
                        .mip_levels(1)
                        .array_layers(1)
                        .samples(vk::SampleCountFlags::TYPE_1)
                        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                        .usage(raw_usage)
                        .sharing_mode(vk::SharingMode::EXCLUSIVE)
                        .initial_layout(vk::ImageLayout::UNDEFINED)
                        .push_next(&mut external)
                        .push_next(&mut modifier);
                    unsafe {
                        let raw = caps.raw_device().raw_device();
                        let image = raw.create_image(&info, None).unwrap();
                        let memory = raw.get_image_memory_requirements(image);
                        raw.destroy_image(image, None);
                        assert!(memory.size > 0 && memory.size <= supported.max_resource_size());
                        created_images += 1;
                    }
                }
            }
        }
        assert!(
            created_images > before,
            "No supported DMA-BUF images for shared={shared}"
        );
    }
    assert!(
        created_images > 0,
        "No supported DMA-BUF images were exercised"
    );

    #[cfg(feature = "validation_canary")]
    {
        let errors = crate::VALIDATION_CANARY.get_and_reset();
        assert!(errors.is_empty(), "{errors:?}");
    }
}
