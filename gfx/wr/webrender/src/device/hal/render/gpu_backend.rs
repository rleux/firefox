/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::{self as wr, GpuBackend};
use crate::device::query::{GpuProfiler, GpuQueryBackend, GpuQueryId, GpuQueryKind};
use crate::internal_types::{RenderTargetInfo, SwizzleSettings};
use api::{ExternalTextureHandle, ImageDescriptor, ImageFormat, Parameter};
use euclid::default::Transform3D;
use malloc_size_of::MallocSizeOfOps;
use std::{borrow::Cow, num::NonZeroUsize, os::raw::c_void, ptr};
use webrender_build::shader::ShaderFeatureFlags;

mod compositor;
mod external;
mod tables;

struct ProgramState {
    shader: Shader,
    samplers: Vec<(&'static str, usize)>,
    transform: Cell<Transform3D<f32>>,
}

pub(crate) struct HalGpuBackend<A: BackendApi> {
    pub(crate) gpu: FrameRenderer<A>,
    compositor_targets: Rc<RefCell<compositor::Targets<A>>>,
    target_view: Option<(DeviceIntPoint, DeviceIntSize)>,
    scissor_offset: DeviceIntVector2D,
    capabilities: wr::Capabilities,
    failure: Rc<RefCell<Option<String>>>,
    next_id: u32,
    frame_id: usize,
    created: u32,
    deleted: u32,
    textures: HashMap<u32, Rc<Texture<A>>>,
    external: Rc<RefCell<external::Registry<A>>>,
    buffers: HashMap<u32, Vec<u8>>,
    programs: HashMap<u32, ProgramState>,
    bound_textures: [u32; 16],
    bound_filters: [Option<TextureFilter>; 16],
    pub(crate) output_origin: DeviceIntPoint,
    bound_program: u32,
    bound_instances: u32,
    bound_instance_stride: usize,
    state: wr::RenderState,
    target: Option<Rc<Texture<A>>>,
    default_target: Option<Rc<Texture<A>>>,
    read_target: Option<Rc<Texture<A>>>,
    scissor: Cell<Option<DeviceIntRect>>,
    scissor_enabled: Cell<bool>,
    draws: Vec<Draw<'static, A>>,
    draw_data: HashMap<&'static str, Rc<Texture<A>>>,
    upload_method: wr::UploadMethod,
    pink: bool,
    in_pass: bool,
    active_default: bool,
    tables: HashMap<u64, tables::Table<A>>,
    stats: DrawStats,
    query: Option<Rc<crate::device::hal::query::Slot<A>>>,
    pub(crate) composition_damage: Option<DeviceIntRect>,
}

impl<A: BackendApi> HalGpuBackend<A> {
    pub(crate) fn new(device: Device<A>) -> Result<Self> {
        Self::from_renderer(FrameRenderer::new(device)?)
    }

    pub(crate) fn from_renderer(mut gpu: FrameRenderer<A>) -> Result<Self> {
        let device = &gpu.owner;
        let capabilities = wr::Capabilities {
            supports_copy_image_sub_data: true,
            supports_buffer_storage: true,
            supports_dual_source_blending: device.supports_dual_source_blending(),
            supports_nonzero_pbo_offsets: true,
            supports_texture_rect: true,
            supports_render_target_partial_update: true,
            supports_alpha_target_clears: true,
            supports_render_target_invalidate: true,
            supports_r8_texture_upload: true,
            supports_bgra_read: true,
            supports_base_instance: true,
            readback_rows_top_down: true,
            renderer_name: device.info.name.clone(),
            ..Default::default()
        };
        gpu.buffer_tables = false;
        let compositor_targets = Rc::new(RefCell::new(compositor::Targets::new(&gpu)));
        Ok(Self {
            compositor_targets,
            target_view: None,
            scissor_offset: DeviceIntVector2D::zero(),
            gpu,
            capabilities,
            failure: Rc::new(RefCell::new(None)),
            next_id: 1,
            frame_id: 0,
            created: 0,
            deleted: 0,
            external: Rc::new(RefCell::new(Default::default())),
            textures: HashMap::new(),
            buffers: HashMap::new(),
            programs: HashMap::new(),
            bound_textures: [0; 16],
            bound_filters: [None; 16],
            output_origin: DeviceIntPoint::zero(),
            bound_program: 0,
            bound_instances: 0,
            bound_instance_stride: 0,
            state: Default::default(),
            target: None,
            default_target: None,
            read_target: None,
            scissor: Cell::new(None),
            scissor_enabled: Cell::new(false),
            draws: Vec::new(),
            draw_data: HashMap::new(),
            upload_method: wr::UploadMethod::PixelBuffer(wr::VertexUsageHint::Stream),
            tables: HashMap::new(),
            stats: DrawStats::default(),
            query: None,
            pink: false,
            in_pass: false,
            active_default: false,
            composition_damage: None,
        })
    }

    pub(crate) fn output(
        &mut self,
        origin: DeviceIntPoint,
        mut stats: DrawStats,
        present: bool,
    ) -> Result<RenderedFrame<A>> {
        let texture = if present {
            self.default_target.clone()
        } else {
            None
        };
        stats.data_table_uploads = self.stats.data_table_uploads;
        stats.data_table_copies = self.stats.data_table_copies;
        let serial = self.gpu.submissions.submit_serial()?;
        if let Some(texture) = &texture {
            let size = [texture.size.width, texture.size.height];
            let damage = self.composition_damage.unwrap_or_else(|| {
                DeviceIntRect::from_size(DeviceIntSize::new(size[0] as i32, size[1] as i32))
            });
            self.gpu.output_history.record(serial, size, damage);
        }
        Ok(RenderedFrame {
            size: texture
                .as_ref()
                .map_or([0, 0], |t| [t.size.width, t.size.height]),
            origin,
            stats,
            serial,
            texture,
        })
    }

    fn id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id = id.checked_add(1).expect("HAL resource ID exhaustion");
        id
    }

    pub(crate) fn failure(&self) -> Option<String> {
        self.failure.borrow().clone()
    }

    fn record_result<T>(&self, result: Result<T>) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                self.failure
                    .borrow_mut()
                    .get_or_insert_with(|| error.clone());
                self.gpu.failed.set(true);
                log::error!("HAL backend operation failed: {error}");
                None
            }
        }
    }

    fn flush(&mut self) {
        if self.draws.is_empty() {
            return;
        }
        if self.gpu.is_failed() {
            self.draws.clear();
            return;
        }
        let target = self
            .target
            .as_ref()
            .expect("HAL draw without a target")
            .clone();
        let table_result = self.bind_tables();
        if self.record_result(table_result).is_none() {
            self.draws.clear();
            return;
        }
        let projection = self.gpu.projection_override;
        if self.draws.iter().all(|draw| draw.clear_color.is_some()) {
            self.gpu.projection_override = None;
        }
        let result = self
            .gpu
            .draw_pass(&target, &self.draws, &self.draw_data, &mut self.stats);
        self.gpu.projection_override = projection;
        self.record_result(result);
        self.draws.clear();
    }

    fn target(&mut self, target: wr::DrawTarget) -> Rc<Texture<A>> {
        self.target_view = None;
        self.scissor_offset = DeviceIntVector2D::zero();
        match target {
            wr::DrawTarget::Texture { fbo_id, .. } => self.textures[&fbo_id.0].clone(),
            wr::DrawTarget::Default { total_size, .. } => {
                let size = [total_size.width as u32, total_size.height as u32];
                if self
                    .default_target
                    .as_ref()
                    .map(|t| [t.size.width, t.size.height])
                    != Some(size)
                {
                    self.default_target = Some(
                        self.record_result(Texture::new(
                            &self.gpu.owner,
                            size[0],
                            size[1],
                            wgt::TextureFormat::Rgba8Unorm,
                            TextureFilter::Linear,
                            true,
                        ))
                        .unwrap_or_else(|| self.gpu.dummy.clone()),
                    );
                }
                self.default_target.as_ref().unwrap().clone()
            }
            wr::DrawTarget::NativeSurface {
                offset,
                handle,
                dimensions,
            } => {
                let targets = self.compositor_targets.borrow();
                let key = if handle.0 == 0 {
                    targets.current.unwrap_or(0)
                } else {
                    handle.0
                };
                let Some(target) = targets.targets.get(&key) else {
                    return self.gpu.dummy.clone();
                };
                let origin = if handle.0 == 0 {
                    self.scissor_offset = target.origin.to_vector();
                    offset + self.scissor_offset
                } else {
                    offset
                };
                self.target_view = Some((origin, dimensions));
                target.texture.clone()
            }
        }
    }

    fn read_target(&self, target: wr::ReadTarget) -> Rc<Texture<A>> {
        match target {
            wr::ReadTarget::Default => self.default_target.as_ref().unwrap().clone(),
            wr::ReadTarget::Texture { fbo_id } => self.textures[&fbo_id.0].clone(),
            wr::ReadTarget::NativeSurface { fbo_id, .. } => {
                self.compositor_targets.borrow().targets[&u64::from(fbo_id.0)]
                    .texture
                    .clone()
            }
        }
    }

    fn rect(&self) -> DeviceIntRect {
        let target = self.target.as_ref().unwrap();
        let full = DeviceIntRect::from_size(DeviceIntSize::new(
            target.size.width as i32,
            target.size.height as i32,
        ));
        let rect = if self.scissor_enabled.get() {
            self.scissor.get().map_or(full, |rect| {
                if self.active_default {
                    rect.translate(-self.output_origin.to_vector())
                } else {
                    rect
                }
            })
        } else {
            full
        };
        if self.active_default {
            if let Some(damage) = self.composition_damage {
                return rect
                    .intersection(&damage)
                    .unwrap_or_else(DeviceIntRect::zero);
            }
        }
        rect.translate(self.scissor_offset)
    }

    fn draw(&mut self, count: i32, base: u32) {
        assert!(self.in_pass);
        if count == 0 || self.gpu.is_failed() {
            return;
        }
        let program = &self.programs[&self.bound_program];
        let mut data = HashMap::new();
        for &(name, slot) in &program.samplers {
            if let Some(texture) = self.texture_handle(self.bound_textures[slot]) {
                data.insert(name, texture.clone());
            }
        }
        let binding = |name| {
            data.get(name)
                .cloned()
                .unwrap_or_else(|| self.gpu.dummy.clone())
        };
        let textures = DrawTextures {
            colors: [binding("sColor0"), binding("sColor1"), binding("sColor2")],
            clip: binding("sClipMask"),
        };
        let shader = program.shader;
        let mut matrix = program.transform.get().to_array();
        // Convert GL clip coordinates to HAL's downward Y and [0, 1] depth.
        for i in [2, 6, 10, 14] {
            matrix[i] = (matrix[i] + matrix[i + 1]) * 0.5;
        }
        for i in [1, 5, 9, 13] {
            matrix[i] = -matrix[i];
        }
        if self.active_default {
            let target = self.target.as_ref().unwrap();
            matrix[12] -= 2.0 * self.output_origin.x as f32 / target.size.width as f32;
            matrix[13] += 2.0 * self.output_origin.y as f32 / target.size.height as f32;
        }
        if let Some((origin, size)) = self.target_view {
            let target = self.target.as_ref().unwrap();
            let sx = size.width as f32 / target.size.width as f32;
            let sy = size.height as f32 / target.size.height as f32;
            let tx = (2.0 * origin.x as f32 + size.width as f32) / target.size.width as f32 - 1.0;
            let ty = 1.0 - (2.0 * origin.y as f32 + size.height as f32) / target.size.height as f32;
            for i in [0, 4, 8, 12] {
                matrix[i] = matrix[i] * sx + matrix[i + 3] * tx;
                matrix[i + 1] = matrix[i + 1] * sy + matrix[i + 3] * ty;
            }
        }
        let filter = program
            .samplers
            .iter()
            .find(|(name, _)| *name == "sColor0")
            .and_then(|(_, slot)| self.bound_filters[*slot]);
        let stride = self.bound_instance_stride;
        let first = base as usize * stride;
        let end = first.checked_add(count as usize * stride).unwrap();
        let instances = Instances::owned(&self.buffers[&self.bound_instances][first..end]);
        if matches!(shader, Shader::Other("ps_copy", _)) {
            self.flush();
            let target = self.target.as_ref().unwrap().clone();
            let projection = self.gpu.projection_override.take();
            for instance in instances.bytes().chunks_exact(stride) {
                let value = |offset| {
                    f32::from_ne_bytes(instance[offset..offset + 4].try_into().unwrap()) as i32
                };
                let rect = |offset| {
                    DeviceIntRect::new(
                        DeviceIntPoint::new(value(offset), value(offset + 4)),
                        DeviceIntPoint::new(value(offset + 8), value(offset + 12)),
                    )
                };
                let result = self
                    .gpu
                    .copy(&textures.colors[0], &target, rect(0), rect(16));
                self.record_result(result);
            }
            self.gpu.projection_override = projection;
            return;
        }
        let mut blend = match self.state.blend_mode {
            wr::BlendMode::None => 0,
            wr::BlendMode::PremultipliedAlpha => 1,
            wr::BlendMode::Alpha => 2,
            wr::BlendMode::Multiply => 3,
            wr::BlendMode::PremultipliedDestOut => 4,
            wr::BlendMode::SubpixelDualSource => 5,
            wr::BlendMode::Screen => 6,
            wr::BlendMode::Exclusion => 7,
            wr::BlendMode::PlusLighter | wr::BlendMode::ShowOverdraw => 8,
            wr::BlendMode::Advanced(_) => panic!("HAL does not advertise advanced blend support"),
        };
        if !self.state.color_write {
            blend |= 0x80;
        }
        let depth = match (self.state.depth_test, self.state.depth_write) {
            (None, _) => 0,
            (Some(wr::DepthFunction::Always), false) => 3,
            (Some(wr::DepthFunction::Always), true) => 4,
            (Some(wr::DepthFunction::Less), true) => 5,
            (Some(wr::DepthFunction::Less), false) => 6,
            (Some(_), true) => 1,
            (Some(_), false) => 2,
        };
        if self.gpu.projection_override != Some(matrix)
            || data.len() != self.draw_data.len()
            || data.iter().any(|(name, value)| {
                self.draw_data
                    .get(name)
                    .map_or(true, |old| !Rc::ptr_eq(old, value))
            })
        {
            self.flush();
            self.draw_data = data;
            self.gpu.projection_override = Some(matrix);
        }
        let count = if is_quad_shader(shader) {
            packed_instance_size(shader, instances.bytes()).unwrap() / 16
        } else {
            count as usize
        };
        self.draws.push(Draw {
            shader,
            blend,
            depth,
            count: count as u32,
            instances,
            textures,
            filter,
            clear_color: None,
            count_in_stats: true,
            readback: None,
            scissor: self.rect(),
        });
    }

    fn update_mips(&mut self, texture: &wr::Texture) {
        if texture.filter != TextureFilter::Trilinear || self.gpu.is_failed() {
            return;
        }
        let raw = self.textures[&texture.id].clone();
        let projection = self.gpu.projection_override.take();
        let result = self.gpu.generate_mips(&raw);
        self.gpu.projection_override = projection;
        self.record_result(result);
    }

    fn read(
        &mut self,
        target: Rc<Texture<A>>,
        rect: DeviceIntRect,
        format: ImageFormat,
    ) -> Vec<u8> {
        self.flush();
        let bpp = format.bytes_per_pixel() as usize;
        if self.gpu.is_failed() {
            return vec![0; rect.area() as usize * bpp];
        }
        let requested = texture_format(format).unwrap();
        if target.format != requested
            && !matches!(
                (target.format, requested),
                (
                    wgt::TextureFormat::Rgba8Unorm,
                    wgt::TextureFormat::Bgra8Unorm
                ) | (
                    wgt::TextureFormat::Bgra8Unorm,
                    wgt::TextureFormat::Rgba8Unorm
                )
            )
        {
            self.record_result::<()>(Err("Unsupported HAL readback conversion".into()));
            return vec![0; rect.area() as usize * bpp];
        }
        if let Some(bytes) = self.table_pixels(&target, rect) {
            return bytes;
        }
        let result = (|| -> Result<Vec<u8>> {
            let layout = ReadbackLayout::with_pixel_size(
                rect.width() as u32,
                rect.height() as u32,
                self.gpu
                    .owner
                    .capabilities
                    .alignments
                    .buffer_copy_pitch
                    .get(),
                bpp as u32,
            )?;
            if !target.initialized() {
                return Ok(vec![0; rect.area() as usize * bpp]);
            }
            let buffer = self.gpu.readback_buffer(&layout)?;
            let previous = target.current_usage();
            let mut commands = self.gpu.submissions.recording()?;
            buffer.transition(&mut commands, wgt::BufferUses::COPY_DST);
            target.transition(&mut commands, wgt::TextureUses::COPY_SRC);
            unsafe {
                commands.encoder().copy_texture_to_buffer(
                    &target.raw,
                    wgt::TextureUses::COPY_SRC,
                    &buffer.raw,
                    std::iter::once(hal::BufferTextureCopy {
                        buffer_layout: wgt::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(layout.pitch),
                            rows_per_image: Some(rect.height() as u32),
                        },
                        texture_base: hal::TextureCopyBase {
                            mip_level: target.base_mip,
                            array_layer: 0,
                            origin: wgt::Origin3d {
                                x: rect.min.x as u32,
                                y: rect.min.y as u32,
                                z: 0,
                            },
                            aspect: target.copy_aspect(),
                        },
                        size: wgt::Extent3d {
                            width: rect.width() as u32,
                            height: rect.height() as u32,
                            depth_or_array_layers: 1,
                        }
                        .into(),
                    }),
                );
            }
            target.transition(&mut commands, previous);
            buffer.transition(&mut commands, wgt::BufferUses::MAP_READ);
            drop(commands);
            self.gpu.submissions.wait()?;
            self.gpu.owner.map_readback(&buffer.raw, &layout)
        })();
        let mut bytes = self
            .record_result(result)
            .unwrap_or_else(|| vec![0; rect.area() as usize * bpp]);
        if matches!(
            (target.format, format),
            (wgt::TextureFormat::Bgra8Unorm, ImageFormat::RGBA8)
                | (wgt::TextureFormat::Rgba8Unorm, ImageFormat::BGRA8)
        ) {
            for pixel in bytes.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
        }
        bytes
    }

    fn vao(
        &mut self,
        descriptor: &wr::VertexDescriptor,
        divisor: u32,
        base: Option<&wr::VAO>,
        share_instances: bool,
    ) -> wr::VAO {
        let stride = descriptor
            .instance_attributes
            .iter()
            .map(|attribute| {
                attribute.count as usize
                    * match attribute.kind {
                        wr::VertexAttributeKind::U8Norm => 1,
                        wr::VertexAttributeKind::U16Norm | wr::VertexAttributeKind::U16 => 2,
                        _ => 4,
                    }
            })
            .sum();
        let id = self.id();
        let ibo_id = base
            .map(|v| v.ibo_id)
            .unwrap_or_else(|| wr::IBOId(self.id()));
        let main_vbo_id = base
            .map(|v| v.main_vbo_id)
            .unwrap_or_else(|| wr::VBOId(self.id()));
        let instance_vbo_id = if share_instances {
            base.unwrap().instance_vbo_id
        } else {
            wr::VBOId(self.id())
        };
        self.buffers.entry(ibo_id.0).or_default();
        self.buffers.entry(main_vbo_id.0).or_default();
        self.buffers.entry(instance_vbo_id.0).or_default();
        wr::VAO {
            id,
            ibo_id,
            main_vbo_id,
            instance_vbo_id,
            instance_stride: stride,
            instance_divisor: divisor,
            owns_vertices_and_indices: base.is_none(),
            owns_instances: !share_instances,
        }
    }
}

impl<A: BackendApi + 'static> GpuBackend for HalGpuBackend<A> {
    fn textures_created(&self) -> u32 {
        self.created
    }
    fn textures_deleted(&self) -> u32 {
        self.deleted
    }
    fn set_initialize_color_targets_with_pink(&mut self, enabled: bool) {
        self.pink = enabled;
    }
    fn create_gpu_profiler(&self, _: bool) -> GpuProfiler {
        GpuProfiler::new(Rc::new(NoQueries))
    }
    fn set_parameter(&mut self, _: &Parameter) {}
    fn max_texture_size(&self) -> i32 {
        self.gpu.owner.max_texture_size()
    }
    fn surface_origin_is_top_left(&self) -> bool {
        true
    }
    fn get_capabilities(&self) -> &wr::Capabilities {
        &self.capabilities
    }
    fn api_info(&self) -> wr::GraphicsApiInfo {
        wr::GraphicsApiInfo {
            kind: match self.gpu.info().backend {
                wgt::Backend::Vulkan => wr::GraphicsApi::Vulkan,
                wgt::Backend::Metal => wr::GraphicsApi::Metal,
                _ => unreachable!(),
            },
            renderer: self.gpu.info().name.clone(),
            version: self.gpu.info().driver_info.clone(),
        }
    }
    fn take_out_of_memory_error(&self) -> bool {
        self.gpu.is_failed()
    }
    fn blend_barrier(&self) {}
    fn shader_feature_flags(&self) -> ShaderFeatureFlags {
        ShaderFeatureFlags::GL
    }
    fn preferred_color_formats(&self) -> wr::TextureFormatPair<ImageFormat> {
        ImageFormat::BGRA8.into()
    }
    fn swizzle_settings(&self) -> Option<SwizzleSettings> {
        None
    }
    fn max_depth_ids(&self) -> i32 {
        crate::renderer::hal::MAX_DEPTH_IDS
    }
    fn ortho_near_plane(&self) -> f32 {
        -(self.max_depth_ids() as f32)
    }
    fn ortho_far_plane(&self) -> f32 {
        (self.max_depth_ids() - 1) as f32
    }
    fn required_transfer_stride(&self) -> wr::StrideAlignment {
        wr::StrideAlignment::Bytes(NonZeroUsize::new(4).unwrap())
    }
    fn upload_method(&self) -> &wr::UploadMethod {
        &self.upload_method
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
        self.flush();
        self.bound_program = 0;
        self.bound_textures = [0; 16];
    }
    fn begin_frame(&mut self) -> wr::GpuFrameId {
        self.record_result(self.gpu.poll());
        self.bound_textures = [0; 16];
        self.bound_filters = [None; 16];
        self.bound_program = 0;
        self.frame_id += 1;
        self.created = 0;
        self.deleted = 0;
        self.stats = DrawStats::default();
        if !self.gpu.is_failed() {
            self.query = self
                .record_result(self.gpu.queries.borrow_mut().begin(&self.gpu.submissions))
                .flatten();
        }
        wr::GpuFrameId::new(self.frame_id)
    }
    fn bind_texture(&mut self, slot: wr::TextureSlot, texture: &wr::Texture, _: Swizzle) {
        self.bound_textures[slot.0] = texture.id;
        self.bound_filters[slot.0] = None;
    }
    fn bind_external_texture(&mut self, slot: wr::TextureSlot, texture: &wr::ExternalTexture) {
        self.bound_textures[slot.0] = texture.id;
        self.bound_filters[slot.0] = Some(
            if texture.image_rendering == api::ImageRendering::Pixelated {
                TextureFilter::Nearest
            } else {
                TextureFilter::Linear
            },
        );
    }
    fn reset_read_target(&mut self) {
        self.read_target = self.default_target.clone();
    }
    fn begin_render_pass(&mut self, desc: &wr::RenderPassDescriptor) {
        assert!(!self.in_pass);
        self.bound_textures[..3].fill(0);
        self.active_default = desc.target.is_default();
        let target = self.target(desc.target);
        let area = desc
            .target
            .build_scissor_rect(Some(
                desc.render_area
                    .unwrap_or_else(|| DeviceIntRect::from_size(desc.target.dimensions())),
            ))
            .cast_unit()
            .translate(self.scissor_offset);
        let full = DeviceIntRect::from_size(DeviceIntSize::new(
            target.size.width as i32,
            target.size.height as i32,
        ));
        // An unspecified render area can contain persistent cache entries.
        if desc.render_area.is_some()
            && area == full
            && !(self.active_default && self.composition_damage.is_some())
            && desc.color_load == wr::LoadOp::DontCare
            && !self.gpu.is_failed()
        {
            if let Some(mut commands) = self.record_result(self.gpu.submissions.recording()) {
                target.invalidate(&mut commands);
            }
        }
        self.target = Some(target);
        self.in_pass = true;
    }
    fn end_render_pass(&mut self, depth_store: wr::StoreOp) {
        assert!(self.in_pass);
        self.flush();
        self.in_pass = false;
        if depth_store == wr::StoreOp::Discard && !self.gpu.is_failed() {
            let target = self.target.as_ref().unwrap();
            if let Some(depth) = self
                .gpu
                .depths
                .get(&(target.allocation_id, target.base_mip))
            {
                if let Some(mut commands) = self.record_result(self.gpu.submissions.recording()) {
                    depth.invalidate(&mut commands);
                }
            }
        }
    }
    fn link_program(
        &mut self,
        program: &mut wr::Program,
        _: &wr::VertexDescriptor,
    ) -> std::result::Result<(), wr::ShaderError> {
        program.is_initialized = true;
        Ok(())
    }
    fn bind_pipeline(&mut self, program: &wr::Program, state: &wr::RenderState) -> bool {
        let changed = self.bound_program != program.id || self.state != *state;
        self.bound_program = program.id;
        self.state = *state;
        changed
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
        assert!(matches!(
            target,
            ImageBufferKind::Texture2D | ImageBufferKind::TextureRect
        ));
        let raw = self
            .record_result(Texture::new(
                &self.gpu.owner,
                width as u32,
                height as u32,
                texture_format(format).unwrap(),
                filter,
                render_target.is_some(),
            ))
            .unwrap_or_else(|| self.gpu.dummy.clone());
        let id = self.id();
        self.textures.insert(id, raw);
        self.created += 1;
        wr::Texture {
            id,
            target,
            format,
            size: DeviceIntSize::new(width, height),
            filter,
            flags: wr::TextureFlags::empty(),
            active_swizzle: Cell::new(Swizzle::default()),
            fbo: render_target.as_ref().map(|_| wr::FBOId(id)),
            fbo_with_depth: render_target.filter(|r| r.has_depth).map(|_| wr::FBOId(id)),
            last_frame_used: wr::GpuFrameId::new(self.frame_id),
        }
    }
    fn copy_texture_sub_region(
        &mut self,
        src: &wr::Texture,
        x: usize,
        y: usize,
        dst: &wr::Texture,
        dx: usize,
        dy: usize,
        width: usize,
        height: usize,
    ) {
        self.flush();
        if self.gpu.is_failed() {
            return;
        }
        let source = self.textures[&src.id].clone();
        let target = self.textures[&dst.id].clone();
        let size = DeviceIntSize::new(width as i32, height as i32);
        self.record_result(self.gpu.copy_native(
            &source,
            &target,
            DeviceIntRect::from_origin_and_size(DeviceIntPoint::new(x as i32, y as i32), size),
            DeviceIntRect::from_origin_and_size(DeviceIntPoint::new(dx as i32, dy as i32), size),
        ));
    }
    fn invalidate_render_target(&mut self, texture: &wr::Texture) {
        self.flush();
        if self.gpu.is_failed() {
            return;
        }
        // Later draws may leave an unused sampler bound to this discarded target.
        for binding in &mut self.bound_textures {
            if *binding == texture.id {
                *binding = 0;
            }
        }
        if let Some(mut commands) = self.record_result(self.gpu.submissions.recording()) {
            self.textures[&texture.id].invalidate(&mut commands);
        }
    }
    fn reuse_render_target(&mut self, texture: &mut wr::Texture, info: RenderTargetInfo) {
        texture.fbo_with_depth = info.has_depth.then_some(wr::FBOId(texture.id));
        if !info.has_depth {
            let raw = &self.textures[&texture.id];
            self.gpu.depths.remove(&(raw.allocation_id, raw.base_mip));
        }
        texture.last_frame_used = wr::GpuFrameId::new(self.frame_id);
    }
    fn blit_render_target(
        &mut self,
        src: wr::ReadTarget,
        sr: FramebufferIntRect,
        dst: wr::DrawTarget,
        dr: FramebufferIntRect,
        filter: TextureFilter,
    ) {
        self.flush();
        let source = self.read_target(src);
        let view = self.target_view;
        let scissor_offset = self.scissor_offset;
        let target = self.target(dst);
        self.target_view = view;
        self.scissor_offset = scissor_offset;
        let projection = self.gpu.projection_override.take();
        let result = self.gpu.record_blit(
            &source,
            &target,
            sr.cast_unit(),
            dr.cast_unit(),
            filter,
            &mut DrawStats::default(),
        );
        self.gpu.projection_override = projection;
        self.record_result(result);
    }
    fn delete_texture(&mut self, mut texture: wr::Texture) {
        self.flush();
        if let Some(raw) = self.textures.remove(&texture.id) {
            self.gpu.descriptors.borrow_mut().clear();
            self.gpu.frame_descriptors.borrow_mut().clear();
            self.tables.remove(&raw.allocation_id);
            self.gpu.depths.remove(&(raw.allocation_id, raw.base_mip));
        }
        texture.id = 0;
        self.deleted += 1;
    }
    #[cfg(feature = "replay")]
    fn delete_external_texture(&mut self, texture: wr::ExternalTexture) {
        self.flush();
        self.textures.remove(&texture.id);
    }
    fn delete_program(&mut self, mut program: wr::Program) {
        self.flush();
        self.programs.remove(&program.id);
        program.id = 0;
    }
    fn create_program(
        &mut self,
        name: &'static str,
        features: &[&'static str],
    ) -> std::result::Result<wr::Program, wr::ShaderError> {
        let features_text = features.join(",").replace("TEXTURE_RECT", "TEXTURE_2D");
        let artifact = shaders::SHADERS
            .iter()
            .find(|s| !s.buffer_tables && s.name == name && s.features == features_text)
            .ok_or_else(|| {
                wr::ShaderError::Compilation(
                    name.into(),
                    format!("No HAL shader for {features_text}"),
                    Vec::new(),
                )
            })?;
        let id = self.id();
        self.programs.insert(
            id,
            ProgramState {
                shader: Shader::Other(artifact.name, artifact.features),
                samplers: Vec::new(),
                transform: Cell::new(Transform3D::identity()),
            },
        );
        Ok(wr::Program {
            id,
            u_transform: 0,
            u_texture_size: 0,
            is_initialized: false,
            source_info: wr::ProgramSourceInfo {
                base_filename: name,
                features: features.to_vec(),
                full_name_cstr: Rc::new(
                    std::ffi::CString::new(format!("{name} {features_text}")).unwrap(),
                ),
                source_type: wr::ProgramSourceType::Unoptimized,
                digest: Default::default(),
                #[cfg(feature = "debugger")]
                from_source_override: false,
            },
        })
    }
    #[cfg(feature = "debugger")]
    fn supports_shader_source_override(&self) -> bool {
        false
    }
    #[cfg(feature = "debugger")]
    fn shader_file_names(&self) -> Vec<&'static str> {
        Vec::new()
    }
    #[cfg(feature = "debugger")]
    fn builtin_shader_source(&self, _: &str) -> Option<&'static str> {
        None
    }
    fn get_shader_source(&self, _: &str) -> Cow<'static, str> {
        Cow::Borrowed("")
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
        panic!("HAL shader overrides are unavailable")
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
        Default::default()
    }
    fn bind_shader_samplers(
        &mut self,
        program: &wr::Program,
        bindings: &[(&'static str, wr::TextureSlot)],
    ) {
        self.programs.get_mut(&program.id).unwrap().samplers = bindings
            .iter()
            .map(|(name, slot)| (*name, slot.0))
            .collect();
    }
    fn set_uniforms(&self, program: &wr::Program, transform: &Transform3D<f32>) {
        self.programs[&program.id].transform.set(*transform);
    }
    fn set_shader_texture_size(&self, _: &wr::Program, _: DeviceSize) {}
    fn create_transfer_buffer_with_size(&mut self, size: usize) -> wr::TransferBuffer {
        let id = self.id();
        self.buffers.insert(id, vec![0; size]);
        wr::TransferBuffer {
            id,
            reserved_size: size,
        }
    }
    fn read_pixels_into_transfer_buffer(
        &mut self,
        target: wr::ReadTarget,
        rect: DeviceIntRect,
        format: ImageFormat,
        pbo: &wr::TransferBuffer,
    ) {
        let bytes = self.read(self.read_target(target), rect, format);
        self.buffers.get_mut(&pbo.id).unwrap()[..bytes.len()].copy_from_slice(&bytes);
    }
    fn map_transfer_buffer<'a>(
        &'a mut self,
        pbo: &'a wr::TransferBuffer,
    ) -> Option<wr::MappedTransferBuffer<'a>> {
        let data = self.buffers.get(&pbo.id)?;
        let slice = unsafe { std::slice::from_raw_parts(data.as_ptr(), data.len()) };
        Some(wr::MappedTransferBuffer {
            device: self,
            data: slice,
        })
    }
    fn unmap_transfer_buffer(&mut self) {}
    fn delete_transfer_buffer(&mut self, mut buffer: wr::TransferBuffer) {
        self.buffers.remove(&buffer.id);
        buffer.id = 0;
    }
    fn create_transfer_buffer(&mut self) -> wr::TransferBuffer {
        self.create_transfer_buffer_with_size(0)
    }
    fn required_upload_size_and_stride(
        &self,
        size: DeviceIntSize,
        format: ImageFormat,
    ) -> (usize, usize) {
        let stride = size.width as usize * format.bytes_per_pixel() as usize;
        (stride * size.height as usize, stride)
    }
    fn allocate_upload_buffer(
        &mut self,
        buffer: &mut wr::TransferBuffer,
        size: usize,
        _: wr::VertexUsageHint,
        persistent: bool,
    ) -> Result<wr::UploadBufferMapping> {
        let data = self.buffers.get_mut(&buffer.id).unwrap();
        data.resize(size, 0);
        buffer.reserved_size = size;
        let ptr = ptr::NonNull::new(data.as_mut_ptr().cast()).unwrap();
        Ok(if persistent {
            wr::UploadBufferMapping::Persistent(ptr)
        } else {
            wr::UploadBufferMapping::Transient(ptr)
        })
    }
    fn map_upload_buffer(
        &mut self,
        buffer: &wr::TransferBuffer,
    ) -> Result<ptr::NonNull<mem::MaybeUninit<u8>>> {
        Ok(ptr::NonNull::new(
            self.buffers
                .get_mut(&buffer.id)
                .unwrap()
                .as_mut_ptr()
                .cast(),
        )
        .unwrap())
    }
    fn flush_upload_buffer(
        &mut self,
        buffer: &wr::TransferBuffer,
        _: &wr::UploadBufferMapping,
        size: usize,
        chunks: &[wr::UploadChunk],
    ) {
        self.flush();
        if self.gpu.is_failed() {
            return;
        }
        let data = self.buffers.remove(&buffer.id).unwrap();
        assert!(size <= data.len());
        for chunk in chunks {
            let bytes = chunk.rect.width() as u64
                * chunk.rect.height() as u64
                * chunk
                    .format_override
                    .unwrap_or(chunk.texture.format)
                    .bytes_per_pixel() as u64;
            self.gpu.resource_upload_bytes += bytes;
            self.gpu.count(RenderCounter::ResourceUploads, 1);
            self.gpu.count(RenderCounter::ResourceUploadBytes, bytes);
            if self.upload_table(
                chunk.texture,
                chunk.rect,
                chunk.stride,
                &data[chunk.offset..],
            ) {
                continue;
            }
            self.record_result(self.textures[&chunk.texture.id].upload_recorded(
                &self.gpu.owner,
                &self.gpu.submissions,
                chunk.rect,
                &data[..size],
                chunk.stride,
                i32::try_from(chunk.offset).unwrap(),
                chunk.format_override,
            ));
            self.update_mips(chunk.texture);
        }
        self.buffers.insert(buffer.id, data);
    }
    fn orphan_upload_buffer(&mut self, buffer: &mut wr::TransferBuffer) {
        self.buffers.insert(buffer.id, Vec::new());
        buffer.reserved_size = 0;
    }
    fn upload_texture_region(
        &mut self,
        texture: &wr::Texture,
        rect: DeviceIntRect,
        stride: Option<i32>,
        format: Option<ImageFormat>,
        data: &[u8],
    ) {
        self.flush();
        let bytes = rect.width() as u64
            * rect.height() as u64
            * format.unwrap_or(texture.format).bytes_per_pixel() as u64;
        self.gpu.resource_upload_bytes += bytes;
        self.gpu.count(RenderCounter::ResourceUploads, 1);
        self.gpu.count(RenderCounter::ResourceUploadBytes, bytes);
        if self.gpu.is_failed() {
            return;
        }
        if self.upload_table(texture, rect, stride, data) {
            return;
        }
        self.record_result(self.textures[&texture.id].upload_recorded(
            &self.gpu.owner,
            &self.gpu.submissions,
            rect,
            data,
            stride,
            0,
            format,
        ));
        self.update_mips(texture);
    }
    fn create_fence(&mut self) -> Option<wr::Fence> {
        self.flush();
        self.gpu
            .submissions
            .submit_serial()
            .ok()
            .map(|s| wr::Fence(s as usize))
    }
    fn poll_fence(&self, fence: &wr::Fence) -> wr::FenceStatus {
        match self.gpu.submissions.poll() {
            Ok(serial) if serial >= fence.0 as u64 => wr::FenceStatus::Signaled,
            Ok(_) => wr::FenceStatus::Pending,
            Err(_) => wr::FenceStatus::Error,
        }
    }
    fn delete_fence(&mut self, _: wr::Fence) {}
    fn upload_texture_immediate(&mut self, texture: &wr::Texture, data: &[u8]) {
        self.upload_texture_region(
            texture,
            DeviceIntRect::from_size(texture.size),
            None,
            None,
            data,
        );
    }
    fn read_pixels(&mut self, descriptor: &ImageDescriptor) -> Vec<u8> {
        self.read(
            self.read_target
                .clone()
                .or_else(|| self.default_target.clone())
                .unwrap(),
            DeviceIntRect::from_size(descriptor.size),
            descriptor.format,
        )
    }
    fn read_pixels_into(
        &mut self,
        rect: FramebufferIntRect,
        format: ImageFormat,
        output: &mut [u8],
    ) {
        let bytes = self.read(
            self.read_target
                .clone()
                .or_else(|| self.default_target.clone())
                .unwrap(),
            rect.cast_unit(),
            format,
        );
        output.copy_from_slice(&bytes);
    }
    fn attach_read_texture_external(&mut self, handle: ExternalTextureHandle, _: ImageBufferKind) {
        self.read_target = self.texture_handle(u32::try_from(handle.0).unwrap());
    }
    fn attach_read_texture(&mut self, texture: &wr::Texture) {
        self.read_target = Some(self.textures[&texture.id].clone());
    }
    fn bind_vao(&mut self, vao: &wr::VAO) {
        self.bound_instances = vao.instance_vbo_id.0;
        self.bound_instance_stride = vao.instance_stride;
    }
    fn create_vao(&mut self, descriptor: &wr::VertexDescriptor, divisor: u32) -> wr::VAO {
        self.vao(descriptor, divisor, None, false)
    }
    fn delete_vao(&mut self, mut vao: wr::VAO) {
        if vao.owns_vertices_and_indices {
            self.buffers.remove(&vao.ibo_id.0);
            self.buffers.remove(&vao.main_vbo_id.0);
        }
        if vao.owns_instances {
            self.buffers.remove(&vao.instance_vbo_id.0);
        }
        vao.id = 0;
    }
    fn create_vao_with_new_instances(
        &mut self,
        descriptor: &wr::VertexDescriptor,
        base: &wr::VAO,
    ) -> wr::VAO {
        self.vao(descriptor, base.instance_divisor, Some(base), false)
    }
    fn create_vao_with_shared_instances(
        &mut self,
        descriptor: &wr::VertexDescriptor,
        base: &wr::VAO,
    ) -> wr::VAO {
        self.vao(descriptor, base.instance_divisor, Some(base), true)
    }
    fn update_vao_main_vertices(&mut self, vao: &wr::VAO, data: &[u8], _: wr::VertexUsageHint) {
        self.buffers.insert(vao.main_vbo_id.0, data.to_vec());
    }
    fn update_vao_instances(
        &mut self,
        vao: &wr::VAO,
        data: &[u8],
        stride: usize,
        _: wr::VertexUsageHint,
        repeat: Option<NonZeroUsize>,
    ) {
        let output = self.buffers.get_mut(&vao.instance_vbo_id.0).unwrap();
        output.clear();
        for instance in data.chunks_exact(stride) {
            for _ in 0..repeat.map_or(1, |n| n.get()) {
                output.extend_from_slice(instance);
            }
        }
    }
    fn update_vao_indices(&mut self, vao: &wr::VAO, data: &[u8], _: wr::VertexUsageHint) {
        self.buffers.insert(vao.ibo_id.0, data.to_vec());
    }
    fn reallocate_vbo(&mut self, vbo: wr::VBOId, size: usize) {
        self.buffers.insert(vbo.0, vec![0; size]);
    }
    fn update_vbo_data_unsynchronized(&mut self, vbo: wr::VBOId, data: &[u8], offset: usize) {
        self.buffers.get_mut(&vbo.0).unwrap()[offset..offset + data.len()].copy_from_slice(data);
    }
    fn draw_triangles_u32(&mut self, _: i32, _: i32) {
        panic!("HAL non-instanced triangles are unavailable")
    }
    fn draw_nonindexed_lines(&mut self, _: i32, _: i32) {
        panic!("HAL line rendering is unavailable")
    }
    fn draw_indexed_triangles(&mut self, _: i32) {
        self.draw(1, 0);
    }
    fn draw_indexed_triangles_instanced_u16(&mut self, _: i32, count: i32) {
        self.draw(count, 0);
    }
    fn draw_indexed_triangles_instanced_base_instance_u16(
        &mut self,
        _: i32,
        count: i32,
        base: u32,
    ) {
        self.draw(count, base);
    }
    fn deinit(&mut self) {
        self.flush();
        self.record_result(self.gpu.submissions.wait());
    }
    fn end_frame(&mut self) {
        self.flush();
        self.draw_data.clear();
        self.target = None;
        self.gpu.clear_frame_data();
        self.gpu.projection_override = None;
        for table in self.tables.values_mut() {
            table.release_buffer();
        }
        if !self.gpu.is_failed() {
            let query = self.query.take();
            self.record_result(
                self.gpu
                    .queries
                    .borrow_mut()
                    .finish(&self.gpu.submissions, query),
            );
        } else {
            self.gpu.abort();
        }
    }
    fn clear_target(
        &mut self,
        color: Option<[f32; 4]>,
        depth: Option<f32>,
        rect: Option<FramebufferIntRect>,
    ) {
        self.flush();
        if let Some(color) = color {
            let mut rect = rect
                .map(|r| r.cast_unit().translate(self.scissor_offset))
                .unwrap_or_else(|| self.rect());
            if self.active_default {
                if let Some(damage) = self.composition_damage {
                    rect = rect
                        .intersection(&damage)
                        .unwrap_or_else(DeviceIntRect::zero);
                }
            }
            self.draws.push(
                self.gpu
                    .clear(rect, ColorF::new(color[0], color[1], color[2], color[3])),
            );
        }
        if depth.is_some() && !self.gpu.is_failed() {
            let target = self.target.as_ref().unwrap();
            if let Some(depth) = self
                .gpu
                .depths
                .get(&(target.allocation_id, target.base_mip))
            {
                if let Some(mut commands) = self.record_result(self.gpu.submissions.recording()) {
                    depth.invalidate(&mut commands);
                }
            }
        }
    }
    fn set_scissor_rect(&self, rect: FramebufferIntRect) {
        self.scissor.set(Some(rect.cast_unit()));
    }
    fn enable_scissor(&self) {
        self.scissor_enabled.set(true);
    }
    fn disable_scissor(&self) {
        self.scissor_enabled.set(false);
    }
    fn echo_driver_messages(&self) {}
    fn report_memory(
        &self,
        _: &MallocSizeOfOps,
        _: *mut c_void,
    ) -> crate::render_api::MemoryReport {
        crate::render_api::MemoryReport {
            depth_target_textures: self.depth_targets_memory(),
            ..Default::default()
        }
    }
    fn depth_targets_memory(&self) -> usize {
        self.gpu
            .depths
            .values()
            .map(|t| t.size.width as usize * t.size.height as usize * 4)
            .sum()
    }
}

struct NoQueries;
impl GpuQueryBackend for NoQueries {
    fn create_queries(&self, _: usize) -> Vec<GpuQueryId> {
        Vec::new()
    }
    fn delete_queries(&self, queries: &[GpuQueryId]) {
        assert!(queries.is_empty());
    }
    fn begin_query(&self, _: GpuQueryKind, _: GpuQueryId) {
        unreachable!()
    }
    fn end_query(&self, _: GpuQueryKind) {
        unreachable!()
    }
    fn query_result(&self, _: GpuQueryId) -> u64 {
        unreachable!()
    }
    fn supports_markers(&self) -> bool {
        false
    }
    fn push_marker_group(&self, _: &str) {
        unreachable!()
    }
    fn pop_marker_group(&self) {
        unreachable!()
    }
    fn insert_marker(&self, _: &str) {
        unreachable!()
    }
}

#[cfg(all(test, wr_hal_vulkan))]
mod tests {
    use super::*;
    use api::*;
    use crate::render_api::Transaction;
    use std::sync::mpsc::{self, Sender};

    struct Notice(Sender<()>);
    impl RenderNotifier for Notice {
        fn clone(&self) -> Box<dyn RenderNotifier> {
            Box::new(Self(self.0.clone()))
        }
        fn wake_up(&self, _: bool) {}
        fn new_frame_ready(&self, _: DocumentId, _: FramePublishId, _: &FrameReadyParams) {
            let _ = self.0.send(());
        }
        fn shut_down(&self) {}
    }

    #[test]
    #[ignore = "Requires Vulkan"]
    fn upstream_renderer_draws_through_hal_backend() {
        let config = crate::device::hal::create_vulkan_backend(&Options {
            validation: true,
            ..Default::default()
        })
        .unwrap();
        let (tx, rx) = mpsc::channel();
        let options = crate::WebRenderOptions {
            enable_subpixel_aa: false,
            ..Default::default()
        };
        let (mut renderer, sender) =
            crate::create_webrender_instance(config, Box::new(Notice(tx)), options, None).unwrap();
        let mut api = sender.create_api();
        let size = DeviceIntSize::new(32, 32);
        let document = api.add_document(size);
        let pipeline = PipelineId(0, 0);
        let rect = LayoutRect::from_size(LayoutSize::new(32.0, 32.0));
        let info = CommonItemProperties {
            clip_rect: rect,
            clip_chain_id: ClipChainId::INVALID,
            spatial_id: SpatialId::root_scroll_node(pipeline),
            flags: PrimitiveFlags::default(),
        };
        let mut builder = DisplayListBuilder::new(pipeline);
        builder.begin(60.0);
        builder.push_rect(&info, rect, ColorF::new(1.0, 0.0, 0.0, 1.0));
        let inset = LayoutRect::from_origin_and_size(
            LayoutPoint::new(8.0, 8.0),
            LayoutSize::new(16.0, 16.0),
        );
        builder.push_rect(&info, inset, ColorF::new(0.0, 1.0, 0.0, 0.5));
        let mut transaction = Transaction::new();
        transaction.set_root_pipeline(pipeline);
        transaction.set_display_list(Epoch(0), api.get_namespace_id(), builder.end());
        transaction.generate_frame(1, true, false, RenderReasons::TESTING);
        api.send_transaction(document, transaction);
        rx.recv_timeout(std::time::Duration::from_secs(15)).unwrap();
        renderer.update();
        renderer.render(size, 0).unwrap();
        let pixels = renderer.read_pixels_rgba8(FramebufferIntRect::from_size(size.cast_unit()));
        for (i, pixel) in pixels.chunks_exact(4).enumerate() {
            if (8..24).contains(&(i % 32)) && (8..24).contains(&(i / 32)) {
                assert!(
                    (pixel[0] as i32 - 127).abs() <= 1
                        && (pixel[1] as i32 - 128).abs() <= 1
                        && pixel[2] == 0
                        && pixel[3] == 255,
                    "{}: {:?}",
                    i,
                    pixel
                );
            } else {
                assert_eq!(pixel, [255, 0, 0, 255], "pixel {i}");
            }
        }
        api.delete_document(document);
        renderer.deinit();
    }
    #[test]
    #[ignore = "Requires Vulkan"]
    fn text_after_invalidated_clip_mask() {
        let config = crate::device::hal::create_vulkan_backend(&Options {
            validation: true,
            ..Default::default()
        })
        .unwrap();
        let (tx, rx) = mpsc::channel();
        let (mut renderer, sender) = crate::create_webrender_instance(
            config,
            Box::new(Notice(tx)),
            crate::WebRenderOptions {
                enable_subpixel_aa: true,
                ..Default::default()
            },
            None,
        )
        .unwrap();
        let mut api = sender.create_api();
        let size = DeviceIntSize::new(512, 128);
        let document = api.add_document(size);
        let pipeline = PipelineId(0, 0);
        let font = api.generate_font_key();
        let instance = api.generate_font_instance_key();
        let mut transaction = Transaction::new();
        transaction.add_raw_font(
            font,
            std::fs::read(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../wrench/reftests/text/FreeSans.ttf"),
            )
            .unwrap(),
            0,
        );
        transaction.add_font_instance(
            instance,
            font,
            20.0,
            Some(FontInstanceOptions {
                render_mode: FontRenderMode::Subpixel,
                ..Default::default()
            }),
            None,
            Vec::new(),
        );
        api.send_transaction(document, transaction);
        let indices = api.get_glyph_indices(font, "Clip mask lifetime");
        let rect = LayoutRect::from_size(LayoutSize::new(512.0, 128.0));
        let info = CommonItemProperties {
            clip_rect: rect,
            clip_chain_id: ClipChainId::INVALID,
            spatial_id: SpatialId::root_scroll_node(pipeline),
            flags: PrimitiveFlags::default(),
        };
        let mut builder = DisplayListBuilder::new(pipeline);
        builder.begin(60.0);
        builder.push_rect(&info, rect, ColorF::WHITE);
        let clip = builder.define_clip_rounded_rect(
            info.spatial_id,
            ComplexClipRegion::new(
                LayoutRect::from_origin_and_size(
                    LayoutPoint::new(8.0, 8.0),
                    LayoutSize::new(180.0, 40.0),
                ),
                BorderRadius::uniform(16.0),
                Default::default(),
                ClipMode::Clip,
            ),
        );
        let chain = builder.define_clip_chain(None, std::iter::once(clip));
        let clipped = CommonItemProperties {
            clip_chain_id: chain,
            ..info
        };
        builder.push_stacking_context(
            info.spatial_id,
            info.flags,
            None,
            TransformStyle::Flat,
            MixBlendMode::Normal,
            &[FilterOp::Blur(1.0, 1.0, true)],
            &[],
            RasterSpace::Screen,
            StackingContextFlags::empty(),
            None,
        );
        let glyphs: Vec<_> = indices
            .iter()
            .enumerate()
            .map(|(i, glyph)| GlyphInstance {
                index: glyph.unwrap(),
                point: LayoutPoint::new(10.0 + i as f32 * 12.0, 32.0),
            })
            .collect();
        builder.push_text(&clipped, rect, &glyphs, instance, ColorF::BLACK, None);
        builder.pop_stacking_context();
        let glyphs: Vec<_> = glyphs
            .iter()
            .map(|glyph| GlyphInstance {
                index: glyph.index,
                point: glyph.point + LayoutVector2D::new(0.0, 60.0),
            })
            .collect();
        builder.push_text(&info, rect, &glyphs, instance, ColorF::BLACK, None);
        let mut transaction = Transaction::new();
        transaction.set_root_pipeline(pipeline);
        transaction.set_display_list(Epoch(0), api.get_namespace_id(), builder.end());
        transaction.generate_frame(1, true, false, RenderReasons::TESTING);
        api.send_transaction(document, transaction);
        rx.recv_timeout(std::time::Duration::from_secs(15)).unwrap();
        renderer.update();
        let result = renderer.render(size, 0);
        assert!(
            result.is_ok(),
            "{result:?}: {:?}",
            renderer
                .device
                .backend::<HalGpuBackend<hal::api::Vulkan>>()
                .failure()
        );
        let pixels = renderer.read_pixels_rgba8(FramebufferIntRect::from_size(size.cast_unit()));
        assert!(pixels[70 * 512 * 4..100 * 512 * 4]
            .chunks_exact(4)
            .any(|pixel| pixel[0] < 100));
        api.delete_document(document);
        renderer.deinit();
    }

    #[test]
    #[ignore = "Requires Vulkan"]
    fn cached_target_clear_preserves_other_images() {
        let device = create_vulkan_device(&Options {
            validation: true,
            ..Default::default()
        })
        .unwrap();
        let mut backend = HalGpuBackend::new(device).unwrap();
        backend.begin_frame();
        let texture = backend.create_texture(
            ImageBufferKind::Texture2D,
            ImageFormat::BGRA8,
            16,
            16,
            TextureFilter::Linear,
            Some(RenderTargetInfo { has_depth: false }),
        );
        let clear =
            DeviceIntRect::from_origin_and_size(DeviceIntPoint::new(4, 4), DeviceIntSize::new(8, 8));
        for render_area in [None, Some(clear)] {
            backend.upload_texture_immediate(&texture, &[0, 0, 255, 255].repeat(256));
            backend.begin_render_pass(&wr::RenderPassDescriptor {
                target: wr::DrawTarget::from_texture(&texture, false),
                render_area,
                color_load: wr::LoadOp::DontCare,
            });
            backend.clear_target(Some([0.0, 1.0, 0.0, 1.0]), None, Some(clear.cast_unit()));
            backend.end_render_pass(wr::StoreOp::Store);
            backend.attach_read_texture(&texture);
            let pixels = backend.read_pixels(&ImageDescriptor::new(
                16,
                16,
                ImageFormat::RGBA8,
                ImageDescriptorFlags::empty(),
            ));
            for (i, pixel) in pixels.chunks_exact(4).enumerate() {
                let expected = if (4..12).contains(&(i % 16)) && (4..12).contains(&(i / 16)) {
                    [0, 255, 0, 255]
                } else {
                    [255, 0, 0, 255]
                };
                assert_eq!(pixel, expected, "area {render_area:?}, pixel {i}");
            }
        }
        backend.delete_texture(texture);
        backend.deinit();
        backend.end_frame();
    }

    #[test]
    #[ignore = "Requires Vulkan"]
    fn backend_upload_copy_and_readback_formats() {
        let device = create_vulkan_device(&Options {
            validation: true,
            ..Default::default()
        })
        .unwrap();
        let mut backend = HalGpuBackend::new(device).unwrap();
        backend.begin_frame();
        for format in [ImageFormat::R8, ImageFormat::RGBA8, ImageFormat::RGBAI32] {
            let bpp = format.bytes_per_pixel() as usize;
            let source = backend.create_texture(
                ImageBufferKind::Texture2D,
                format,
                4,
                4,
                TextureFilter::Nearest,
                None,
            );
            let target = backend.create_texture(
                ImageBufferKind::Texture2D,
                format,
                2,
                2,
                TextureFilter::Nearest,
                None,
            );
            assert!(!backend.gpu.is_failed(), "{:?}", backend.failure());
            let bytes: Vec<u8> = (0..16 * bpp).map(|n| n as u8).collect();
            backend.upload_texture_immediate(&source, &bytes);
            backend.copy_texture_sub_region(&source, 1, 1, &target, 0, 0, 2, 2);
            backend.attach_read_texture(&target);
            let actual = backend.read_pixels(&ImageDescriptor::new(
                2,
                2,
                format,
                ImageDescriptorFlags::empty(),
            ));
            let expected: Vec<u8> = [5usize, 9]
                .iter()
                .flat_map(|&pixel| bytes[pixel * bpp..(pixel + 2) * bpp].iter().copied())
                .collect();
            assert_eq!(actual, expected, "{format:?}");
            backend.delete_texture(source);
            backend.delete_texture(target);
        }
        backend.deinit();
        backend.end_frame();
    }
}
