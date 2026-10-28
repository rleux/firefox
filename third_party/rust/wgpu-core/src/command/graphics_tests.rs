use super::{
    CommandEncoder, CommandEncoderStatus, ComputePassDescriptor, LoadOp, RenderPass,
    RenderPassColorAttachment, ResolvedRenderPassDescriptor, StoreOp,
};
use alloc::{borrow::Cow, boxed::Box, string::ToString, sync::Arc, vec, vec::Vec};
use core::{
    any::Any,
    ops::Range,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};
use hal::*;
use std::sync::Mutex;
use wgpu_graphics::{RoutedCommandBuffer, Segment};

#[derive(Debug, Default)]
struct RecorderState {
    events: Mutex<Vec<&'static str>>,
    drops: AtomicUsize,
    fail_end: AtomicBool,
    draws: Mutex<Vec<[u32; 4]>>,
}

#[derive(Debug)]
struct Provider(Arc<RecorderState>);

impl crate::graphics::GraphicsRecordingProvider for Provider {
    unsafe fn create_encoder(
        &self,
        _device: &dyn DynDevice,
        native: Box<dyn DynCommandEncoder>,
    ) -> Result<Box<dyn DynCommandEncoder>, hal::DeviceError> {
        Ok(Box::new(wgpu_graphics::RoutedEncoder::new(
            native,
            Box::new(Recorder {
                state: self.0.clone(),
                count: 0,
                debug_depth: 0,
            }),
        )))
    }
}

#[derive(Debug)]
struct Recorder {
    state: Arc<RecorderState>,
    count: usize,
    debug_depth: usize,
}

impl GraphicsRecorder for Recorder {
    unsafe fn begin_encoding(&mut self, _label: hal::Label) -> Result<(), hal::DeviceError> {
        self.state.events.lock().unwrap().push("begin");
        Ok(())
    }
    unsafe fn begin_render_pass(
        &mut self,
        _desc: &hal::RenderPassDescriptor<dyn DynQuerySet, dyn DynTextureView>,
    ) -> Result<(), hal::DeviceError> {
        self.state.events.lock().unwrap().push("render");
        Ok(())
    }
    unsafe fn record(&mut self, command: GraphicsCommand<'_>) {
        let event = match command {
            GraphicsCommand::BeginDebugMarker { .. } => {
                self.debug_depth += 1;
                "debug-begin"
            }
            GraphicsCommand::EndDebugMarker => {
                self.debug_depth = self.debug_depth.checked_sub(1).unwrap();
                "debug-end"
            }
            GraphicsCommand::SetRenderPipeline { pipeline } => {
                assert!(pipeline.as_any().is::<hal::noop::Resource>());
                "pipeline"
            }
            GraphicsCommand::Draw {
                first_vertex,
                vertex_count,
                first_instance,
                instance_count,
            } => {
                self.state.draws.lock().unwrap().push([
                    first_vertex,
                    vertex_count,
                    first_instance,
                    instance_count,
                ]);
                "draw"
            }
            _ => "command",
        };
        self.state.events.lock().unwrap().push(event);
    }
    unsafe fn end_render_pass(&mut self) {
        assert_eq!(self.debug_depth, 0);
        self.state.events.lock().unwrap().push("end-render");
    }
    unsafe fn end_encoding(&mut self) -> Result<usize, hal::DeviceError> {
        self.state.events.lock().unwrap().push("end");
        if self.state.fail_end.load(Ordering::SeqCst) {
            return Err(hal::DeviceError::OutOfMemory);
        }
        let index = self.count;
        self.count += 1;
        Ok(index)
    }
    fn finish(self: Box<Self>) -> Result<Box<dyn GraphicsExecution>, hal::DeviceError> {
        panic!("The recording-only seam must not submit graphics")
    }
}
impl Drop for Recorder {
    fn drop(&mut self) {
        self.state.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Debug)]
struct NativeBuffer {
    raw: Option<Box<dyn DynCommandBuffer>>,
    events: Vec<&'static str>,
}
impl DynResource for NativeBuffer {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
impl DynCommandBuffer for NativeBuffer {}

#[derive(Debug)]
struct NativeProbe {
    raw: Box<dyn DynCommandEncoder>,
    events: Vec<&'static str>,
    resets: Arc<AtomicUsize>,
    debug_depth: usize,
    hoisted_faults: Option<[Arc<AtomicUsize>; 2]>,
    hoisted: bool,
}

impl NativeProbe {
    fn fail_hoisted(&self, stage: usize) -> Result<(), hal::DeviceError> {
        match self
            .hoisted_faults
            .as_ref()
            .map(|faults| faults[stage].swap(0, Ordering::SeqCst))
        {
            None | Some(0) => Ok(()),
            Some(1) => Err(hal::DeviceError::OutOfMemory),
            Some(2) => Err(hal::DeviceError::Unexpected),
            _ => panic!("Invalid hoisted barrier fault"),
        }
    }
}

pub(crate) fn queue_barrier_fault_encoder(
    raw: Box<dyn DynCommandEncoder>,
    faults: [Arc<AtomicUsize>; 2],
) -> Box<dyn DynCommandEncoder> {
    Box::new(NativeProbe {
        raw,
        events: Vec::new(),
        resets: Arc::default(),
        debug_depth: 0,
        hoisted_faults: Some(faults),
        hoisted: false,
    })
}

impl DynResource for NativeProbe {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
#[allow(unused_variables)]
impl DynCommandEncoder for NativeProbe {
    unsafe fn begin_encoding(&mut self, label: hal::Label) -> Result<(), hal::DeviceError> {
        assert_eq!(self.debug_depth, 0);
        self.events.clear();
        self.hoisted = label == Some("(wgpu internal) PendingWrites Transit");
        if self.hoisted {
            self.fail_hoisted(0)?;
        }
        unsafe { self.raw.begin_encoding(label) }
    }
    unsafe fn discard_encoding(&mut self) {
        unsafe { self.raw.discard_encoding() };
        self.debug_depth = 0;
    }
    unsafe fn end_encoding(&mut self) -> Result<Box<dyn DynCommandBuffer>, hal::DeviceError> {
        assert_eq!(self.debug_depth, 0);
        if self.hoisted {
            self.fail_hoisted(1)?;
        }
        let raw = unsafe { self.raw.end_encoding()? };
        Ok(Box::new(NativeBuffer {
            raw: Some(raw),
            events: core::mem::take(&mut self.events),
        }))
    }
    unsafe fn reset_all(&mut self, buffers: Vec<Box<dyn DynCommandBuffer>>) {
        let mut native = Vec::new();
        for mut buffer in buffers {
            let buffer = buffer.as_any_mut().downcast_mut::<NativeBuffer>().unwrap();
            native.push(buffer.raw.take().unwrap());
        }
        self.resets.fetch_add(1, Ordering::SeqCst);
        unsafe { self.raw.reset_all(native) };
    }
    unsafe fn set_acceleration_structure_dependencies(
        &self,
        buffers: &[Box<dyn DynCommandBuffer>],
        dependencies: &[&dyn DynAccelerationStructure],
    ) {
        for buffer in buffers {
            let buffer = buffer.as_any().downcast_ref::<NativeBuffer>().unwrap();
            unsafe {
                self.raw.set_acceleration_structure_dependencies(
                    core::slice::from_ref(buffer.raw.as_ref().unwrap()),
                    dependencies,
                )
            };
        }
    }
    unsafe fn transition_buffers(&mut self, barriers: &[BufferBarrier<'_, dyn DynBuffer>]) {
        self.events.push("transition_buffers");
        unsafe { self.raw.transition_buffers(barriers) }
    }
    unsafe fn transition_textures(&mut self, barriers: &[TextureBarrier<'_, dyn DynTexture>]) {
        self.events.push("transition_textures");
        unsafe { self.raw.transition_textures(barriers) }
    }
    unsafe fn clear_buffer(&mut self, buffer: &dyn DynBuffer, range: MemoryRange) {
        self.events.push("clear_buffer");
        unsafe { self.raw.clear_buffer(buffer, range) }
    }
    unsafe fn copy_buffer_to_buffer(
        &mut self,
        src: &dyn DynBuffer,
        dst: &dyn DynBuffer,
        regions: &[BufferCopy],
    ) {
        self.events.push("copy_buffer_to_buffer");
        unsafe { self.raw.copy_buffer_to_buffer(src, dst, regions) }
    }
    unsafe fn copy_texture_to_texture(
        &mut self,
        src: &dyn DynTexture,
        src_usage: wgt::TextureUses,
        dst: &dyn DynTexture,
        regions: &[TextureCopy],
    ) {
        self.events.push("copy_texture_to_texture");
        unsafe {
            self.raw
                .copy_texture_to_texture(src, src_usage, dst, regions)
        }
    }
    unsafe fn copy_buffer_to_texture(
        &mut self,
        src: &dyn DynBuffer,
        dst: &dyn DynTexture,
        regions: &[BufferTextureCopy],
    ) {
        self.events.push("copy_buffer_to_texture");
        unsafe { self.raw.copy_buffer_to_texture(src, dst, regions) }
    }
    unsafe fn copy_texture_to_buffer(
        &mut self,
        src: &dyn DynTexture,
        src_usage: wgt::TextureUses,
        dst: &dyn DynBuffer,
        regions: &[BufferTextureCopy],
    ) {
        self.events.push("copy_texture_to_buffer");
        unsafe {
            self.raw
                .copy_texture_to_buffer(src, src_usage, dst, regions)
        }
    }
    unsafe fn set_bind_group(
        &mut self,
        layout: &dyn DynPipelineLayout,
        index: u32,
        group: &dyn DynBindGroup,
        dynamic_offsets: &[wgt::DynamicOffset],
    ) {
        self.events.push("set_bind_group");
        unsafe {
            self.raw
                .set_bind_group(layout, index, group, dynamic_offsets)
        }
    }
    unsafe fn set_immediates(
        &mut self,
        layout: &dyn DynPipelineLayout,
        offset_bytes: u32,
        data: &[u32],
    ) {
        self.events.push("set_immediates");
        unsafe { self.raw.set_immediates(layout, offset_bytes, data) }
    }
    unsafe fn insert_debug_marker(&mut self, label: &str) {
        self.events.push("insert_debug_marker");
        unsafe { self.raw.insert_debug_marker(label) }
    }
    unsafe fn begin_debug_marker(&mut self, group_label: &str) {
        self.debug_depth += 1;
        self.events.push("begin_debug_marker");
        unsafe { self.raw.begin_debug_marker(group_label) }
    }
    unsafe fn end_debug_marker(&mut self) {
        self.debug_depth = self.debug_depth.checked_sub(1).unwrap();
        self.events.push("end_debug_marker");
        unsafe { self.raw.end_debug_marker() }
    }
    unsafe fn begin_query(&mut self, set: &dyn DynQuerySet, index: u32) {
        self.events.push("begin_query");
        unsafe { self.raw.begin_query(set, index) }
    }
    unsafe fn end_query(&mut self, set: &dyn DynQuerySet, index: u32) {
        self.events.push("end_query");
        unsafe { self.raw.end_query(set, index) }
    }
    unsafe fn write_timestamp(&mut self, set: &dyn DynQuerySet, index: u32) {
        self.events.push("write_timestamp");
        unsafe { self.raw.write_timestamp(set, index) }
    }
    unsafe fn reset_queries(&mut self, set: &dyn DynQuerySet, range: Range<u32>) {
        self.events.push("reset_queries");
        unsafe { self.raw.reset_queries(set, range) }
    }
    unsafe fn copy_query_results(
        &mut self,
        set: &dyn DynQuerySet,
        range: Range<u32>,
        buffer: &dyn DynBuffer,
        offset: wgt::BufferAddress,
        stride: wgt::BufferSize,
    ) {
        self.events.push("copy_query_results");
        unsafe {
            self.raw
                .copy_query_results(set, range, buffer, offset, stride)
        }
    }
    unsafe fn begin_render_pass(
        &mut self,
        desc: &hal::RenderPassDescriptor<dyn DynQuerySet, dyn DynTextureView>,
    ) -> Result<(), hal::DeviceError> {
        panic!("Core native encoder received graphics: begin_render_pass")
    }
    unsafe fn end_render_pass(&mut self) {
        panic!("Core native encoder received graphics: end_render_pass")
    }
    unsafe fn set_render_pipeline(&mut self, pipeline: &dyn DynRenderPipeline) {
        panic!("Core native encoder received graphics: set_render_pipeline")
    }
    unsafe fn set_index_buffer<'a>(
        &mut self,
        binding: BufferBinding<'a, dyn DynBuffer, wgt::BufferAddress>,
        format: wgt::IndexFormat,
    ) {
        panic!("Core native encoder received graphics: set_index_buffer")
    }
    unsafe fn set_vertex_buffer<'a>(
        &mut self,
        index: u32,
        binding: BufferBinding<'a, dyn DynBuffer, wgt::BufferAddress>,
    ) {
        panic!("Core native encoder received graphics: set_vertex_buffer")
    }
    unsafe fn set_viewport(&mut self, rect: &Rect<f32>, depth_range: Range<f32>) {
        panic!("Core native encoder received graphics: set_viewport")
    }
    unsafe fn set_scissor_rect(&mut self, rect: &Rect<u32>) {
        panic!("Core native encoder received graphics: set_scissor_rect")
    }
    unsafe fn set_stencil_reference(&mut self, value: u32) {
        panic!("Core native encoder received graphics: set_stencil_reference")
    }
    unsafe fn set_blend_constants(&mut self, color: &[f32; 4]) {
        panic!("Core native encoder received graphics: set_blend_constants")
    }
    unsafe fn draw(
        &mut self,
        first_vertex: u32,
        vertex_count: u32,
        first_instance: u32,
        instance_count: u32,
    ) {
        panic!("Core native encoder received graphics: draw")
    }
    unsafe fn draw_indexed(
        &mut self,
        first_index: u32,
        index_count: u32,
        base_vertex: i32,
        first_instance: u32,
        instance_count: u32,
    ) {
        panic!("Core native encoder received graphics: draw_indexed")
    }
    unsafe fn draw_mesh_tasks(
        &mut self,
        group_count_x: u32,
        group_count_y: u32,
        group_count_z: u32,
    ) {
        panic!("Core native encoder received graphics: draw_mesh_tasks")
    }
    unsafe fn draw_indirect(
        &mut self,
        buffer: &dyn DynBuffer,
        offset: wgt::BufferAddress,
        draw_count: u32,
    ) {
        panic!("Core native encoder received graphics: draw_indirect")
    }
    unsafe fn draw_indexed_indirect(
        &mut self,
        buffer: &dyn DynBuffer,
        offset: wgt::BufferAddress,
        draw_count: u32,
    ) {
        panic!("Core native encoder received graphics: draw_indexed_indirect")
    }
    unsafe fn draw_mesh_tasks_indirect(
        &mut self,
        buffer: &dyn DynBuffer,
        offset: wgt::BufferAddress,
        draw_count: u32,
    ) {
        panic!("Core native encoder received graphics: draw_mesh_tasks_indirect")
    }
    unsafe fn draw_indirect_count(
        &mut self,
        buffer: &dyn DynBuffer,
        offset: wgt::BufferAddress,
        count_buffer: &dyn DynBuffer,
        count_offset: wgt::BufferAddress,
        max_count: u32,
    ) {
        panic!("Core native encoder received graphics: draw_indirect_count")
    }
    unsafe fn draw_indexed_indirect_count(
        &mut self,
        buffer: &dyn DynBuffer,
        offset: wgt::BufferAddress,
        count_buffer: &dyn DynBuffer,
        count_offset: wgt::BufferAddress,
        max_count: u32,
    ) {
        panic!("Core native encoder received graphics: draw_indexed_indirect_count")
    }
    unsafe fn draw_mesh_tasks_indirect_count(
        &mut self,
        buffer: &dyn DynBuffer,
        offset: wgt::BufferAddress,
        count_buffer: &dyn DynBuffer,
        count_offset: wgt::BufferAddress,
        max_count: u32,
    ) {
        panic!("Core native encoder received graphics: draw_mesh_tasks_indirect_count")
    }
    unsafe fn begin_compute_pass(&mut self, desc: &hal::ComputePassDescriptor<dyn DynQuerySet>) {
        self.events.push("begin_compute_pass");
        unsafe { self.raw.begin_compute_pass(desc) }
    }
    unsafe fn end_compute_pass(&mut self) {
        self.events.push("end_compute_pass");
        unsafe { self.raw.end_compute_pass() }
    }
    unsafe fn set_compute_pipeline(&mut self, pipeline: &dyn DynComputePipeline) {
        self.events.push("set_compute_pipeline");
        unsafe { self.raw.set_compute_pipeline(pipeline) }
    }
    unsafe fn dispatch_workgroups(&mut self, count: [u32; 3]) {
        self.events.push("dispatch_workgroups");
        unsafe { self.raw.dispatch_workgroups(count) }
    }
    unsafe fn dispatch_workgroups_indirect(
        &mut self,
        buffer: &dyn DynBuffer,
        offset: wgt::BufferAddress,
    ) {
        self.events.push("dispatch_workgroups_indirect");
        unsafe { self.raw.dispatch_workgroups_indirect(buffer, offset) }
    }
    unsafe fn begin_ray_tracing_pass(&mut self, desc: &RayTracingPassDescriptor) {
        self.events.push("begin_ray_tracing_pass");
        unsafe { self.raw.begin_ray_tracing_pass(desc) }
    }
    unsafe fn end_ray_tracing_pass(&mut self) {
        self.events.push("end_ray_tracing_pass");
        unsafe { self.raw.end_ray_tracing_pass() }
    }
    unsafe fn trace_rays(
        &mut self,
        count: [u32; 3],
        ray_generation_group_data: hal::PipelineGroupData<dyn DynBuffer>,
        miss_group_data: hal::PipelineGroupData<dyn DynBuffer>,
        intersection_group_data: hal::PipelineGroupData<dyn DynBuffer>,
    ) {
        self.events.push("trace_rays");
        unsafe {
            self.raw.trace_rays(
                count,
                ray_generation_group_data,
                miss_group_data,
                intersection_group_data,
            )
        }
    }
    unsafe fn set_ray_tracing_pipeline(&mut self, pipeline: &dyn DynRayTracingPipeline) {
        self.events.push("set_ray_tracing_pipeline");
        unsafe { self.raw.set_ray_tracing_pipeline(pipeline) }
    }
    unsafe fn build_acceleration_structures<'a>(
        &mut self,
        descriptors: &'a [BuildAccelerationStructureDescriptor<
            'a,
            dyn DynBuffer,
            dyn DynAccelerationStructure,
        >],
    ) {
        self.events.push("build_acceleration_structures");
        unsafe { self.raw.build_acceleration_structures(descriptors) }
    }
    unsafe fn place_acceleration_structure_barrier(
        &mut self,
        barrier: AccelerationStructureBarrier,
    ) {
        self.events.push("place_acceleration_structure_barrier");
        unsafe { self.raw.place_acceleration_structure_barrier(barrier) }
    }
    unsafe fn copy_acceleration_structure_to_acceleration_structure(
        &mut self,
        src: &dyn DynAccelerationStructure,
        dst: &dyn DynAccelerationStructure,
        copy: wgt::AccelerationStructureCopy,
    ) {
        self.events
            .push("copy_acceleration_structure_to_acceleration_structure");
        unsafe {
            self.raw
                .copy_acceleration_structure_to_acceleration_structure(src, dst, copy)
        }
    }
    unsafe fn read_acceleration_structure_compact_size(
        &mut self,
        acceleration_structure: &dyn DynAccelerationStructure,
        buf: &dyn DynBuffer,
    ) {
        self.events.push("read_acceleration_structure_compact_size");
        unsafe {
            self.raw
                .read_acceleration_structure_compact_size(acceleration_structure, buf)
        }
    }
}

struct Fixture {
    device: Arc<crate::device::Device>,
    queue: Arc<crate::device::queue::Queue>,
    provider: Arc<Provider>,
    resets: Arc<AtomicUsize>,
}

impl Fixture {
    fn new() -> Self {
        let instance = crate::instance::Instance::new(
            "routed graphics test",
            wgt::InstanceDescriptor {
                backends: wgt::Backends::NOOP,
                backend_options: wgt::BackendOptions {
                    noop: wgt::NoopBackendOptions {
                        enable: true,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                flags: wgt::InstanceFlags::default(),
                memory_budget_thresholds: Default::default(),
                display: None,
            },
            None,
        );
        let adapter = instance
            .enumerate_adapters(wgt::Backends::NOOP, false)
            .pop()
            .unwrap();
        let mut desc = crate::device::DeviceDescriptor {
            required_features: wgt::Features::CLEAR_TEXTURE,
            ..Default::default()
        };
        adapter.validate_device_descriptor(&mut desc).unwrap();
        let open = unsafe {
            adapter.raw.adapter.open(
                desc.required_features,
                &desc.required_limits,
                &desc.memory_hints,
            )
        }
        .unwrap();
        let provider = Arc::new(Provider(Arc::default()));
        let (device, queue) = unsafe {
            adapter.create_device_and_queue_from_hal_with_graphics(
                open,
                &desc,
                Some(provider.clone()),
            )
        }
        .unwrap();
        Self {
            device,
            queue,
            provider,
            resets: Arc::default(),
        }
    }

    fn encoder(&self) -> Arc<CommandEncoder> {
        let native = unsafe {
            self.device
                .raw()
                .create_command_encoder(&hal::CommandEncoderDescriptor {
                    label: None,
                    queue: self.queue.raw(),
                })
        }
        .unwrap();
        let native = Box::new(NativeProbe {
            raw: native,
            events: Vec::new(),
            resets: self.resets.clone(),
            debug_depth: 0,
            hoisted_faults: None,
            hoisted: false,
        });
        let encoder = unsafe {
            crate::graphics::GraphicsRecordingProvider::create_encoder(
                &*self.provider,
                self.device.raw(),
                native,
            )
        }
        .unwrap();
        Arc::new(CommandEncoder::new(encoder, &self.device, &None))
    }

    fn texture(&self) -> Arc<crate::resource::Texture> {
        self.device
            .create_texture(&crate::resource::TextureDescriptor {
                label: None,
                size: wgt::Extent3d {
                    width: 4,
                    height: 4,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgt::TextureDimension::D2,
                format: wgt::TextureFormat::Rgba8Unorm,
                usage: wgt::TextureUsages::RENDER_ATTACHMENT | wgt::TextureUsages::COPY_DST,
                view_formats: Vec::new(),
            })
    }

    fn pass(
        &self,
        encoder: &Arc<CommandEncoder>,
        texture: &Arc<crate::resource::Texture>,
    ) -> RenderPass {
        self.pass_with_load(encoder, texture, LoadOp::Clear(wgt::Color::RED))
    }

    fn pass_with_load(
        &self,
        encoder: &Arc<CommandEncoder>,
        texture: &Arc<crate::resource::Texture>,
        load_op: LoadOp<wgt::Color>,
    ) -> RenderPass {
        let view = texture.create_view(&Default::default());
        encoder.begin_render_pass(ResolvedRenderPassDescriptor {
            label: None,
            color_attachments: Cow::Owned(vec![Some(RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                load_op,
                store_op: StoreOp::Store,
            })]),
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        })
    }
}

#[test]
fn routed_finish_preserves_prepass_and_compute_order() {
    let f = Fixture::new();
    let encoder = f.encoder();
    let mut compute = encoder.begin_compute_pass(&ComputePassDescriptor {
        label: None,
        timestamp_writes: None,
    });
    compute.end();
    f.pass(&encoder, &f.texture()).end();
    let mut compute = encoder.begin_compute_pass(&ComputePassDescriptor {
        label: None,
        timestamp_writes: None,
    });
    compute.end();
    let (buffer, error) = encoder.finish_inner(&Default::default());
    assert!(error.is_none(), "{error:?}");
    let status = buffer.data.lock();
    let CommandEncoderStatus::Finished(data) = &*status else {
        panic!("Not finished");
    };
    let mut order = Vec::new();
    for chunk in &data.encoder.list {
        let chunk = chunk
            .as_any()
            .downcast_ref::<RoutedCommandBuffer>()
            .unwrap();
        for segment in chunk.segments() {
            match segment {
                Segment::Graphics(index) => {
                    assert_eq!(*index, 0);
                    order.push("graphics");
                }
                Segment::Native(buffer) => {
                    let buffer = buffer.as_any().downcast_ref::<NativeBuffer>().unwrap();
                    order.extend(buffer.events.iter().copied().filter(|event| {
                        matches!(*event, "begin_compute_pass" | "transition_textures")
                    }));
                }
            }
        }
    }
    let graphics = order.iter().position(|event| *event == "graphics").unwrap();
    assert!(order[..graphics].contains(&"begin_compute_pass"));
    assert!(order[..graphics].contains(&"transition_textures"));
    assert!(order[graphics + 1..].contains(&"begin_compute_pass"));
    assert_eq!(f.resets.load(Ordering::SeqCst), 0);
    drop(status);
    drop(buffer);
    assert_eq!(f.resets.load(Ordering::SeqCst), 1);
}

#[test]
fn routed_late_validation_error_discards_partial_graphics() {
    let f = Fixture::new();
    let encoder = f.encoder();
    let mut pass = f.pass(&encoder, &f.texture());
    pass.draw(3, 1, 0, 0);
    pass.end();
    let (_, error) = encoder.finish_inner(&Default::default());
    assert!(error.is_some());
    assert!(f.provider.0.events.lock().unwrap().contains(&"render"));
    assert_eq!(f.resets.load(Ordering::SeqCst), 1);
    assert_eq!(f.provider.0.drops.load(Ordering::SeqCst), 1);
}

#[test]
fn routed_clear_helper_uses_graphics_recorder() {
    let f = Fixture::new();
    let encoder = f.encoder();
    encoder.clear_texture(f.texture(), &wgt::ImageSubresourceRange::default());
    let (buffer, error) = encoder.finish_inner(&Default::default());
    assert!(error.is_none(), "{error:?}");
    assert!(f.provider.0.events.lock().unwrap().contains(&"render"));
    drop(buffer);
    assert_eq!(f.resets.load(Ordering::SeqCst), 1);
}

#[test]
fn routed_destroyed_texture_still_fails_submit_validation() {
    let f = Fixture::new();
    let encoder = f.encoder();
    let texture = f.texture();
    f.pass(&encoder, &texture).end();
    let (buffer, error) = encoder.finish_inner(&Default::default());
    assert!(error.is_none(), "{error:?}");
    texture.destroy();
    f.device
        .push_error_scope(wgt::error::ErrorFilter::Validation);
    f.queue.submit(&[buffer]);
    let error = f
        .device
        .pop_error_scope()
        .unwrap()
        .expect("Destroyed texture must fail validation");
    assert!(error.to_string().contains("destroyed"), "{error}");
    assert!(f.device.is_valid());
}

#[test]
fn routed_as_hal_does_not_change_encoding_api() {
    let f = Fixture::new();
    let encoder = f.device.create_command_encoder(&Default::default());
    unsafe {
        encoder.as_hal_mut::<hal::api::Noop, _, _>(|raw| assert!(raw.is_none()));
    }
    f.pass(&encoder, &f.texture()).end();
    let (_, error) = encoder.finish_inner(&Default::default());
    assert!(error.is_none(), "{error:?}");
}

#[test]
fn routed_submit_initialization_is_recorded_but_never_submitted() {
    let f = Fixture::new();
    let encoder = f.encoder();
    f.pass_with_load(&encoder, &f.texture(), LoadOp::Load).end();
    let (buffer, error) = encoder.finish_inner(&Default::default());
    assert!(error.is_none(), "{error:?}");
    let renders_before = f
        .provider
        .0
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| **event == "render")
        .count();
    f.queue.submit(&[buffer]);
    let renders_after = f
        .provider
        .0
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| **event == "render")
        .count();
    assert!(
        renders_after > renders_before,
        "Submit-time initialization must use WR"
    );
    assert!(
        !f.device.is_valid(),
        "Unimplemented submission must fail explicitly"
    );
    assert_eq!(
        f.device
            .last_successful_submission_index
            .load(Ordering::SeqCst),
        0
    );
}

#[test]
fn routed_end_failure_is_returned_at_finish() {
    let f = Fixture::new();
    let encoder = f.encoder();
    f.pass(&encoder, &f.texture()).end();
    f.provider.0.fail_end.store(true, Ordering::SeqCst);
    let (_, error) = encoder.finish_inner(&Default::default());
    assert!(error.is_some());
    assert_eq!(f.resets.load(Ordering::SeqCst), 1);
    assert_eq!(f.provider.0.drops.load(Ordering::SeqCst), 1);
}

#[test]
fn routed_unfinished_encoder_releases_owner() {
    let f = Fixture::new();
    let encoder = f.encoder();
    f.pass(&encoder, &f.texture()).end();
    drop(encoder);
    assert!(f.provider.0.events.lock().unwrap().is_empty());
    assert_eq!(f.resets.load(Ordering::SeqCst), 1);
    assert_eq!(f.provider.0.drops.load(Ordering::SeqCst), 1);
}

#[test]
fn routed_destroyed_before_finish_retains_deferred_validation() {
    let f = Fixture::new();
    let encoder = f.encoder();
    let texture = f.texture();
    f.pass(&encoder, &texture).end();
    texture.destroy();
    let (buffer, error) = encoder.finish_inner(&Default::default());
    assert!(error.is_none(), "{error:?}");
    f.device
        .push_error_scope(wgt::error::ErrorFilter::Validation);
    f.queue.submit(&[buffer]);
    let error = f
        .device
        .pop_error_scope()
        .unwrap()
        .expect("Destroyed texture must fail submission");
    assert!(error.to_string().contains("destroyed"), "{error}");
    assert!(f.device.is_valid());
}

#[test]
fn routed_debug_groups_remain_balanced_across_passes() {
    let f = Fixture::new();
    let encoder = f.encoder();
    encoder.push_debug_group("outside");
    let mut pass = f.pass(&encoder, &f.texture());
    pass.push_debug_group("inside", 0);
    pass.pop_debug_group();
    pass.end();
    encoder.pop_debug_group();
    let (buffer, error) = encoder.finish_inner(&Default::default());
    assert!(error.is_none(), "{error:?}");
    let events = f.provider.0.events.lock().unwrap();
    let begins = events
        .iter()
        .filter(|event| **event == "debug-begin")
        .count();
    let ends = events.iter().filter(|event| **event == "debug-end").count();
    assert!(
        begins >= 2,
        "Both enclosing and pass-local groups must reach WR"
    );
    assert_eq!(begins, ends);
    drop(events);
    drop(buffer);
    assert_eq!(f.resets.load(Ordering::SeqCst), 1);
}

#[cfg(feature = "wgsl")]
#[test]
fn routed_valid_pipeline_and_draw_reach_renderer() {
    use crate::pipeline;

    let f = Fixture::new();
    f.device
        .push_error_scope(wgt::error::ErrorFilter::Validation);
    let shader = f.device.create_shader_module(
        &pipeline::ShaderModuleDescriptor {
            label: None,
            runtime_checks: Default::default(),
        },
        pipeline::ShaderModuleSource::Wgsl(Cow::Borrowed(
            "@vertex fn vs() -> @builtin(position) vec4f {
                return vec4f(0.0, 0.0, 0.0, 1.0);
            }
            @fragment fn fs() -> @location(0) vec4f {
                return vec4f(1.0, 0.0, 0.0, 1.0);
            }",
        )),
    );
    let stage = |entry_point: &'static str| pipeline::ProgrammableStageDescriptor {
        module: shader.clone(),
        entry_point: Some(Cow::Borrowed(entry_point)),
        constants: Default::default(),
        zero_initialize_workgroup_memory: true,
    };
    let pipeline = f.device.create_render_pipeline(
        pipeline::RenderPipelineDescriptor {
            label: None,
            layout: None,
            vertex: pipeline::VertexState {
                stage: stage("vs"),
                buffers: Cow::Borrowed(&[]),
            },
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(pipeline::FragmentState {
                stage: stage("fs"),
                targets: Cow::Owned(vec![Some(wgt::ColorTargetState {
                    format: wgt::TextureFormat::Rgba8Unorm,
                    blend: None,
                    write_mask: wgt::ColorWrites::ALL,
                })]),
            }),
            multiview_mask: None,
            cache: None,
        }
        .into(),
    );
    let encoder = f.encoder();
    let mut pass = f.pass(&encoder, &f.texture());
    pass.set_pipeline(pipeline);
    pass.draw(3, 2, 5, 7);
    pass.end();
    let (buffer, error) = encoder.finish_inner(&Default::default());
    assert!(error.is_none(), "{error:?}");
    assert!(f.device.pop_error_scope().unwrap().is_none());
    let events = f.provider.0.events.lock().unwrap();
    let commands: Vec<_> = events
        .iter()
        .copied()
        .filter(|event| matches!(*event, "pipeline" | "draw"))
        .collect();
    assert_eq!(commands, ["pipeline", "draw"]);
    assert_eq!(*f.provider.0.draws.lock().unwrap(), [[5, 3, 7, 2]]);
    drop(events);
    drop(buffer);
    assert_eq!(f.resets.load(Ordering::SeqCst), 1);
}
