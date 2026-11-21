//! Experimental, explicitly enabled private-WebRender descriptor heaps.
//! ABI subset from Vulkan-Headers 1.4.341; no SPIR-V rewriting is performed.
use alloc::{boxed::Box, sync::Arc, vec::Vec};
use ash::vk;
use core::{
    ffi::{c_void, CStr},
    mem, ptr,
    sync::atomic::{AtomicU64, Ordering},
};
use wgpu_sync::Mutex;

pub const NAME: &CStr = c"VK_EXT_descriptor_heap";
const MAX_BINDINGS: usize = 16;
const MAX_SLOTS: u32 = 256;
const MAX_PAGES: usize = 32;
const HEAP_BUFFER_USAGE: vk::BufferUsageFlags = vk::BufferUsageFlags::from_raw(0x1000_0000);
pub(super) const HEAP_PIPELINE_FLAG: u64 = 0x10_0000_0000;

#[repr(C)]
#[derive(Debug)]
struct Features {
    s_type: vk::StructureType,
    p_next: *mut c_void,
    enabled: u32,
    capture_replay: u32,
}
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Properties {
    s_type: vk::StructureType,
    p_next: *mut c_void,
    sampler_heap_alignment: u64,
    resource_heap_alignment: u64,
    max_sampler_heap_size: u64,
    max_resource_heap_size: u64,
    min_sampler_reserved: u64,
    min_sampler_reserved_embedded: u64,
    min_resource_reserved: u64,
    sampler_size: u64,
    image_size: u64,
    buffer_size: u64,
    sampler_alignment: u64,
    image_alignment: u64,
    buffer_alignment: u64,
    max_push_data: u64,
    capture_size: usize,
    max_embedded_samplers: u32,
    ycbcr_count: u32,
    sparse: u32,
    protected: u32,
}
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct AddressRange {
    address: u64,
    size: u64,
}
#[repr(C)]
struct HostRange {
    address: *mut c_void,
    size: usize,
}
#[repr(C)]
struct ConstHostRange {
    address: *const c_void,
    size: usize,
}
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct BindInfo {
    s_type: vk::StructureType,
    p_next: *const c_void,
    range: AddressRange,
    reserved_offset: u64,
    reserved_size: u64,
}
#[repr(C)]
struct PushInfo {
    s_type: vk::StructureType,
    p_next: *const c_void,
    offset: u32,
    data: ConstHostRange,
}
#[repr(C)]
struct ImageInfo {
    s_type: vk::StructureType,
    p_next: *const c_void,
    view: *const vk::ImageViewCreateInfo<'static>,
    layout: vk::ImageLayout,
}
#[repr(C)]
union ResourceData {
    image: *const ImageInfo,
    address: *const AddressRange,
}
#[repr(C)]
struct ResourceInfo {
    s_type: vk::StructureType,
    p_next: *const c_void,
    ty: vk::DescriptorType,
    data: ResourceData,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct PushIndex {
    heap_offset: u32,
    push_offset: u32,
    index_stride: u32,
    array_stride: u32,
    embedded_sampler: *const vk::SamplerCreateInfo<'static>,
    combined: u32,
    sampler_heap_offset: u32,
    sampler_push_offset: u32,
    sampler_index_stride: u32,
    sampler_array_stride: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
union MappingData {
    push: PushIndex,
    padding: [u64; 7],
}
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct Mapping {
    s_type: vk::StructureType,
    p_next: *const c_void,
    set: u32,
    binding: u32,
    count: u32,
    mask: u32,
    source: u32,
    data: MappingData,
}
#[repr(C)]
pub(super) struct MappingInfo {
    pub s_type: vk::StructureType,
    pub p_next: *const c_void,
    pub count: u32,
    pub mappings: *const Mapping,
}

type WriteSamplers = unsafe extern "system" fn(
    vk::Device,
    u32,
    *const vk::SamplerCreateInfo<'static>,
    *const HostRange,
) -> vk::Result;
type WriteResources =
    unsafe extern "system" fn(vk::Device, u32, *const ResourceInfo, *const HostRange) -> vk::Result;
type BindHeap = unsafe extern "system" fn(vk::CommandBuffer, *const BindInfo);
type PushData = unsafe extern "system" fn(vk::CommandBuffer, *const PushInfo);

/// Features must outlive the open_with_callback call using callback().
#[derive(Debug)]
pub struct DeviceFeatures {
    heap: Features,
    maintenance: vk::PhysicalDeviceMaintenance5FeaturesKHR<'static>,
}
impl DeviceFeatures {
    pub fn new(adapter: &super::Adapter) -> Result<Self, crate::DeviceError> {
        // The raw ABI subset and maintenance5 dependency chain are limited to this deployment.
        if !cfg!(target_pointer_width = "64")
            || adapter.instance.instance_api_version < vk::API_VERSION_1_3
            || adapter.phd_capabilities.properties().api_version < vk::API_VERSION_1_3
        {
            return Err(crate::DeviceError::Unexpected);
        }
        for extension in [
            NAME,
            ash::khr::maintenance5::NAME,
            ash::khr::buffer_device_address::NAME,
        ] {
            if !adapter.phd_capabilities.supports_extension(extension) {
                return Err(crate::DeviceError::Unexpected);
            }
        }
        let mut address = vk::PhysicalDeviceBufferDeviceAddressFeatures::default();
        let mut maintenance = vk::PhysicalDeviceMaintenance5FeaturesKHR::default();
        maintenance.p_next =
            (&mut address as *mut vk::PhysicalDeviceBufferDeviceAddressFeatures<'_>).cast();
        let mut heap = Features {
            s_type: vk::StructureType::from_raw(1000135009),
            p_next: (&mut maintenance as *mut vk::PhysicalDeviceMaintenance5FeaturesKHR<'_>).cast(),
            enabled: 0,
            capture_replay: 0,
        };
        let mut query = vk::PhysicalDeviceFeatures2::default();
        query.p_next = (&mut heap as *mut Features).cast();
        unsafe {
            adapter
                .instance
                .raw
                .get_physical_device_features2(adapter.raw, &mut query)
        };
        if heap.enabled == 0 || maintenance.maintenance5 == 0 || address.buffer_device_address == 0
        {
            return Err(crate::DeviceError::Unexpected);
        }
        heap.p_next = ptr::null_mut();
        Ok(Self {
            heap,
            maintenance: vk::PhysicalDeviceMaintenance5FeaturesKHR::default().maintenance5(true),
        })
    }
    pub fn callback<'a>(
        &'a mut self,
        previous: Option<Box<super::CreateDeviceCallback<'a>>>,
    ) -> Box<super::CreateDeviceCallback<'a>> {
        Box::new(move |args| {
            for extension in [
                NAME,
                ash::khr::maintenance5::NAME,
                ash::khr::buffer_device_address::NAME,
            ] {
                if !args.extensions.contains(&extension) {
                    args.extensions.push(extension);
                }
            }
            args.device_features.buffer_device_address = Some(
                vk::PhysicalDeviceBufferDeviceAddressFeatures::default()
                    .buffer_device_address(true),
            );
            self.heap.p_next = args.create_info.p_next.cast_mut();
            self.maintenance.p_next = (&mut self.heap as *mut Features).cast();
            args.create_info.p_next =
                (&self.maintenance as *const vk::PhysicalDeviceMaintenance5FeaturesKHR<'_>).cast();
            if let Some(previous) = previous {
                previous(args);
            }
        })
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct WrDescriptorHeapStats {
    pub pages: usize,
    pub live_slots: usize,
    pub slots_per_page: u32,
    pub heap_pipelines: u64,
    pub heap_binds: u64,
    pub legacy_binds: u64,
    pub heap_draws: u64,
    pub legacy_draws: u64,
    pub diagnostics_enabled: bool,
}
#[derive(Debug)]
pub(super) struct Layout {
    pub mappings: Vec<Mapping>,
    pub offsets: Vec<(u32, bool, u32)>,
}
// All mapping pointers are null; the only non-null pointer is formed transiently by info().
unsafe impl Send for Layout {}
unsafe impl Sync for Layout {}
impl core::fmt::Debug for Mapping {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Mapping")
            .field("binding", &self.binding)
            .finish()
    }
}
impl Layout {
    pub fn info(&self) -> MappingInfo {
        MappingInfo {
            s_type: vk::StructureType::from_raw(1000135006),
            p_next: ptr::null(),
            count: self.mappings.len() as u32,
            mappings: self.mappings.as_ptr(),
        }
    }
}

#[derive(Debug)]
struct Buffer {
    raw: vk::Buffer,
    memory: vk::DeviceMemory,
    mapped: *mut u8,
    bind: BindInfo,
}
#[derive(Debug)]
struct Page {
    resources: Buffer,
    samplers: Buffer,
}
#[derive(Debug, Default)]
struct Pool {
    pages: Vec<Page>,
    free: Vec<(usize, u32)>,
    live: usize,
}
// The mapping is coherent; the pool mutex protects slot allocation and descriptor writes.
unsafe impl Send for Pool {}
pub(super) struct HeapDevice {
    raw: ash::Device,
    properties: Properties,
    memory: vk::PhysicalDeviceMemoryProperties,
    write_samplers: WriteSamplers,
    write_resources: WriteResources,
    get_address: vk::PFN_vkGetBufferDeviceAddress,
    bind_samplers: BindHeap,
    bind_resources: BindHeap,
    push_data: PushData,
    resource_stride: u64,
    sampler_stride: u64,
    resource_block: u64,
    sampler_block: u64,
    slots_per_page: u32,
    pool: Mutex<Pool>,
    pipelines: AtomicU64,
    #[cfg(any(
        feature = "wr-descriptor-heap-diagnostics",
        wr_descriptor_heap_diagnostics
    ))]
    counts: [AtomicU64; 4],
}
impl core::fmt::Debug for HeapDevice {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("WrDescriptorHeap")
            .field("resource_stride", &self.resource_stride)
            .field("sampler_stride", &self.sampler_stride)
            .finish_non_exhaustive()
    }
}
// Query pNext is null after construction; function pointers and immutable properties are shared.
unsafe impl Send for HeapDevice {}
unsafe impl Sync for HeapDevice {}
#[derive(Debug, Clone, Copy)]
pub(super) struct Binding {
    resources: BindInfo,
    samplers: BindInfo,
    offsets: [u32; 1],
}
// Both pNext fields are always null and buffers outlive every encoder via DeviceShared.
unsafe impl Send for Binding {}
unsafe impl Sync for Binding {}
impl Binding {
    pub fn page_key(self) -> (u64, u64) {
        (self.resources.range.address, self.samplers.range.address)
    }
}
#[derive(Debug, Default)]
pub(super) struct GroupState {
    pub set: Option<vk::DescriptorSet>,
    pub heap: Option<Binding>,
    pub layout: vk::DescriptorSetLayout,
    pub dynamic_offsets: Vec<u32>,
}
impl GroupState {
    pub fn clear(&mut self) {
        self.set = None;
        self.heap = None;
        self.layout = vk::DescriptorSetLayout::null();
        self.dynamic_offsets.clear();
    }
    pub fn remember(&mut self, group: &super::BindGroup, offsets: &[u32]) {
        self.set = group.set.as_ref().map(|set| set.raw());
        self.heap = group.heap.as_ref().map(|slice| slice.binding);
        self.layout = group.layout;
        self.dynamic_offsets.clear();
        self.dynamic_offsets.extend_from_slice(offsets);
    }
}
#[derive(Debug)]
pub(super) struct Slice {
    owner: Arc<HeapDevice>,
    page: usize,
    slot: u32,
    pub binding: Binding,
}
impl Drop for Slice {
    fn drop(&mut self) {
        let mut pool = self.owner.pool.lock();
        pool.live -= 1;
        pool.free.push((self.page, self.slot));
    }
}

fn heap_error(error: vk::Result) -> crate::DeviceError {
    match error {
        vk::Result::ERROR_DEVICE_LOST => crate::DeviceError::Lost,
        vk::Result::ERROR_OUT_OF_HOST_MEMORY | vk::Result::ERROR_OUT_OF_DEVICE_MEMORY => {
            crate::DeviceError::OutOfMemory
        }
        _ => {
            log::error!("WR descriptor heap: {error:?}");
            crate::DeviceError::Unexpected
        }
    }
}
fn align(value: u64, alignment: u64) -> u64 {
    (value + alignment - 1) / alignment * alignment
}
fn buffer_layout(
    payload: u64,
    reserved: u64,
    alignment: u64,
    limit: u64,
) -> Result<(u64, u64), crate::DeviceError> {
    if alignment == 0 {
        return Err(crate::DeviceError::Unexpected);
    }
    let offset = if reserved == 0 {
        0
    } else {
        payload
            .checked_add(alignment - 1)
            .ok_or(crate::DeviceError::OutOfMemory)?
            / alignment
            * alignment
    };
    let size = if reserved == 0 {
        payload
    } else {
        offset
            .checked_add(reserved)
            .ok_or(crate::DeviceError::OutOfMemory)?
    };
    if payload == 0 || size > limit || size > isize::MAX as u64 {
        return Err(crate::DeviceError::OutOfMemory);
    }
    Ok((offset, size))
}

fn slot_capacity(block: u64, reserved: u64, alignment: u64, limit: u64) -> u32 {
    if block == 0 || alignment == 0 {
        return 0;
    }
    let available = limit.min(isize::MAX as u64).saturating_sub(reserved);
    // The reserved tail begins at align_up(payload), so round its budget down first.
    let payload = if reserved == 0 {
        available
    } else {
        available / alignment * alignment
    };
    (payload / block).min(u64::from(MAX_SLOTS)) as u32
}

impl HeapDevice {
    fn new(device: &super::Device) -> Result<Self, crate::DeviceError> {
        let shared = &device.shared;
        let mut properties: Properties = unsafe { mem::zeroed() };
        properties.s_type = vk::StructureType::from_raw(1000135008);
        let mut query = vk::PhysicalDeviceProperties2::default();
        query.p_next = (&mut properties as *mut Properties).cast();
        unsafe {
            shared
                .instance
                .raw
                .get_physical_device_properties2(shared.physical_device, &mut query)
        };
        if properties.max_push_data < 4
            || [
                properties.resource_heap_alignment,
                properties.sampler_heap_alignment,
                properties.image_alignment,
                properties.buffer_alignment,
                properties.sampler_alignment,
                properties.image_size,
                properties.buffer_size,
                properties.sampler_size,
            ]
            .contains(&0)
        {
            return Err(crate::DeviceError::Unexpected);
        }
        let resource_stride = align(
            properties.image_size.max(properties.buffer_size),
            properties.image_alignment.max(properties.buffer_alignment),
        );
        let sampler_stride = align(properties.sampler_size, properties.sampler_alignment);
        let resource_block = MAX_BINDINGS as u64 * resource_stride;
        let sampler_block = MAX_BINDINGS as u64 * sampler_stride;
        let slots_per_page = slot_capacity(
            resource_block,
            properties.min_resource_reserved,
            properties
                .resource_heap_alignment
                .max(properties.image_alignment)
                .max(properties.buffer_alignment),
            properties.max_resource_heap_size,
        )
        .min(slot_capacity(
            sampler_block,
            properties.min_sampler_reserved,
            properties
                .sampler_heap_alignment
                .max(properties.sampler_alignment),
            properties.max_sampler_heap_size,
        ));
        if slots_per_page == 0 {
            return Err(crate::DeviceError::OutOfMemory);
        }
        macro_rules! load {
            ($name:literal, $ty:ty) => {{
                let function = unsafe {
                    shared
                        .instance
                        .raw
                        .get_device_proc_addr(shared.raw.handle(), $name.as_ptr())
                }
                .ok_or(crate::DeviceError::Unexpected)?;
                unsafe { mem::transmute::<unsafe extern "system" fn(), $ty>(function) }
            }};
        }
        Ok(Self {
            raw: shared.raw.clone(),
            properties,
            memory: unsafe {
                shared
                    .instance
                    .raw
                    .get_physical_device_memory_properties(shared.physical_device)
            },
            get_address: load!(
                c"vkGetBufferDeviceAddressKHR",
                vk::PFN_vkGetBufferDeviceAddress
            ),
            write_samplers: load!(c"vkWriteSamplerDescriptorsEXT", WriteSamplers),
            write_resources: load!(c"vkWriteResourceDescriptorsEXT", WriteResources),
            bind_samplers: load!(c"vkCmdBindSamplerHeapEXT", BindHeap),
            bind_resources: load!(c"vkCmdBindResourceHeapEXT", BindHeap),
            push_data: load!(c"vkCmdPushDataEXT", PushData),
            resource_stride,
            sampler_stride,
            resource_block,
            sampler_block,
            slots_per_page,
            pool: Mutex::new(Pool::default()),
            pipelines: AtomicU64::new(0),
            #[cfg(any(
                feature = "wr-descriptor-heap-diagnostics",
                wr_descriptor_heap_diagnostics
            ))]
            counts: core::array::from_fn(|_| AtomicU64::new(0)),
        })
    }
    pub fn layout(&self, entries: &[wgt::BindGroupLayoutEntry]) -> Option<Arc<Layout>> {
        let mut mappings = Vec::new();
        let mut offsets = Vec::new();
        let mut resources = 0;
        let mut samplers = 0;
        for (expected_binding, entry) in entries.iter().enumerate() {
            if entry.binding != expected_binding as u32 || entry.count.is_some() {
                return None;
            }
            let (sampler, mask) = match entry.ty {
                wgt::BindingType::Buffer {
                    ty: wgt::BufferBindingType::Uniform,
                    min_binding_size: Some(size),
                    has_dynamic_offset: false,
                } if size.get() == 64 => (false, 0x20),
                wgt::BindingType::Texture {
                    view_dimension: wgt::TextureViewDimension::D2,
                    multisampled: false,
                    ..
                } => (false, 0x2),
                wgt::BindingType::Sampler(
                    wgt::SamplerBindingType::Filtering | wgt::SamplerBindingType::NonFiltering,
                ) => (true, 0x1),
                _ => return None,
            };
            let (index, stride, block) = if sampler {
                let index = samplers;
                samplers += 1;
                (index, self.sampler_stride, self.sampler_block)
            } else {
                let index = resources;
                resources += 1;
                (index, self.resource_stride, self.resource_block)
            };
            if resources > MAX_BINDINGS || samplers > MAX_BINDINGS {
                return None;
            }
            let offset = index as u64 * stride;
            offsets.push((entry.binding, sampler, offset as u32));
            mappings.push(Mapping {
                s_type: vk::StructureType::from_raw(1000135005),
                p_next: ptr::null(),
                set: 0,
                binding: entry.binding,
                count: 1,
                mask,
                source: 1,
                data: MappingData {
                    push: PushIndex {
                        heap_offset: offset as u32,
                        push_offset: 0,
                        index_stride: block as u32,
                        array_stride: stride as u32,
                        embedded_sampler: ptr::null(),
                        combined: 0,
                        sampler_heap_offset: 0,
                        sampler_push_offset: 0,
                        sampler_index_stride: 0,
                        sampler_array_stride: 0,
                    },
                },
            });
        }
        Some(Arc::new(Layout { mappings, offsets }))
    }
    fn buffer(
        &self,
        payload: u64,
        reserved: u64,
        alignment: u64,
        limit: u64,
    ) -> Result<Buffer, crate::DeviceError> {
        let (offset, size) = buffer_layout(payload, reserved, alignment, limit)?;
        let raw = unsafe {
            self.raw.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(size)
                    .usage(HEAP_BUFFER_USAGE | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS),
                None,
            )
        }
        .map_err(heap_error)?;
        let requirements = unsafe { self.raw.get_buffer_memory_requirements(raw) };
        let Some(index) = (0..self.memory.memory_type_count).find(|index| {
            requirements.memory_type_bits & (1 << index) != 0
                && self.memory.memory_types[*index as usize]
                    .property_flags
                    .contains(
                        vk::MemoryPropertyFlags::HOST_VISIBLE
                            | vk::MemoryPropertyFlags::HOST_COHERENT,
                    )
        }) else {
            unsafe { self.raw.destroy_buffer(raw, None) };
            return Err(crate::DeviceError::Unexpected);
        };
        let mut flags =
            vk::MemoryAllocateFlagsInfo::default().flags(vk::MemoryAllocateFlags::DEVICE_ADDRESS);
        let memory = match unsafe {
            self.raw.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(requirements.size)
                    .memory_type_index(index)
                    .push_next(&mut flags),
                None,
            )
        } {
            Ok(memory) => memory,
            Err(error) => {
                unsafe { self.raw.destroy_buffer(raw, None) };
                return Err(heap_error(error));
            }
        };
        let result = (|| {
            unsafe { self.raw.bind_buffer_memory(raw, memory, 0) }.map_err(heap_error)?;
            let mapped = unsafe {
                self.raw
                    .map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
            }
            .map_err(heap_error)?
            .cast::<u8>();
            let address = unsafe {
                (self.get_address)(
                    self.raw.handle(),
                    &vk::BufferDeviceAddressInfo::default().buffer(raw),
                )
            };
            if address == 0 || address % alignment != 0 {
                unsafe { self.raw.unmap_memory(memory) };
                return Err(crate::DeviceError::Unexpected);
            }
            Ok(Buffer {
                raw,
                memory,
                mapped,
                bind: BindInfo {
                    s_type: vk::StructureType::from_raw(1000135003),
                    p_next: ptr::null(),
                    range: AddressRange { address, size },
                    reserved_offset: offset,
                    reserved_size: reserved,
                },
            })
        })();
        if result.is_err() {
            unsafe {
                self.raw.destroy_buffer(raw, None);
                self.raw.free_memory(memory, None);
            }
        }
        result
    }
    fn destroy_buffer(&self, buffer: &Buffer) {
        unsafe {
            self.raw.unmap_memory(buffer.memory);
            self.raw.destroy_buffer(buffer.raw, None);
            self.raw.free_memory(buffer.memory, None);
        }
    }
    pub fn group(
        self: &Arc<Self>,
        desc: &crate::BindGroupDescriptor<
            super::BindGroupLayout,
            super::Buffer,
            super::Sampler,
            super::TextureView,
            super::AccelerationStructure,
        >,
    ) -> Result<Slice, crate::DeviceError> {
        let layout = desc
            .layout
            .heap
            .as_ref()
            .ok_or(crate::DeviceError::Unexpected)?;
        let mut pool = self.pool.lock();
        if pool.free.is_empty() {
            if pool.pages.len() == MAX_PAGES {
                return Err(crate::DeviceError::OutOfMemory);
            }
            let resources = self.buffer(
                self.resource_block * u64::from(self.slots_per_page),
                self.properties.min_resource_reserved,
                self.properties
                    .resource_heap_alignment
                    .max(self.properties.image_alignment)
                    .max(self.properties.buffer_alignment),
                self.properties.max_resource_heap_size,
            )?;
            let samplers = match self.buffer(
                self.sampler_block * u64::from(self.slots_per_page),
                self.properties.min_sampler_reserved,
                self.properties
                    .sampler_heap_alignment
                    .max(self.properties.sampler_alignment),
                self.properties.max_sampler_heap_size,
            ) {
                Ok(buffer) => buffer,
                Err(error) => {
                    self.destroy_buffer(&resources);
                    return Err(error);
                }
            };
            let page = pool.pages.len();
            pool.pages.push(Page {
                resources,
                samplers,
            });
            pool.free
                .extend((0..self.slots_per_page).rev().map(|slot| (page, slot)));
        }
        let (page_index, slot) = pool.free.pop().unwrap();
        let page = &pool.pages[page_index];
        let resource_base = slot as u64 * self.resource_block;
        let sampler_base = slot as u64 * self.sampler_block;
        let result = (|| {
            for entry in desc.entries {
                if entry.count != 1 {
                    return Err(crate::DeviceError::Unexpected);
                }
                let &(_, sampler, relative) = layout
                    .offsets
                    .iter()
                    .find(|value| value.0 == entry.binding)
                    .ok_or(crate::DeviceError::Unexpected)?;
                let (buffer, base, size) = if sampler {
                    (&page.samplers, sampler_base, self.properties.sampler_size)
                } else {
                    (
                        &page.resources,
                        resource_base,
                        if matches!(
                            desc.layout
                                .entries
                                .iter()
                                .find(|value| value.binding == entry.binding)
                                .unwrap()
                                .ty,
                            wgt::BindingType::Buffer { .. }
                        ) {
                            self.properties.buffer_size
                        } else {
                            self.properties.image_size
                        },
                    )
                };
                let destination = HostRange {
                    address: unsafe { buffer.mapped.add((base + u64::from(relative)) as usize) }
                        .cast(),
                    size: size as usize,
                };
                let ty = desc
                    .layout
                    .entries
                    .iter()
                    .find(|value| value.binding == entry.binding)
                    .unwrap()
                    .ty;
                let code = match ty {
                    wgt::BindingType::Sampler(_) => unsafe {
                        (self.write_samplers)(
                            self.raw.handle(),
                            1,
                            &desc.samplers[entry.resource_index as usize].create_info,
                            &destination,
                        )
                    },
                    wgt::BindingType::Buffer { .. } => {
                        let source = &desc.buffers[entry.resource_index as usize];
                        let address = unsafe {
                            (self.get_address)(
                                self.raw.handle(),
                                &vk::BufferDeviceAddressInfo::default().buffer(source.buffer.raw),
                            )
                        };
                        if address == 0 {
                            return Err(crate::DeviceError::Unexpected);
                        }
                        let range = AddressRange {
                            address: address
                                .checked_add(source.offset)
                                .ok_or(crate::DeviceError::Unexpected)?,
                            size: source.size.get(),
                        };
                        let info = ResourceInfo {
                            s_type: vk::StructureType::from_raw(1000135002),
                            p_next: ptr::null(),
                            ty: vk::DescriptorType::UNIFORM_BUFFER,
                            data: ResourceData { address: &range },
                        };
                        unsafe { (self.write_resources)(self.raw.handle(), 1, &info, &destination) }
                    }
                    wgt::BindingType::Texture { .. } => {
                        let source = &desc.textures[entry.resource_index as usize];
                        let mut usage =
                            vk::ImageViewUsageCreateInfo::default().usage(source.view.heap_usage);
                        let mut view = source.view.heap_view;
                        if !source.view.heap_usage.is_empty() {
                            view = view.push_next(&mut usage);
                        }
                        let image = ImageInfo {
                            s_type: vk::StructureType::from_raw(1000135001),
                            p_next: ptr::null(),
                            view: (&view as *const vk::ImageViewCreateInfo<'_>).cast(),
                            layout: super::conv::derive_image_layout(
                                source.usage,
                                source.view.format,
                            ),
                        };
                        let info = ResourceInfo {
                            s_type: vk::StructureType::from_raw(1000135002),
                            p_next: ptr::null(),
                            ty: vk::DescriptorType::SAMPLED_IMAGE,
                            data: ResourceData { image: &image },
                        };
                        unsafe { (self.write_resources)(self.raw.handle(), 1, &info, &destination) }
                    }
                    _ => return Err(crate::DeviceError::Unexpected),
                };
                if code != vk::Result::SUCCESS {
                    return Err(heap_error(code));
                }
            }
            Ok(Binding {
                resources: page.resources.bind,
                samplers: page.samplers.bind,
                offsets: [slot],
            })
        })();
        match result {
            Ok(binding) => {
                pool.live += 1;
                Ok(Slice {
                    owner: self.clone(),
                    page: page_index,
                    slot,
                    binding,
                })
            }
            Err(error) => {
                pool.free.push((page_index, slot));
                Err(error)
            }
        }
    }
    pub unsafe fn bind(&self, command: vk::CommandBuffer, binding: Binding, rebind: bool) {
        unsafe {
            if rebind {
                (self.bind_resources)(command, &binding.resources);
                (self.bind_samplers)(command, &binding.samplers);
            }
            (self.push_data)(
                command,
                &PushInfo {
                    s_type: vk::StructureType::from_raw(1000135004),
                    p_next: ptr::null(),
                    offset: 0,
                    data: ConstHostRange {
                        address: binding.offsets.as_ptr().cast(),
                        size: mem::size_of_val(&binding.offsets),
                    },
                },
            );
        }
    }
    pub fn pipeline_created(&self) {
        self.pipelines.fetch_add(1, Ordering::Relaxed);
    }
    pub fn stats(&self) -> WrDescriptorHeapStats {
        let pool = self.pool.lock();
        let mut result = WrDescriptorHeapStats {
            pages: pool.pages.len(),
            live_slots: pool.live,
            slots_per_page: self.slots_per_page,
            heap_pipelines: self.pipelines.load(Ordering::Relaxed),
            ..Default::default()
        };
        #[cfg(any(
            feature = "wr-descriptor-heap-diagnostics",
            wr_descriptor_heap_diagnostics
        ))]
        {
            let counts = self
                .counts
                .each_ref()
                .map(|value| value.load(Ordering::Relaxed));
            result.heap_binds = counts[0];
            result.legacy_binds = counts[1];
            result.heap_draws = counts[2];
            result.legacy_draws = counts[3];
            result.diagnostics_enabled = true;
        }
        result
    }
    #[cfg(any(
        feature = "wr-descriptor-heap-diagnostics",
        wr_descriptor_heap_diagnostics
    ))]
    pub fn add_counts(&self, counts: [u64; 4]) {
        for (total, count) in self.counts.iter().zip(counts) {
            total.fetch_add(count, Ordering::Relaxed);
        }
    }
}
impl Drop for HeapDevice {
    fn drop(&mut self) {
        for page in self.pool.lock().pages.drain(..) {
            self.destroy_buffer(&page.resources);
            self.destroy_buffer(&page.samplers);
        }
    }
}
impl super::Device {
    /// Enable only on a fresh, dedicated trusted-WR device opened using DeviceFeatures::callback.
    /// Heap-only layouts must use one group, passthrough shaders, zero dynamic offsets and no immediates.
    /// The device must not be shared with WebGPU or use render bundles/compute with heap-only layouts.
    pub unsafe fn enable_wr_descriptor_heap(&self) -> Result<(), crate::DeviceError> {
        if !self.shared.enabled_extensions.contains(&NAME) {
            return Err(crate::DeviceError::Unexpected);
        }
        let mut heap = self.shared.wr_heap.lock();
        if heap.is_some() {
            return Err(crate::DeviceError::Unexpected);
        }
        *heap = Some(Arc::new(HeapDevice::new(self)?));
        Ok(())
    }
    pub fn wr_descriptor_heap_enabled(&self) -> bool {
        self.shared.wr_heap.lock().is_some()
    }
    pub fn wr_descriptor_heap_stats(&self) -> Option<WrDescriptorHeapStats> {
        self.shared.wr_heap.lock().as_ref().map(|heap| heap.stats())
    }
}

#[cfg(test)]
mod capacity_tests {
    use super::*;

    fn check_capacity(block: u64, reserved: u64, alignment: u64, limit: u64, expected: u32) {
        let slots = slot_capacity(block, reserved, alignment, limit);
        assert_eq!(slots, expected);
        if slots != 0 {
            assert!(buffer_layout(u64::from(slots) * block, reserved, alignment, limit).is_ok());
        }
        if slots < MAX_SLOTS {
            assert!(
                buffer_layout(u64::from(slots + 1) * block, reserved, alignment, limit).is_err()
            );
        }
    }

    #[test]
    fn intel_and_nvidia_heap_limits() {
        // Intel Xe: resource stride64 and sampler stride32, no reserved tails.
        let intel_resources = slot_capacity(16 * 64, 0, 64, 67_108_864);
        let intel_samplers = slot_capacity(16 * 32, 0, 64, 3_221_221_376);
        assert_eq!(intel_resources.min(intel_samplers), 256);
        // NVIDIA: resource stride32, sampler stride32; sampler tail consumes one slot.
        let nvidia_resources = slot_capacity(16 * 32, 96_768, 64, 33_554_432);
        let nvidia_samplers = slot_capacity(16 * 32, 512, 32, 131_072);
        assert_eq!(nvidia_resources.min(nvidia_samplers), 255);
        check_capacity(16 * 32, 512, 32, 131_072, 255);
        assert_eq!(
            buffer_layout(255 * 512, 512, 32, 131_072).unwrap(),
            (130_560, 131_072)
        );
    }

    #[test]
    fn reservation_alignment_zero_capacity_and_address_space() {
        check_capacity(64, 0, 256, 64, 1);
        assert_eq!(buffer_layout(64, 0, 256, 64).unwrap(), (0, 64));
        check_capacity(64, 1, 256, 257, 4);
        check_capacity(64, 1, 256, 256, 0);
        check_capacity(64, 513, 32, 512, 0);
        check_capacity(64, 0, 64, 63, 0);
        assert_eq!(slot_capacity(0, 0, 64, 1_024), 0);
        assert_eq!(slot_capacity(64, 0, 0, 1_024), 0);
        let block = isize::MAX as u64 / 2 + 1;
        assert_eq!(slot_capacity(block, 0, 1, u64::MAX), 1);
        assert!(buffer_layout(block * 2, 0, 1, u64::MAX).is_err());
    }
}
