/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::submission::{Submission, SubmissionQueue};
use std::cell::Cell;
use std::rc::Rc;
use api::{ImageFormat, units::DeviceIntRect};

pub(super) struct Owned<A: hal::Api, T> {
    owner: Rc<Device<A>>,
    raw: Option<T>,
    destroy: unsafe fn(&A::Device, T),
    memory: Option<(bool, u64)>,
}

impl<A: hal::Api, T> Owned<A, T> {
    pub fn new(owner: &Rc<Device<A>>, raw: T, destroy: unsafe fn(&A::Device, T)) -> Self {
        Self {
            owner: owner.clone(),
            raw: Some(raw),
            destroy,
            memory: None,
        }
    }
    pub(super) fn take(&mut self) -> T { assert!(self.memory.is_none()); self.raw.take().unwrap() }

    fn accounted(mut self, texture: bool, bytes: u64) -> Self {
        let mut memory = self.owner.memory.get();
        if texture {
            memory.textures += 1;
            memory.texture_bytes += bytes;
        } else {
            memory.buffers += 1;
            memory.buffer_bytes += bytes;
        }
        self.owner.memory.set(memory);
        self.memory = Some((texture, bytes));
        self
    }
}
impl<A: hal::Api, T> Deref for Owned<A, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.raw.as_ref().unwrap()
    }
}
impl<A: hal::Api, T> Drop for Owned<A, T> {
    fn drop(&mut self) {
        if let Some(raw) = self.raw.take() { unsafe { (self.destroy)(&self.owner.open.device, raw) } }
        if let Some((texture, bytes)) = self.memory {
            let mut memory = self.owner.memory.get();
            if texture {
                memory.textures -= 1;
                memory.texture_bytes -= bytes;
            } else {
                memory.buffers -= 1;
                memory.buffer_bytes -= bytes;
            }
            self.owner.memory.set(memory);
        }
    }
}

pub(super) struct Buffer<A: hal::Api> {
    pub raw: Owned<A, A::Buffer>,
    pub size: u64,
    pub allocation_id: u64,
    pub usage: wgt::BufferUses,
    mapping: Option<hal::BufferMapping>,
    used_size: Cell<u64>,
    state: Cell<wgt::BufferUses>,
    committed_state: Cell<wgt::BufferUses>,
}

impl<A: hal::Api> Buffer<A> {
    pub fn new(owner: &Rc<Device<A>>, bytes: &[u8], usage: wgt::BufferUses) -> Result<Rc<Self>> {
        let size = (bytes.len() as u64).max(4).next_power_of_two();
        if size > owner.capabilities.limits.max_buffer_size {
            return Err("HAL buffer exceeds device limit".into());
        }
        let allocation_id = owner.next_texture_id.get();
        let next_id = allocation_id.checked_add(1).ok_or("HAL buffer identity overflow")?;
        let device = &owner.open.device;
        let raw = unsafe {
            device.create_buffer(&hal::BufferDescriptor {
                label: Some("WR HAL buffer"),
                size,
                usage: usage | wgt::BufferUses::MAP_WRITE,
                memory_flags: hal::MemoryFlags::PREFER_COHERENT,
            })
        }
        .map_err(|e| format!("Creating buffer: {e:?}"))?;
        let raw = Owned::new(owner, raw, A::Device::destroy_buffer).accounted(false, size);
        let mapping = unsafe { device.map_buffer(&raw, 0..size) }
            .map_err(|e| format!("Mapping upload: {e:?}"))?;
        let buffer = Rc::new(Self {
            allocation_id,
            raw,
            size,
            usage: usage | wgt::BufferUses::MAP_WRITE,
            mapping: Some(mapping),
            used_size: Cell::new((bytes.len() as u64).max(4)),
            state: Cell::new(wgt::BufferUses::MAP_WRITE),
            committed_state: Cell::new(wgt::BufferUses::MAP_WRITE),
        });
        owner.next_texture_id.set(next_id);
        let mapping = buffer.mapping.as_ref().unwrap();
        unsafe {
            std::ptr::write_bytes(mapping.ptr.as_ptr(), 0, size as usize);
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), mapping.ptr.as_ptr(), bytes.len());
            if !mapping.is_coherent {
                device.flush_mapped_ranges(&buffer.raw, std::iter::once(0..size));
            }
        }
        Ok(buffer)
    }

    pub fn readback(owner: &Rc<Device<A>>, layout: &ReadbackLayout) -> Result<Rc<Self>> {
        let raw = unsafe {
            owner.open.device.create_buffer(&hal::BufferDescriptor {
                label: Some("WR readback"),
                size: layout.size,
                usage: wgt::BufferUses::COPY_DST | wgt::BufferUses::MAP_READ,
                memory_flags: hal::MemoryFlags::PREFER_COHERENT,
            })
        }
        .map_err(|e| format!("Creating readback: {e:?}"))?;
        let allocation_id = owner.next_texture_id.get();
        owner.next_texture_id.set(
            allocation_id
                .checked_add(1)
                .ok_or("HAL buffer identity overflow")?,
        );
        Ok(Rc::new(Self {
            allocation_id,
            raw: Owned::new(owner, raw, A::Device::destroy_buffer).accounted(false, layout.size),
            size: layout.size,
            usage: wgt::BufferUses::COPY_DST | wgt::BufferUses::MAP_READ,
            mapping: None,
            used_size: Cell::new(layout.size),
            state: Cell::new(wgt::BufferUses::COPY_DST),
            committed_state: Cell::new(wgt::BufferUses::COPY_DST),
        }))
    }

    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        if bytes.len() as u64 > self.size {
            return Err("HAL pooled buffer is too small".into());
        }
        let device = &self.raw.owner.open.device;
        let mapping = self.mapping.as_ref().ok_or("HAL buffer is not mapped for upload")?;
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), mapping.ptr.as_ptr(), bytes.len());
            if !mapping.is_coherent {
                device.flush_mapped_ranges(&self.raw, std::iter::once(0..self.size));
            }
        }
        self.used_size.set((bytes.len() as u64).max(4));
        self.state.set(wgt::BufferUses::MAP_WRITE);
        self.committed_state.set(wgt::BufferUses::MAP_WRITE);
        Ok(())
    }

    pub fn transition(self: &Rc<Self>, commands: &mut Submission<A>, to: wgt::BufferUses) {
        commands.keep(self.clone());
        let resource = self.clone();
        commands.commit(move || resource.committed_state.set(to));
        let from = self.state.replace(to);
        if from != to {
            unsafe {
                commands
                    .encoder()
                    .transition_buffers(std::iter::once(hal::BufferBarrier {
                        buffer: &*self.raw,
                        usage: hal::StateTransition { from, to },
                    }))
            }
        }
    }
    pub fn binding(&self) -> hal::BufferBinding<'_, A::Buffer> {
        hal::BufferBinding::new_unchecked(
            &*self.raw,
            0,
            std::num::NonZeroU64::new(self.used_size.get()),
        )
    }
}

impl<A: hal::Api> Drop for Buffer<A> {
    fn drop(&mut self) {
        if self.mapping.take().is_some() {
            unsafe { self.raw.owner.open.device.unmap_buffer(&self.raw); }
        }
    }
}

struct TextureState {
    usage: Cell<wgt::TextureUses>,
    committed: Cell<wgt::TextureUses>,
    initialized: Cell<bool>,
    committed_initialized: Cell<bool>,
}

pub(super) struct Texture<A: hal::Api> {
    pub view: Owned<A, A::TextureView>,
    pub target: Option<Owned<A, A::TextureView>>,
    pub raw: Rc<Owned<A, A::Texture>>,
    pub size: wgt::Extent3d,
    pub format: wgt::TextureFormat,
    pub filter: crate::device::TextureFilter,
    pub allocation_id: u64,
    pub transient: Cell<bool>,
    pub base_mip: u32,
    pub mip_count: u32,
    aspect: wgt::TextureAspect,
    states: Rc<Vec<TextureState>>,
    lease: Option<Rc<super::external::LeaseState>>,
}

pub(super) fn texture_format(format: ImageFormat) -> Result<wgt::TextureFormat> {
    Ok(match format {
        ImageFormat::RGBA8 => wgt::TextureFormat::Rgba8Unorm,
        ImageFormat::BGRA8 => wgt::TextureFormat::Bgra8Unorm,
        ImageFormat::R8 => wgt::TextureFormat::R8Unorm,
        ImageFormat::RG8 => wgt::TextureFormat::Rg8Unorm,
        ImageFormat::R16 => wgt::TextureFormat::R16Unorm,
        ImageFormat::RG16 => wgt::TextureFormat::Rg16Unorm,
        ImageFormat::RGBAF32 => wgt::TextureFormat::Rgba32Float,
        ImageFormat::RGBAI32 => wgt::TextureFormat::Rgba32Sint,
    })
}

pub(super) fn bytes_per_pixel(format: wgt::TextureFormat) -> usize {
    match format {
        wgt::TextureFormat::R8Unorm => 1,
        wgt::TextureFormat::Rg8Unorm | wgt::TextureFormat::R16Unorm => 2,
        wgt::TextureFormat::Rg16Unorm => 4,
        wgt::TextureFormat::Rgba32Float | wgt::TextureFormat::Rgba32Sint => 16,
        _ => 4,
    }
}

impl<A: hal::Api> Texture<A> {
    pub fn new(
        owner: &Rc<Device<A>>,
        width: u32,
        height: u32,
        format: wgt::TextureFormat,
        filter: crate::device::TextureFilter,
        renderable: bool,
    ) -> Result<Rc<Self>> {
        owner.layout(width, height)?;
        if !owner.features.contains(format.required_features()) {
            return Err(format!("HAL device lacks features for {format:?}"));
        }
        let mip_count = if filter == crate::device::TextureFilter::Trilinear {
            32 - width.max(height).leading_zeros()
        } else {
            1
        };
        let renderable = renderable || mip_count > 1;
        let depth = format == wgt::TextureFormat::Depth32Float;
        let target_usage = if depth {
            wgt::TextureUses::DEPTH_WRITE
        } else {
            wgt::TextureUses::COLOR_TARGET
        };
        let mut usage = wgt::TextureUses::COPY_SRC | wgt::TextureUses::COPY_DST;
        if !depth {
            usage |= wgt::TextureUses::RESOURCE;
        }
        if renderable {
            usage |= target_usage;
        }
        let mut required =
            hal::TextureFormatCapabilities::COPY_SRC | hal::TextureFormatCapabilities::COPY_DST;
        if !depth {
            required |= hal::TextureFormatCapabilities::SAMPLED;
        }
        if renderable {
            required |= if depth {
                hal::TextureFormatCapabilities::DEPTH_STENCIL_ATTACHMENT
            } else {
                hal::TextureFormatCapabilities::COLOR_ATTACHMENT
                    | hal::TextureFormatCapabilities::COLOR_ATTACHMENT_BLEND
            };
        }
        if filter != crate::device::TextureFilter::Nearest {
            required |= hal::TextureFormatCapabilities::SAMPLED_LINEAR;
        }
        let caps = owner
            .formats
            .iter()
            .find(|(f, _)| *f == format)
            .ok_or("Unknown HAL texture format")?
            .1;
        if !caps.contains(required) {
            return Err(format!(
                "Unsupported HAL format usages {format:?}: {required:?}"
            ));
        }
        let size = wgt::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let device = &owner.open.device;
        let mut descriptor = texture_descriptor(size, format, usage);
        descriptor.mip_level_count = mip_count;
        let raw = unsafe { device.create_texture(&descriptor) }
            .map_err(|e| format!("Creating {format:?} texture: {e:?}"))?;
        Self::from_raw(owner, raw, &descriptor, filter, renderable, wgt::TextureUses::UNINITIALIZED)
    }

    pub fn from_raw(
        owner: &Rc<Device<A>>, raw: A::Texture, descriptor: &hal::TextureDescriptor,
        filter: crate::device::TextureFilter, renderable: bool, initial_usage: wgt::TextureUses,
    ) -> Result<Rc<Self>> {
        let size = descriptor.size;
        let (width, height) = (size.width, size.height);
        let format = descriptor.format;
        let mip_count = descriptor.mip_level_count;
        let depth = format == wgt::TextureFormat::Depth32Float;
        let target_usage = if depth { wgt::TextureUses::DEPTH_WRITE } else { wgt::TextureUses::COLOR_TARGET };
        let device = &owner.open.device;
        let bytes = (0..mip_count)
            .map(|level| {
                u64::from((width >> level).max(1))
                    * u64::from((height >> level).max(1))
                    * bytes_per_pixel(format) as u64
            })
            .sum();
        let raw =
            Rc::new(Owned::new(owner, raw, A::Device::destroy_texture).accounted(true, bytes));
        let view = |usage, levels| -> Result<_> {
            let raw_view = unsafe {
                device.create_texture_view(
                    &raw,
                    &hal::TextureViewDescriptor {
                        swizzle: Default::default(),
                        label: Some("WR HAL view"),
                        format,
                        dimension: wgt::TextureViewDimension::D2,
                        usage,
                        range: wgt::ImageSubresourceRange {
                            mip_level_count: Some(levels),
                            array_layer_count: Some(1),
                            ..Default::default()
                        },
                    },
                )
            }
            .map_err(|e| format!("Creating view: {e:?}"))?;
            Ok(Owned::new(owner, raw_view, A::Device::destroy_texture_view))
        };
        let sample_view = view(
            if depth {
                target_usage
            } else {
                wgt::TextureUses::RESOURCE
            },
            mip_count,
        )?;
        let target = if renderable {
            Some(view(target_usage, 1)?)
        } else {
            None
        };
        let allocation_id = owner.next_texture_id.get();
        owner.next_texture_id.set(
            allocation_id
                .checked_add(1)
                .ok_or("HAL texture identity overflow")?,
        );
        Ok(Rc::new(Self {
            allocation_id,
            transient: Cell::new(false),
            view: sample_view,
            target,
            raw,
            size,
            format,
            filter,
            base_mip: 0,
            lease: None,
            mip_count,
            aspect: wgt::TextureAspect::All,
            states: Rc::new(
                (0..mip_count)
                    .map(|_| TextureState {
                        usage: Cell::new(initial_usage),
                        committed: Cell::new(initial_usage),
                        initialized: Cell::new(initial_usage != wgt::TextureUses::UNINITIALIZED),
                        committed_initialized: Cell::new(initial_usage != wgt::TextureUses::UNINITIALIZED),
                    })
                    .collect(),
            ),
        }))
    }

    pub fn belongs_to(&self, owner: &Rc<Device<A>>) -> bool {
        Rc::ptr_eq(&self.raw.owner, owner)
    }

    #[cfg(all(target_os = "linux", feature = "hal-linux-dmabuf"))]
    pub fn from_yuv(
        owner: &Rc<Device<A>>, raw: A::Texture, size: [u32; 2], bytes: u64,
        formats: [wgt::TextureFormat; 2],
    ) -> Result<[Rc<Self>; 2]> {
        let raw = Rc::new(Owned::new(owner, raw, A::Device::destroy_texture).accounted(true, bytes));
        let states = Rc::new(vec![TextureState {
            usage: Cell::new(wgt::TextureUses::RESOURCE),
            committed: Cell::new(wgt::TextureUses::RESOURCE),
            initialized: Cell::new(true),
            committed_initialized: Cell::new(true),
        }]);
        let allocation_id = owner.next_texture_id.get();
        owner.next_texture_id.set(allocation_id.checked_add(1).ok_or("HAL texture identity overflow")?);
        let plane = |index: u32, aspect, format| -> Result<Rc<Self>> {
            let view = unsafe { owner.open.device.create_texture_view(&raw, &hal::TextureViewDescriptor {
                label: Some("WR YUV plane"), swizzle: Default::default(),
                format, dimension: wgt::TextureViewDimension::D2, usage: wgt::TextureUses::RESOURCE,
                range: wgt::ImageSubresourceRange {
                    aspect, mip_level_count: Some(1), array_layer_count: Some(1), ..Default::default()
                },
            }) }.map_err(|error| format!("Creating YUV plane view: {error:?}"))?;
            Ok(Rc::new(Self {
                view: Owned::new(owner, view, A::Device::destroy_texture_view), target: None,
                raw: raw.clone(), size: wgt::Extent3d {
                    width: size[0] >> index, height: size[1] >> index, depth_or_array_layers: 1,
                },
                format, filter: crate::device::TextureFilter::Linear, allocation_id,
                transient: Cell::new(true), base_mip: 0, mip_count: 1, aspect,
                states: states.clone(), lease: None,
            }))
        };
        let y = plane(0, wgt::TextureAspect::Plane0, formats[0])?;
        #[cfg(any(test, feature = "hal-testing"))]
        owner.check_fault(FailurePoint::VideoPlaneView)?;
        let uv = plane(1, wgt::TextureAspect::Plane1, formats[1])?;
        Ok([y, uv])
    }

    pub fn copy_aspect(&self) -> hal::FormatAspects {
        match self.aspect {
            wgt::TextureAspect::Plane0 => hal::FormatAspects::PLANE_0,
            wgt::TextureAspect::Plane1 => hal::FormatAspects::PLANE_1,
            _ => hal::FormatAspects::COLOR,
        }
    }

    pub fn mip_view(self: &Rc<Self>, level: u32) -> Result<Rc<Self>> {
        if self.target.is_none() {
            return Err("HAL mip views require renderable texture storage".into());
        }
        if level >= self.mip_count {
            return Err("Invalid HAL mip view".into());
        }
        let base_mip = self.base_mip + level;
        let owner = &self.raw.owner;
        let view = |usage| {
            let raw = unsafe {
                owner.open.device.create_texture_view(
                    &self.raw,
                    &hal::TextureViewDescriptor {
                        swizzle: Default::default(),
                        label: Some("WR mip view"),
                        format: self.format,
                        dimension: wgt::TextureViewDimension::D2,
                        usage,
                        range: wgt::ImageSubresourceRange {
                            base_mip_level: base_mip,
                            mip_level_count: Some(1),
                            array_layer_count: Some(1),
                            ..Default::default()
                        },
                    },
                )
            }
            .map_err(|e| format!("Creating mip view: {e:?}"))?;
            Ok::<_, String>(Owned::new(owner, raw, A::Device::destroy_texture_view))
        };
        Ok(Rc::new(Self {
            view: view(wgt::TextureUses::RESOURCE)?,
            target: Some(view(wgt::TextureUses::COLOR_TARGET)?),
            raw: self.raw.clone(),
            size: wgt::Extent3d {
                width: (self.size.width >> level).max(1),
                height: (self.size.height >> level).max(1),
                depth_or_array_layers: 1,
            },
            format: self.format,
            filter: crate::device::TextureFilter::Linear,
            allocation_id: self.allocation_id,
            transient: Cell::new(self.transient.get()),
            base_mip,
            mip_count: 1,
            aspect: self.aspect,
            states: self.states.clone(),
            lease: self.lease.clone(),
        }))
    }

    pub fn current_usage(&self) -> wgt::TextureUses {
        self.states[self.base_mip as usize].usage.get()
    }

    pub fn with_lease(&self, lease: Rc<super::external::LeaseState>, filter: crate::device::TextureFilter, opaque: bool) -> Result<Rc<Self>> {
        let owner = &self.raw.owner;
        let view = |usage, levels| {
            let mut swizzle = wgt::TextureComponentSwizzle::default();
            if opaque && usage == wgt::TextureUses::RESOURCE
                && owner.info.backend == wgt::Backend::Vulkan
                && matches!(self.format, wgt::TextureFormat::Rgba8Unorm | wgt::TextureFormat::Bgra8Unorm) {
                swizzle.a = wgt::ComponentSwizzle::One;
            }
            let raw = unsafe { owner.open.device.create_texture_view(&self.raw, &hal::TextureViewDescriptor {
                swizzle,
                label: Some("WR acquired image view"), format: self.format, dimension: wgt::TextureViewDimension::D2, usage,
                range: wgt::ImageSubresourceRange { aspect: self.aspect, base_mip_level: self.base_mip, mip_level_count: Some(levels), array_layer_count: Some(1), ..Default::default() },
            }) }.map_err(|error| format!("Creating acquired image view: {error:?}"))?;
            Ok::<_, String>(Owned::new(owner, raw, A::Device::destroy_texture_view))
        };
        Ok(Rc::new(Self {
            view: view(wgt::TextureUses::RESOURCE, self.mip_count)?,
            target: if self.target.is_some() { Some(view(wgt::TextureUses::COLOR_TARGET, 1)?) } else { None },
            raw: self.raw.clone(), size: self.size, format: self.format, filter,
            allocation_id: self.allocation_id, transient: Cell::new(true), base_mip: self.base_mip,
            mip_count: self.mip_count, aspect: self.aspect, states: self.states.clone(), lease: Some(lease),
        }))
    }

    pub fn overlaps(&self, target: &Self) -> bool {
        Rc::ptr_eq(&self.raw, &target.raw)
            && target.base_mip >= self.base_mip
            && target.base_mip < self.base_mip + self.mip_count
    }

    pub fn initialized(&self) -> bool {
        self.states[self.base_mip as usize].initialized.get()
    }

    pub fn sample_initialized(&self) -> bool {
        (self.base_mip..self.base_mip + self.mip_count)
            .all(|level| self.states[level as usize].initialized.get())
    }

    pub fn invalidate(self: &Rc<Self>, commands: &mut Submission<A>) {
        for level in self.base_mip..self.base_mip + self.mip_count {
            self.states[level as usize].initialized.set(false);
            let texture = self.clone();
            commands.commit(move || {
                texture.states[level as usize]
                    .committed_initialized
                    .set(false)
            });
        }
    }

    pub fn initialize(self: &Rc<Self>, commands: &mut Submission<A>) {
        self.states[self.base_mip as usize].initialized.set(true);
        let texture = self.clone();
        commands.commit(move || {
            texture.states[texture.base_mip as usize]
                .committed_initialized
                .set(true)
        });
    }

    #[cfg(test)]
    pub fn committed_usage(&self) -> wgt::TextureUses {
        self.states[self.base_mip as usize].committed.get()
    }

    pub fn transition(self: &Rc<Self>, commands: &mut Submission<A>, to: wgt::TextureUses) {
        commands.keep(self.clone());
        if let Some(lease) = &self.lease { lease.track(commands); }
        let count = if to == wgt::TextureUses::RESOURCE {
            self.mip_count
        } else {
            1
        };
        for level in self.base_mip..self.base_mip + count {
            let resource = self.clone();
            commands.commit(move || resource.states[level as usize].committed.set(to));
            let from = self.states[level as usize].usage.replace(to);
            if from != to {
                unsafe {
                    commands
                        .encoder()
                        .transition_textures(std::iter::once(hal::TextureBarrier {
                            queue_family_ownership_transfer: None,
                            texture: &**self.raw,
                            range: wgt::ImageSubresourceRange {
                                base_mip_level: level,
                                mip_level_count: Some(1),
                                array_layer_count: Some(1),
                                ..Default::default()
                            },
                            usage: hal::StateTransition { from, to },
                        }));
                }
            }
        }
    }

    #[cfg(test)]
    pub fn upload(
        self: &Rc<Self>,
        owner: &Rc<Device<A>>,
        rect: DeviceIntRect,
        data: &[u8],
        stride: Option<i32>,
        offset: i32,
        source_format: Option<ImageFormat>,
    ) -> Result<()> {
        let queue = SubmissionQueue::new(owner, 3, false);
        self.upload_recorded(owner, &queue, rect, data, stride, offset, source_format)?;
        queue.wait()
    }

    pub fn upload_recorded(
        self: &Rc<Self>,
        owner: &Rc<Device<A>>,
        queue: &SubmissionQueue<A>,
        rect: DeviceIntRect,
        data: &[u8],
        stride: Option<i32>,
        offset: i32,
        source_format: Option<ImageFormat>,
    ) -> Result<()> {
        if self.aspect != wgt::TextureAspect::All {
            return Err("Cannot upload into a foreign video plane".into());
        }
        if rect.min.x < 0
            || rect.min.y < 0
            || rect.width() <= 0
            || rect.height() <= 0
            || rect.max.x as u32 > self.size.width
            || rect.max.y as u32 > self.size.height
            || offset < 0
        {
            return Err("Invalid HAL upload rectangle/offset".into());
        }
        if !Rc::ptr_eq(&self.raw.owner, owner) {
            return Err("HAL upload device mismatch".into());
        }
        let bpp = bytes_per_pixel(self.format);
        let row_bytes = rect.width() as usize * bpp;
        let source_stride = usize::try_from(stride.unwrap_or(row_bytes as i32))
            .map_err(|_| "Invalid upload stride")?;
        let end = (offset as usize)
            .checked_add(
                source_stride
                    .checked_mul(rect.height() as usize - 1)
                    .ok_or("Upload size overflow")?,
            )
            .and_then(|n| n.checked_add(row_bytes))
            .ok_or("Upload size overflow")?;
        if source_stride < row_bytes || end > data.len() {
            return Err("Upload source is too short".into());
        }
        let source_format = source_format
            .map(texture_format)
            .transpose()?
            .unwrap_or(self.format);
        let swizzle = source_format != self.format;
        if swizzle
            && !matches!(
                (source_format, self.format),
                (
                    wgt::TextureFormat::Rgba8Unorm,
                    wgt::TextureFormat::Bgra8Unorm
                ) | (
                    wgt::TextureFormat::Bgra8Unorm,
                    wgt::TextureFormat::Rgba8Unorm
                )
            )
        {
            return Err("Unsupported HAL upload conversion".into());
        }
        let alignment = owner.capabilities.alignments.buffer_copy_pitch.get() as usize;
        let destination = if self.initialized() {
            rect
        } else {
            DeviceIntRect::from_size(api::units::DeviceIntSize::new(
                self.size.width as i32,
                self.size.height as i32,
            ))
        };
        let pitch = (destination.width() as usize * bpp).div_ceil(alignment) * alignment;
        let packed_size = pitch
            .checked_mul(destination.height() as usize)
            .ok_or("HAL upload size overflow")?;
        if packed_size as u64 > owner.capabilities.limits.max_buffer_size
            || packed_size > isize::MAX as usize
        {
            return Err("HAL upload exceeds buffer limits".into());
        }
        let packed = if !swizzle
            && destination == rect
            && source_stride == row_bytes
            && pitch == row_bytes
        {
            std::borrow::Cow::Borrowed(&data[offset as usize..end])
        } else {
            let mut packed = vec![0; packed_size];
            for y in 0..rect.height() as usize {
                let src = offset as usize + y * source_stride;
                let start = (y + (rect.min.y - destination.min.y) as usize) * pitch
                    + (rect.min.x - destination.min.x) as usize * bpp;
                let dst = &mut packed[start..start + row_bytes];
                dst.copy_from_slice(&data[src..src + row_bytes]);
                if swizzle {
                    for pixel in dst.chunks_exact_mut(4) {
                        pixel.swap(0, 2);
                    }
                }
            }
            std::borrow::Cow::Owned(packed)
        };
        let (mut commands, staging) = queue.upload_recording(&packed, wgt::BufferUses::COPY_SRC)?;
        staging.transition(&mut commands, wgt::BufferUses::COPY_SRC);
        self.transition(&mut commands, wgt::TextureUses::COPY_DST);
        unsafe {
            commands.encoder().copy_buffer_to_texture(
                &staging.raw,
                &self.raw,
                std::iter::once(hal::BufferTextureCopy {
                    buffer_layout: wgt::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(pitch as u32),
                        rows_per_image: Some(destination.height() as u32),
                    },
                    texture_base: hal::TextureCopyBase {
                        mip_level: self.base_mip,
                        array_layer: 0,
                        origin: wgt::Origin3d {
                            x: destination.min.x as u32,
                            y: destination.min.y as u32,
                            z: 0,
                        },
                        aspect: hal::FormatAspects::COLOR,
                    },
                    size: wgt::Extent3d {
                        width: destination.width() as u32,
                        height: destination.height() as u32,
                        depth_or_array_layers: 1,
                    }
                    .into(),
                }),
            );
        }
        self.transition(&mut commands, wgt::TextureUses::RESOURCE);
        self.initialize(&mut commands);
        Ok(())
    }
}

#[cfg(all(test, wr_hal_vulkan))]
mod tests {
    use super::*;

    #[test]
    #[ignore = "Requires Vulkan"]
    fn persistent_upload_mappings_preserve_pool_ownership() {
        let owner = Rc::new(create_vulkan_device(&Options { validation: true, ..Options::default() }).unwrap());
        let pool = super::super::pool::BufferPool::new(&owner);
        let first = pool.upload(&[3, 5, 7], wgt::BufferUses::COPY_SRC).unwrap();
        let pointer = first.mapping.as_ref().unwrap().ptr;
        let second = pool.upload(&[11, 13, 17], wgt::BufferUses::COPY_SRC).unwrap();
        assert!(!Rc::ptr_eq(&first, &second));
        unsafe {
            assert_eq!(std::slice::from_raw_parts(pointer.as_ptr(), 4), &[3, 5, 7, 0]);
        }
        drop(first);
        drop(second);
        let reused = pool.upload(&[19, 23, 29, 31], wgt::BufferUses::COPY_SRC).unwrap();
        assert_eq!(reused.mapping.as_ref().unwrap().ptr, pointer);
        unsafe {
            assert_eq!(std::slice::from_raw_parts(pointer.as_ptr(), 4), &[19, 23, 29, 31]);
        }
        drop(reused);
        pool.clear();
        assert_eq!(owner.memory.get().buffers, 0);
        let readback = Buffer::readback(&owner, &owner.layout(2, 2).unwrap()).unwrap();
        assert!(readback.mapping.is_none());
        assert!(readback.write(&[0; 16]).is_err());
    }

    #[test]
    #[ignore = "Requires Vulkan"]
    fn contiguous_uploads_preserve_offsets_and_conversion() {
        let owner = Rc::new(
            create_vulkan_device(&Options {
                validation: true,
                ..Options::default()
            })
            .unwrap(),
        );
        let width = (owner.capabilities.alignments.buffer_copy_pitch.get() / 4).max(2);
        let row = width as usize * 4;
        for format in [ImageFormat::RGBA8, ImageFormat::BGRA8] {
            for padding in [0, 4] {
                let queue = SubmissionQueue::new(&owner, 3, false);
                let texture = Texture::new(
                    &owner,
                    width,
                    2,
                    wgt::TextureFormat::Rgba8Unorm,
                    crate::device::TextureFilter::Nearest,
                    false,
                )
                .unwrap();
                let stride = row + padding;
                let mut source = vec![0xa5; 7 + stride + row];
                let color = if format == ImageFormat::RGBA8 {
                    [23, 47, 89, 255]
                } else {
                    [89, 47, 23, 255]
                };
                for y in 0..2 {
                    for pixel in source[7 + y * stride..7 + y * stride + row].chunks_exact_mut(4) {
                        pixel.copy_from_slice(&color);
                    }
                }
                texture
                    .upload_recorded(
                        &owner,
                        &queue,
                        DeviceIntRect::from_size(api::units::DeviceIntSize::new(width as i32, 2)),
                        &source,
                        Some(stride as i32),
                        7,
                        Some(format),
                    )
                    .unwrap();
                let layout = owner.layout(width, 2).unwrap();
                let readback = Buffer::readback(&owner, &layout).unwrap();
                let mut commands = queue.recording().unwrap();
                commands.keep(readback.clone());
                texture.transition(&mut commands, wgt::TextureUses::COPY_SRC);
                unsafe {
                    copy_readback::<wgpu_hal::api::Vulkan>(
                        commands.encoder(),
                        &texture.raw,
                        &readback.raw,
                        &layout,
                        texture.size,
                        hal::FormatAspects::COLOR,
                    );
                }
                readback.transition(&mut commands, wgt::BufferUses::MAP_READ);
                drop(commands);
                queue.wait().unwrap();
                assert_eq!(
                    owner.map_readback(&readback.raw, &layout).unwrap(),
                    [23, 47, 89, 255].repeat(width as usize * 2)
                );
            }
        }
    }

    #[test]
    #[ignore = "Requires Vulkan"]
    fn rejects_cross_device_and_invalid_uploads() {
        let first = Rc::new(create_vulkan_device(&Options::default()).unwrap());
        let second = Rc::new(create_vulkan_device(&Options::default()).unwrap());
        let texture = Texture::new(
            &first,
            2,
            2,
            wgt::TextureFormat::Rgba8Unorm,
            crate::device::TextureFilter::Nearest,
            false,
        )
        .unwrap();
        let rect = DeviceIntRect::from_size(api::units::DeviceIntSize::new(2, 2));
        assert!(texture
            .upload(&second, rect, &[0; 16], None, 0, None)
            .unwrap_err()
            .contains("device mismatch"));
        assert!(texture
            .upload(&first, rect, &[0; 15], None, 0, None)
            .is_err());
        assert!(texture
            .upload(&first, rect, &[0; 16], Some(4), 0, None)
            .is_err());
        assert!(texture
            .upload(&first, rect, &[0; 16], None, -1, None)
            .is_err());
        assert_eq!(
            texture.states[0].usage.get(),
            wgt::TextureUses::UNINITIALIZED
        );
    }
    #[test]
    #[ignore = "Requires Vulkan"]
    fn first_partial_upload_initializes_padding() {
        let owner = Rc::new(
            create_vulkan_device(&Options {
                validation: true,
                ..Options::default()
            })
            .unwrap(),
        );
        let queue = SubmissionQueue::new(&owner, 3, false);
        let texture = Texture::new(
            &owner,
            7,
            5,
            wgt::TextureFormat::Rgba8Unorm,
            crate::device::TextureFilter::Nearest,
            false,
        )
        .unwrap();
        let rect = DeviceIntRect::from_origin_and_size(
            api::units::DeviceIntPoint::new(2, 1),
            api::units::DeviceIntSize::new(1, 1),
        );
        texture
            .upload_recorded(&owner, &queue, rect, &[10, 20, 30, 255], None, 0, None)
            .unwrap();
        assert!(texture.initialized());
        assert!(!texture.states[0].committed_initialized.get());
        let layout = owner.layout(7, 5).unwrap();
        let readback = Buffer::readback(&owner, &layout).unwrap();
        let mut commands = queue.recording().unwrap();
        commands.keep(readback.clone());
        texture.transition(&mut commands, wgt::TextureUses::COPY_SRC);
        unsafe {
            copy_readback::<wgpu_hal::api::Vulkan>(
                commands.encoder(),
                &texture.raw,
                &readback.raw,
                &layout,
                texture.size,
                hal::FormatAspects::COLOR,
            );
        }
        readback.transition(&mut commands, wgt::BufferUses::MAP_READ);
        drop(commands);
        queue.wait().unwrap();
        assert!(texture.states[0].committed_initialized.get());
        let pixels = owner.map_readback(&readback.raw, &layout).unwrap();
        for (index, pixel) in pixels.chunks_exact(4).enumerate() {
            assert_eq!(
                pixel,
                if index == 9 {
                    &[10, 20, 30, 255]
                } else {
                    &[0; 4]
                }
            );
        }
    }
}
