use super::*;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

pub fn export_image(
    owner: Arc<crate::DynOpenDevice>,
    format: wgt::TextureFormat,
    modifier: u64,
    size: [u32; 2],
) -> Result<(Arc<Image>, OwnedFd), String> {
    let device = owner
        .device
        .as_any()
        .downcast_ref::<super::super::Device>()
        .ok_or("DMA-BUF requires Vulkan")?;
    let queue = owner
        .queue
        .as_any()
        .downcast_ref::<super::super::Queue>()
        .ok_or("DMA-BUF requires Vulkan")?;
    if !Arc::ptr_eq(&device.shared, &queue.device)
        || device.shared.queue_index != 0
        || !super::super::renderer_imports::enabled(&device.shared)
    {
        return Err("DMA-BUF export requires the enabled graphics endpoint".into());
    }
    let group = device.shared.shared_queue_group.as_ref();
    let _host = group.map(|g| g.host[0].lock());
    if group.is_some_and(|g| g.closed.load(Ordering::Acquire)) {
        return Err("Shared native context is closed".into());
    }
    let usage =
        wgt::TextureUses::RESOURCE | wgt::TextureUses::COPY_SRC | wgt::TextureUses::COPY_DST;
    let (format_raw, usage_raw, _) = image_parameters(format, usage)?;
    let modifiers = [modifier];
    let mut drm =
        vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&modifiers);
    let mut external = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(format_raw)
        .extent(vk::Extent3D {
            width: size[0],
            height: size[1],
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(usage_raw)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut external)
        .push_next(&mut drm);
    let raw = device.raw_device();
    let identity = super::super::external_sync::properties(device)
        .map_err(|error| format!("Export identity: {error}"))?;
    let (device_uuid, driver_uuid) = (identity.device_uuid, identity.driver_uuid);
    let image = unsafe { raw.create_image(&info, None) }
        .map_err(|error| format!("DMA-BUF test export: {error:?}"))?;
    let mut output = Image {
        owner: owner.clone(),
        image,
        memory: vk::DeviceMemory::null(),
        descriptor: DmaBufImageDescriptor {
            size,
            format,
            usage,
            modifier,
            row_pitch: 0,
            offset: 0,
            device_uuid,
            driver_uuid,
        },
    };
    unsafe {
        let requirements = raw.get_image_memory_requirements(image);
        let properties = device
            .shared_instance()
            .raw_instance()
            .get_physical_device_memory_properties(device.raw_physical_device());
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let mut export = vk::ExportMemoryAllocateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        output.memory = raw
            .allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(requirements.size)
                    .memory_type_index(memory_type(requirements.memory_type_bits, &properties)?)
                    .push_next(&mut dedicated)
                    .push_next(&mut export),
                None,
            )
            .map_err(|error| format!("DMA-BUF test export: {error:?}"))?;
        raw.bind_image_memory(image, output.memory, 0)
            .map_err(|error| format!("DMA-BUF test export: {error:?}"))?;
        let layout = raw.get_image_subresource_layout(
            image,
            vk::ImageSubresource::default().aspect_mask(vk::ImageAspectFlags::MEMORY_PLANE_0_EXT),
        );
        output.descriptor.offset = layout.offset;
        output.descriptor.row_pitch = layout.row_pitch;
        let extension =
            ash::khr::external_memory_fd::Device::new(device.shared_instance().raw_instance(), raw);
        let fd = extension
            .get_memory_fd(
                &vk::MemoryGetFdInfoKHR::default()
                    .memory(output.memory)
                    .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT),
            )
            .map_err(|error| format!("DMA-BUF test export: {error:?}"))?;
        Ok((Arc::new(output), OwnedFd::from_raw_fd(fd)))
    }
}

/// # Safety
/// The image must be a test export owned exclusively by this producer; reacquire
/// requires the external consumer's completed release synchronization.
pub unsafe fn publish(
    image: &Image,
    encoder: &mut dyn crate::DynCommandEncoder,
    color: [f32; 4],
    reacquire_external: bool,
) -> Result<(), String> {
    image.validate_access(encoder, ExternalDomain::External)?;
    let encoder = encoder
        .as_any_mut()
        .downcast_mut::<super::super::CommandEncoder>()
        .unwrap();
    let family = image.device().shared.family_index;
    unsafe {
        image.barrier(
            encoder,
            if reacquire_external {
                vk::ImageLayout::GENERAL
            } else {
                vk::ImageLayout::UNDEFINED
            },
            vk::ImageLayout::GENERAL,
            if reacquire_external {
                vk::QUEUE_FAMILY_EXTERNAL
            } else {
                vk::QUEUE_FAMILY_IGNORED
            },
            if reacquire_external {
                family
            } else {
                vk::QUEUE_FAMILY_IGNORED
            },
            vk::AccessFlags::empty(),
            vk::AccessFlags::TRANSFER_WRITE,
        );
        image.device().shared.raw.cmd_clear_color_image(
            encoder.active,
            image.image,
            vk::ImageLayout::GENERAL,
            &vk::ClearColorValue { float32: color },
            &[vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1)],
        );
        image.barrier(
            encoder,
            vk::ImageLayout::GENERAL,
            vk::ImageLayout::GENERAL,
            family,
            vk::QUEUE_FAMILY_EXTERNAL,
            vk::AccessFlags::TRANSFER_WRITE,
            vk::AccessFlags::empty(),
        );
    }
    Ok(())
}

#[repr(C)]
struct SyncobjCreate {
    handle: u32,
    flags: u32,
}

#[repr(C)]
struct SyncobjExport {
    handle: u32,
    flags: u32,
    fd: i32,
    pad: u32,
}

pub fn signaled_sync_file(owner: Arc<crate::DynOpenDevice>) -> Result<OwnedFd, String> {
    let device = owner.device.as_any().downcast_ref::<super::super::Device>()
        .ok_or("SYNC_FD requires Vulkan")?;
    if device.shared.shared_queue_group.is_some() {
        return Err("SYNC_FD fixture requires an isolated legacy endpoint".into());
    }
    let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
    unsafe {
        device.shared_instance().raw_instance().get_physical_device_properties2(
            device.raw_physical_device(),
            &mut vk::PhysicalDeviceProperties2::default().push_next(&mut drm),
        );
    }
    if drm.has_render != vk::TRUE || drm.render_major < 0 || drm.render_minor < 0 {
        return Err("Real sync-file fixtures require a DRM render node".into());
    }
    let info = std::fs::read_to_string(format!(
        "/sys/dev/char/{}:{}/uevent", drm.render_major, drm.render_minor,
    )).map_err(|error| format!("Reading DRM render node: {error}"))?;
    let node = info.lines().find_map(|line| line.strip_prefix("DEVNAME="))
        .ok_or("DRM render node has no device name")?;
    // Closing this private DRM file releases its syncobj handles.
    let file = std::fs::OpenOptions::new().read(true).write(true)
        .open(format!("/dev/{node}"))
        .map_err(|error| format!("Opening DRM render node: {error}"))?;
    let mut create = SyncobjCreate { handle: 0, flags: 1 };
    if unsafe {
        libc::ioctl(file.as_raw_fd(), libc::_IOWR::<SyncobjCreate>(b'd'.into(), 0xbf), &mut create)
    } < 0 {
        return Err(format!("Creating signaled DRM syncobj requires SYNCOBJ support: {}",
            std::io::Error::last_os_error()));
    }
    let mut export = SyncobjExport { handle: create.handle, flags: 1, fd: -1, pad: 0 };
    if unsafe {
        libc::ioctl(file.as_raw_fd(), libc::_IOWR::<SyncobjExport>(b'd'.into(), 0xc1), &mut export)
    } < 0 {
        return Err(format!("Exporting signaled DRM sync_file: {}",
            std::io::Error::last_os_error()));
    }
    if export.fd < 0 {
        return Err("DRM syncobj export did not return a real descriptor".into());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(export.fd) })
}

struct SyncSignal {
    owner: Arc<crate::DynOpenDevice>,
    semaphore: vk::Semaphore,
    fence: Option<super::super::Fence>,
}
impl Drop for SyncSignal {
    fn drop(&mut self) {
        use crate::Device as _;
        let device = self
            .owner
            .device
            .as_any()
            .downcast_ref::<super::super::Device>()
            .unwrap();
        unsafe {
            if let Some(fence) = self.fence.take() {
                device.destroy_fence(fence);
            }
            device.shared.raw.destroy_semaphore(self.semaphore, None);
        }
    }
}

/// Signal and retire an isolated legacy endpoint, returning a real sync_file.
pub fn signal_sync_file(owner: Arc<crate::DynOpenDevice>) -> Result<OwnedFd, String> {
    use crate::Device as _;
    let device = owner
        .device
        .as_any()
        .downcast_ref::<super::super::Device>()
        .ok_or("SYNC_FD requires Vulkan")?;
    let queue = owner
        .queue
        .as_any()
        .downcast_ref::<super::super::Queue>()
        .ok_or("SYNC_FD requires Vulkan")?;
    if !Arc::ptr_eq(&device.shared, &queue.device) || device.shared.shared_queue_group.is_some() {
        return Err("SYNC_FD test signal requires its legacy endpoint".into());
    }
    if !device
        .shared
        .enabled_extensions
        .contains(&ash::khr::external_semaphore_fd::NAME)
    {
        return Err("SYNC_FD export is unavailable".into());
    }
    let mut properties = vk::ExternalSemaphoreProperties::default();
    unsafe {
        device
            .shared
            .instance
            .raw
            .get_physical_device_external_semaphore_properties(
                device.shared.physical_device,
                &vk::PhysicalDeviceExternalSemaphoreInfo::default()
                    .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD),
                &mut properties,
            );
    }
    if !properties
        .external_semaphore_features
        .contains(vk::ExternalSemaphoreFeatureFlags::EXPORTABLE)
    {
        return Err("SYNC_FD export is unavailable".into());
    }
    let mut export = vk::ExportSemaphoreCreateInfo::default()
        .handle_types(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
    let semaphore = unsafe {
        device.shared.raw.create_semaphore(
            &vk::SemaphoreCreateInfo::default().push_next(&mut export),
            None,
        )
    }
    .map_err(|error| format!("Creating SYNC_FD signal: {error:?}"))?;
    let mut signal = SyncSignal {
        owner: owner.clone(),
        semaphore,
        fence: None,
    };
    signal.fence = Some(
        unsafe { device.create_fence() }
            .map_err(|error| format!("Creating SYNC_FD completion: {error:?}"))?,
    );
    let fence = signal.fence.as_ref().unwrap();
    let completion = match fence {
        super::super::Fence::TimelineSemaphore(raw) => Some(
            super::super::semaphore_list::SemaphoreType::Timeline(*raw, 1),
        ),
        super::super::Fence::FencePool(_) => None,
    };
    let terminal = if completion.is_some() {
        None
    } else {
        Some((fence, 1))
    };
    let submitted = unsafe {
        queue.submit_impl(
            &[],
            &[],
            terminal,
            &[],
            &[super::super::semaphore_list::SemaphoreType::Binary(
                semaphore,
            )],
            completion.as_slice(),
        )
    };
    if let Err(error) = submitted {
        core::mem::forget(signal);
        return Err(format!("Submitting SYNC_FD signal: {error:?}"));
    }
    let exported = unsafe {
        ash::khr::external_semaphore_fd::Device::new(
            &device.shared.instance.raw,
            &device.shared.raw,
        )
        .get_semaphore_fd(
            &vk::SemaphoreGetFdInfoKHR::default()
                .semaphore(semaphore)
                .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD),
        )
    }
    .map_err(|error| format!("Exporting SYNC_FD signal: {error:?}"))
    .and_then(|fd| {
        if fd == -1 {
            Ok(None)
        } else if fd < 0 {
            Err("SYNC_FD export returned an invalid negative descriptor".into())
        } else {
            Ok(Some(unsafe { OwnedFd::from_raw_fd(fd) }))
        }
    });
    match unsafe {
        device.wait(
            signal.fence.as_ref().unwrap(),
            1,
            Some(core::time::Duration::from_secs(5)),
        )
    } {
        Ok(true) => drop(signal),
        result => {
            core::mem::forget(signal);
            return Err(format!("SYNC_FD signal retirement uncertain: {result:?}"));
        }
    }
    exported?.map_or_else(|| signaled_sync_file(owner), Ok)
}
