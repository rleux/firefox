/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device as wr;
use crate::device::query::{GpuProfiler, GpuQueryBackend, GpuQueryId, GpuQueryKind};
use crate::internal_types::{RenderTargetInfo, Swizzle, SwizzleSettings};
use crate::render_api::MemoryReport;
use api::{ExternalTextureHandle, ImageBufferKind, ImageDescriptor, Parameter};
use api::units::DeviceSize;
use euclid::default::Transform3D;
use malloc_size_of::MallocSizeOfOps;
use std::{borrow::Cow, cell::Cell, mem::MaybeUninit, num::NonZeroUsize, os::raw::c_void, ptr::NonNull};
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

fn empty_vao(descriptor: &wr::VertexDescriptor, divisor: u32) -> wr::VAO {
    wr::VAO {
        id: 0,
        ibo_id: wr::IBOId(0),
        main_vbo_id: wr::VBOId(0),
        instance_vbo_id: wr::VBOId(0),
        instance_stride: descriptor
            .instance_attributes
            .iter()
            .fold(0usize, |stride, attribute| {
                stride.saturating_add(
                    (attribute.count as usize)
                        .saturating_mul(attribute.kind.size_in_bytes() as usize),
                )
            }),
        instance_divisor: divisor,
        owns_vertices_and_indices: false,
        owns_instances: false,
    }
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
            device.draw_instanced(
                base,
                u32::try_from(instances).map_err(|_| "Negative Vulkan instance count")?,
            )
        });
    }
}

impl wr::GpuBackend for RenderDevice {
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
        false
    }
    fn failure(&self) -> Option<&str> {
        RenderDevice::failure(self)
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
    fn bind_external_texture(&mut self, _: wr::TextureSlot, _: &wr::ExternalTexture) {
        self.unsupported("external textures");
    }
    fn reset_read_target(&mut self) {}
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
        self.programs.link(program, descriptor)
    }
    fn bind_pipeline(&mut self, program: &Program, state: &RenderState) -> bool {
        self.operation(|device| RenderDevice::bind_pipeline(device, program, *state))
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
        self.operation(|device| RenderDevice::invalidate_render_target(device, texture));
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
        self.programs.create(name, features, false)
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
    fn bind_shader_samplers(
        &mut self,
        program: &Program,
        bindings: &[(&'static str, wr::TextureSlot)],
    ) {
        self.operation(|device| {
            device.programs.state_mut(program)?.bind_samplers(bindings);
            Ok(())
        });
    }
    fn set_uniforms(&self, program: &Program, transform: &Transform3D<f32>) {
        if self.failure().is_none() {
            self.programs
                .state(program)
                .expect("Vulkan uniforms require a linked program")
                .set_transform(transform);
        }
    }
    fn set_shader_texture_size(&self, _: &Program, _: DeviceSize) {}
    fn create_transfer_buffer_with_size(&mut self, size: usize) -> TransferBuffer {
        self.unsupported("capture readback");
        TransferBuffer {
            id: 0,
            reserved_size: size,
        }
    }
    fn read_pixels_into_transfer_buffer(
        &mut self,
        _: ReadTarget,
        _: DeviceIntRect,
        _: ImageFormat,
        _: &TransferBuffer,
    ) {
        self.unsupported("capture readback");
    }
    fn map_transfer_buffer<'a>(
        &'a mut self,
        _: &'a TransferBuffer,
    ) -> Option<wr::MappedTransferBuffer<'a>> {
        None
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
        self.operation(|device| device.uploads.layout(size, format))
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
        self.operation(|device| device.submissions.create_fence())
    }
    fn poll_fence(&self, fence: &wr::Fence) -> wr::FenceStatus {
        if self.failure().is_some() {
            wr::FenceStatus::Error
        } else {
            self.submissions.poll_fence(fence)
        }
    }
    fn delete_fence(&mut self, _: wr::Fence) {}
    fn upload_texture_immediate(&mut self, texture: &wr::Texture, data: &[u8]) {
        self.operation(|device| RenderDevice::upload_texture_immediate(device, texture, data));
    }
    fn read_pixels(&mut self, _: &ImageDescriptor) -> Vec<u8> {
        self.unsupported("capture readback");
        Vec::new()
    }
    fn read_pixels_into(&mut self, _: FramebufferIntRect, _: ImageFormat, output: &mut [u8]) {
        self.unsupported("capture readback");
        output.fill(0);
    }
    fn attach_read_texture_external(&mut self, _: ExternalTextureHandle, _: ImageBufferKind) {
        self.unsupported("external textures");
    }
    fn attach_read_texture(&mut self, _: &wr::Texture) {
        self.unsupported("capture readback");
    }
    fn bind_vao(&mut self, vao: &wr::VAO) {
        self.operation(|device| device.vertex_arrays.bind(vao));
    }
    fn create_vao(&mut self, descriptor: &wr::VertexDescriptor, divisor: u32) -> wr::VAO {
        self.operation(|device| device.vertex_arrays.create(descriptor, divisor))
            .unwrap_or_else(|| empty_vao(descriptor, divisor))
    }
    fn delete_vao(&mut self, mut vao: wr::VAO) {
        let result = self.vertex_arrays.delete(&mut vao);
        self.record_result(result);
        vao.id = 0;
    }
    fn create_vao_with_new_instances(
        &mut self,
        descriptor: &wr::VertexDescriptor,
        base: &wr::VAO,
    ) -> wr::VAO {
        self.operation(|device| device.vertex_arrays.with_new_instances(descriptor, base))
            .unwrap_or_else(|| empty_vao(descriptor, base.instance_divisor))
    }
    fn create_vao_with_shared_instances(
        &mut self,
        descriptor: &wr::VertexDescriptor,
        base: &wr::VAO,
    ) -> wr::VAO {
        self.operation(|device| device.vertex_arrays.with_shared_instances(descriptor, base))
            .unwrap_or_else(|| empty_vao(descriptor, base.instance_divisor))
    }
    fn update_vao_main_vertices(&mut self, vao: &wr::VAO, bytes: &[u8], _: wr::VertexUsageHint) {
        self.operation(|device| device.vertex_arrays.update_vertices(vao, bytes));
    }
    fn update_vao_instances(
        &mut self,
        vao: &wr::VAO,
        bytes: &[u8],
        stride: usize,
        _: wr::VertexUsageHint,
        repeat: Option<NonZeroUsize>,
    ) {
        self.operation(|device| {
            device
                .vertex_arrays
                .update_instances(vao, bytes, stride, repeat)
        });
    }
    fn update_vao_indices(&mut self, vao: &wr::VAO, bytes: &[u8], _: wr::VertexUsageHint) {
        self.operation(|device| device.vertex_arrays.update_indices(vao, bytes));
    }
    fn reallocate_vbo(&mut self, id: wr::VBOId, size: usize) {
        self.operation(|device| device.vertex_arrays.reallocate(id, size));
    }
    fn update_vbo_data_unsynchronized(&mut self, id: wr::VBOId, bytes: &[u8], offset: usize) {
        self.operation(|device| device.vertex_arrays.update_range(id, offset, bytes));
    }
    fn draw_triangles_u32(&mut self, _: i32, count: i32) {
        if count != 0 {
            self.unsupported("non-instanced triangles");
        }
    }
    fn draw_nonindexed_lines(&mut self, _: i32, count: i32) {
        if count != 0 {
            self.unsupported("line drawing");
        }
    }
    fn draw_indexed_triangles(&mut self, count: i32) {
        if count != 0 {
            self.unsupported("non-instanced triangles");
        }
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
        let result = self.submissions.wait();
        self.record_result(result);
        let result = self.submissions.trim();
        self.record_result(result);
        self.blitter = None;
        self.scratch.clear();
        self.programs.unbind();
        self.vertex_arrays.unbind();
        self.textures.reset_bindings();
    }
    fn end_frame(&mut self) {
        self.operation(RenderDevice::end_frame);
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
        match rect {
            Some(rect) => {
                self.passes.set_scissor_rect(rect);
                self.passes.enable_scissor();
            }
            None => self.passes.disable_scissor(),
        }
    }
    fn echo_driver_messages(&self) {}
    fn report_memory(&self, _: &MallocSizeOfOps, _: *mut c_void) -> MemoryReport {
        MemoryReport {
            depth_target_textures: self.textures.depth_bytes(),
            ..MemoryReport::default()
        }
    }
    fn depth_targets_memory(&self) -> usize {
        self.textures.depth_bytes()
    }
}

#[cfg(test)]
#[path = "gpu_backend_tests.rs"]
mod tests;
