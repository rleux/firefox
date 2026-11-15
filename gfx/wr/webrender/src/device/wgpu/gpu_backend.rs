/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::wgpu::{SurfaceWindow, SurfaceOptions};
use crate::device as wr;
use crate::device::query::{GpuProfiler, GpuQueryBackend, GpuQueryId, GpuQueryKind};
use crate::internal_types::{RenderTargetInfo, Swizzle, SwizzleSettings};
use crate::render_api::MemoryReport;
use api::units::DeviceSize;
#[cfg(feature = "capture")]
use api::{ExternalTextureHandle, ImageDescriptor};
use api::{ImageBufferKind, Parameter};
use euclid::default::Transform3D;
use std::{borrow::Cow, cell::Cell, mem::MaybeUninit, num::NonZeroUsize, ptr::NonNull, rc::Rc};
use webrender_build::shader::ShaderFeatureFlags;

struct DisabledQueries;

impl GpuQueryBackend for DisabledQueries {
    fn create_queries(&self, _: usize) -> Vec<GpuQueryId> {
        Vec::new()
    }
    fn delete_queries(&self, queries: &[GpuQueryId]) {
        debug_assert!(queries.is_empty());
    }
    fn begin_query(&self, _: GpuQueryKind, _: GpuQueryId) {
        unreachable!("Vulkan queries are disabled")
    }
    fn end_query(&self, _: GpuQueryKind) {
        unreachable!("Vulkan queries are disabled")
    }
    fn query_result(&self, _: GpuQueryId) -> u64 {
        unreachable!("Vulkan queries are disabled")
    }
    fn supports_markers(&self) -> bool {
        false
    }
    fn push_marker_group(&self, _: &str) {}
    fn pop_marker_group(&self) {}
    fn insert_marker(&self, _: &str) {}
}

impl RenderDevice {
    fn unsupported(&mut self, operation: &str) {
        self.operation::<()>(|_| Err(format!("Vulkan {operation} is not supported")));
    }

    fn draw_quads(&mut self, indices: i32, instances: i32, base: u32) {
        self.operation(|device| {
            if indices == 0 || instances == 0 {
                return Ok(());
            }
            if indices != 6 {
                return Err("Vulkan draws require six quad indices".into());
            }
            device.draw(
                0,
                indices,
                instances,
                base,
                Some(wgt::IndexFormat::Uint16),
                wgt::PrimitiveTopology::TriangleList,
                None,
            )
        });
    }
}

impl wr::GpuBackend for RenderDevice {
    #[cfg(test)]
    fn wgpu_test_fail_submit(&mut self) {
        let encoder = self.commands().unwrap();
        drop(encoder.finish(&wgt::CommandBufferDescriptor::default()));
    }
    #[cfg(test)]
    fn wgpu_test_lose_device(&self) {
        self.owner.device.destroy();
    }
    fn textures_created(&self) -> u32 {
        self.textures.created()
    }
    fn textures_deleted(&self) -> u32 {
        self.textures.deleted()
    }
    fn set_initialize_color_targets_with_pink(&mut self, enabled: bool) {
        if enabled {
            self.unsupported("pink target initialization");
        }
    }
    fn create_gpu_profiler(&self, _: bool) -> GpuProfiler {
        GpuProfiler::new(Rc::new(DisabledQueries))
    }
    fn set_parameter(&mut self, _: &Parameter) {}
    fn max_texture_size(&self) -> i32 {
        self.properties.max_texture_size
    }
    fn surface_origin_is_top_left(&self) -> bool {
        self.properties.surface_origin_is_top_left()
    }
    fn get_capabilities(&self) -> &wr::Capabilities {
        &self.properties.capabilities
    }
    fn api_info(&self) -> wr::GraphicsApiInfo {
        self.properties.api_info.clone()
    }
    fn take_out_of_memory_error(&self) -> bool {
        self.out_of_memory.replace(false)
    }
    fn failure(&self) -> Option<&str> {
        RenderDevice::failure(self)
    }
    fn present_result(&self) -> Option<wr::PresentResult> {
        self.surface.as_ref().and_then(|s| s.outcome)
    }
    fn gpu_submission_status(&mut self) -> Result<Option<wr::GpuSubmissionStatus>, String> {
        let result = self.poll();
        if let Err(error) = result {
            self.record_result::<()>(Err(error));
        }
        if let Some(error) = self.failure() {
            return Err(error.to_owned());
        }
        Ok(Some(wr::GpuSubmissionStatus {
            submitted: self.submitted,
            completed: self.completed.load(Ordering::Acquire),
        }))
    }
    fn set_surface_paused(&mut self, paused: bool) -> Result<(), String> {
        if self.surface.is_none() {
            return Ok(());
        }
        let result = self
            .drain()
            .and_then(|_| self.surface.as_mut().unwrap().pause(paused));
        if result.is_ok() {
            self.capture_pending = false;
            self.capture_this_frame = false;
        }
        result
    }
    fn set_wgpu_surface(
        &mut self,
        window: Option<Rc<dyn SurfaceWindow>>,
        options: SurfaceOptions,
    ) -> Result<(), String> {
        if self.surface.is_none() {
            return Err("Vulkan device was not created for window presentation".into());
        }
        let drained = self.drain();
        let result = if window.is_none() {
            self.textures.set_output(None);
            let detached = self.surface.as_mut().unwrap().replace(None, options);
            drained.and(detached)
        } else {
            drained.and_then(|_| self.surface.as_mut().unwrap().replace(window, options))
        };
        if result.is_ok() {
            self.capture_pending = false;
            self.capture_this_frame = false;
            self.textures.set_output(None);
        }
        result
    }
    fn prepare_readback(&mut self) -> Result<bool, String> {
        let result = self
            .surface
            .as_mut()
            .map(|s| s.enable_readback())
            .unwrap_or(Ok(true));
        let supported = self
            .record_result(result)
            .ok_or_else(|| self.failure().unwrap().to_owned())?;
        self.capture_pending |= supported && self.surface.is_some();
        Ok(supported)
    }
    fn readback_ready(&self) -> bool {
        self.textures.output().is_some()
    }
    fn blend_barrier(&self) {}
    fn shader_feature_flags(&self) -> ShaderFeatureFlags {
        self.properties.shader_feature_flags()
    }
    fn preferred_color_formats(&self) -> wr::TextureFormatPair<ImageFormat> {
        self.properties.color_formats.clone()
    }
    fn swizzle_settings(&self) -> Option<SwizzleSettings> {
        None
    }
    fn max_depth_ids(&self) -> i32 {
        self.properties.max_depth_ids()
    }
    fn ortho_near_plane(&self) -> f32 {
        self.properties.ortho_near_plane()
    }
    fn ortho_far_plane(&self) -> f32 {
        self.properties.ortho_far_plane()
    }
    fn required_transfer_stride(&self) -> wr::StrideAlignment {
        wr::StrideAlignment::Bytes(NonZeroUsize::new(4).unwrap())
    }
    fn upload_method(&self) -> &wr::UploadMethod {
        &self.properties.upload_method
    }
    fn use_batched_texture_uploads(&self) -> bool {
        true
    }
    fn use_draw_calls_for_texture_copy(&self) -> bool {
        false
    }
    fn batched_upload_threshold(&self) -> i32 {
        0
    }
    fn reset_state(&mut self) {
        self.programs.unbind();
        self.textures.reset_bindings();
        self.vertex_arrays.unbind();
    }
    fn begin_frame(&mut self) -> GpuFrameId {
        self.operation(RenderDevice::begin_frame)
            .unwrap_or(self.frame)
    }
    fn bind_texture(&mut self, slot: wr::TextureSlot, texture: &wr::Texture, swizzle: Swizzle) {
        self.operation(|device| {
            if swizzle != Swizzle::default() {
                return Err("Vulkan sampling swizzles are unsupported".into());
            }
            device.textures.bind(slot, texture)
        });
    }
    #[cfg(target_os = "linux")]
    fn wgpu_external_textures(&self) -> Option<Rc<crate::wgpu::ExternalTextureRegistry>> {
        self.external_images.clone()
    }
    fn bind_external_texture(&mut self, slot: wr::TextureSlot, external: &wr::ExternalTexture) {
        self.operation(|device| {
            if external.id != 0 {
                #[cfg(target_os = "linux")]
                {
                    let image = device
                        .external_images
                        .as_ref()
                        .ok_or("Native images unavailable")?
                        .image(external.id)?;
                    return device.textures.bind_external_image(
                        slot,
                        image,
                        external.image_rendering,
                    );
                }
                #[cfg(not(target_os = "linux"))]
                return Err("Vulkan native external images are unavailable".into());
            }
            if external.target != ImageBufferKind::Texture2D {
                return Err("Vulkan external textures require Texture2D".into());
            }
            let image = Image::new(
                &device.owner,
                1,
                1,
                wgt::TextureFormat::Rgba8Unorm,
                TextureFilter::Nearest,
                false,
            )?;
            device
                .textures
                .bind_external_image(slot, image, external.image_rendering)
        });
    }
    fn begin_render_pass(&mut self, descriptor: &RenderPassDescriptor) {
        self.operation(|device| RenderDevice::begin_render_pass(device, descriptor));
    }
    fn end_render_pass(&mut self, store: StoreOp) {
        self.operation(|device| RenderDevice::end_render_pass(device, store));
    }
    fn link_program(
        &mut self,
        program: &mut Program,
        descriptor: &wr::VertexDescriptor,
        samplers: &[(&'static str, wr::TextureSlot)],
    ) -> Result<(), wr::ShaderError> {
        if let Some(error) = self.failure().map(str::to_owned) {
            let result = self.programs.delete(program);
            self.record_result(result);
            program.id = 0;
            program.is_initialized = false;
            return Err(wr::ShaderError::Link(
                program.source_info.base_filename.into(),
                error,
                Vec::new(),
            ));
        }
        self.programs.link(program, descriptor)?;
        self.programs
            .state_mut(program)
            .expect("linked Vulkan program")
            .bind_samplers(samplers);
        Ok(())
    }
    fn bind_pipeline(&mut self, program: &Program, state: &RenderState) -> bool {
        self.operation(|device| device.programs.bind(program, *state))
            .unwrap_or(false)
    }
    fn create_texture(
        &mut self,
        target: ImageBufferKind,
        format: ImageFormat,
        width: i32,
        height: i32,
        filter: TextureFilter,
        render_target: Option<RenderTargetInfo>,
    ) -> wr::Texture {
        let size = DeviceIntSize::new(width, height);
        self.operation(|device| {
            device
                .textures
                .create(target, format, size, filter, render_target)
        })
        .unwrap_or_else(|| wr::Texture {
            id: 0,
            target_id: wr::TextureId(0),
            target,
            format,
            size,
            filter,
            flags: wr::TextureFlags::empty(),
            active_swizzle: Cell::default(),
            render_target,
            last_frame_used: self.frame,
        })
    }
    fn copy_texture_sub_region(
        &mut self,
        source: &wr::Texture,
        x: usize,
        y: usize,
        target: &wr::Texture,
        dx: usize,
        dy: usize,
        width: usize,
        height: usize,
    ) {
        self.operation(|device| {
            RenderDevice::copy_texture_sub_region(
                device, source, x, y, target, dx, dy, width, height,
            )
        });
    }
    fn invalidate_render_target(&mut self, texture: &wr::Texture) {
        // This is a discard hint; subsequent core load/store ops decide the contents.
        self.operation(|device| {
            device.flush_pass(StoreOp::Store)?;
            device.textures.image(texture)?;
            Ok(())
        });
    }
    fn reuse_render_target(&mut self, texture: &mut wr::Texture, info: RenderTargetInfo) {
        self.operation(|device| device.textures.reuse_render_target(texture, info));
    }
    fn blit_render_target(
        &mut self,
        source: ReadTarget,
        source_rect: FramebufferIntRect,
        target: DrawTarget,
        target_rect: FramebufferIntRect,
        filter: TextureFilter,
    ) {
        self.operation(|device| {
            RenderDevice::blit_render_target(
                device,
                source,
                source_rect,
                target,
                target_rect,
                filter,
            )
        });
    }
    fn delete_texture(&mut self, mut texture: wr::Texture) {
        let result = self.textures.delete(&mut texture);
        self.record_result(result);
        texture.id = 0;
    }
    #[cfg(feature = "replay")]
    fn delete_external_texture(&mut self, _: wr::ExternalTexture) {
        self.unsupported("external textures");
    }
    fn delete_program(&mut self, mut program: Program) {
        let result = self.programs.delete(&mut program);
        self.record_result(result);
        program.id = 0;
    }
    fn create_program(
        &mut self,
        name: &'static str,
        features: &[&'static str],
    ) -> Result<Program, wr::ShaderError> {
        if let Some(error) = self.failure() {
            return Err(wr::ShaderError::Compilation(
                name.into(),
                error.into(),
                Vec::new(),
            ));
        }
        self.programs.create(name, features)
    }
    #[cfg(feature = "debugger")]
    fn supports_shader_source_override(&self) -> bool {
        false
    }
    #[cfg(feature = "debugger")]
    fn shader_file_names(&self) -> Vec<&'static str> {
        let mut names: Vec<_> = crate::shader_source::UNOPTIMIZED_SHADERS
            .keys()
            .copied()
            .collect();
        names.sort_unstable();
        names
    }
    #[cfg(feature = "debugger")]
    fn builtin_shader_source(&self, name: &str) -> Option<&'static str> {
        crate::shader_source::UNOPTIMIZED_SHADERS
            .get(name)
            .map(|shader| shader.source)
    }
    fn get_shader_source(&self, name: &str) -> Cow<'static, str> {
        Cow::Borrowed(
            crate::shader_source::UNOPTIMIZED_SHADERS
                .get(name)
                .map_or("", |shader| shader.source),
        )
    }
    #[cfg(feature = "debugger")]
    fn shader_source_override(&self, _: &str) -> Option<&str> {
        None
    }
    #[cfg(feature = "debugger")]
    fn has_shader_source_overrides(&self) -> bool {
        false
    }
    #[cfg(feature = "debugger")]
    fn set_shader_source_override(&mut self, _: &str, _: String) {
        self.unsupported("shader source overrides");
    }
    #[cfg(feature = "debugger")]
    fn clear_shader_source_override(&mut self, _: &str) -> bool {
        false
    }
    #[cfg(feature = "debugger")]
    fn shader_include_closure(&self, _: &str) -> crate::internal_types::FastHashSet<String> {
        Default::default()
    }
    #[cfg(feature = "debugger")]
    fn expanded_shader_source(&self, _: &str, _: &[&'static str]) -> (String, String) {
        (String::new(), String::new())
    }
    fn set_uniforms(&self, program: &Program, transform: &Transform3D<f32>) {
        if self.failure.get().is_none() {
            self.programs
                .state(program)
                .expect("Vulkan uniforms require a linked program")
                .set_transform(transform);
        }
    }
    fn set_shader_texture_size(&self, _: &Program, _: DeviceSize) {}
    fn create_transfer_buffer_with_size(&mut self, size: usize) -> TransferBuffer {
        self.operation(|device| {
            let mut buffer = device.uploads.create()?;
            let mapping = match device.uploads.allocate(&mut buffer, size, false) {
                Ok(mapping) => mapping,
                Err(error) => {
                    device.uploads.delete(&mut buffer)?;
                    return Err(error);
                }
            };
            device.uploads.flush(&buffer, &mapping, 0)?;
            Ok(buffer)
        })
        .unwrap_or(TransferBuffer {
            id: 0,
            reserved_size: size,
        })
    }
    fn read_pixels_into_transfer_buffer(
        &mut self,
        target: ReadTarget,
        rect: DeviceIntRect,
        format: ImageFormat,
        pbo: &TransferBuffer,
    ) {
        self.operation(|device| {
            let image = device.textures.read_target(target)?;
            let length = usize::try_from(rect.width())
                .ok()
                .and_then(|w| {
                    usize::try_from(rect.height())
                        .ok()
                        .and_then(|h| w.checked_mul(h))
                })
                .and_then(|n| n.checked_mul(format.bytes_per_pixel() as usize))
                .filter(|n| *n <= pbo.reserved_size)
                .ok_or("Vulkan readback exceeds transfer buffer")?;
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(pbo.reserved_size)
                .map_err(|e| e.to_string())?;
            bytes.resize(pbo.reserved_size, 0);
            device.capture_into(image, rect, format, &mut bytes[..length])?;
            device.uploads.write_readback(pbo, bytes)
        });
    }
    fn map_transfer_buffer<'a>(
        &'a mut self,
        pbo: &'a TransferBuffer,
    ) -> Option<wr::MappedTransferBuffer<'a>> {
        let bytes = self.uploads.read(pbo).ok()?;
        let (ptr, len) = (bytes.as_ptr(), bytes.len());
        // The boxed bytes stay fixed while the mapped view exclusively borrows the backend.
        Some(wr::MappedTransferBuffer {
            device: self,
            data: unsafe { std::slice::from_raw_parts(ptr, len) },
        })
    }
    fn unmap_transfer_buffer(&mut self) {}
    fn delete_transfer_buffer(&mut self, mut buffer: TransferBuffer) {
        let result = self.uploads.delete(&mut buffer);
        self.record_result(result);
        buffer.id = 0;
    }
    fn create_transfer_buffer(&mut self) -> TransferBuffer {
        self.operation(|device| device.uploads.create())
            .unwrap_or(TransferBuffer {
                id: 0,
                reserved_size: 0,
            })
    }
    fn required_upload_size_and_stride(
        &mut self,
        size: DeviceIntSize,
        format: ImageFormat,
    ) -> Result<(usize, usize), String> {
        self.operation(|device| {
            if size.width <= 0 || size.height <= 0 {
                return Err("Invalid Vulkan upload dimensions".into());
            }
            let stride = (size.width as usize)
                .checked_mul(format.bytes_per_pixel() as usize)
                .and_then(|s| s.checked_add(3))
                .map(|s| s / 4 * 4)
                .ok_or("Vulkan upload row overflow")?;
            let length = stride
                .checked_mul(size.height as usize)
                .filter(|n| {
                    *n <= isize::MAX as usize && *n as u64 <= device.owner.limits().max_buffer_size
                })
                .ok_or("Vulkan upload exceeds buffer or address-space limits")?;
            Ok((length, stride))
        })
        .ok_or_else(|| self.failure().unwrap().to_owned())
    }
    fn allocate_upload_buffer(
        &mut self,
        buffer: &mut TransferBuffer,
        size: usize,
        _: wr::VertexUsageHint,
        persistent: bool,
    ) -> Result<UploadBufferMapping, String> {
        self.operation(|device| device.uploads.allocate(buffer, size, persistent))
            .ok_or_else(|| self.failure().unwrap().to_owned())
    }
    fn map_upload_buffer(
        &mut self,
        buffer: &TransferBuffer,
    ) -> Result<NonNull<MaybeUninit<u8>>, String> {
        self.operation(|device| device.uploads.map(buffer))
            .ok_or_else(|| self.failure().unwrap().to_owned())
    }
    fn flush_upload_buffer(
        &mut self,
        buffer: &TransferBuffer,
        mapping: &UploadBufferMapping,
        size: usize,
        chunks: &[UploadChunk<'_>],
    ) {
        self.operation(|device| {
            RenderDevice::flush_upload_buffer(device, buffer, mapping, size, chunks)
        });
    }
    fn orphan_upload_buffer(&mut self, buffer: &mut TransferBuffer) {
        let result = self.uploads.orphan(buffer);
        self.record_result(result);
    }
    fn upload_texture_region(
        &mut self,
        texture: &wr::Texture,
        rect: DeviceIntRect,
        stride: Option<i32>,
        format: Option<ImageFormat>,
        data: &[u8],
    ) {
        self.operation(|device| {
            RenderDevice::upload_texture_region(device, texture, rect, stride, format, data)
        });
    }
    fn create_fence(&mut self) -> Option<wr::Fence> {
        self.operation(|device| {
            device.submit().and_then(|index| {
                usize::try_from(index)
                    .map(wr::Fence)
                    .map_err(|e| e.to_string())
            })
        })
    }
    fn poll_fence(&self, fence: &wr::Fence) -> wr::FenceStatus {
        if self.owner.poll().is_err() || self.failure().is_some() {
            wr::FenceStatus::Error
        } else if fence.0 as u64 <= self.completed.load(Ordering::Acquire) {
            wr::FenceStatus::Signaled
        } else {
            wr::FenceStatus::Pending
        }
    }
    fn delete_fence(&mut self, _: wr::Fence) {}
    fn upload_texture_immediate(&mut self, texture: &wr::Texture, data: &[u8]) {
        self.operation(|device| RenderDevice::upload_texture_immediate(device, texture, data));
    }
    #[cfg(feature = "capture")]
    fn read_external_texture(
        &mut self,
        handle: ExternalTextureHandle,
        kind: ImageBufferKind,
        descriptor: &ImageDescriptor,
    ) -> Vec<u8> {
        #[cfg(target_os = "linux")]
        return self
            .operation(|device| {
                if kind != ImageBufferKind::Texture2D {
                    return Err("Native capture requires a 2D texture".into());
                }
                device.capture_external(handle, descriptor)
            })
            .unwrap_or_default();
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (handle, kind, descriptor);
            self.unsupported("external capture");
            Vec::new()
        }
    }
    fn read_pixels_into(
        &mut self,
        target: ReadTarget,
        rect: FramebufferIntRect,
        format: ImageFormat,
        output: &mut [u8],
    ) {
        self.operation(|device| {
            let image = device.textures.read_target(target)?;
            device.capture_into(image, rect.cast_unit(), format, output)
        });
    }
    fn read_texture(&mut self, texture: &wr::Texture, format: ImageFormat, output: &mut [u8]) {
        self.operation(|device| {
            let image = device.textures.image(texture)?;
            let rect = bounds(&image);
            device.capture_into(image, rect, format, output)
        });
    }
    fn create_buffer(&mut self, kind: wr::BufferKind) -> wr::Buffer {
        self.operation(|device| device.vertex_arrays.create_buffer(kind))
            .unwrap_or(wr::Buffer {
                id: 0,
                kind,
                size: 0,
            })
    }
    fn delete_buffer(&mut self, mut buffer: wr::Buffer) {
        let result = self.vertex_arrays.delete_buffer(&mut buffer);
        self.record_result(result);
        buffer.id = 0;
    }
    fn write_buffer(&mut self, buffer: &mut wr::Buffer, bytes: &[u8], _: wr::VertexUsageHint) {
        self.operation(|device| device.vertex_arrays.write_buffer(buffer, bytes));
    }
    fn write_buffer_repeated(
        &mut self,
        buffer: &mut wr::Buffer,
        bytes: &[u8],
        element_size: usize,
        repeat: NonZeroUsize,
        _: wr::VertexUsageHint,
    ) {
        self.operation(|device| {
            device
                .vertex_arrays
                .write_buffer_repeated(buffer, bytes, element_size, repeat)
        });
    }
    fn reallocate_buffer(&mut self, buffer: &mut wr::Buffer, size: usize) {
        self.operation(|device| device.vertex_arrays.reallocate(buffer, size));
    }
    fn write_buffer_unsynchronized(&mut self, buffer: &wr::Buffer, offset: usize, bytes: &[u8]) {
        self.operation(|device| device.vertex_arrays.update_range(buffer, offset, bytes));
    }
    fn create_vertex_array(
        &mut self,
        descriptor: &wr::VertexDescriptor,
        vertices: &wr::Buffer,
        instances: Option<&wr::Buffer>,
        indices: Option<&wr::Buffer>,
        divisor: u32,
    ) -> wr::VertexArray {
        self.operation(|device| {
            device
                .vertex_arrays
                .create(descriptor, vertices, instances, indices, divisor)
        })
        .unwrap_or_else(|| wr::VertexArray {
            id: 0,
            vertices: wr::BufferId(vertices.id),
            instances: instances.map(|buffer| wr::BufferId(buffer.id)),
            indices: indices.map(|buffer| wr::BufferId(buffer.id)),
            instance_stride: descriptor.instance_attributes.iter().fold(
                0usize,
                |stride, attribute| {
                    stride.saturating_add(
                        (attribute.count as usize)
                            .saturating_mul(attribute.kind.size_in_bytes() as usize),
                    )
                },
            ),
        })
    }
    fn delete_vertex_array(&mut self, mut array: wr::VertexArray) {
        let result = self.vertex_arrays.delete(&mut array);
        self.record_result(result);
        array.id = 0;
    }
    fn bind_vertex_array(&mut self, array: &wr::VertexArray) {
        self.operation(|device| device.vertex_arrays.bind(array));
    }
    fn draw_triangles_u32(&mut self, first: i32, count: i32) {
        self.operation(|device| {
            device.draw(
                first,
                count,
                1,
                0,
                Some(wgt::IndexFormat::Uint32),
                wgt::PrimitiveTopology::TriangleList,
                None,
            )
        });
    }
    fn draw_nonindexed_lines(&mut self, first: i32, count: i32) {
        self.operation(|device| {
            device.draw(
                first,
                count,
                1,
                0,
                None,
                wgt::PrimitiveTopology::LineList,
                None,
            )
        });
    }
    fn draw_indexed_triangles(&mut self, count: i32) {
        self.operation(|device| {
            device.draw(
                0,
                count,
                1,
                0,
                Some(wgt::IndexFormat::Uint32),
                wgt::PrimitiveTopology::TriangleList,
                None,
            )
        });
    }
    fn draw_instanced_batch(
        &mut self,
        buffer: &mut wr::Buffer,
        data: &[u8],
        instances: i32,
        _: wr::VertexUsageHint,
    ) {
        self.operation(|device| {
            device.draw(
                0,
                6,
                instances,
                0,
                Some(wgt::IndexFormat::Uint16),
                wgt::PrimitiveTopology::TriangleList,
                Some((buffer, data)),
            )
        });
    }
    fn draw_indexed_triangles_instanced_u16(&mut self, indices: i32, instances: i32) {
        self.draw_quads(indices, instances, 0);
    }
    fn draw_indexed_triangles_instanced_base_instance_u16(
        &mut self,
        indices: i32,
        instances: i32,
        base: u32,
    ) {
        self.draw_quads(indices, instances, base);
    }
    fn deinit(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(native) = &self.external_images {
            native.close();
        }
        self.pass = None;
        self.last_bindings = None;
        self.skipped_pass = false;
        if self.failure.get().is_none() {
            let result = self.submit().and_then(|index| {
                #[cfg(target_os = "linux")]
                if self.surface.is_none()
                    && self
                        .external_images
                        .as_ref()
                        .is_some_and(|native| native.pending_work())
                {
                    return Ok(());
                }
                self.owner.wait_for(index).map_err(|e| e.to_string())
            });
            self.record_result(result);
        }
        self.encoder = None;
        self.ordered_commands.clear();
        #[cfg(target_os = "linux")]
        if let Some(native) = &self.external_images {
            native.retire();
        }
        self.textures.set_output(None);
        self.surface.take();
        self.blit_pipelines.clear();
        self.clear_pipelines.clear();
        self.programs.unbind();
        self.vertex_arrays.unbind();
        self.textures.reset_bindings();
    }
    fn end_frame(&mut self) {
        #[cfg(target_os = "linux")]
        if self
            .external_images
            .as_ref()
            .is_some_and(|native| !native.is_attached())
        {
            self.inside_frame = false;
            return;
        }
        self.operation(RenderDevice::end_frame);
    }
    fn finish_resource_work(&mut self) {
        self.operation(RenderDevice::finish_resource_work);
    }
    fn flush_external_images(&mut self) {
        self.operation(|device| {
            device.collect_native_commands()?;
            device.textures.clear_external_bindings();
            Ok(())
        });
    }
    fn clear_rect(
        &mut self,
        rect: FramebufferIntRect,
        color: Option<[f32; 4]>,
        depth: Option<f32>,
    ) {
        self.operation(|device| device.clear_target(color, depth, Some(rect)));
    }
    fn set_scissor(&mut self, rect: Option<FramebufferIntRect>) {
        self.operation(|device| {
            if device.skipped_pass {
                return Ok(());
            }
            let pass = device
                .pass
                .as_mut()
                .ok_or("No Vulkan render pass is active")?;
            pass.scissor = rect.map(|r| r.cast_unit());
            Ok(())
        });
    }
    fn echo_driver_messages(&self) {}
    fn report_memory(&self) -> MemoryReport {
        MemoryReport {
            depth_target_textures: self.textures.depth_bytes(),
            texture_upload_pbos: self.uploads.bytes(),
            ..MemoryReport::default()
        }
    }
    fn depth_targets_memory(&self) -> usize {
        self.textures.depth_bytes()
    }
}
