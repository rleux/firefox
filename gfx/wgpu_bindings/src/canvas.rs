/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! One-shot canvas publications. The remote Global and all submit-capable calls
//! remain on CanvasRenderThread; retirement only polls, never submits.
use crate::server::Global;
use ash::{khr, vk};
use std::{
    borrow::Cow,
    fs::File,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    ptr,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc, Arc, OnceLock,
    },
    time::Duration,
};
use wgpu_core_remote_types::id;
use wgt::error::ErrorFilter;

const MAX_EXPORTS: usize = 6;
const MAX_BYTES: u64 = 256 * 1024 * 1024;
static EXPORTS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);

struct Permit {
    bytes: u64,
}
impl Permit {
    fn new() -> Option<Self> {
        EXPORTS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_EXPORTS).then_some(n + 1)
            })
            .ok()?;
        Some(Self { bytes: 0 })
    }
    fn reserve(&mut self, bytes: u64) -> bool {
        if bytes == 0 || self.bytes != 0 {
            return false;
        }
        if BYTES
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                n.checked_add(bytes).filter(|sum| *sum <= MAX_BYTES)
            })
            .is_err()
        {
            return false;
        }
        self.bytes = bytes;
        true
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        BYTES.fetch_sub(self.bytes, Ordering::AcqRel);
        EXPORTS.fetch_sub(1, Ordering::AcqRel);
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct CanvasExportInfo {
    pub memory_fd: i32,
    pub ready_fd: i32,
    pub width: u32,
    pub height: u32,
    pub bgra: bool,
    pub modifier: u64,
    pub offset: u64,
    pub stride: u64,
    pub device_uuid: [u8; 16],
    pub driver_uuid: [u8; 16],
}
extern "C" {
    fn wgpu_parent_native_canvas_supported(info: &CanvasExportInfo, opaque: bool) -> bool;
}

struct Export {
    device: Arc<wgc::device::Device>,
    source: Arc<wgc::resource::Texture>,
    image: vk::Image,
    memory: vk::DeviceMemory,
    ready: vk::Semaphore,
    memory_fd: Option<OwnedFd>,
    ready_fd: Option<OwnedFd>,
    info: CanvasExportInfo,
    submitted: bool,
    done: Arc<AtomicBool>,
    _permit: Permit,
}
impl Drop for Export {
    fn drop(&mut self) {
        let hal = unsafe { self.device.clone().as_hal::<wgc::api::Vulkan>() }.unwrap();
        unsafe {
            let raw = hal.raw_device();
            raw.destroy_semaphore(self.ready, None);
            raw.destroy_image(self.image, None);
            raw.free_memory(self.memory, None);
        }
    }
}

// Consumer return transfers a one-shot allocation here; it is never reused.
// Native completion and core completion are distinct, including on logical loss.
// Do not retain the core queue: Global drops it on CanvasRenderThread, where its
// destructor drains callbacks before that thread and its local tasks shut down.
fn completion(native: Result<u64, vk::Result>, core_done: bool) -> bool {
    native == Err(vk::Result::ERROR_DEVICE_LOST)
        || (native.is_ok_and(|value| value >= 1) && core_done)
}
fn retired(export: &Export) -> bool {
    if !export.submitted {
        return true;
    }
    let hal = unsafe { export.device.clone().as_hal::<wgc::api::Vulkan>() }.unwrap();
    let result = unsafe {
        if hal
            .enabled_device_extensions()
            .contains(&khr::timeline_semaphore::NAME)
        {
            khr::timeline_semaphore::Device::new(
                hal.shared_instance().raw_instance(),
                hal.raw_device(),
            )
            .get_semaphore_counter_value(export.ready)
        } else {
            hal.raw_device().get_semaphore_counter_value(export.ready)
        }
    };
    completion(result, export.done.load(Ordering::Acquire))
}
fn retire(export: Box<Export>) {
    if retired(&export) {
        return;
    }
    static WORKER: OnceLock<Option<mpsc::Sender<Box<Export>>>> = OnceLock::new();
    let sender = WORKER.get_or_init(|| {
        let (sender, receiver) = mpsc::channel::<Box<Export>>();
        std::thread::Builder::new()
            .name("WebGPU exports".into())
            .spawn(move || {
                let mut pending = Vec::new();
                loop {
                    if pending.is_empty() {
                        match receiver.recv() {
                            Ok(export) => pending.push(export),
                            Err(_) => return,
                        }
                    } else if let Ok(export) = receiver.recv_timeout(Duration::from_millis(20)) {
                        pending.push(export);
                    }
                    pending.retain(|export| !retired(export));
                }
            })
            .ok()
            .map(|_| sender)
    });
    let Some(sender) = sender else {
        std::mem::forget(export);
        return;
    };
    if let Err(error) = sender.send(export) {
        // An indeterminate native failure must not free potentially live handles.
        std::mem::forget(error.0);
    }
}

pub struct CanvasExport {
    export: Option<Box<Export>>,
}

fn descriptor_supported(d: &wgt::TextureDescriptor<String, Vec<wgt::TextureFormat>>) -> bool {
    matches!(
        d.format,
        wgt::TextureFormat::Rgba8Unorm | wgt::TextureFormat::Bgra8Unorm
    ) && d.dimension == wgt::TextureDimension::D2
        && d.mip_level_count == 1
        && d.sample_count == 1
        && d.size.width > 0
        && d.size.height > 0
        && d.size.depth_or_array_layers == 1
        && d.usage
            .contains(wgt::TextureUsages::RENDER_ATTACHMENT | wgt::TextureUsages::COPY_SRC)
}

fn allocate(
    global: &Global,
    device_id: id::DeviceId,
    queue_id: id::QueueId,
    texture_id: id::TextureId,
    opaque: bool,
) -> Option<Box<Export>> {
    use wgc::resource::ParentDevice;
    let permit = Permit::new()?;
    let source = global.resolve_texture_id(texture_id);
    let device = global.resolve_device_id(device_id);
    let queue = global.resolve_queue_id(queue_id);
    if source.same_device(&device).is_err()
        || queue.same_device(&device).is_err()
        || device.check_is_valid().is_err()
        || !descriptor_supported(source.descriptor())
    {
        return None;
    }
    let hal = unsafe { device.clone().as_hal::<wgc::api::Vulkan>() }?;
    if ![
        khr::external_memory_fd::NAME,
        ash::ext::external_memory_dma_buf::NAME,
        ash::ext::image_drm_format_modifier::NAME,
        khr::external_semaphore_fd::NAME,
    ]
    .iter()
    .all(|name| hal.enabled_device_extensions().contains(name))
    {
        return None;
    }
    let fence = unsafe { device.clone().fence_as_hal::<wgc::api::Vulkan>() }?;
    if !matches!(&*fence, wgh::vulkan::Fence::TimelineSemaphore(_)) {
        return None;
    }
    let instance = hal.shared_instance().raw_instance();
    let physical = hal.raw_physical_device();
    let raw = hal.raw_device();
    let mut ty = vk::SemaphoreTypeCreateInfo::default().semaphore_type(vk::SemaphoreType::TIMELINE);
    let semaphore_info = vk::PhysicalDeviceExternalSemaphoreInfo::default()
        .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD)
        .push_next(&mut ty);
    let mut semaphore_properties = vk::ExternalSemaphoreProperties::default();
    unsafe {
        instance.get_physical_device_external_semaphore_properties(
            physical,
            &semaphore_info,
            &mut semaphore_properties,
        );
    }
    if !semaphore_properties.external_semaphore_features.contains(
        vk::ExternalSemaphoreFeatureFlags::EXPORTABLE
            | vk::ExternalSemaphoreFeatureFlags::IMPORTABLE,
    ) || !semaphore_properties
        .compatible_handle_types
        .contains(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD)
    {
        return None;
    }
    let desc = source.descriptor();
    let format = if desc.format == wgt::TextureFormat::Bgra8Unorm {
        vk::Format::B8G8R8A8_UNORM
    } else {
        vk::Format::R8G8B8A8_UNORM
    };
    let mut ids = vk::PhysicalDeviceIDProperties::default();
    unsafe {
        instance.get_physical_device_properties2(
            physical,
            &mut vk::PhysicalDeviceProperties2::default().push_next(&mut ids),
        );
    }
    let mut modifiers = vk::DrmFormatModifierPropertiesListEXT::default();
    unsafe {
        instance.get_physical_device_format_properties2(
            physical,
            format,
            &mut vk::FormatProperties2::default().push_next(&mut modifiers),
        );
    }
    if modifiers.drm_format_modifier_count == 0 || modifiers.drm_format_modifier_count > 4096 {
        return None;
    }
    let mut entries = vec![
        vk::DrmFormatModifierPropertiesEXT::default();
        modifiers.drm_format_modifier_count as usize
    ];
    modifiers.p_drm_format_modifier_properties = entries.as_mut_ptr();
    unsafe {
        instance.get_physical_device_format_properties2(
            physical,
            format,
            &mut vk::FormatProperties2::default().push_next(&mut modifiers),
        );
    }
    entries.truncate(modifiers.drm_format_modifier_count as usize);
    let usage = vk::ImageUsageFlags::SAMPLED
        | vk::ImageUsageFlags::TRANSFER_SRC
        | vk::ImageUsageFlags::TRANSFER_DST;
    let required = vk::FormatFeatureFlags::SAMPLED_IMAGE
        | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
        | vk::FormatFeatureFlags::TRANSFER_SRC
        | vk::FormatFeatureFlags::TRANSFER_DST;
    let mut export = Box::new(Export {
        device: device.clone(),
        source: source.clone(),
        image: vk::Image::null(),
        memory: vk::DeviceMemory::null(),
        ready: vk::Semaphore::null(),
        memory_fd: None,
        ready_fd: None,
        info: CanvasExportInfo {
            memory_fd: -1,
            ready_fd: -1,
            width: desc.size.width,
            height: desc.size.height,
            bgra: desc.format == wgt::TextureFormat::Bgra8Unorm,
            device_uuid: ids.device_uuid,
            driver_uuid: ids.driver_uuid,
            ..Default::default()
        },
        submitted: false,
        done: Arc::new(AtomicBool::new(false)),
        _permit: permit,
    });
    for entry in entries.iter().filter(|entry| {
        entry.drm_format_modifier_plane_count == 1
            && entry.drm_format_modifier_tiling_features.contains(required)
    }) {
        export.info.modifier = entry.drm_format_modifier;
        // Capability snapshots match identity/format/usage/extent, without importing FDs.
        if !unsafe { wgpu_parent_native_canvas_supported(&export.info, opaque) } {
            continue;
        }
        let values = [entry.drm_format_modifier];
        let mut modifier = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
            .drm_format_modifier(entry.drm_format_modifier)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let mut external = vk::PhysicalDeviceExternalImageFormatInfo::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let properties_info = vk::PhysicalDeviceImageFormatInfo2::default()
            .format(format)
            .ty(vk::ImageType::TYPE_2D)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(usage)
            .push_next(&mut modifier)
            .push_next(&mut external);
        let mut memory_properties = vk::ExternalImageFormatProperties::default();
        let mut properties =
            vk::ImageFormatProperties2::default().push_next(&mut memory_properties);
        if unsafe {
            instance.get_physical_device_image_format_properties2(
                physical,
                &properties_info,
                &mut properties,
            )
        }
        .is_err()
        {
            continue;
        }
        // Read the primary output before its pNext output, ending that borrow.
        let limits = properties.image_format_properties;
        let memory_caps = memory_properties.external_memory_properties;
        if !memory_caps.external_memory_features.contains(
            vk::ExternalMemoryFeatureFlags::EXPORTABLE | vk::ExternalMemoryFeatureFlags::IMPORTABLE,
        ) || !memory_caps
            .compatible_handle_types
            .contains(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            || !limits.sample_counts.contains(vk::SampleCountFlags::TYPE_1)
            || desc.size.width > limits.max_extent.width
            || desc.size.height > limits.max_extent.height
            || limits.max_mip_levels == 0
            || limits.max_array_layers == 0
        {
            continue;
        }
        let mut modifier =
            vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&values);
        let mut external = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: desc.size.width,
                height: desc.size.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .push_next(&mut modifier)
            .push_next(&mut external);
        export.image = match unsafe { raw.create_image(&image_info, None) } {
            Ok(image) => image,
            Err(_) => continue,
        };
        let mut dedicated = vk::MemoryDedicatedRequirements::default();
        let mut requirements = vk::MemoryRequirements2::default().push_next(&mut dedicated);
        unsafe {
            raw.get_image_memory_requirements2(
                &vk::ImageMemoryRequirementsInfo2::default().image(export.image),
                &mut requirements,
            );
        }
        let requirements = requirements.memory_requirements;
        if requirements.size > limits.max_resource_size
            || !export._permit.reserve(requirements.size)
        {
            return None;
        }
        let memory_types = unsafe { instance.get_physical_device_memory_properties(physical) };
        let index = (0..memory_types.memory_type_count).find(|index| {
            requirements.memory_type_bits & (1 << index) != 0
                && memory_types.memory_types[*index as usize]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
                && !memory_types.memory_types[*index as usize]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::PROTECTED)
        })?;
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(export.image);
        let mut external = vk::ExportMemoryAllocateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        export.memory = unsafe {
            raw.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(requirements.size)
                    .memory_type_index(index)
                    .push_next(&mut dedicated)
                    .push_next(&mut external),
                None,
            )
        }
        .ok()?;
        unsafe { raw.bind_image_memory(export.image, export.memory, 0) }.ok()?;
        let fd = unsafe {
            khr::external_memory_fd::Device::new(instance, raw).get_memory_fd(
                &vk::MemoryGetFdInfoKHR::default()
                    .memory(export.memory)
                    .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT),
            )
        }
        .ok()?;
        export.memory_fd = Some(unsafe { OwnedFd::from_raw_fd(fd) });
        let bytes = File::from(export.memory_fd.as_ref()?.try_clone().ok()?)
            .metadata()
            .ok()?
            .len();
        if bytes < requirements.size {
            return None;
        }
        let mut fd_properties = vk::MemoryFdPropertiesKHR::default();
        unsafe {
            khr::external_memory_fd::Device::new(instance, raw).get_memory_fd_properties(
                vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                fd,
                &mut fd_properties,
            )
        }
        .ok()?;
        if fd_properties.memory_type_bits & (1 << index) == 0 {
            return None;
        }
        let layout = unsafe {
            raw.get_image_subresource_layout(
                export.image,
                vk::ImageSubresource::default()
                    .aspect_mask(vk::ImageAspectFlags::MEMORY_PLANE_0_EXT),
            )
        };
        let mut actual_modifier = vk::ImageDrmFormatModifierPropertiesEXT::default();
        unsafe {
            ash::ext::image_drm_format_modifier::Device::new(instance, raw)
                .get_image_drm_format_modifier_properties(export.image, &mut actual_modifier)
        }
        .ok()?;
        if actual_modifier.drm_format_modifier != entry.drm_format_modifier
            || layout.offset >= bytes
            || layout.row_pitch == 0
        {
            return None;
        }
        if entry.drm_format_modifier == 0
            && (layout.row_pitch < u64::from(desc.size.width) * 4
                || layout.row_pitch % 4 != 0
                || layout.offset % 4 != 0
                || layout
                    .row_pitch
                    .checked_mul(u64::from(desc.size.height - 1))
                    .and_then(|n| n.checked_add(layout.offset))
                    .and_then(|n| n.checked_add(u64::from(desc.size.width) * 4))
                    .is_none_or(|end| end > bytes))
        {
            return None;
        }
        let mut ty =
            vk::SemaphoreTypeCreateInfo::default().semaphore_type(vk::SemaphoreType::TIMELINE);
        let mut external = vk::ExportSemaphoreCreateInfo::default()
            .handle_types(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD);
        export.ready = unsafe {
            raw.create_semaphore(
                &vk::SemaphoreCreateInfo::default()
                    .push_next(&mut ty)
                    .push_next(&mut external),
                None,
            )
        }
        .ok()?;
        let fd = unsafe {
            khr::external_semaphore_fd::Device::new(instance, raw).get_semaphore_fd(
                &vk::SemaphoreGetFdInfoKHR::default()
                    .semaphore(export.ready)
                    .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD),
            )
        }
        .ok()?;
        export.ready_fd = Some(unsafe { OwnedFd::from_raw_fd(fd) });
        export.info.memory_fd = export.memory_fd.as_ref()?.as_raw_fd();
        export.info.ready_fd = fd;
        export.info.modifier = entry.drm_format_modifier;
        export.info.offset = layout.offset;
        export.info.stride = layout.row_pitch;
        if !unsafe { wgpu_parent_native_canvas_supported(&export.info, opaque) } {
            return None;
        }
        return Some(export);
    }
    None
}

fn submit(export: &mut Export, producer_queue: &Arc<wgc::device::queue::Queue>) -> bool {
    let device = &export.device;
    if device.check_is_valid().is_err() {
        return false;
    }
    for filter in [
        ErrorFilter::Validation,
        ErrorFilter::OutOfMemory,
        ErrorFilter::Internal,
    ] {
        device.push_error_scope(filter);
    }
    let prepared = device.create_command_encoder(&wgt::CommandEncoderDescriptor {
        label: Some("WebGPU canvas initialize".into()),
    });
    let mut pass = prepared.begin_render_pass(wgc::command::ResolvedRenderPassDescriptor {
        color_attachments: Cow::Owned(vec![Some(wgc::command::RenderPassColorAttachment {
            view: export.source.create_view(&Default::default()),
            depth_slice: None,
            resolve_target: None,
            load_op: wgt::LoadOp::Load,
            store_op: wgt::StoreOp::Store,
        })]),
        ..Default::default()
    });
    pass.end();
    prepared.transition_resources(
        std::iter::empty(),
        std::iter::once(wgt::TextureTransition {
            texture: export.source.clone(),
            selector: None,
            state: wgt::TextureUses::COPY_SRC,
        }),
    );
    let native = device.create_command_encoder(&wgt::CommandEncoderDescriptor {
        label: Some("WebGPU canvas export".into()),
    });
    let hal = unsafe { device.clone().as_hal::<wgc::api::Vulkan>() }.unwrap();
    let source = unsafe { export.source.clone().as_hal::<wgc::api::Vulkan>() };
    let mut encoded = false;
    if let Some(source) = source.as_ref() {
        unsafe {
            native.as_hal_mut::<wgc::api::Vulkan, _, _>(|encoder| {
                let Some(encoder) = encoder else {
                    return;
                };
                let raw = hal.raw_device();
                let command = encoder.raw_handle();
                let range = vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1);
                raw.cmd_pipeline_barrier(
                    command,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[vk::ImageMemoryBarrier::default()
                        .image(export.image)
                        .subresource_range(range)
                        .old_layout(vk::ImageLayout::UNDEFINED)
                        .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)],
                );
                let layers = vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .layer_count(1);
                raw.cmd_copy_image(
                    command,
                    source.raw_handle(),
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    export.image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[vk::ImageCopy::default()
                        .src_subresource(layers)
                        .dst_subresource(layers)
                        .extent(vk::Extent3D {
                            width: export.info.width,
                            height: export.info.height,
                            depth: 1,
                        })],
                );
                raw.cmd_pipeline_barrier(
                    command,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[vk::ImageMemoryBarrier::default()
                        .image(export.image)
                        .subresource_range(range)
                        .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                        .new_layout(vk::ImageLayout::GENERAL)
                        .src_queue_family_index(hal.queue_family_index())
                        .dst_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
                        .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)],
                );
                encoded = true;
            });
        }
    }
    drop(source);
    let commands = [
        prepared.finish(&Default::default()),
        native.finish(&Default::default()),
    ];
    let mut valid = encoded && device.check_is_valid().is_ok();
    // Error scopes are popped before staging; no callback or map can steal hooks.
    for _ in 0..3 {
        valid &= device.pop_error_scope().is_ok_and(|error| error.is_none());
    }
    if !valid {
        return false;
    }
    for filter in [
        ErrorFilter::Validation,
        ErrorFilter::OutOfMemory,
        ErrorFilter::Internal,
    ] {
        device.push_error_scope(filter);
    }
    let queue = unsafe { producer_queue.clone().as_hal::<wgc::api::Vulkan>() }.unwrap();
    queue.add_signal_semaphore(export.ready, Some(1));
    let index = producer_queue.submit(&commands);
    export.submitted = !queue.remove_signal_semaphore(export.ready);
    valid = export.submitted && index.is_some() && device.check_is_valid().is_ok();
    for _ in 0..3 {
        valid &= device.pop_error_scope().is_ok_and(|error| error.is_none());
    }
    if export.submitted {
        let completed = export.done.clone();
        producer_queue
            .on_submitted_work_done(Box::new(move || completed.store(true, Ordering::Release)));
    }
    valid && device.check_is_valid().is_ok()
}

#[no_mangle]
pub extern "C" fn wgpu_canvas_export(
    global: &Global,
    device_id: id::DeviceId,
    queue_id: id::QueueId,
    texture_id: id::TextureId,
    opaque: bool,
    output: &mut CanvasExportInfo,
) -> *mut CanvasExport {
    let Some(mut export) = allocate(global, device_id, queue_id, texture_id, opaque) else {
        log::debug!("WebGPU native canvas texture={texture_id:?}: admission/allocation unavailable; CPU fallback");
        return ptr::null_mut();
    };
    let queue = global.resolve_queue_id(queue_id);
    if !submit(&mut export, &queue) {
        log::warn!(
            "WebGPU native canvas texture={texture_id:?}: export submission failed; CPU fallback"
        );
        retire(export);
        return ptr::null_mut();
    }
    *output = export.info;
    Box::into_raw(Box::new(CanvasExport {
        export: Some(export),
    }))
}

/// The caller owns this handle and has ended its publication loan, if accepted.
#[no_mangle]
pub unsafe extern "C" fn wgpu_canvas_export_delete(export: *mut CanvasExport) {
    if !export.is_null() {
        let mut export = unsafe { Box::from_raw(export) };
        if let Some(export) = export.export.take() {
            retire(export);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_exports_release_reservations_on_failure() {
        let before = EXPORTS.load(Ordering::Acquire);
        let mut permits = Vec::new();
        while let Some(permit) = Permit::new() {
            permits.push(permit);
        }
        assert_eq!(EXPORTS.load(Ordering::Acquire), MAX_EXPORTS);
        assert!(Permit::new().is_none());
        assert!(!permits[0].reserve(MAX_BYTES + 1));
        assert!(permits[0].reserve(MAX_BYTES));
        assert!(!permits[0].reserve(1));
        drop(permits);
        assert_eq!(BYTES.load(Ordering::Acquire), 0);
        assert_eq!(EXPORTS.load(Ordering::Acquire), before);
    }
    #[test]
    fn native_completion_and_core_retirement_are_both_required() {
        assert!(!completion(Ok(0), true));
        assert!(!completion(Ok(1), false));
        assert!(!completion(Err(vk::Result::ERROR_UNKNOWN), true));
        assert!(completion(Ok(1), true));
        assert!(completion(Err(vk::Result::ERROR_DEVICE_LOST), false));
    }
    #[test]
    fn rejects_non_renderable_and_non_rgb_canvas_descriptors() {
        let mut desc = wgt::TextureDescriptor {
            label: String::new(),
            size: wgt::Extent3d {
                width: 97,
                height: 65,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgt::TextureDimension::D2,
            format: wgt::TextureFormat::Rgba8Unorm,
            usage: wgt::TextureUsages::RENDER_ATTACHMENT | wgt::TextureUsages::COPY_SRC,
            view_formats: vec![],
        };
        assert!(descriptor_supported(&desc));
        desc.format = wgt::TextureFormat::Rgba16Float;
        assert!(!descriptor_supported(&desc));
        desc.format = wgt::TextureFormat::Bgra8Unorm;
        desc.usage = wgt::TextureUsages::COPY_SRC;
        assert!(!descriptor_supported(&desc));
    }
}
