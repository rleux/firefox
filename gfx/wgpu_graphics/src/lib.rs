/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

extern crate alloc;

use wgpu_hal as hal;
use wgpu_types as wgt;

use alloc::{boxed::Box, string::String, vec::Vec};
use core::{any::Any, mem, ops::Range};
use hal::*;

#[derive(Debug)]
pub enum Segment {
    Native(Box<dyn DynCommandBuffer>),
    Graphics(usize),
}

#[derive(Debug)]
pub struct RoutedCommandBuffer {
    segments: Vec<Segment>,
}

impl RoutedCommandBuffer {
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }
}

impl DynResource for RoutedCommandBuffer {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
impl DynCommandBuffer for RoutedCommandBuffer {}

#[derive(Debug)]
pub struct RoutedEncoder {
    native: Box<dyn DynCommandEncoder>,
    graphics: Option<Box<dyn GraphicsRecorder>>,
    segments: Vec<Segment>,
    discarded: Vec<Box<dyn DynCommandBuffer>>,
    label: Option<String>,
    debug_labels: Vec<String>,
    native_open: bool,
    graphics_open: bool,
    in_render_pass: bool,
    error: Option<DeviceError>,
}

impl RoutedEncoder {
    pub fn new(native: Box<dyn DynCommandEncoder>, graphics: Box<dyn GraphicsRecorder>) -> Self {
        Self {
            native,
            graphics: Some(graphics),
            segments: Vec::new(),
            discarded: Vec::new(),
            label: None,
            debug_labels: Vec::new(),
            native_open: false,
            graphics_open: false,
            in_render_pass: false,
            error: None,
        }
    }

    unsafe fn close_native(&mut self) -> Result<(), DeviceError> {
        for _ in &self.debug_labels {
            unsafe { self.native.end_debug_marker() };
        }
        self.native_open = false;
        let buffer = unsafe { self.native.end_encoding()? };
        self.segments.push(Segment::Native(buffer));
        Ok(())
    }

    fn take_native_segments(&mut self) {
        for segment in self.segments.drain(..) {
            if let Segment::Native(buffer) = segment {
                self.discarded.push(buffer);
            }
        }
    }

    unsafe fn graphics_command(&mut self, command: GraphicsCommand<'_>) {
        if self.error.is_none() {
            assert!(self.in_render_pass);
            unsafe { self.graphics.as_mut().unwrap().record(command) };
        }
    }
}

impl DynResource for RoutedEncoder {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl DynCommandEncoder for RoutedEncoder {
    unsafe fn begin_encoding(&mut self, label: Label) -> Result<(), DeviceError> {
        assert!(!self.native_open && !self.graphics_open);
        self.label = label.map(String::from);
        self.native_open = true;
        if let Err(error) = unsafe { self.native.begin_encoding(label) } {
            self.native_open = false;
            return Err(error);
        }
        for label in &self.debug_labels {
            unsafe { self.native.begin_debug_marker(label) };
        }
        Ok(())
    }

    unsafe fn discard_encoding(&mut self) {
        if self.native_open {
            unsafe { self.native.discard_encoding() };
            self.native_open = false;
        }
        // The recorder owns every graphics segment, including any partial pass.
        self.graphics.take();
        self.graphics_open = false;
        self.in_render_pass = false;
        self.take_native_segments();
        self.debug_labels.clear();
    }

    unsafe fn end_encoding(&mut self) -> Result<Box<dyn DynCommandBuffer>, DeviceError> {
        if let Some(error) = self.error.take() {
            return Err(error);
        }
        assert!(!self.in_render_pass && !self.graphics_open);
        unsafe { self.close_native()? };
        Ok(Box::new(RoutedCommandBuffer {
            segments: mem::take(&mut self.segments),
        }))
    }

    unsafe fn reset_all(&mut self, command_buffers: Vec<Box<dyn DynCommandBuffer>>) {
        assert!(!self.native_open);
        self.take_native_segments();
        for mut buffer in command_buffers {
            let buffer = buffer
                .as_any_mut()
                .downcast_mut::<RoutedCommandBuffer>()
                .unwrap();
            for segment in mem::take(&mut buffer.segments) {
                if let Segment::Native(buffer) = segment {
                    self.discarded.push(buffer);
                }
            }
        }
        unsafe { self.native.reset_all(mem::take(&mut self.discarded)) };
        self.graphics.take();
    }

    unsafe fn begin_render_pass(
        &mut self,
        desc: &RenderPassDescriptor<dyn DynQuerySet, dyn DynTextureView>,
    ) -> Result<(), DeviceError> {
        if let Some(error) = self.error.take() {
            return Err(error);
        }
        unsafe { self.close_native()? };
        let graphics = self.graphics.as_mut().unwrap();
        unsafe { graphics.begin_encoding(self.label.as_deref())? };
        self.graphics_open = true;
        unsafe { graphics.begin_render_pass(desc)? };
        self.in_render_pass = true;
        for label in &self.debug_labels {
            unsafe { graphics.record(GraphicsCommand::BeginDebugMarker { label }) };
        }
        Ok(())
    }

    unsafe fn end_render_pass(&mut self) {
        if self.error.is_some() {
            return;
        }
        let graphics = self.graphics.as_mut().unwrap();
        for _ in &self.debug_labels {
            unsafe { graphics.record(GraphicsCommand::EndDebugMarker) };
        }
        unsafe { graphics.end_render_pass() };
        self.in_render_pass = false;
        self.graphics_open = false;
        match unsafe { graphics.end_encoding() } {
            Ok(index) => self.segments.push(Segment::Graphics(index)),
            Err(error) => {
                self.error = Some(error);
                return;
            }
        }
        match unsafe { self.native.begin_encoding(self.label.as_deref()) } {
            Ok(()) => {
                self.native_open = true;
                for label in &self.debug_labels {
                    unsafe { self.native.begin_debug_marker(label) };
                }
            }
            Err(error) => self.error = Some(error),
        }
    }

    unsafe fn set_acceleration_structure_dependencies(
        &self,
        command_buffers: &[Box<dyn DynCommandBuffer>],
        dependencies: &[&dyn DynAccelerationStructure],
    ) {
        for buffer in command_buffers {
            let buffer = buffer
                .as_any()
                .downcast_ref::<RoutedCommandBuffer>()
                .unwrap();
            for segment in &buffer.segments {
                if let Segment::Native(buffer) = segment {
                    unsafe {
                        self.native.set_acceleration_structure_dependencies(
                            core::slice::from_ref(buffer),
                            dependencies,
                        )
                    };
                }
            }
        }
    }
    unsafe fn transition_buffers(&mut self, barriers: &[BufferBarrier<'_, dyn DynBuffer>]) {
        if self.error.is_some() {
            return;
        }
        assert!(!self.in_render_pass);
        unsafe { self.native.transition_buffers(barriers) };
    }

    unsafe fn transition_textures(&mut self, barriers: &[TextureBarrier<'_, dyn DynTexture>]) {
        if self.error.is_some() {
            return;
        }
        assert!(!self.in_render_pass);
        unsafe { self.native.transition_textures(barriers) };
    }

    unsafe fn clear_buffer(&mut self, buffer: &dyn DynBuffer, range: MemoryRange) {
        if self.error.is_some() {
            return;
        }
        assert!(!self.in_render_pass);
        unsafe { self.native.clear_buffer(buffer, range) };
    }

    unsafe fn copy_buffer_to_buffer(
        &mut self,
        src: &dyn DynBuffer,
        dst: &dyn DynBuffer,
        regions: &[BufferCopy],
    ) {
        if self.error.is_some() {
            return;
        }
        assert!(!self.in_render_pass);
        unsafe { self.native.copy_buffer_to_buffer(src, dst, regions) };
    }

    unsafe fn copy_texture_to_texture(
        &mut self,
        src: &dyn DynTexture,
        src_usage: wgt::TextureUses,
        dst: &dyn DynTexture,
        regions: &[TextureCopy],
    ) {
        if self.error.is_some() {
            return;
        }
        assert!(!self.in_render_pass);
        unsafe {
            self.native
                .copy_texture_to_texture(src, src_usage, dst, regions)
        };
    }

    unsafe fn copy_buffer_to_texture(
        &mut self,
        src: &dyn DynBuffer,
        dst: &dyn DynTexture,
        regions: &[BufferTextureCopy],
    ) {
        if self.error.is_some() {
            return;
        }
        assert!(!self.in_render_pass);
        unsafe { self.native.copy_buffer_to_texture(src, dst, regions) };
    }

    unsafe fn copy_texture_to_buffer(
        &mut self,
        src: &dyn DynTexture,
        src_usage: wgt::TextureUses,
        dst: &dyn DynBuffer,
        regions: &[BufferTextureCopy],
    ) {
        if self.error.is_some() {
            return;
        }
        assert!(!self.in_render_pass);
        unsafe {
            self.native
                .copy_texture_to_buffer(src, src_usage, dst, regions)
        };
    }

    unsafe fn set_bind_group(
        &mut self,
        layout: &dyn DynPipelineLayout,
        index: u32,
        group: &dyn DynBindGroup,
        dynamic_offsets: &[wgt::DynamicOffset],
    ) {
        if self.error.is_some() {
            return;
        }
        if self.in_render_pass {
            unsafe {
                self.graphics_command(GraphicsCommand::SetBindGroup {
                    layout,
                    index,
                    group,
                    dynamic_offsets,
                })
            };
        } else {
            unsafe {
                self.native
                    .set_bind_group(layout, index, group, dynamic_offsets)
            };
        }
    }

    unsafe fn set_immediates(
        &mut self,
        layout: &dyn DynPipelineLayout,
        offset_bytes: u32,
        data: &[u32],
    ) {
        if self.error.is_some() {
            return;
        }
        if self.in_render_pass {
            unsafe {
                self.graphics_command(GraphicsCommand::SetImmediates {
                    layout,
                    offset_bytes,
                    data,
                })
            };
        } else {
            unsafe { self.native.set_immediates(layout, offset_bytes, data) };
        }
    }

    unsafe fn insert_debug_marker(&mut self, label: &str) {
        if self.error.is_some() {
            return;
        }
        if self.in_render_pass {
            unsafe { self.graphics_command(GraphicsCommand::InsertDebugMarker { label }) };
        } else {
            unsafe { self.native.insert_debug_marker(label) };
        }
    }

    unsafe fn begin_debug_marker(&mut self, group_label: &str) {
        if self.error.is_some() {
            return;
        }
        if self.in_render_pass {
            unsafe {
                self.graphics_command(GraphicsCommand::BeginDebugMarker { label: group_label })
            };
        } else {
            unsafe { self.native.begin_debug_marker(group_label) };
        }
        self.debug_labels.push(String::from(group_label));
    }

    unsafe fn end_debug_marker(&mut self) {
        if self.error.is_some() {
            return;
        }
        self.debug_labels.pop().unwrap();
        if self.in_render_pass {
            unsafe { self.graphics_command(GraphicsCommand::EndDebugMarker) };
        } else {
            unsafe { self.native.end_debug_marker() };
        }
    }

    unsafe fn begin_query(&mut self, set: &dyn DynQuerySet, index: u32) {
        if self.error.is_some() {
            return;
        }
        if self.in_render_pass {
            unsafe { self.graphics_command(GraphicsCommand::BeginQuery { set, index }) };
        } else {
            unsafe { self.native.begin_query(set, index) };
        }
    }

    unsafe fn end_query(&mut self, set: &dyn DynQuerySet, index: u32) {
        if self.error.is_some() {
            return;
        }
        if self.in_render_pass {
            unsafe { self.graphics_command(GraphicsCommand::EndQuery { set, index }) };
        } else {
            unsafe { self.native.end_query(set, index) };
        }
    }

    unsafe fn write_timestamp(&mut self, set: &dyn DynQuerySet, index: u32) {
        if self.error.is_some() {
            return;
        }
        if self.in_render_pass {
            unsafe { self.graphics_command(GraphicsCommand::WriteTimestamp { set, index }) };
        } else {
            unsafe { self.native.write_timestamp(set, index) };
        }
    }

    unsafe fn reset_queries(&mut self, set: &dyn DynQuerySet, range: Range<u32>) {
        if self.error.is_some() {
            return;
        }
        assert!(!self.in_render_pass);
        unsafe { self.native.reset_queries(set, range) };
    }

    unsafe fn copy_query_results(
        &mut self,
        set: &dyn DynQuerySet,
        range: Range<u32>,
        buffer: &dyn DynBuffer,
        offset: wgt::BufferAddress,
        stride: wgt::BufferSize,
    ) {
        if self.error.is_some() {
            return;
        }
        assert!(!self.in_render_pass);
        unsafe {
            self.native
                .copy_query_results(set, range, buffer, offset, stride)
        };
    }

    unsafe fn set_render_pipeline(&mut self, pipeline: &dyn DynRenderPipeline) {
        unsafe { self.graphics_command(GraphicsCommand::SetRenderPipeline { pipeline }) };
    }

    unsafe fn set_index_buffer<'a>(
        &mut self,
        binding: BufferBinding<'a, dyn DynBuffer, wgt::BufferAddress>,
        format: wgt::IndexFormat,
    ) {
        unsafe { self.graphics_command(GraphicsCommand::SetIndexBuffer { binding, format }) };
    }

    unsafe fn set_vertex_buffer<'a>(
        &mut self,
        index: u32,
        binding: BufferBinding<'a, dyn DynBuffer, wgt::BufferAddress>,
    ) {
        unsafe { self.graphics_command(GraphicsCommand::SetVertexBuffer { index, binding }) };
    }

    unsafe fn set_viewport(&mut self, rect: &Rect<f32>, depth_range: Range<f32>) {
        unsafe {
            self.graphics_command(GraphicsCommand::SetViewport {
                rect: rect.clone(),
                depth_range,
            })
        };
    }

    unsafe fn set_scissor_rect(&mut self, rect: &Rect<u32>) {
        unsafe { self.graphics_command(GraphicsCommand::SetScissorRect { rect: rect.clone() }) };
    }

    unsafe fn set_stencil_reference(&mut self, value: u32) {
        unsafe { self.graphics_command(GraphicsCommand::SetStencilReference { value }) };
    }

    unsafe fn set_blend_constants(&mut self, color: &[f32; 4]) {
        unsafe { self.graphics_command(GraphicsCommand::SetBlendConstants { color: *color }) };
    }

    unsafe fn draw(
        &mut self,
        first_vertex: u32,
        vertex_count: u32,
        first_instance: u32,
        instance_count: u32,
    ) {
        unsafe {
            self.graphics_command(GraphicsCommand::Draw {
                first_vertex,
                vertex_count,
                first_instance,
                instance_count,
            })
        };
    }

    unsafe fn draw_indexed(
        &mut self,
        first_index: u32,
        index_count: u32,
        base_vertex: i32,
        first_instance: u32,
        instance_count: u32,
    ) {
        unsafe {
            self.graphics_command(GraphicsCommand::DrawIndexed {
                first_index,
                index_count,
                base_vertex,
                first_instance,
                instance_count,
            })
        };
    }

    unsafe fn draw_mesh_tasks(
        &mut self,
        group_count_x: u32,
        group_count_y: u32,
        group_count_z: u32,
    ) {
        unsafe {
            self.graphics_command(GraphicsCommand::DrawMeshTasks {
                group_count_x,
                group_count_y,
                group_count_z,
            })
        };
    }

    unsafe fn draw_indirect(
        &mut self,
        buffer: &dyn DynBuffer,
        offset: wgt::BufferAddress,
        draw_count: u32,
    ) {
        unsafe {
            self.graphics_command(GraphicsCommand::DrawIndirect {
                buffer,
                offset,
                draw_count,
            })
        };
    }

    unsafe fn draw_indexed_indirect(
        &mut self,
        buffer: &dyn DynBuffer,
        offset: wgt::BufferAddress,
        draw_count: u32,
    ) {
        unsafe {
            self.graphics_command(GraphicsCommand::DrawIndexedIndirect {
                buffer,
                offset,
                draw_count,
            })
        };
    }

    unsafe fn draw_mesh_tasks_indirect(
        &mut self,
        buffer: &dyn DynBuffer,
        offset: wgt::BufferAddress,
        draw_count: u32,
    ) {
        unsafe {
            self.graphics_command(GraphicsCommand::DrawMeshTasksIndirect {
                buffer,
                offset,
                draw_count,
            })
        };
    }

    unsafe fn draw_indirect_count(
        &mut self,
        buffer: &dyn DynBuffer,
        offset: wgt::BufferAddress,
        count_buffer: &dyn DynBuffer,
        count_offset: wgt::BufferAddress,
        max_count: u32,
    ) {
        unsafe {
            self.graphics_command(GraphicsCommand::DrawIndirectCount {
                buffer,
                offset,
                count_buffer,
                count_offset,
                max_count,
            })
        };
    }

    unsafe fn draw_indexed_indirect_count(
        &mut self,
        buffer: &dyn DynBuffer,
        offset: wgt::BufferAddress,
        count_buffer: &dyn DynBuffer,
        count_offset: wgt::BufferAddress,
        max_count: u32,
    ) {
        unsafe {
            self.graphics_command(GraphicsCommand::DrawIndexedIndirectCount {
                buffer,
                offset,
                count_buffer,
                count_offset,
                max_count,
            })
        };
    }

    unsafe fn draw_mesh_tasks_indirect_count(
        &mut self,
        buffer: &dyn DynBuffer,
        offset: wgt::BufferAddress,
        count_buffer: &dyn DynBuffer,
        count_offset: wgt::BufferAddress,
        max_count: u32,
    ) {
        unsafe {
            self.graphics_command(GraphicsCommand::DrawMeshTasksIndirectCount {
                buffer,
                offset,
                count_buffer,
                count_offset,
                max_count,
            })
        };
    }

    unsafe fn begin_compute_pass(&mut self, desc: &ComputePassDescriptor<dyn DynQuerySet>) {
        if self.error.is_some() {
            return;
        }
        assert!(!self.in_render_pass);
        unsafe { self.native.begin_compute_pass(desc) };
    }

    unsafe fn end_compute_pass(&mut self) {
        if self.error.is_some() {
            return;
        }
        assert!(!self.in_render_pass);
        unsafe { self.native.end_compute_pass() };
    }

    unsafe fn set_compute_pipeline(&mut self, pipeline: &dyn DynComputePipeline) {
        if self.error.is_some() {
            return;
        }
        assert!(!self.in_render_pass);
        unsafe { self.native.set_compute_pipeline(pipeline) };
    }

    unsafe fn dispatch_workgroups(&mut self, count: [u32; 3]) {
        if self.error.is_some() {
            return;
        }
        assert!(!self.in_render_pass);
        unsafe { self.native.dispatch_workgroups(count) };
    }

    unsafe fn dispatch_workgroups_indirect(
        &mut self,
        buffer: &dyn DynBuffer,
        offset: wgt::BufferAddress,
    ) {
        if self.error.is_some() {
            return;
        }
        assert!(!self.in_render_pass);
        unsafe { self.native.dispatch_workgroups_indirect(buffer, offset) };
    }

    unsafe fn begin_ray_tracing_pass(&mut self, desc: &RayTracingPassDescriptor) {
        if self.error.is_some() {
            return;
        }
        assert!(!self.in_render_pass);
        unsafe { self.native.begin_ray_tracing_pass(desc) };
    }

    unsafe fn end_ray_tracing_pass(&mut self) {
        if self.error.is_some() {
            return;
        }
        assert!(!self.in_render_pass);
        unsafe { self.native.end_ray_tracing_pass() };
    }

    unsafe fn trace_rays(
        &mut self,
        count: [u32; 3],
        ray_generation_group_data: hal::PipelineGroupData<dyn DynBuffer>,
        miss_group_data: hal::PipelineGroupData<dyn DynBuffer>,
        intersection_group_data: hal::PipelineGroupData<dyn DynBuffer>,
    ) {
        if self.error.is_some() {
            return;
        }
        assert!(!self.in_render_pass);
        unsafe {
            self.native.trace_rays(
                count,
                ray_generation_group_data,
                miss_group_data,
                intersection_group_data,
            )
        };
    }

    unsafe fn set_ray_tracing_pipeline(&mut self, pipeline: &dyn DynRayTracingPipeline) {
        if self.error.is_some() {
            return;
        }
        assert!(!self.in_render_pass);
        unsafe { self.native.set_ray_tracing_pipeline(pipeline) };
    }

    unsafe fn build_acceleration_structures<'a>(
        &mut self,
        descriptors: &'a [BuildAccelerationStructureDescriptor<
            'a,
            dyn DynBuffer,
            dyn DynAccelerationStructure,
        >],
    ) {
        if self.error.is_some() {
            return;
        }
        assert!(!self.in_render_pass);
        unsafe { self.native.build_acceleration_structures(descriptors) };
    }

    unsafe fn place_acceleration_structure_barrier(
        &mut self,
        barrier: AccelerationStructureBarrier,
    ) {
        if self.error.is_some() {
            return;
        }
        assert!(!self.in_render_pass);
        unsafe { self.native.place_acceleration_structure_barrier(barrier) };
    }

    unsafe fn copy_acceleration_structure_to_acceleration_structure(
        &mut self,
        src: &dyn DynAccelerationStructure,
        dst: &dyn DynAccelerationStructure,
        copy: wgt::AccelerationStructureCopy,
    ) {
        if self.error.is_some() {
            return;
        }
        assert!(!self.in_render_pass);
        unsafe {
            self.native
                .copy_acceleration_structure_to_acceleration_structure(src, dst, copy)
        };
    }

    unsafe fn read_acceleration_structure_compact_size(
        &mut self,
        acceleration_structure: &dyn DynAccelerationStructure,
        buf: &dyn DynBuffer,
    ) {
        if self.error.is_some() {
            return;
        }
        assert!(!self.in_render_pass);
        unsafe {
            self.native
                .read_acceleration_structure_compact_size(acceleration_structure, buf)
        };
    }
}
