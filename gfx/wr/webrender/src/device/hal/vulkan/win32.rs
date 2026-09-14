/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::super::{
    resources::{Owned, Texture},
    submission::{Submission, SubmissionSync},
};
use super::win32_layout::{plane_format, Win32ImageLayout};
use super::*;
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
use std::rc::Rc;
type V = hal::api::Vulkan;

pub struct Win32Image {
    handle: OwnedHandle,
    layout: Win32ImageLayout,
}
impl Win32Image {
    /// # Safety
    /// This must be an OPAQUE_WIN32 allocation exported from Vulkan with the declared metadata,
    /// dedicated to one optimal-tiled 2D, single-mip/layer/sample image with TRANSFER_SRC | TRANSFER_DST usage, exclusive sharing and no image creation flags.
    pub unsafe fn from_handle(handle: OwnedHandle, layout: Win32ImageLayout) -> Self {
        Self { handle, layout }
    }
    pub fn as_handle(&self) -> BorrowedHandle<'_> {
        self.handle.as_handle()
    }
    pub fn layout(&self) -> Win32ImageLayout {
        self.layout
    }
    pub fn into_parts(self) -> (OwnedHandle, Win32ImageLayout) {
        (self.handle, self.layout)
    }
}

pub struct Win32Semaphore {
    handle: OwnedHandle,
    device_uuid: [u8; 16],
    driver_uuid: [u8; 16],
}
impl Win32Semaphore {
    /// # Safety
    /// The handle must reference a Vulkan binary semaphore exported as OPAQUE_WIN32 with no flags.
    /// Its signal must already be submitted, and exactly one consumer may wait on this payload.
    pub unsafe fn from_handle(
        handle: OwnedHandle,
        device_uuid: [u8; 16],
        driver_uuid: [u8; 16],
    ) -> Self {
        Self {
            handle,
            device_uuid,
            driver_uuid,
        }
    }
    pub fn as_handle(&self) -> BorrowedHandle<'_> {
        self.handle.as_handle()
    }
    pub fn into_parts(self) -> (OwnedHandle, [u8; 16], [u8; 16]) {
        (self.handle, self.device_uuid, self.driver_uuid)
    }
}

pub struct Win32Export {
    image: Win32Image,
    ready: Win32Semaphore,
}
impl Win32Export {
    pub fn image(&self) -> &Win32Image {
        &self.image
    }
    pub fn into_parts(self) -> (Win32Image, Win32Semaphore) {
        (self.image, self.ready)
    }
}

pub struct Win32Copy {
    image: ExternalNativeImage,
    release: Win32Semaphore,
}
impl Win32Copy {
    pub fn image(&self) -> &ExternalNativeImage {
        &self.image
    }
    pub fn into_parts(self) -> (ExternalNativeImage, Win32Semaphore) {
        (self.image, self.release)
    }
}

pub(super) fn open_adapter(
    adapter: &hal::ExposedAdapter<V>,
    mut features: wgt::Features,
) -> Result<(hal::OpenDevice<V>, wgt::Features)> {
    let mut semaphore = vk::ExternalSemaphoreProperties::default();
    unsafe {
        adapter
            .adapter
            .shared_instance()
            .raw_instance()
            .get_physical_device_external_semaphore_properties(
                adapter.adapter.raw_physical_device(),
                &vk::PhysicalDeviceExternalSemaphoreInfo::default()
                    .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_WIN32),
                &mut semaphore,
            );
    }
    let supported = adapter
        .features
        .contains(wgt::Features::VULKAN_EXTERNAL_MEMORY_WIN32)
        && adapter
            .adapter
            .physical_device_capabilities()
            .supports_extension(ash::khr::external_semaphore_win32::NAME)
        && semaphore.external_semaphore_features.contains(
            vk::ExternalSemaphoreFeatureFlags::IMPORTABLE
                | vk::ExternalSemaphoreFeatureFlags::EXPORTABLE,
        );
    if supported {
        features |= wgt::Features::VULKAN_EXTERNAL_MEMORY_WIN32;
    }
    let callback: Option<Box<hal::vulkan::CreateDeviceCallback<'_>>> = if supported {
        Some(Box::new(|args| {
            if !args
                .extensions
                .contains(&ash::khr::external_semaphore_win32::NAME)
            {
                args.extensions
                    .push(ash::khr::external_semaphore_win32::NAME);
            }
        }))
    } else {
        None
    };
    let open = unsafe {
        adapter.adapter.open_with_callback(
            features,
            &adapter.capabilities.limits,
            &wgt::MemoryHints::default(),
            callback,
        )
    }
    .map_err(|error| format!("Opening Win32-capable Vulkan device: {error:?}"))?;
    Ok((open, features))
}

fn supported(owner: &Device<V>) -> bool {
    owner
        .features
        .contains(wgt::Features::VULKAN_EXTERNAL_MEMORY_WIN32)
        && owner
            .open
            .device
            .enabled_device_extensions()
            .contains(&ash::khr::external_semaphore_win32::NAME)
}

fn ids(owner: &Device<V>) -> ([u8; 16], [u8; 16]) {
    let mut ids = vk::PhysicalDeviceIDProperties::default();
    unsafe {
        owner
            .open
            .device
            .shared_instance()
            .raw_instance()
            .get_physical_device_properties2(
                owner.open.device.raw_physical_device(),
                &mut vk::PhysicalDeviceProperties2::default().push_next(&mut ids),
            );
    }
    (ids.device_uuid, ids.driver_uuid)
}

fn limits(owner: &Device<V>, format: wgt::TextureFormat) -> Result<vk::ImageFormatProperties> {
    if !supported(owner) {
        return Err("Vulkan opaque Win32 memory/semaphore sharing is unavailable".into());
    }
    let required = hal::TextureFormatCapabilities::SAMPLED
        | hal::TextureFormatCapabilities::SAMPLED_LINEAR
        | hal::TextureFormatCapabilities::COPY_SRC
        | hal::TextureFormatCapabilities::COPY_DST;
    if !owner.features.contains(format.required_features())
        || !owner
            .formats
            .iter()
            .any(|(candidate, caps)| *candidate == format && caps.contains(required))
    {
        return Err("Win32 shared format cannot be materialized as a WR sampling texture".into());
    }
    let mut external = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::OPAQUE_WIN32);
    let info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(owner.adapter.texture_format_as_raw(format))
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST)
        .push_next(&mut external);
    let mut external_limits = vk::ExternalImageFormatProperties::default();
    let mut properties = vk::ImageFormatProperties2::default().push_next(&mut external_limits);
    unsafe {
        owner
            .open
            .device
            .shared_instance()
            .raw_instance()
            .get_physical_device_image_format_properties2(
                owner.open.device.raw_physical_device(),
                &info,
                &mut properties,
            )
    }
    .map_err(|error| format!("Querying opaque Win32 image format: {error:?}"))?;
    let properties = properties.image_format_properties;
    if !external_limits
        .external_memory_properties
        .external_memory_features
        .contains(
            vk::ExternalMemoryFeatureFlags::IMPORTABLE | vk::ExternalMemoryFeatureFlags::EXPORTABLE,
        )
    {
        return Err("Win32 image format cannot be imported and exported".into());
    }
    Ok(properties)
}

fn check_image(owner: &Device<V>, descriptor: api::ImageDescriptor) -> Result<()> {
    validate_descriptor(descriptor)?;
    let format = plane_format(descriptor.format)?;
    let limits = limits(owner, format)?;
    let width = descriptor.size.width as u32;
    let height = descriptor.size.height as u32;
    owner.layout(width, height)?;
    if descriptor
        .flags
        .contains(api::ImageDescriptorFlags::ALLOW_MIPMAPS)
        || width > limits.max_extent.width
        || height > limits.max_extent.height
        || limits.max_mip_levels == 0
        || limits.max_array_layers == 0
        || !limits.sample_counts.contains(vk::SampleCountFlags::TYPE_1)
        || u64::from(width)
            * u64::from(height)
            * super::super::resources::bytes_per_pixel(format) as u64
            > limits.max_resource_size
    {
        return Err("Win32 image extent/mips exceed external format limits".into());
    }
    Ok(())
}

unsafe fn owned_handle(value: vk::HANDLE) -> Result<OwnedHandle> {
    if value == 0 || value == -1 {
        return Err("Vulkan returned an invalid Win32 NT handle".into());
    }
    Ok(OwnedHandle::from_raw_handle(value as *mut std::ffi::c_void))
}

struct TransferSync {
    wait: Option<Owned<V, vk::Semaphore>>,
    signal: Option<Owned<V, vk::Semaphore>>,
}
impl SubmissionSync<V> for TransferSync {
    fn stage(&self, queue: &hal::vulkan::Queue) {
        if let Some(wait) = &self.wait {
            queue.add_wait_semaphore(**wait, None, vk::PipelineStageFlags::ALL_COMMANDS);
        }
        if let Some(signal) = &self.signal {
            queue.add_signal_semaphore(**signal, None);
        }
    }
    fn unstage(&self, queue: &hal::vulkan::Queue) {
        if let Some(wait) = &self.wait {
            queue.remove_wait_semaphore(**wait);
        }
        if let Some(signal) = &self.signal {
            queue.remove_signal_semaphore(**signal);
        }
    }
}
impl TransferSync {
    fn new(owner: &Rc<Device<V>>, ready: Option<Win32Semaphore>, signal: bool) -> Result<Rc<Self>> {
        let create = |export| -> Result<Owned<V, vk::Semaphore>> {
            let mut external = vk::ExportSemaphoreCreateInfo::default()
                .handle_types(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_WIN32);
            let mut info = vk::SemaphoreCreateInfo::default();
            if export {
                info = info.push_next(&mut external);
            }
            let raw = unsafe { owner.open.device.raw_device().create_semaphore(&info, None) }
                .map_err(|error| format!("Creating Win32 semaphore: {error:?}"))?;
            Ok(Owned::new(owner, raw, |device, semaphore| unsafe {
                device.raw_device().destroy_semaphore(semaphore, None)
            }))
        };
        let wait = if let Some(ready) = ready {
            if (ready.device_uuid, ready.driver_uuid) != ids(owner) {
                return Err("Win32 semaphore device/driver mismatch".into());
            }
            let semaphore = create(false)?;
            let duplicate = ready
                .handle
                .try_clone()
                .map_err(|error| format!("Duplicating Win32 semaphore: {error}"))?;
            let extension = ash::khr::external_semaphore_win32::Device::new(
                owner.open.device.shared_instance().raw_instance(),
                owner.open.device.raw_device(),
            );
            unsafe {
                extension.import_semaphore_win32_handle(
                    &vk::ImportSemaphoreWin32HandleInfoKHR::default()
                        .semaphore(*semaphore)
                        .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_WIN32)
                        .handle(duplicate.as_raw_handle() as vk::HANDLE),
                )
            }
            .map_err(|error| format!("Importing Win32 semaphore: {error:?}"))?;
            Some(semaphore)
        } else {
            None
        };
        Ok(Rc::new(Self {
            wait,
            signal: if signal { Some(create(true)?) } else { None },
        }))
    }
    fn receipt(&self, owner: &Device<V>) -> Result<Win32Semaphore> {
        let extension = ash::khr::external_semaphore_win32::Device::new(
            owner.open.device.shared_instance().raw_instance(),
            owner.open.device.raw_device(),
        );
        let handle = unsafe {
            extension
                .get_semaphore_win32_handle(
                    &vk::SemaphoreGetWin32HandleInfoKHR::default()
                        .semaphore(
                            **self
                                .signal
                                .as_ref()
                                .ok_or("No Win32 release signal was requested")?,
                        )
                        .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_WIN32),
                )
                .map_err(|error| format!("Exporting Win32 release semaphore: {error:?}"))
                .and_then(|handle| owned_handle(handle))
        }
        .map_err(|error| {
            owner.lost.set(true);
            error
        })?;
        let (device_uuid, driver_uuid) = ids(owner);
        Ok(Win32Semaphore {
            handle,
            device_uuid,
            driver_uuid,
        })
    }
}

unsafe fn barrier(
    owner: &Device<V>,
    commands: &mut Submission<V>,
    image: vk::Image,
    old: vk::ImageLayout,
    new: vk::ImageLayout,
    src_family: u32,
    dst_family: u32,
    src: vk::AccessFlags,
    dst: vk::AccessFlags,
) {
    owner.open.device.raw_device().cmd_pipeline_barrier(
        commands.encoder().raw_handle(),
        vk::PipelineStageFlags::ALL_COMMANDS,
        vk::PipelineStageFlags::ALL_COMMANDS,
        vk::DependencyFlags::empty(),
        &[],
        &[],
        &[vk::ImageMemoryBarrier::default()
            .image(image)
            .old_layout(old)
            .new_layout(new)
            .src_queue_family_index(src_family)
            .dst_queue_family_index(dst_family)
            .src_access_mask(src)
            .dst_access_mask(dst)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            )],
    );
}

fn copy_region(descriptor: api::ImageDescriptor) -> hal::TextureCopy {
    let base = hal::TextureCopyBase {
        mip_level: 0,
        array_layer: 0,
        origin: wgt::Origin3d::ZERO,
        aspect: hal::FormatAspects::COLOR,
    };
    hal::TextureCopy {
        src_base: base.clone(),
        dst_base: base,
        size: wgt::Extent3d {
            width: descriptor.size.width as u32,
            height: descriptor.size.height as u32,
            depth_or_array_layers: 1,
        }
        .into(),
    }
}

fn allocate(
    owner: &Rc<Device<V>>,
    descriptor: api::ImageDescriptor,
    imported: Option<&Win32Image>,
) -> Result<(Rc<Owned<V, hal::vulkan::Texture>>, Option<Win32Image>)> {
    check_image(owner, descriptor)?;
    let device = &owner.open.device;
    let raw = device.raw_device();
    let size = wgt::Extent3d {
        width: descriptor.size.width as u32,
        height: descriptor.size.height as u32,
        depth_or_array_layers: 1,
    };
    let format = plane_format(descriptor.format)?;
    let mut external = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_WIN32);
    let info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(owner.adapter.texture_format_as_raw(format))
        .extent(vk::Extent3D {
            width: size.width,
            height: size.height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .push_next(&mut external);
    let image = unsafe { raw.create_image(&info, None) }
        .map_err(|error| format!("Creating Win32 shared image: {error:?}"))?;
    let mut image = Owned::new(owner, image, |device, image| unsafe {
        device.raw_device().destroy_image(image, None)
    });
    let requirements = unsafe { raw.get_image_memory_requirements(*image) };
    let properties = unsafe {
        device
            .shared_instance()
            .raw_instance()
            .get_physical_device_memory_properties(device.raw_physical_device())
    };
    let (device_uuid, driver_uuid) = ids(owner);
    let memory_type = if let Some(imported) = imported {
        imported.layout.validate_device(device_uuid, driver_uuid)?;
        if imported.layout.memory_type() >= properties.memory_type_count
            || requirements.memory_type_bits & (1 << imported.layout.memory_type()) == 0
            || imported.layout.allocation_size() < requirements.size
        {
            return Err(
                "Win32 allocation is incompatible with the imported image requirements".into(),
            );
        }
        imported.layout.memory_type()
    } else {
        (0..properties.memory_type_count)
            .filter(|index| requirements.memory_type_bits & (1 << index) != 0)
            .min_by_key(|index| {
                !properties.memory_types[*index as usize]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
            })
            .ok_or("No exportable Win32 memory type")?
    };
    let allocation_size =
        imported.map_or(requirements.size, |image| image.layout.allocation_size());
    let duplicate = imported
        .map(|image| image.handle.try_clone())
        .transpose()
        .map_err(|error| format!("Duplicating Win32 memory handle: {error}"))?;
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(*image);
    let mut export = vk::ExportMemoryAllocateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_WIN32);
    let mut import = vk::ImportMemoryWin32HandleInfoKHR::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::OPAQUE_WIN32)
        .handle(
            duplicate
                .as_ref()
                .map_or(0, |handle| handle.as_raw_handle() as vk::HANDLE),
        );
    let mut allocate = vk::MemoryAllocateInfo::default()
        .allocation_size(allocation_size)
        .memory_type_index(memory_type)
        .push_next(&mut dedicated);
    if imported.is_some() {
        allocate = allocate.push_next(&mut import);
    } else {
        allocate = allocate.push_next(&mut export);
    }
    let memory = unsafe { raw.allocate_memory(&allocate, None) }
        .map_err(|error| format!("Allocating/importing Win32 memory: {error:?}"))?;
    let mut memory = Owned::new(owner, memory, |device, memory| unsafe {
        device.raw_device().free_memory(memory, None)
    });
    unsafe { raw.bind_image_memory(*image, *memory, 0) }
        .map_err(|error| format!("Binding Win32 memory: {error:?}"))?;
    let exported = if imported.is_none() {
        let extension = ash::khr::external_memory_win32::Device::new(
            device.shared_instance().raw_instance(),
            raw,
        );
        let handle = unsafe {
            extension.get_memory_win32_handle(
                &vk::MemoryGetWin32HandleInfoKHR::default()
                    .memory(*memory)
                    .handle_type(vk::ExternalMemoryHandleTypeFlags::OPAQUE_WIN32),
            )
        }
        .map_err(|error| format!("Exporting Win32 allocation: {error:?}"))?;
        Some(Win32Image {
            handle: unsafe { owned_handle(handle) }?,
            layout: Win32ImageLayout::new(
                descriptor,
                allocation_size,
                memory_type,
                device_uuid,
                driver_uuid,
            )?,
        })
    } else {
        None
    };
    let desc = texture_descriptor(
        size,
        format,
        wgt::TextureUses::COPY_SRC | wgt::TextureUses::COPY_DST,
    );
    let texture = unsafe {
        device.texture_from_raw(
            image.take(),
            &desc,
            None,
            hal::vulkan::TextureMemory::Dedicated(memory.take()),
        )
    };
    Ok((
        Rc::new(Owned::new(
            owner,
            texture,
            <hal::vulkan::Device as hal::Device>::destroy_texture,
        )),
        exported,
    ))
}

impl ExternalImageDevice {
    pub fn win32_sharing_formats(&self) -> Result<Vec<api::ImageFormat>> {
        let owner = &self.vulkan_producer()?.owner;
        if !supported(owner) {
            return Ok(Vec::new());
        }
        Ok([
            api::ImageFormat::RGBA8,
            api::ImageFormat::BGRA8,
            api::ImageFormat::R8,
            api::ImageFormat::RG8,
            api::ImageFormat::R16,
            api::ImageFormat::RG16,
        ]
        .iter()
        .copied()
        .filter(|format| limits(owner, plane_format(*format).unwrap()).is_ok())
        .collect())
    }

    pub fn export_win32_image(&self, image: &ExternalNativeImage) -> Result<Win32Export> {
        let producer = self.vulkan_producer()?;
        let owner = &producer.owner;
        image.ensure_idle()?;
        let descriptor = image.descriptor();
        let source = image.texture(owner)?;
        if source.mip_count != 1 || !source.sample_initialized() {
            return Err("Win32 export needs an initialized single mip".into());
        }
        let (target, exported) = allocate(owner, descriptor, None)?;
        let sync = TransferSync::new(owner, None, true)?;
        {
            let mut commands = producer.submissions.recording()?;
            commands.keep(target.clone());
            commands.synchronize(sync.clone());
            let previous = source.current_usage();
            source.transition(&mut commands, wgt::TextureUses::COPY_SRC);
            unsafe {
                barrier(
                    owner,
                    &mut commands,
                    target.raw_handle(),
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::QUEUE_FAMILY_IGNORED,
                    vk::QUEUE_FAMILY_IGNORED,
                    vk::AccessFlags::empty(),
                    vk::AccessFlags::TRANSFER_WRITE,
                );
                commands.encoder().copy_texture_to_texture(
                    &source.raw,
                    wgt::TextureUses::COPY_SRC,
                    &target,
                    std::iter::once(copy_region(descriptor)),
                );
                barrier(
                    owner,
                    &mut commands,
                    target.raw_handle(),
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::ImageLayout::GENERAL,
                    vk::QUEUE_FAMILY_IGNORED,
                    vk::QUEUE_FAMILY_IGNORED,
                    vk::AccessFlags::TRANSFER_WRITE,
                    vk::AccessFlags::empty(),
                );
                barrier(
                    owner,
                    &mut commands,
                    target.raw_handle(),
                    vk::ImageLayout::GENERAL,
                    vk::ImageLayout::GENERAL,
                    owner.open.device.queue_family_index(),
                    vk::QUEUE_FAMILY_EXTERNAL,
                    vk::AccessFlags::TRANSFER_WRITE,
                    vk::AccessFlags::empty(),
                );
            }
            source.transition(&mut commands, previous);
        }
        producer.submissions.submit()?;
        Ok(Win32Export {
            image: exported.unwrap(),
            ready: sync.receipt(owner)?,
        })
    }

    /// Copies an opaque Vulkan allocation into independent WR storage on the GPU.
    /// # Safety
    /// Source metadata must describe its live allocation. Producer work must release GENERAL ownership
    /// to QUEUE_FAMILY_EXTERNAL before signaling `ready`. No access or aliasing imports are allowed
    /// until the returned release semaphore is waited. On failure retire the device before reuse.
    /// D3D textures/fences, KMT handles, multiplanar aliases and cross-adapter imports are not accepted.
    pub unsafe fn copy_win32_image(
        &self,
        image: &Win32Image,
        ready: Win32Semaphore,
    ) -> Result<Win32Copy> {
        let producer = self.vulkan_producer()?;
        let owner = &producer.owner;
        #[cfg(any(test, feature = "hal-testing"))]
        owner.check_fault(FailurePoint::Import)?;
        let (device_uuid, driver_uuid) = ids(owner);
        image.layout.validate_device(device_uuid, driver_uuid)?;
        let descriptor = image.layout.descriptor();
        let (source, _) = allocate(owner, descriptor, Some(image))?;
        let target = Texture::new(
            owner,
            descriptor.size.width as u32,
            descriptor.size.height as u32,
            plane_format(descriptor.format)?,
            crate::device::TextureFilter::Linear,
            false,
        )?;
        let sync = TransferSync::new(owner, Some(ready), true)?;
        {
            let mut commands = producer.submissions.recording()?;
            commands.keep(source.clone());
            commands.synchronize(sync.clone());
            let family = owner.open.device.queue_family_index();
            barrier(
                owner,
                &mut commands,
                source.raw_handle(),
                vk::ImageLayout::GENERAL,
                vk::ImageLayout::GENERAL,
                vk::QUEUE_FAMILY_EXTERNAL,
                family,
                vk::AccessFlags::empty(),
                vk::AccessFlags::TRANSFER_READ,
            );
            barrier(
                owner,
                &mut commands,
                source.raw_handle(),
                vk::ImageLayout::GENERAL,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                vk::QUEUE_FAMILY_IGNORED,
                vk::QUEUE_FAMILY_IGNORED,
                vk::AccessFlags::empty(),
                vk::AccessFlags::TRANSFER_READ,
            );
            target.transition(&mut commands, wgt::TextureUses::COPY_DST);
            commands.encoder().copy_texture_to_texture(
                &source,
                wgt::TextureUses::COPY_SRC,
                &target.raw,
                std::iter::once(copy_region(descriptor)),
            );
            target.initialize(&mut commands);
            barrier(
                owner,
                &mut commands,
                source.raw_handle(),
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                vk::ImageLayout::GENERAL,
                vk::QUEUE_FAMILY_IGNORED,
                vk::QUEUE_FAMILY_IGNORED,
                vk::AccessFlags::TRANSFER_READ,
                vk::AccessFlags::empty(),
            );
            barrier(
                owner,
                &mut commands,
                source.raw_handle(),
                vk::ImageLayout::GENERAL,
                vk::ImageLayout::GENERAL,
                family,
                vk::QUEUE_FAMILY_EXTERNAL,
                vk::AccessFlags::TRANSFER_READ,
                vk::AccessFlags::empty(),
            );
        }
        producer.submissions.submit()?;
        Ok(Win32Copy {
            image: ExternalNativeImage::new(target, descriptor),
            release: sync.receipt(owner)?,
        })
    }

    pub fn wait_win32_release(&self, release: Win32Semaphore) -> Result<()> {
        let producer = self.vulkan_producer()?;
        if !supported(&producer.owner) {
            return Err("Win32 semaphore sharing is unavailable".into());
        }
        let sync = TransferSync::new(&producer.owner, Some(release), false)?;
        producer.submissions.recording()?.synchronize(sync);
        producer.submissions.wait()
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::resources::{bytes_per_pixel, Buffer};
    use super::*;

    fn read(device: &ExternalImageDevice, image: &ExternalNativeImage) -> Vec<u8> {
        let producer = device.vulkan_producer().unwrap();
        let owner = &producer.owner;
        let texture = image.texture(owner).unwrap();
        let layout = ReadbackLayout::with_pixel_size(
            texture.size.width,
            texture.size.height,
            owner.capabilities.alignments.buffer_copy_pitch.get(),
            bytes_per_pixel(texture.format) as u32,
        )
        .unwrap();
        let buffer = Buffer::readback(owner, &layout).unwrap();
        {
            let mut commands = producer.submissions.recording().unwrap();
            texture.transition(&mut commands, wgt::TextureUses::COPY_SRC);
            buffer.transition(&mut commands, wgt::BufferUses::COPY_DST);
            unsafe {
                copy_readback::<V>(
                    commands.encoder(),
                    &texture.raw,
                    &buffer.raw,
                    &layout,
                    texture.size,
                    hal::FormatAspects::COLOR,
                );
            }
            buffer.transition(&mut commands, wgt::BufferUses::MAP_READ);
        }
        producer.submissions.wait().unwrap();
        owner.map_readback(&buffer.raw, &layout).unwrap()
    }

    #[test]
    #[ignore = "Requires native Windows Vulkan and validation; not executed by Linux gates"]
    fn win32_cross_device_copy_and_source_replacement() {
        let options = Options {
            validation: true,
            ..Default::default()
        };
        let producer = create_vulkan_image_device(&options).unwrap();
        let consumer = create_vulkan_image_device(&options).unwrap();
        let formats = producer.win32_sharing_formats().unwrap();
        assert!(
            !formats.is_empty(),
            "Win32 sharing capability is required for this positive gate"
        );
        assert_ne!(
            producer.vulkan_context().unwrap().device.handle(),
            consumer.vulkan_context().unwrap().device.handle()
        );
        for format in formats {
            let descriptor =
                api::ImageDescriptor::new(17, 13, format, api::ImageDescriptorFlags::empty());
            let size = 17 * 13 * bytes_per_pixel(plane_format(format).unwrap());
            let pixels: Vec<u8> = (0..size).map(|i| (i * 19 + 7) as u8).collect();
            let original = producer.create_image(descriptor, &pixels).unwrap();
            let (shared, ready) = producer.export_win32_image(&original).unwrap().into_parts();
            producer
                .update_image(&original, descriptor, &vec![0; size])
                .unwrap();
            let (image, release) = unsafe { consumer.copy_win32_image(&shared, ready) }
                .unwrap()
                .into_parts();
            producer.wait_win32_release(release).unwrap();
            drop(shared);
            assert_eq!(read(&consumer, &image), pixels);
            assert_eq!(read(&producer, &original), vec![0; size]);
        }
    }
}
