/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::hal;
use std::{fmt, sync::Arc};

#[derive(Debug, Default)]
pub struct GraphicsRecorderFactory;

impl hal::GraphicsRecorderFactory for GraphicsRecorderFactory {
    unsafe fn create(
        &self,
        owner: Arc<hal::DynOpenDevice>,
    ) -> Result<Box<dyn hal::GraphicsRecorder>, hal::DeviceError> {
        let encoder = unsafe {
            owner
                .device
                .create_command_encoder(&hal::CommandEncoderDescriptor {
                    label: Some("WR graphics recorder"),
                    queue: &*owner.queue,
                })
        }?;
        Ok(Box::new(Recorder {
            encoder: Some(encoder),
            buffers: Vec::new(),
            recording: false,
            pass: false,
            failed: false,
            owner,
        }))
    }
}

pub(super) unsafe fn begin_render_pass(
    encoder: &mut dyn hal::DynCommandEncoder,
    desc: &hal::RenderPassDescriptor<dyn hal::DynQuerySet, dyn hal::DynTextureView>,
) -> Result<(), hal::DeviceError> {
    unsafe { encoder.begin_render_pass(desc) }
}

pub(super) unsafe fn record(
    encoder: &mut dyn hal::DynCommandEncoder,
    command: hal::GraphicsCommand<'_>,
) {
    unsafe {
        match command {
            hal::GraphicsCommand::SetBindGroup {
                layout,
                index,
                group,
                dynamic_offsets,
            } => encoder.set_bind_group(layout, index, group, dynamic_offsets),
            hal::GraphicsCommand::SetImmediates {
                layout,
                offset_bytes,
                data,
            } => encoder.set_immediates(layout, offset_bytes, data),
            hal::GraphicsCommand::InsertDebugMarker { label } => encoder.insert_debug_marker(label),
            hal::GraphicsCommand::BeginDebugMarker { label } => encoder.begin_debug_marker(label),
            hal::GraphicsCommand::EndDebugMarker => encoder.end_debug_marker(),
            hal::GraphicsCommand::BeginQuery { set, index } => encoder.begin_query(set, index),
            hal::GraphicsCommand::EndQuery { set, index } => encoder.end_query(set, index),
            hal::GraphicsCommand::WriteTimestamp { set, index } => {
                encoder.write_timestamp(set, index)
            }
            hal::GraphicsCommand::SetRenderPipeline { pipeline } => {
                encoder.set_render_pipeline(pipeline)
            }
            hal::GraphicsCommand::SetIndexBuffer { binding, format } => {
                encoder.set_index_buffer(binding, format)
            }
            hal::GraphicsCommand::SetVertexBuffer { index, binding } => {
                encoder.set_vertex_buffer(index, binding)
            }
            hal::GraphicsCommand::SetViewport { rect, depth_range } => {
                encoder.set_viewport(&rect, depth_range)
            }
            hal::GraphicsCommand::SetScissorRect { rect } => encoder.set_scissor_rect(&rect),
            hal::GraphicsCommand::SetStencilReference { value } => {
                encoder.set_stencil_reference(value)
            }
            hal::GraphicsCommand::SetBlendConstants { color } => {
                encoder.set_blend_constants(&color)
            }
            hal::GraphicsCommand::Draw {
                first_vertex,
                vertex_count,
                first_instance,
                instance_count,
            } => encoder.draw(first_vertex, vertex_count, first_instance, instance_count),
            hal::GraphicsCommand::DrawIndexed {
                first_index,
                index_count,
                base_vertex,
                first_instance,
                instance_count,
            } => encoder.draw_indexed(
                first_index,
                index_count,
                base_vertex,
                first_instance,
                instance_count,
            ),
            hal::GraphicsCommand::DrawMeshTasks {
                group_count_x,
                group_count_y,
                group_count_z,
            } => encoder.draw_mesh_tasks(group_count_x, group_count_y, group_count_z),
            hal::GraphicsCommand::DrawIndirect {
                buffer,
                offset,
                draw_count,
            } => encoder.draw_indirect(buffer, offset, draw_count),
            hal::GraphicsCommand::DrawIndexedIndirect {
                buffer,
                offset,
                draw_count,
            } => encoder.draw_indexed_indirect(buffer, offset, draw_count),
            hal::GraphicsCommand::DrawMeshTasksIndirect {
                buffer,
                offset,
                draw_count,
            } => encoder.draw_mesh_tasks_indirect(buffer, offset, draw_count),
            hal::GraphicsCommand::DrawIndirectCount {
                buffer,
                offset,
                count_buffer,
                count_offset,
                max_count,
            } => encoder.draw_indirect_count(buffer, offset, count_buffer, count_offset, max_count),
            hal::GraphicsCommand::DrawIndexedIndirectCount {
                buffer,
                offset,
                count_buffer,
                count_offset,
                max_count,
            } => encoder.draw_indexed_indirect_count(
                buffer,
                offset,
                count_buffer,
                count_offset,
                max_count,
            ),
            hal::GraphicsCommand::DrawMeshTasksIndirectCount {
                buffer,
                offset,
                count_buffer,
                count_offset,
                max_count,
            } => encoder.draw_mesh_tasks_indirect_count(
                buffer,
                offset,
                count_buffer,
                count_offset,
                max_count,
            ),
        }
    }
}

pub(super) unsafe fn end_render_pass(encoder: &mut dyn hal::DynCommandEncoder) {
    unsafe { encoder.end_render_pass() }
}

struct Recorder {
    encoder: Option<Box<dyn hal::DynCommandEncoder>>,
    buffers: Vec<Box<dyn hal::DynCommandBuffer>>,
    recording: bool,
    pass: bool,
    failed: bool,
    owner: Arc<hal::DynOpenDevice>,
}

impl fmt::Debug for Recorder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WRGraphicsRecorder")
            .field("buffers", &self.buffers.len())
            .field("recording", &self.recording)
            .field("pass", &self.pass)
            .field("failed", &self.failed)
            .finish()
    }
}

impl hal::GraphicsRecorder for Recorder {
    unsafe fn begin_encoding(&mut self, label: hal::Label) -> Result<(), hal::DeviceError> {
        if self.recording || self.failed {
            return Err(hal::DeviceError::Unexpected);
        }
        let result = unsafe { self.encoder.as_mut().unwrap().begin_encoding(label) };
        self.recording = result.is_ok();
        self.failed = result.is_err();
        result
    }

    unsafe fn begin_render_pass(
        &mut self,
        desc: &hal::RenderPassDescriptor<dyn hal::DynQuerySet, dyn hal::DynTextureView>,
    ) -> Result<(), hal::DeviceError> {
        if !self.recording || self.pass || self.failed {
            return Err(hal::DeviceError::Unexpected);
        }
        let result = unsafe { begin_render_pass(self.encoder.as_mut().unwrap().as_mut(), desc) };
        self.pass = result.is_ok();
        self.failed = result.is_err();
        result
    }

    unsafe fn record(&mut self, command: hal::GraphicsCommand<'_>) {
        assert!(self.recording && !self.failed);
        unsafe { record(self.encoder.as_mut().unwrap().as_mut(), command) };
    }

    unsafe fn end_render_pass(&mut self) {
        assert!(self.recording && self.pass && !self.failed);
        unsafe { end_render_pass(self.encoder.as_mut().unwrap().as_mut()) };
        self.pass = false;
    }

    unsafe fn end_encoding(&mut self) -> Result<usize, hal::DeviceError> {
        if !self.recording || self.pass || self.failed {
            return Err(hal::DeviceError::Unexpected);
        }
        self.recording = false;
        match unsafe { self.encoder.as_mut().unwrap().end_encoding() } {
            Ok(buffer) => {
                let index = self.buffers.len();
                self.buffers.push(buffer);
                Ok(index)
            }
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    fn finish(mut self: Box<Self>) -> Result<Box<dyn hal::GraphicsExecution>, hal::DeviceError> {
        if self.recording || self.failed {
            return Err(hal::DeviceError::Unexpected);
        }
        Ok(Box::new(Execution {
            encoder: self.encoder.take().unwrap(),
            buffers: std::mem::take(&mut self.buffers),
            _owner: self.owner.clone(),
        }))
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if let Some(encoder) = &mut self.encoder {
            unsafe {
                if self.recording {
                    encoder.discard_encoding();
                }
                encoder.reset_all(std::mem::take(&mut self.buffers));
            }
        }
    }
}

struct Execution {
    encoder: Box<dyn hal::DynCommandEncoder>,
    buffers: Vec<Box<dyn hal::DynCommandBuffer>>,
    _owner: Arc<hal::DynOpenDevice>,
}

impl fmt::Debug for Execution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WRGraphicsExecution")
            .field("buffers", &self.buffers.len())
            .finish()
    }
}

impl hal::GraphicsExecution for Execution {
    fn command_buffers(&self) -> &[Box<dyn hal::DynCommandBuffer>] {
        &self.buffers
    }

    fn queue(&self) -> &dyn hal::DynQueue {
        &*self._owner.queue
    }
}

impl Drop for Execution {
    fn drop(&mut self) {
        unsafe { self.encoder.reset_all(std::mem::take(&mut self.buffers)) };
    }
}

#[cfg(test)]
#[path = "graphics_tests.rs"]
mod tests;
