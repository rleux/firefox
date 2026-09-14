/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::backend::{BackendApi, ShaderCache, ShaderInputMode};
use super::external::{validate_descriptor, ExternalImageDevice, NativeImage, Producer};
use super::resources::{Owned, Texture};
use super::submission::{CompletionCheck, SubmissionSync};
use super::*;
use objc2::{
    rc::{autoreleasepool, Retained},
    runtime::ProtocolObject,
    MainThreadMarker,
};
use objc2_core_foundation::CFRetained;
use objc2_io_surface::IOSurfaceRef;
use objc2_metal::{
    MTLCommandBuffer, MTLCommandBufferStatus, MTLCommandQueue, MTLDevice, MTLPixelFormat,
    MTLResource, MTLSharedEvent, MTLSharedEventHandle, MTLStorageMode, MTLTexture,
    MTLTextureDescriptor, MTLTextureType, MTLTextureUsage,
};
use objc2_quartz_core::CAMetalLayer;
use std::rc::Rc;
type M = hal::api::Metal;

impl super::backend::sealed::Sealed for M {}
impl BackendApi for M {
    fn create_device(
        options: &Options,
        window: Option<Rc<dyn SurfaceWindow>>,
    ) -> Result<(Device<Self>, Option<super::surface::SurfaceSetup<Self>>)> {
        validate_options(options, window.is_some())?;
        autoreleasepool(|_| {
            let (mut device, surface) = Device::new_with_window(options, window)?;
            device.completion_probe = Some(completion_probe);
            Ok((device, surface))
        })
    }
    fn create_metal_layer_surface(
        instance: &Self::Instance,
        layer: &CAMetalLayer,
    ) -> Result<Self::Surface> {
        MainThreadMarker::new()
            .ok_or("Metal-layer surface creation must run on the main thread")?;
        Ok(instance.create_surface_from_layer(layer))
    }
    fn timestamp_valid_bits(device: &Device<Self>) -> u32 {
        if device.features.contains(wgt::Features::TIMESTAMP_QUERY) {
            64
        } else {
            0
        }
    }
    fn shader_input() -> Result<ShaderInputMode> {
        match std::env::var("WR_HAL_SHADER_INPUT") {
            Err(std::env::VarError::NotPresent) => Ok(ShaderInputMode::Naga),
            Ok(value) if value == "naga" => Ok(ShaderInputMode::Naga),
            _ => Err("Metal requires Naga shader input; raw Vulkan SPIR-V is unavailable".into()),
        }
    }
    fn create_shader_module(
        device: &Self::Device,
        artifact: &webrender_build::hal::ShaderArtifact,
        fragment: bool,
        mode: ShaderInputMode,
        cache: &mut ShaderCache,
    ) -> Result<Self::ShaderModule> {
        if mode != ShaderInputMode::Naga {
            return Err("Metal requires Naga shader input".into());
        }
        cache.create_module::<Self>(device, artifact, fragment, mode)
    }
}

fn validate_options(options: &Options, surface: bool) -> Result<()> {
    if options.validation {
        return Err("Pinned Metal HAL cannot verify --hal-validation; configure Metal API validation externally at process launch".into());
    }
    if surface {
        MainThreadMarker::new()
            .ok_or("Metal native surfaces must be created on the main thread")?;
    }
    Ok(())
}

pub fn create_metal_device(options: &Options) -> Result<Device<M>> {
    M::create_device(options, None).map(|(device, _)| device)
}
pub fn create_metal_image_device(options: &Options) -> Result<ExternalImageDevice> {
    Ok(ExternalImageDevice::new(&Rc::new(create_metal_device(
        options,
    )?)))
}
pub(crate) fn create_device_for_layer(
    options: &Options,
    layer: Retained<CAMetalLayer>,
) -> Result<(Device<M>, super::surface::SurfaceSetup<M>)> {
    validate_options(options, true)?;
    autoreleasepool(|_| {
        let (mut device, surface) = Device::new_with_owner(
            options,
            Some(super::surface::platform::WindowOwner::MetalLayer(layer)),
        )?;
        device.completion_probe = Some(completion_probe);
        Ok((
            device,
            surface.ok_or("Metal layer initialization returned no surface")?,
        ))
    })
}

fn completion_probe(
    encoder: &hal::metal::CommandEncoder,
    owner: &Device<M>,
) -> Result<CompletionCheck> {
    let command = encoder
        .raw_command_buffer()
        .ok_or("Metal encoder has no command buffer")?;
    let command = unsafe { Retained::retain(command as *const _ as *mut _) }
        .ok_or("Retaining Metal command buffer failed")?;
    let lost = owner.lost.clone();
    let complete = block2::RcBlock::new(
        move |buffer: std::ptr::NonNull<ProtocolObject<dyn MTLCommandBuffer>>| {
            if unsafe { buffer.as_ref() }.status() == MTLCommandBufferStatus::Error {
                lost.set(true);
            }
        },
    );
    unsafe {
        command.addCompletedHandler(block2::RcBlock::as_ptr(&complete));
    }
    Ok(Box::new(move |wait| {
        if wait {
            command.waitUntilCompleted();
        }
        match command.status() {
            MTLCommandBufferStatus::Completed => Ok(true),
            MTLCommandBufferStatus::Error => Err(format!(
                "Metal command buffer failed: {:?}",
                command.error()
            )),
            _ => Ok(false),
        }
    }))
}

pub struct MetalDeviceContext<'a> {
    pub device: &'a ProtocolObject<dyn MTLDevice>,
    pub queue: &'a ProtocolObject<dyn MTLCommandQueue>,
    pub registry_id: u64,
}

pub struct MetalEvent {
    event: Retained<ProtocolObject<dyn MTLSharedEvent>>,
    value: u64,
    registry_id: u64,
}
impl MetalEvent {
    pub fn value(&self) -> u64 {
        self.value
    }
    pub fn registry_id(&self) -> u64 {
        self.registry_id
    }
    pub fn shared_handle(&self) -> Retained<MTLSharedEventHandle> {
        self.event.newSharedEventHandle()
    }
}

#[derive(Clone)]
pub struct MetalPlane {
    texture: Retained<ProtocolObject<dyn MTLTexture>>,
    descriptor: api::ImageDescriptor,
    _surface: Option<CFRetained<IOSurfaceRef>>,
}
impl MetalPlane {
    /// # Safety
    /// The texture must contain initialized pixels matching the descriptor. Native access must obey
    /// the acquire/release protocol; aliases of the same storage must not race with WR's GPU copy.
    pub unsafe fn from_texture(
        texture: Retained<ProtocolObject<dyn MTLTexture>>,
        descriptor: api::ImageDescriptor,
    ) -> Result<Self> {
        validate_descriptor(descriptor)?;
        Ok(Self {
            texture,
            descriptor,
            _surface: None,
        })
    }
    pub fn descriptor(&self) -> api::ImageDescriptor {
        self.descriptor
    }
    pub fn texture(&self) -> &ProtocolObject<dyn MTLTexture> {
        &self.texture
    }
}

pub struct MetalCopy {
    images: Vec<NativeImage>,
    release: MetalEvent,
}
impl MetalCopy {
    pub fn images(&self) -> &[NativeImage] {
        &self.images
    }
    pub fn release(&self) -> &MetalEvent {
        &self.release
    }
    pub fn into_parts(self) -> (Vec<NativeImage>, MetalEvent) {
        (self.images, self.release)
    }
}

struct TransferSync {
    _wait: Option<Retained<ProtocolObject<dyn MTLSharedEvent>>>,
    signal: Retained<ProtocolObject<dyn MTLSharedEvent>>,
}
impl SubmissionSync<M> for TransferSync {
    fn stage(&self, queue: &hal::metal::Queue) {
        queue.add_signal_event(self.signal.clone(), 1);
    }
    fn unstage(&self, queue: &hal::metal::Queue) {
        queue.remove_signal_event(&self.signal);
    }
}

fn format(format: api::ImageFormat) -> Result<(wgt::TextureFormat, MTLPixelFormat)> {
    Ok(match format {
        api::ImageFormat::RGBA8 => (wgt::TextureFormat::Rgba8Unorm, MTLPixelFormat::RGBA8Unorm),
        api::ImageFormat::BGRA8 => (wgt::TextureFormat::Bgra8Unorm, MTLPixelFormat::BGRA8Unorm),
        api::ImageFormat::R8 => (wgt::TextureFormat::R8Unorm, MTLPixelFormat::R8Unorm),
        api::ImageFormat::RG8 => (wgt::TextureFormat::Rg8Unorm, MTLPixelFormat::RG8Unorm),
        api::ImageFormat::R16 => (wgt::TextureFormat::R16Unorm, MTLPixelFormat::R16Unorm),
        api::ImageFormat::RG16 => (wgt::TextureFormat::Rg16Unorm, MTLPixelFormat::RG16Unorm),
        _ => return Err("Metal plane copy requires RGBA8/BGRA8/R8/RG8/R16/RG16".into()),
    })
}

fn shared_event(owner: &Device<M>) -> Result<Retained<ProtocolObject<dyn MTLSharedEvent>>> {
    if !objc2::available!(macos = 10.14, ..) {
        return Err("Metal shared events require macOS 10.14 or later".into());
    }
    owner
        .open
        .device
        .raw_device()
        .newSharedEvent()
        .ok_or("Creating Metal shared event failed".into())
}

fn check_plane(owner: &Rc<Device<M>>, plane: &MetalPlane) -> Result<wgt::TextureFormat> {
    let descriptor = plane.descriptor;
    validate_descriptor(descriptor)?;
    let (format, native) = format(descriptor.format)?;
    let device = plane.texture.device();
    if !std::ptr::eq(device.as_ref(), owner.open.device.raw_device().as_ref()) {
        return Err("Native Metal texture must belong to this device; use an IOSurface view for a separate device".into());
    }
    if plane.texture.textureType() != MTLTextureType::Type2D
        || plane.texture.pixelFormat() != native
        || plane.texture.width() != descriptor.size.width as usize
        || plane.texture.height() != descriptor.size.height as usize
        || plane.texture.arrayLength() != 1
        || plane.texture.mipmapLevelCount() != 1
        || plane.texture.sampleCount() != 1
        || plane.texture.isFramebufferOnly()
        || plane.texture.storageMode() == MTLStorageMode::Memoryless
        || descriptor
            .flags
            .contains(api::ImageDescriptorFlags::ALLOW_MIPMAPS)
    {
        return Err(
            "Unsupported Metal texture shape, format, storage or framebuffer-only usage".into(),
        );
    }
    owner.layout(descriptor.size.width as u32, descriptor.size.height as u32)?;
    Ok(format)
}

fn copy_region(size: wgt::Extent3d) -> hal::TextureCopy {
    let base = hal::TextureCopyBase {
        mip_level: 0,
        array_layer: 0,
        origin: wgt::Origin3d::ZERO,
        aspect: hal::FormatAspects::COLOR,
    };
    hal::TextureCopy {
        src_base: base.clone(),
        dst_base: base,
        size: size.into(),
    }
}

impl ExternalImageDevice {
    fn metal_producer(&self) -> Result<&Producer<M>> {
        let producer = self
            .0
            .as_any()
            .downcast_ref::<Producer<M>>()
            .ok_or("External image device is not Metal")?;
        producer.ensure_healthy()?;
        Ok(producer)
    }
    pub fn metal_context(&self) -> Result<MetalDeviceContext<'_>> {
        let owner = &self.metal_producer()?.owner;
        let device = owner.open.device.raw_device();
        Ok(MetalDeviceContext {
            device,
            queue: owner.open.queue.as_raw(),
            registry_id: device.registryID(),
        })
    }
    /// # Safety
    /// Coordinate all native submissions through this call, do not reenter WR, and encode foreign
    /// waits inside their consuming command buffer instead of staging queue-global HAL waits.
    pub unsafe fn with_metal_queue<T>(
        &self,
        operation: impl FnOnce(MetalDeviceContext<'_>) -> T,
    ) -> Result<T> {
        let owner = &self.metal_producer()?.owner;
        let _guard = owner.lock_queue()?;
        Ok(operation(self.metal_context()?))
    }
    /// # Safety
    /// The producer must have submitted the signal, or already completed it. The event value must
    /// cover all native writes to the imported planes, and the producer must not regress that value.
    /// The caller must guarantee signal progress and handle producer failure; pending native waits cannot be cancelled here.
    pub unsafe fn import_metal_event(
        &self,
        handle: &MTLSharedEventHandle,
        value: u64,
        registry_id: u64,
    ) -> Result<MetalEvent> {
        let owner = &self.metal_producer()?.owner;
        if value == 0 || registry_id != owner.open.device.raw_device().registryID() {
            return Err("Metal event value/device identity mismatch".into());
        }
        if !objc2::available!(macos = 10.14, ..) {
            return Err("Metal shared events are unavailable".into());
        }
        let event = owner
            .open
            .device
            .raw_device()
            .newSharedEventWithHandle(handle)
            .ok_or("Importing Metal shared event failed")?;
        Ok(MetalEvent {
            event,
            value,
            registry_id,
        })
    }
    /// # Safety
    /// Planes must be initialized and exclusively available after `ready`. Do not modify native
    /// storage until the release event signals; on failure retire the device before reuse.
    /// Color/range/chroma interpretation is supplied separately to WR; this copy preserves bits.
    pub unsafe fn copy_metal_planes(
        &self,
        planes: &[MetalPlane],
        ready: &MetalEvent,
    ) -> Result<MetalCopy> {
        let producer = self.metal_producer()?;
        let owner = &producer.owner;
        if planes.is_empty() || planes.len() > 3 {
            return Err("Metal copy requires one to three planes".into());
        }
        if ready.registry_id != owner.open.device.raw_device().registryID() {
            return Err("Metal acquire event belongs to another GPU".into());
        }
        #[cfg(any(test, feature = "hal-testing"))]
        owner.check_fault(FailurePoint::Import)?;
        let mut sources = Vec::new();
        let mut targets = Vec::new();
        for plane in planes {
            let format = check_plane(owner, plane)?;
            let size = wgt::Extent3d {
                width: plane.descriptor.size.width as u32,
                height: plane.descriptor.size.height as u32,
                depth_or_array_layers: 1,
            };
            let raw = hal::metal::Device::texture_from_raw(
                plane.texture.clone(),
                format,
                MTLTextureType::Type2D,
                1,
                1,
                size.into(),
                None,
            );
            sources.push(Rc::new(Owned::new(
                owner,
                raw,
                <hal::metal::Device as hal::Device>::destroy_texture,
            )));
            targets.push(Texture::new(
                owner,
                size.width,
                size.height,
                format,
                crate::device::TextureFilter::Linear,
                false,
            )?);
        }
        let signal = shared_event(owner)?;
        producer.submissions.submit()?;
        {
            let mut commands = producer.submissions.recording()?;
            commands
                .encoder()
                .raw_command_buffer()
                .ok_or("Metal encoder has no command buffer")?
                .encodeWaitForEvent_value(ready.event.as_ref(), ready.value);
            commands.synchronize(Rc::new(TransferSync {
                _wait: Some(ready.event.clone()),
                signal: signal.clone(),
            }));
            for ((source, target), plane) in sources.iter().zip(&targets).zip(planes) {
                commands.keep(source.clone());
                commands.keep(plane.clone());
                target.transition(&mut commands, wgt::TextureUses::COPY_DST);
                commands.encoder().copy_texture_to_texture(
                    source,
                    wgt::TextureUses::COPY_SRC,
                    &target.raw,
                    std::iter::once(copy_region(target.size)),
                );
                target.initialize(&mut commands);
            }
        }
        producer.submissions.submit()?;
        Ok(MetalCopy {
            images: targets
                .into_iter()
                .zip(planes)
                .map(|(texture, plane)| NativeImage::new(texture, plane.descriptor))
                .collect(),
            release: MetalEvent {
                event: signal,
                value: 1,
                registry_id: ready.registry_id,
            },
        })
    }

    pub fn export_metal_image(&self, image: &NativeImage) -> Result<(MetalPlane, MetalEvent)> {
        let producer = self.metal_producer()?;
        let owner = &producer.owner;
        image.ensure_idle()?;
        let descriptor = image.descriptor();
        let source = image.texture(owner)?;
        if source.mip_count != 1 || !source.sample_initialized() {
            return Err("Metal export needs an initialized single mip".into());
        }
        let (format, _) = format(descriptor.format)?;
        let target = Texture::new(
            owner,
            descriptor.size.width as u32,
            descriptor.size.height as u32,
            format,
            crate::device::TextureFilter::Linear,
            false,
        )?;
        let signal = shared_event(owner)?;
        {
            let mut commands = producer.submissions.recording()?;
            commands.synchronize(Rc::new(TransferSync {
                _wait: None,
                signal: signal.clone(),
            }));
            let previous = source.current_usage();
            source.transition(&mut commands, wgt::TextureUses::COPY_SRC);
            target.transition(&mut commands, wgt::TextureUses::COPY_DST);
            unsafe {
                commands.encoder().copy_texture_to_texture(
                    &source.raw,
                    wgt::TextureUses::COPY_SRC,
                    &target.raw,
                    std::iter::once(copy_region(target.size)),
                );
            }
            target.initialize(&mut commands);
            source.transition(&mut commands, previous);
        }
        producer.submissions.submit()?;
        let raw = target.raw.raw_handle();
        let texture = unsafe { Retained::retain(raw as *const _ as *mut _) }
            .ok_or("Retaining exported Metal texture failed")?;
        Ok((
            MetalPlane {
                texture,
                descriptor,
                _surface: None,
            },
            MetalEvent {
                event: signal,
                value: 1,
                registry_id: owner.open.device.raw_device().registryID(),
            },
        ))
    }
}

impl ExternalImageDevice {
    /// # Safety
    /// The surface contents, alpha and color metadata must match the descriptor. The caller must
    /// coordinate native reads/writes through the copy acquire/release protocol and retain IPC rights.
    pub unsafe fn metal_plane_from_iosurface(
        &self,
        surface: CFRetained<IOSurfaceRef>,
        plane: usize,
        descriptor: api::ImageDescriptor,
    ) -> Result<MetalPlane> {
        let owner = &self.metal_producer()?.owner;
        validate_descriptor(descriptor)?;
        let count = surface.plane_count();
        let expected = super::metal_layout::iosurface_format(surface.pixel_format(), count, plane)?;
        if expected != descriptor.format {
            return Err("IOSurface format does not match the declared Metal plane".into());
        }
        let (width, height, stride, element) = if count == 0 {
            (
                surface.width(),
                surface.height(),
                surface.bytes_per_row(),
                surface.bytes_per_element(),
            )
        } else {
            (
                surface.width_of_plane(plane),
                surface.height_of_plane(plane),
                surface.bytes_per_row_of_plane(plane),
                surface.bytes_per_element_of_plane(plane),
            )
        };
        let (format, native) = format(descriptor.format)?;
        if width != descriptor.size.width as usize
            || height != descriptor.size.height as usize
            || element != super::resources::bytes_per_pixel(format)
            || width.checked_mul(element).map_or(true, |row| stride < row)
            || stride
                .checked_mul(height)
                .map_or(true, |bytes| bytes > surface.alloc_size())
        {
            return Err("IOSurface plane dimensions/stride/element size mismatch".into());
        }
        let texture = MTLTextureDescriptor::new();
        texture.setTextureType(MTLTextureType::Type2D);
        texture.setPixelFormat(native);
        texture.setWidth(width);
        texture.setHeight(height);
        texture.setMipmapLevelCount(1);
        texture.setArrayLength(1);
        texture.setSampleCount(1);
        let shared = objc2::available!(macos = 10.15, ..)
            && owner.open.device.raw_device().hasUnifiedMemory();
        texture.setStorageMode(if shared {
            MTLStorageMode::Shared
        } else {
            MTLStorageMode::Managed
        });
        texture.setUsage(MTLTextureUsage::ShaderRead);
        let texture = owner
            .open
            .device
            .raw_device()
            .newTextureWithDescriptor_iosurface_plane(&texture, &surface, plane)
            .ok_or("Creating Metal IOSurface plane view failed")?;
        let plane = MetalPlane {
            texture,
            descriptor,
            _surface: Some(surface),
        };
        check_plane(owner, &plane)?;
        Ok(plane)
    }

    pub fn poll_metal_event(&self, event: &MetalEvent) -> Result<bool> {
        let producer = self.metal_producer()?;
        if event.registry_id != producer.owner.open.device.raw_device().registryID() {
            return Err("Metal event belongs to another GPU".into());
        }
        producer.submissions.poll()?;
        Ok(event.event.signaledValue() >= event.value)
    }

    /// # Safety
    /// The destination must be exclusively writable after `ready` and remain unavailable to its
    /// producer until the returned event signals. It must not alias the source or other live WR data.
    pub unsafe fn copy_image_to_metal_plane(
        &self,
        image: &NativeImage,
        destination: &MetalPlane,
        ready: &MetalEvent,
    ) -> Result<MetalEvent> {
        let producer = self.metal_producer()?;
        let owner = &producer.owner;
        image.ensure_idle()?;
        let descriptor = image.descriptor();
        let format = check_plane(owner, destination)?;
        if descriptor.size != destination.descriptor.size
            || descriptor.format != destination.descriptor.format
            || ready.registry_id != owner.open.device.raw_device().registryID()
        {
            return Err("Metal export destination shape/format/GPU mismatch".into());
        }
        let source = image.texture(owner)?;
        if source.mip_count != 1
            || !source.sample_initialized()
            || std::ptr::eq(source.raw.raw_handle(), destination.texture.as_ref())
        {
            return Err("Metal export requires an initialized, non-aliased single mip".into());
        }
        let raw = hal::metal::Device::texture_from_raw(
            destination.texture.clone(),
            format,
            MTLTextureType::Type2D,
            1,
            1,
            source.size.into(),
            None,
        );
        let target = Rc::new(Owned::new(
            owner,
            raw,
            <hal::metal::Device as hal::Device>::destroy_texture,
        ));
        let signal = shared_event(owner)?;
        producer.submissions.submit()?;
        {
            let mut commands = producer.submissions.recording()?;
            commands
                .encoder()
                .raw_command_buffer()
                .ok_or("Metal encoder has no command buffer")?
                .encodeWaitForEvent_value(ready.event.as_ref(), ready.value);
            commands.synchronize(Rc::new(TransferSync {
                _wait: Some(ready.event.clone()),
                signal: signal.clone(),
            }));
            commands.keep(target.clone());
            commands.keep(destination.clone());
            let previous = source.current_usage();
            source.transition(&mut commands, wgt::TextureUses::COPY_SRC);
            commands.encoder().copy_texture_to_texture(
                &source.raw,
                wgt::TextureUses::COPY_SRC,
                &target,
                std::iter::once(copy_region(source.size)),
            );
            source.transition(&mut commands, previous);
        }
        producer.submissions.submit()?;
        Ok(MetalEvent {
            event: signal,
            value: 1,
            registry_id: ready.registry_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::resources::Buffer;
    use super::*;

    #[test]
    #[ignore = "Requires native macOS Metal; configure API validation before process launch"]
    fn metal_plane_copy_source_replacement_and_event_retirement() {
        let device = create_metal_image_device(&Options::default()).unwrap();
        let descriptor = api::ImageDescriptor::new(
            17,
            13,
            api::ImageFormat::RGBA8,
            api::ImageDescriptorFlags::empty(),
        );
        let pixels: Vec<u8> = (0..17 * 13 * 4).map(|i| (i * 19 + 7) as u8).collect();
        let image = device.create_image(descriptor, &pixels).unwrap();
        let (plane, ready) = device.export_metal_image(&image).unwrap();
        device
            .update_image(&image, descriptor, &vec![0; pixels.len()])
            .unwrap();
        let copy = unsafe { device.copy_metal_planes(&[plane], &ready) }.unwrap();
        let producer = device.metal_producer().unwrap();
        let owner = &producer.owner;
        let texture = copy.images()[0].texture(owner).unwrap();
        let layout = owner.layout(17, 13).unwrap();
        let buffer = Buffer::readback(owner, &layout).unwrap();
        {
            let mut commands = producer.submissions.recording().unwrap();
            texture.transition(&mut commands, wgt::TextureUses::COPY_SRC);
            buffer.transition(&mut commands, wgt::BufferUses::COPY_DST);
            unsafe {
                copy_readback::<M>(
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
        assert!(device.poll_metal_event(copy.release()).unwrap());
        assert_eq!(owner.map_readback(&buffer.raw, &layout).unwrap(), pixels);
    }
}
