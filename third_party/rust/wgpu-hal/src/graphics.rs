/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use alloc::{boxed::Box, sync::Arc};
use core::{fmt, ops::Range};

use crate::{
    BufferBinding, DeviceError, DynBindGroup, DynBuffer, DynCommandBuffer, DynOpenDevice,
    DynPipelineLayout, DynQuerySet, DynQueue, DynRenderPipeline, DynTextureView, Label, Rect,
    RenderPassDescriptor,
};

/// Borrowed graphics arguments consumed by one synchronous recorder call.
#[derive(Debug)]
pub enum GraphicsCommand<'a> {
    SetBindGroup {
        layout: &'a dyn DynPipelineLayout,
        index: u32,
        group: &'a dyn DynBindGroup,
        dynamic_offsets: &'a [wgt::DynamicOffset],
    },
    SetImmediates {
        layout: &'a dyn DynPipelineLayout,
        offset_bytes: u32,
        data: &'a [u32],
    },
    InsertDebugMarker {
        label: &'a str,
    },
    BeginDebugMarker {
        label: &'a str,
    },
    EndDebugMarker,
    BeginQuery {
        set: &'a dyn DynQuerySet,
        index: u32,
    },
    EndQuery {
        set: &'a dyn DynQuerySet,
        index: u32,
    },
    WriteTimestamp {
        set: &'a dyn DynQuerySet,
        index: u32,
    },
    SetRenderPipeline {
        pipeline: &'a dyn DynRenderPipeline,
    },
    SetIndexBuffer {
        binding: BufferBinding<'a, dyn DynBuffer, wgt::BufferAddress>,
        format: wgt::IndexFormat,
    },
    SetVertexBuffer {
        index: u32,
        binding: BufferBinding<'a, dyn DynBuffer, wgt::BufferAddress>,
    },
    SetViewport {
        rect: Rect<f32>,
        depth_range: Range<f32>,
    },
    SetScissorRect {
        rect: Rect<u32>,
    },
    SetStencilReference {
        value: u32,
    },
    SetBlendConstants {
        color: [f32; 4],
    },
    Draw {
        first_vertex: u32,
        vertex_count: u32,
        first_instance: u32,
        instance_count: u32,
    },
    DrawIndexed {
        first_index: u32,
        index_count: u32,
        base_vertex: i32,
        first_instance: u32,
        instance_count: u32,
    },
    DrawMeshTasks {
        group_count_x: u32,
        group_count_y: u32,
        group_count_z: u32,
    },
    DrawIndirect {
        buffer: &'a dyn DynBuffer,
        offset: wgt::BufferAddress,
        draw_count: u32,
    },
    DrawIndexedIndirect {
        buffer: &'a dyn DynBuffer,
        offset: wgt::BufferAddress,
        draw_count: u32,
    },
    DrawMeshTasksIndirect {
        buffer: &'a dyn DynBuffer,
        offset: wgt::BufferAddress,
        draw_count: u32,
    },
    DrawIndirectCount {
        buffer: &'a dyn DynBuffer,
        offset: wgt::BufferAddress,
        count_buffer: &'a dyn DynBuffer,
        count_offset: wgt::BufferAddress,
        max_count: u32,
    },
    DrawIndexedIndirectCount {
        buffer: &'a dyn DynBuffer,
        offset: wgt::BufferAddress,
        count_buffer: &'a dyn DynBuffer,
        count_offset: wgt::BufferAddress,
        max_count: u32,
    },
    DrawMeshTasksIndirectCount {
        buffer: &'a dyn DynBuffer,
        offset: wgt::BufferAddress,
        count_buffer: &'a dyn DynBuffer,
        count_offset: wgt::BufferAddress,
        max_count: u32,
    },
}

/// All command buffers remain owned together with their originating encoder.
/// The surrounding owner must retain the logical device and resources until GPU retirement.
/// A failed submission or logical device loss alone does not establish retirement.
pub trait GraphicsExecution: fmt::Debug + wgt::WasmNotSendSync {
    fn command_buffers(&self) -> &[Box<dyn DynCommandBuffer>];
    fn queue(&self) -> &dyn DynQueue;
}

/// Records only graphics work; compute, copies and preparation barriers remain separate.
///
/// # Safety
/// Callers must satisfy the corresponding CommandEncoder requirements, retain the
/// device/resources, and keep finished execution tokens alive until GPU completion.
/// Dropping an unfinished recorder discards its unsubmitted work.
pub trait GraphicsRecorder: fmt::Debug + wgt::WasmNotSendSync {
    unsafe fn begin_encoding(&mut self, label: Label) -> Result<(), DeviceError>;
    unsafe fn begin_render_pass(
        &mut self,
        desc: &RenderPassDescriptor<dyn DynQuerySet, dyn DynTextureView>,
    ) -> Result<(), DeviceError>;
    unsafe fn record(&mut self, command: GraphicsCommand<'_>);
    unsafe fn end_render_pass(&mut self);
    unsafe fn end_encoding(&mut self) -> Result<usize, DeviceError>;
    fn finish(self: Box<Self>) -> Result<Box<dyn GraphicsExecution>, DeviceError>;
}

pub trait GraphicsRecorderFactory: fmt::Debug + wgt::WasmNotSendSync {
    /// # Safety
    /// The endpoint's queue must support graphics on its device. The recorder
    /// and execution retain the endpoint; callers must retain referenced logical
    /// resources and execution tokens until confirmed native retirement.
    unsafe fn create(
        &self,
        owner: Arc<DynOpenDevice>,
    ) -> Result<Box<dyn GraphicsRecorder>, DeviceError>;
}
