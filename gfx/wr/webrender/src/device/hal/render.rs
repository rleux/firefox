/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::backend::{BackendApi, ShaderCache, ShaderInputMode};
use super::submission::SubmissionQueue;
use super::resources::{Buffer, Owned, Texture, texture_format};
use super::diagnostics::{RenderCounter, RenderGauge, RenderMetrics, RenderMetricsSnapshot};
use super::external::{ReleaseQueue, dispatch_releases};
use std::{cell::{Cell, RefCell}, collections::HashMap, mem, rc::Rc};
use api::{ColorF, ImageBufferKind, PremultipliedColorF, units::*};
use crate::batch::{AlphaBatchContainer, BatchKind, BatchTextures, ClipMaskInstanceList};
use crate::composite::{CompositeTileSurface, ResolvedExternalSurface, ResolvedExternalSurfaceColorData, NativeTileId, CompositorClip};
use crate::device::{BlendMode, TextureFilter, VertexAttributeKind, VertexDescriptor};
use crate::frame_builder::Frame;
use crate::gpu_types::{ClearInstance, CompositeInstance, PrimitiveInstanceData, ScalingInstance};
use crate::internal_types::{
    CacheTextureId, DeferredResolveIndex, ResourceUpdateList, Swizzle, TextureCacheAllocationKind, TextureSource,
    TextureUpdateSource,
};
use crate::pattern::PatternKind;
use crate::picture::ResolvedSurfaceTexture;
use crate::render_target::{PictureCacheTargetKind, RenderTarget};
use crate::renderer::{vertex_descriptors as desc, MAX_VERTEX_TEXTURE_WIDTH};
use webrender_build::hal::{ScalarType, ShaderArtifact};
use smallvec::SmallVec;

mod shaders {
    #[cfg(any(wr_hal_vulkan, wr_hal_metal))]
    include!(concat!(env!("OUT_DIR"), "/hal_shaders.rs"));
    #[cfg(any(wr_hal_vulkan, wr_hal_metal))]
    include!(concat!(env!("OUT_DIR"), "/hal_present.rs"));
    #[cfg(not(any(wr_hal_vulkan, wr_hal_metal)))]
    pub static SHADERS: &[webrender_build::hal::ShaderArtifact] = &[];

    pub fn presentation() -> Result<&'static webrender_build::hal::ShaderArtifact, String> {
        #[cfg(any(wr_hal_vulkan, wr_hal_metal))]
        { Ok(&PRESENT) }
        #[cfg(not(any(wr_hal_vulkan, wr_hal_metal)))]
        { Err("No native HAL shader catalog was compiled for this target".into()) }
    }
}

#[cfg(any(feature = "capture", feature = "replay"))]
mod capture;
mod composite;
mod present;
mod present_history;

const PIPELINE_ABI: u32 = 1;

#[cfg(all(test, wr_hal_vulkan, feature = "hal-translate"))]
pub(super) fn shader_catalog_for_test() -> &'static [ShaderArtifact] { shaders::SHADERS }

// Only audited, fully initialized numeric GPU layouts may expose their bytes.
unsafe trait GpuData {
    const SIZE: usize;
}
unsafe impl GpuData for PrimitiveInstanceData {
    const SIZE: usize = 16;
}
unsafe impl GpuData for CompositeInstance {
    const SIZE: usize = 152;
}
unsafe impl GpuData for crate::gpu_types::MaskInstance {
    const SIZE: usize = 32;
}
unsafe impl GpuData for crate::gpu_types::BlurInstance {
    const SIZE: usize = 28;
}
unsafe impl GpuData for crate::gpu_types::BorderInstance {
    const SIZE: usize = 48;
}
unsafe impl GpuData for crate::render_target::LineDecorationJob {
    const SIZE: usize = 36;
}
unsafe impl GpuData for crate::gpu_types::PrimitiveHeaderF {
    const SIZE: usize = 32;
}
unsafe impl GpuData for crate::gpu_types::PrimitiveHeaderI {
    const SIZE: usize = 32;
}
unsafe impl GpuData for crate::gpu_types::SVGFEFilterInstance {
    const SIZE: usize = 64;
}
unsafe impl GpuData for ScalingInstance {
    const SIZE: usize = 36;
}
unsafe impl GpuData for ClearInstance {
    const SIZE: usize = 32;
}
unsafe impl GpuData for crate::transform::TransformData {
    const SIZE: usize = 128;
}
unsafe impl GpuData for crate::render_task::RenderTaskData {
    const SIZE: usize = 32;
}
unsafe impl GpuData for crate::renderer::GpuBufferBlockF {
    const SIZE: usize = 16;
}
unsafe impl GpuData for crate::renderer::GpuBufferBlockI {
    const SIZE: usize = 16;
}

fn bytes<T: GpuData>(values: &[T]) -> &[u8] {
    assert_eq!(mem::size_of::<T>(), T::SIZE);
    unsafe { std::slice::from_raw_parts(values.as_ptr().cast(), mem::size_of_val(values)) }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Shader {
    Quad,
    Composite,
    Clear,
    Other(&'static str, &'static str),
    LegacyBrilinear(&'static str, &'static str),
}

fn is_quad_shader(shader: Shader) -> bool {
    match shader {
        Shader::Quad => true,
        Shader::Other(name, _) | Shader::LegacyBrilinear(name, _) => {
            name.starts_with("ps_quad_") && name != "ps_quad_mask"
        }
        _ => false,
    }
}

fn packed_instance_size(shader: Shader, input: &[u8]) -> Result<usize> {
    if !is_quad_shader(shader) { return Ok(input.len()); }
    assert_eq!(input.len() % 16, 0);
    let mut size = input.len();
    for instance in input.chunks_exact(16) {
        let word = u32::from_ne_bytes(instance[8..12].try_into().unwrap());
        let part = (word >> 8) & 255;
        if (word >> 24) & 8 != 0 && (part == 1 || part == 3) {
            size = size.checked_add(((word >> 16) & 10).count_ones() as usize * 16)
                .ok_or("HAL instance size overflow")?;
        }
    }
    Ok(size)
}

// Match vertices at AA strip joins to prevent subpixel rasterization gaps.
fn pack_instances(shader: Shader, input: &[u8], output: &mut [u8]) {
    if !is_quad_shader(shader) {
        output.copy_from_slice(input);
        return;
    }
    let mut cursor = 0;
    for instance in input.chunks_exact(16) {
        let word = u32::from_ne_bytes(instance[8..12].try_into().unwrap());
        let part = (word >> 8) & 255;
        if (word >> 24) & 8 != 0 && (part == 1 || part == 3) {
            for replacement in [if part == 1 { 6 } else { 8 }, part, if part == 1 { 7 } else { 9 }] {
                if replacement != part {
                    let edge = if replacement == 6 || replacement == 8 { 2 } else { 8 };
                    if (word >> 16) & edge == 0 { continue; }
                }
                let dst = &mut output[cursor..cursor + 16];
                dst[..8].copy_from_slice(&instance[..8]);
                dst[8..12].copy_from_slice(&((word & !0xff00) | (replacement << 8)).to_ne_bytes());
                dst[12..].copy_from_slice(&instance[12..]);
                cursor += 16;
            }
        } else {
            output[cursor..cursor + 16].copy_from_slice(instance);
            cursor += 16;
        }
    }
    assert_eq!(cursor, output.len());
}

enum Instances<'a> {
    Borrowed(&'a [u8]),
    Owned(Vec<u8>),
}

impl Instances<'_> {
    fn owned(bytes: &[u8]) -> Self { Self::Owned(bytes.to_vec()) }
    fn bytes(&self) -> &[u8] {
        match self { Self::Borrowed(bytes) => bytes, Self::Owned(bytes) => bytes }
    }
}

struct ShaderMetadata {
    artifact: &'static ShaderArtifact,
    vertex: Vec<wgt::VertexAttribute>,
    instances: Vec<wgt::VertexAttribute>,
    stride: u64,
    layout_digest: u64,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct PipelineKey {
    device: u64,
    backend: wgt::Backend,
    abi: u32,
    filtering: Filtering,
    dual_source: bool,
    shader_input: ShaderInputMode,
    shader: Shader,
    blend: u8,
    depth: u8,
    format: wgt::TextureFormat,
    shader_digest: u64,
    vertex_layout: u64,
    samples: u32,
    depth_format: Option<wgt::TextureFormat>,
}

#[derive(Clone, Copy, Default, Debug)]
pub struct DrawStats {
    pub draw_calls: usize,
    pub wr_draw_calls: usize,
    pub native_passes: usize,
    pub primitive_instances: usize,
    /// Drawn RGB composite instances, including visible tile fragments.
    pub composite_tiles: usize,
    pub color_targets: usize,
    pub alpha_targets: usize,
}

pub struct FrameOutput {
    pub size: [u32; 2],
    pub pixels: Vec<u8>,
    pub stats: DrawStats,
}

pub(crate) struct RenderedFrame<A: hal::Api> {
    pub size: [u32; 2],
    pub origin: DeviceIntPoint,
    pub stats: DrawStats,
    pub serial: u64,
    texture: Option<Rc<Texture<A>>>,
}

pub(crate) struct PendingReadback<A: hal::Api> {
    buffer: Rc<Buffer<A>>,
    layout: ReadbackLayout,
    serial: u64,
    pub size: [u32; 2],
}

impl<A: hal::Api> PendingReadback<A> {
    pub fn bytes(&self) -> u64 { self.layout.size }
}

struct DrawTextures<A: hal::Api> {
    colors: [Rc<Texture<A>>; 3],
    clip: Rc<Texture<A>>,
}

struct ResolvedImage<A: hal::Api> {
    texture: Rc<Texture<A>>,
    uv: TexelRect,
    return_usage: wgt::TextureUses,
}

struct BoundTarget<A: hal::Api> {
    texture: Rc<Texture<A>>,
    origin: DeviceIntPoint,
    size: DeviceIntSize,
    return_usage: wgt::TextureUses,
}

struct Draw<'a, A: hal::Api> {
    shader: Shader,
    blend: u8,
    depth: u8,
    count: u32,
    instances: Instances<'a>,
    textures: DrawTextures<A>,
    filter: Option<TextureFilter>,
    clear_color: Option<ColorF>,
    count_in_stats: bool,
    readback: Option<crate::batch::InlineReadback>,
    scissor: DeviceIntRect,
}

struct Pipeline<A: hal::Api> {
    raw: Owned<A, A::RenderPipeline>,
    layout: Owned<A, A::PipelineLayout>,
    bindings: Owned<A, A::BindGroupLayout>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct DescriptorKey {
    pipeline: PipelineKey,
    uniform: u64,
    textures: SmallVec<[(u64, u32, u32, wgt::TextureFormat, u8); 16]>,
}

struct Descriptor<A: hal::Api> {
    raw: Owned<A, A::BindGroup>,
    _uniform: Rc<Buffer<A>>,
    _textures: Vec<Rc<Texture<A>>>,
    _pipeline: Rc<Pipeline<A>>,
}

pub(crate) struct FrameRenderer<A: BackendApi> {
    shader_input: ShaderInputMode,
    shader_cache: RefCell<ShaderCache>,
    shader_metadata: RefCell<HashMap<Shader, Rc<ShaderMetadata>>>,
    pub(crate) filtering: Filtering,
    #[cfg(test)]
    projection_override: Option<[f32; 16]>,
    owner: Rc<Device<A>>,
    textures: HashMap<CacheTextureId, Rc<Texture<A>>>,
    pipelines: HashMap<PipelineKey, Rc<Pipeline<A>>>,
    descriptors: RefCell<HashMap<DescriptorKey, Rc<Descriptor<A>>>>,
    samplers: [Owned<A, A::Sampler>; 3],
    quad: Rc<Buffer<A>>,
    submissions: Rc<SubmissionQueue<A>>,
    external_device: ExternalImageDevice,
    dummy: Rc<Texture<A>>,
    dither: Option<Rc<Texture<A>>>,
    depths: HashMap<(u64, u32), Rc<Texture<A>>>,
    texture_pool: super::pool::TexturePool<A>,
    data_textures: RefCell<HashMap<&'static str, Rc<Texture<A>>>>,
    uniforms: HashMap<[u32; 16], Rc<Buffer<A>>>,
    failed: Cell<bool>,
    external_provider: Option<Box<dyn ExternalImageProvider>>,
    external_images: HashMap<DeferredResolveIndex, ResolvedImage<A>>,
    releases: ReleaseQueue,
    compositor: CompositorConfig,
    native_targets: HashMap<NativeTileId, BoundTarget<A>>,
    native_operations: Vec<crate::composite::NativeSurfaceOperation>,
    native_sizes: HashMap<NativeTileId, DeviceIntSize>,
    layer_targets: Vec<BoundTarget<A>>,
    readback_pool: RefCell<Vec<Rc<Buffer<A>>>>,
    capture_pool: super::pool::TexturePool<A>,
    queries: RefCell<super::query::QueryPool<A>>,
    resource_upload_bytes: u64,
    surface: Option<super::surface::SurfaceState<A>>,
    presentation_pipelines: HashMap<wgt::TextureFormat, Rc<present::PresentationPipeline<A>>>,
    output_history: present_history::OutputHistory,
    force_full_present: bool,
    metrics: Option<std::sync::Arc<RenderMetrics>>,
}

impl<A: BackendApi> FrameRenderer<A> {
    pub fn is_failed(&self) -> bool { self.failed.get() || self.owner.lost.get() }

    #[cfg(any(test, feature = "hal-testing"))]
    pub fn inject_failure(&self, point: FailurePoint) { self.owner.fault.set(Some(point)); }

    fn abort(&mut self) {
        self.failed.set(true);
        if let Some(surface) = &mut self.surface { surface.image_versions.clear(); }
        self.submissions.discard_recording();
        self.external_images.clear();
        self.native_targets.clear();
        self.layer_targets.clear();
        dispatch_releases(&self.releases);
    }

    pub fn external_image_device(&self) -> ExternalImageDevice {
        self.external_device.clone()
    }

    pub fn new(device: Device<A>) -> Result<Self> {
        let shader_input = A::shader_input()?;
        println!("HAL shader input: {}", shader_input.name());
        let metrics = RenderMetrics::new(device.cache_id, true);
        let owner = Rc::new(device);
        let native = &owner.open.device;
        let sampler = |filter, mipmap| -> Result<_> {
            Ok(Owned::new(
                &owner,
                unsafe {
                    native.create_sampler(&hal::SamplerDescriptor {
                        label: Some("WR sampler"),
                        address_modes: [wgt::AddressMode::ClampToEdge; 3],
                        mag_filter: filter,
                        min_filter: filter,
                        mipmap_filter: mipmap,
                        lod_clamp: 0.0..if mipmap == wgt::MipmapFilterMode::Linear {
                            32.0
                        } else {
                            0.0
                        },
                        compare: None,
                        anisotropy_clamp: 1,
                        border_color: None,
                    })
                }
                .map_err(|e| format!("Creating sampler: {e:?}"))?,
                A::Device::destroy_sampler,
            ))
        };
        let samplers = [
            sampler(wgt::FilterMode::Nearest, wgt::MipmapFilterMode::Nearest)?,
            sampler(wgt::FilterMode::Linear, wgt::MipmapFilterMode::Nearest)?,
            sampler(wgt::FilterMode::Linear, wgt::MipmapFilterMode::Linear)?,
        ];
        let quad = Buffer::new(
            &owner,
            &[0, 0, 0, 0, 255, 0, 0, 0, 0, 255, 0, 0, 255, 255, 0, 0],
            wgt::BufferUses::VERTEX,
        )?;
        let dummy = Texture::new(
            &owner,
            1,
            1,
            wgt::TextureFormat::Rgba8Unorm,
            TextureFilter::Nearest,
            false,
        )?;
        let mut submissions =
            SubmissionQueue::new(&owner, 3, std::env::var_os("WR_HAL_SYNC").is_some());
        if owner.info.backend == wgt::Backend::Vulkan {
            submissions = submissions.with_wait_timeout(std::time::Duration::from_secs(5));
        }
        dummy.upload_recorded(
            &owner,
            &submissions,
            DeviceIntRect::from_size(DeviceIntSize::new(1, 1)),
            &[255; 4],
            None,
            0,
            None,
        )?;
        let texture_pool = super::pool::TexturePool::new(&owner);
        let capture_pool = super::pool::TexturePool::new(&owner);
        let queries = RefCell::new(super::query::QueryPool::new(&owner));
        let submissions = Rc::new(submissions);
        let releases = Rc::new(RefCell::new(Vec::new()));
        let external_device = ExternalImageDevice::for_renderer(&owner, &submissions, &releases);
        Ok(Self {
            shader_input,
            shader_cache: RefCell::new(ShaderCache::default()),
            shader_metadata: RefCell::new(HashMap::new()),
            filtering: Filtering::Standard,
            #[cfg(test)]
            projection_override: None,
            owner,
            textures: HashMap::new(),
            pipelines: HashMap::new(),
            descriptors: RefCell::new(HashMap::new()),
            samplers,
            quad,
            submissions,
            external_device,
            dummy,
            dither: None,
            depths: HashMap::new(),
            texture_pool,
            data_textures: RefCell::new(HashMap::new()),
            uniforms: HashMap::new(),
            failed: Cell::new(false),
            external_provider: None,
            external_images: HashMap::new(),
            releases,
            compositor: CompositorConfig::Draw,
            native_targets: HashMap::new(),
            native_operations: Vec::new(),
            native_sizes: HashMap::new(),
            layer_targets: Vec::new(),
            readback_pool: RefCell::new(Vec::new()),
            capture_pool,
            queries,
            resource_upload_bytes: 0,
            surface: None,
            presentation_pipelines: HashMap::new(),
            output_history: present_history::OutputHistory::default(),
            force_full_present: std::env::var("WR_HAL_FORCE_FULL_PRESENT").as_deref() == Ok("1"),
            metrics,
        })
    }

    pub(crate) fn metrics(&self) -> Option<std::sync::Arc<RenderMetrics>> { self.metrics.clone() }

    fn count(&self, counter: RenderCounter, amount: u64) {
        if let Some(metrics) = &self.metrics { metrics.add(counter, amount); }
    }

    pub fn render_metrics(&self) -> Option<(RenderMetricsSnapshot, RenderMetricsSnapshot)> {
        Some((self.metrics.as_ref()?.snapshot(), self.owner.metrics.as_ref()?.snapshot()))
    }

    pub fn has_acquired_surface(&self) -> bool {
        self.surface.as_ref().map_or(false, |surface| surface.acquired.is_some())
    }

    pub fn submit_work(&self) -> Result<u64> {
        if self.is_failed() { return Err("HAL renderer requires recreation".into()); }
        let result = self.submissions.submit_serial();
        if result.is_err() { self.failed.set(true); }
        result
    }

    pub fn has_pending_gpu_work(&self) -> bool {
        self.submissions.has_pending_work() || !self.releases.borrow().is_empty()
            || self.external_device.has_pending_gpu_work()
    }

    pub fn has_owned_output(&self, output: &RenderedFrame<A>) -> bool {
        matches!(self.compositor, CompositorConfig::Draw)
            && output.texture.as_ref().map_or(false, |texture| {
                texture.belongs_to(&self.owner) && texture.initialized()
            })
    }

    pub fn memory_stats(&self) -> MemoryStats {
        let mut stats = self.owner.memory.get();
        (stats.query_slots, stats.pending_queries) = self.queries.borrow().counts();
        self.submissions.memory(&mut stats);
        stats.cached_texture_bytes = self.texture_pool.bytes();
        stats.cached_texture_bytes += self.capture_pool.bytes();
        stats.cached_buffer_bytes += self.readback_pool.borrow().iter().map(|buffer| buffer.size).sum::<u64>();
        stats.pipelines = self.pipelines.len() + self.presentation_pipelines.len();
        stats.descriptors = self.descriptors.borrow().len();
        stats
    }

    pub fn trim_transient_resources(&mut self, uploads: bool) -> Result<()> {
        self.submissions.trim(uploads)?;
        self.descriptors.borrow_mut().clear();
        self.depths.clear();
        self.texture_pool.clear();
        self.capture_pool.clear();
        self.readback_pool.borrow_mut().clear();
        self.data_textures.borrow_mut().clear();
        self.uniforms.clear();
        self.queries.borrow_mut().trim();
        dispatch_releases(&self.releases);
        Ok(())
    }

    pub fn info(&self) -> &wgt::AdapterInfo {
        self.owner.info()
    }

    fn source(&self, source: TextureSource) -> Result<Rc<Texture<A>>> {
        match source {
            TextureSource::Invalid | TextureSource::Dummy => Ok(self.dummy.clone()),
            TextureSource::TextureCache(id, Swizzle::Rgba) => self
                .textures
                .get(&id)
                .cloned()
                .ok_or_else(|| format!("Missing HAL texture {id:?}")),
            TextureSource::External(source) => self.external_images.get(&source.index)
                .map(|image| image.texture.clone()).ok_or_else(|| "Missing HAL external image resolution".into()),
            _ => Err(format!("Unsupported HAL texture source {source:?}")),
        }
    }

    pub fn set_external_image_provider(&mut self, provider: Box<dyn ExternalImageProvider>) {
        self.external_provider = Some(provider);
    }

    pub fn set_compositor(&mut self, compositor: CompositorConfig) { self.compositor = compositor; }

    fn acquired_target(&self, target: CompositorTarget, size: DeviceIntSize) -> Result<BoundTarget<A>> {
        target.image.attach_releases(&self.releases);
        let image = match &target.image.source {
            ExternalImageSource::Native(image) => image,
            _ => return Err("Compositor target must be a native image".into()),
        };
        let texture = image.texture(&self.owner)?;
        if texture.target.is_none() || !matches!(texture.format, wgt::TextureFormat::Rgba8Unorm | wgt::TextureFormat::Bgra8Unorm) {
            return Err("Compositor target must support RGBA8/BGRA8 rendering".into());
        }
        if size.is_empty() || target.size != size || target.origin.x < 0 || target.origin.y < 0
            || target.origin.x.checked_add(size.width).map_or(true, |end| end as u32 > texture.size.width)
            || target.origin.y.checked_add(size.height).map_or(true, |end| end as u32 > texture.size.height) {
            return Err("Compositor target origin/extent exceeds its image".into());
        }
        let return_usage = match texture.current_usage() {
            wgt::TextureUses::UNINITIALIZED => wgt::TextureUses::RESOURCE,
            usage => usage,
        };
        let texture = texture.with_lease(target.image.state.clone(), TextureFilter::Linear, false)?;
        Ok(BoundTarget { texture, origin: target.origin, size, return_usage })
    }

    fn bind_native_tile(&mut self, id: NativeTileId, size: DeviceIntSize, dirty: DeviceIntRect, valid: DeviceIntRect) -> Result<()> {
        let full = DeviceIntRect::from_size(size);
        if !full.contains_box(&dirty) || !full.contains_box(&valid) { return Err("Invalid compositor tile update region".into()); }
        let target = match &mut self.compositor {
            CompositorConfig::Native { compositor, .. } => compositor.bind_tile(id, dirty, valid)?,
            _ => return Err("Native tile requires a native compositor".into()),
        };
        let target = self.acquired_target(target, size)?;
        self.native_sizes.insert(id, size);
        self.native_targets.insert(id, target);
        Ok(())
    }

    fn acquire_composite_tiles(&mut self, frame: &Frame) -> Result<()> {
        for tile in &frame.composite_state.tiles {
            if let CompositeTileSurface::Texture { surface: ResolvedSurfaceTexture::Native { id, size } } = tile.surface {
                if !self.native_targets.contains_key(&id) {
                    let target = match &mut self.compositor {
                        CompositorConfig::Native { compositor, .. } => compositor.read_tile(id)?,
                        _ => return Err("Native tile requires a native compositor".into()),
                    };
                    let target = self.acquired_target(target, size)?;
                    self.native_targets.insert(id, target);
                }
            }
        }
        if matches!(self.compositor, CompositorConfig::Native { .. }) {
            for surface in &frame.composite_state.external_surfaces {
                if surface.external_image_id.is_some() { continue; }
                if let Some(surface_id) = surface.native_surface_id {
                    let id = NativeTileId { surface_id, x: 0, y: 0 };
                    if !self.native_targets.contains_key(&id) {
                        let target = match &mut self.compositor {
                            CompositorConfig::Native { compositor, .. } => compositor.read_tile(id)?,
                            _ => unreachable!(),
                        };
                        let size = target.size;
                        let target = self.acquired_target(target, size)?;
                        self.native_targets.insert(id, target);
                    }
                }
            }
        }
        Ok(())
    }

    fn external_uv(&self, source: TextureSource, fallback: TexelRect) -> Result<TexelRect> {
        match source {
            TextureSource::External(source) => self.external_images.get(&source.index)
                .map(|image| image.uv).ok_or_else(|| "Missing external compositor image".into()),
            _ => Ok(fallback),
        }
    }

    fn external_composite(&self, surface: &ResolvedExternalSurface, rect: DeviceRect, clip_rect: DeviceRect,
                          flip: (bool, bool), clip: Option<&CompositorClip>) -> Result<(CompositeInstance, DrawTextures<A>, Shader)> {
        Ok(match &surface.color_data {
            ResolvedExternalSurfaceColorData::Rgb { plane, .. } => (
                CompositeInstance::new_rgb(rect, clip_rect, PremultipliedColorF::WHITE,
                    self.external_uv(plane.texture, plane.uv_rect)?, false, flip, clip),
                self.single_texture(self.source(plane.texture)?), Shader::Composite,
            ),
            ResolvedExternalSurfaceColorData::Yuv { planes, color_space, format, channel_bit_depth, .. } => (
                CompositeInstance::new_yuv(rect, clip_rect, *color_space, *format, *channel_bit_depth,
                    [self.external_uv(planes[0].texture, planes[0].uv_rect)?,
                     self.external_uv(planes[1].texture, planes[1].uv_rect)?,
                     self.external_uv(planes[2].texture, planes[2].uv_rect)?], flip, clip),
                self.batch_textures(&BatchTextures::composite_yuv(planes[0].texture, planes[1].texture, planes[2].texture))?,
                Shader::Other("composite", "TEXTURE_2D,YUV"),
            ),
        })
    }

    fn update_native_external_surfaces(&mut self, frame: &Frame, data: &HashMap<&'static str, Rc<Texture<A>>>, stats: &mut DrawStats) -> Result<()> {
        if !matches!(self.compositor, CompositorConfig::Native { .. }) { return Ok(()); }
        for surface in &frame.composite_state.external_surfaces {
            let Some((surface_id, size)) = surface.update_params else { continue; };
            let rect = DeviceIntRect::from_size(size);
            let id = NativeTileId { surface_id, x: 0, y: 0 };
            self.bind_native_tile(id, size, rect, rect)?;
            let (instance, textures, shader) = self.external_composite(surface, rect.to_f32(), rect.to_f32(), (false, false), None)?;
            let target = &self.native_targets[&id];
            let texture = target.texture.clone();
            let origin = DeviceIntPoint::new(-target.origin.x, -target.origin.y);
            let draw = Draw { shader, blend: 0, depth: 0, count: 1, instances: Instances::owned(bytes(&[instance])),
                textures, filter: None, clear_color: None, count_in_stats: true, readback: None, scissor: rect };
            self.draw_pass_at(&texture, &[self.clear(rect, ColorF::TRANSPARENT), draw], data, stats, origin)?;
            stats.color_targets += 1;
        }
        Ok(())
    }

    pub fn end_compositor_frame(&mut self, frame: &Frame, completion: crate::renderer::hal::FrameCompletion) -> Result<()> {
        let result = match &mut self.compositor {
            CompositorConfig::Draw => Ok(()),
            CompositorConfig::Native { compositor, .. } => compositor.end_frame(&frame.composite_state.descriptor, completion),
            CompositorConfig::Layer { compositor } => compositor.end_frame(completion),
        };
        if result.is_err() { self.failed.set(true); }
        result
    }

    pub fn poll(&self) -> Result<()> { self.poll_completed().map(|_| ()) }

    pub fn poll_completed(&self) -> Result<u64> {
        self.count(RenderCounter::Polls, 1);
        if self.is_failed() {
            let _ = self.submissions.poll();
            dispatch_releases(&self.releases);
            return Err("HAL renderer requires recreation".into());
        }
        let result = self.submissions.poll().and_then(|completed| {
            self.queries.borrow_mut().poll(completed).map(|_| completed)
        });
        dispatch_releases(&self.releases);
        let result = result.and_then(|completed| self.external_device.poll().map(|_| completed));
        if result.is_err() { self.failed.set(true); }
        if let Some(metrics) = &self.owner.metrics {
            let memory = self.owner.memory.get();
            metrics.set(RenderGauge::TextureBytes, memory.texture_bytes);
            metrics.set(RenderGauge::BufferBytes, memory.buffer_bytes);
            metrics.report_if_due();
        }
        if let Some(metrics) = &self.metrics {
            metrics.set(RenderGauge::InitializedSurfaceImages,
                self.surface.as_ref().map_or(0, |surface| surface.image_versions.len()) as u64);
            metrics.report_if_due();
        }
        result
    }

    pub fn capabilities(&self) -> RendererCapabilities {
        RendererCapabilities {
            backend: self.owner.info.backend,
            shader_input: self.shader_input.name(),
            frame_timestamps: self.queries.borrow().supported(),
            validation_request_supported: self.owner.info.backend == wgt::Backend::Vulkan,
            validation_requested: self.owner.validation_requested,
            dual_source_blending: self.owner.supports_dual_source_blending(),
            max_texture_size: self.owner.max_texture_size(),
        }
    }

    pub fn configure_timestamps(&self, bits: u32) { self.queries.borrow_mut().configure(bits); }
    pub fn take_resource_upload_bytes(&mut self) -> u64 { std::mem::replace(&mut self.resource_upload_bytes, 0) }
    pub fn enable_gpu_profiling(&self, enabled: bool) -> bool { self.queries.borrow_mut().enable(enabled) }
    pub fn take_gpu_timings(&self) -> Result<Vec<(u64, f64)>> {
        self.poll()?;
        Ok(self.queries.borrow_mut().take())
    }

    fn acquire_external(&mut self, id: api::ExternalImageId, channel: u8, composited: bool) -> Result<ExternalImageLease> {
        let lease = self.external_provider.as_mut().ok_or("No HAL external-image provider is installed")?
            .acquire(id, channel, composited)?;
        lease.attach_releases(&self.releases);
        lease.attach_metrics(self.owner.metrics.as_ref());
        Ok(lease)
    }

    fn resolve_external_images(&mut self, frame: &mut Frame) -> Result<()> {
        for (index, resolve) in frame.deferred_resolves.iter().enumerate() {
            let props = &resolve.image_properties;
            let external = props.external_image.ok_or("Deferred image has no external descriptor")?;
            if !matches!(external.image_type, api::ExternalImageType::TextureHandle(ImageBufferKind::Texture2D | ImageBufferKind::TextureRect)) {
                return Err("HAL external sampler type requires a platform adapter".into());
            }
            let lease = self.acquire_external(external.id, external.channel_index, resolve.is_composited)?;
            let native = match &lease.source {
                ExternalImageSource::Native(native) => native,
                _ => return Err("Deferred external image requires a native texture".into()),
            };
            let texture = native.texture(&self.owner)?;
            if !texture.sample_initialized() { return Err("External image contents are not initialized".into()); }
            let filter = if resolve.rendering == api::ImageRendering::Pixelated { TextureFilter::Nearest } else { TextureFilter::Linear };
            let return_usage = texture.current_usage();
            let texture = texture.with_lease(lease.state.clone(), filter,
                props.descriptor.flags.contains(api::ImageDescriptorFlags::IS_OPAQUE))?;
            let mut uv = lease.uv.to_array();
            if external.normalized_uvs {
                if props.descriptor.size.is_empty() { return Err("Invalid normalized external image size".into()); }
                uv[0] /= props.descriptor.size.width as f32;
                uv[2] /= props.descriptor.size.width as f32;
                uv[1] /= props.descriptor.size.height as f32;
                uv[3] /= props.descriptor.size.height as f32;
            }
            let address = frame.gpu_buffer_f.resolve_handle(resolve.handle).as_u32() as usize;
            if address.checked_add(1).map_or(true, |end| end >= frame.gpu_buffer_f.data.len()) {
                return Err("Invalid deferred external image address".into());
            }
            frame.gpu_buffer_f.data[address] = uv.into();
            frame.gpu_buffer_f.data[address + 1] = [0.0; 4].into();
            self.external_images.insert(DeferredResolveIndex(index as u32), ResolvedImage { texture, uv: lease.uv, return_usage });
        }
        frame.gpu_buffer_f.apply_deferred_uv_copies();
        Ok(())
    }

    fn restore_external_images(&self) -> Result<()> {
        for image in self.external_images.values() {
            if image.texture.current_usage() != image.return_usage {
                let mut commands = self.submissions.recording()?;
                image.texture.transition(&mut commands, image.return_usage);
            }
        }
        for target in self.native_targets.values().chain(self.layer_targets.iter()) {
            if target.texture.current_usage() != target.return_usage {
                let mut commands = self.submissions.recording()?;
                target.texture.transition(&mut commands, target.return_usage);
            }
        }
        Ok(())
    }

    pub fn enable_dithering(&mut self) -> Result<()> {
        let matrix: [u8; 64] = [
            0, 48, 12, 60, 3, 51, 15, 63, 32, 16, 44, 28, 35, 19, 47, 31, 8, 56, 4, 52, 11, 59, 7,
            55, 40, 24, 36, 20, 43, 27, 39, 23, 2, 50, 14, 62, 1, 49, 13, 61, 34, 18, 46, 30, 33,
            17, 45, 29, 10, 58, 6, 54, 9, 57, 5, 53, 42, 26, 38, 22, 41, 25, 37, 21,
        ];
        let texture = Texture::new(
            &self.owner,
            8,
            8,
            wgt::TextureFormat::R8Unorm,
            TextureFilter::Nearest,
            false,
        )?;
        texture.upload_recorded(
            &self.owner,
            &self.submissions,
            DeviceIntRect::from_size(DeviceIntSize::new(8, 8)),
            &matrix,
            None,
            0,
            None,
        )?;
        self.dither = Some(texture);
        Ok(())
    }

    fn quad_shader(&self, pattern: PatternKind) -> Result<Shader> {
        Ok(match pattern {
            PatternKind::ColorOrTexture | PatternKind::TextureRect => Shader::Quad,
            PatternKind::Gradient => Shader::Other(
                "ps_quad_gradient",
                if self.dither.is_some() {
                    "DITHERING"
                } else {
                    ""
                },
            ),
            PatternKind::Repeat => Shader::Other("ps_quad_repeat", ""),
            PatternKind::Blend => Shader::Other("ps_quad_blend", "TEXTURE_2D"),
            PatternKind::Yuv | PatternKind::YuvTextureRect => Shader::Other("ps_quad_yuv", "TEXTURE_2D"),
            PatternKind::Backdrop => Shader::Other("ps_quad_backdrop", "TEXTURE_2D"),
            PatternKind::MixBlend => Shader::Other("ps_quad_mix_blend", "TEXTURE_2D"),
            PatternKind::BoxShadow => Shader::Other("ps_quad_box_shadow", ""),
            PatternKind::BoxShadowSuperellipse => {
                Shader::Other("ps_quad_box_shadow", "SUPERELLIPSE")
            }
            _ => return Err(format!("Unsupported HAL pattern {pattern:?}")),
        })
    }

    fn single_texture(&self, source: Rc<Texture<A>>) -> DrawTextures<A> {
        DrawTextures {
            colors: [source, self.dummy.clone(), self.dummy.clone()],
            clip: self.dummy.clone(),
        }
    }

    fn batch_textures(&self, textures: &BatchTextures) -> Result<DrawTextures<A>> {
        Ok(DrawTextures {
            colors: [
                self.source(textures.input.colors[0])?,
                self.source(textures.input.colors[1])?,
                self.source(textures.input.colors[2])?,
            ],
            clip: self.source(textures.clip_mask)?,
        })
    }

    fn task_draw<'a, T: GpuData>(
        &self,
        shader: Shader,
        blend: u8,
        instances: &'a [T],
        textures: &BatchTextures,
        scissor: DeviceIntRect,
    ) -> Result<Draw<'a, A>> {
        let input = bytes(instances);
        let size = packed_instance_size(shader, input)?;
        Ok(Draw {
            shader,
            blend,
            depth: 0,
            count: u32::try_from(size / T::SIZE).map_err(|_| "Too many HAL instances")?,
            instances: Instances::Borrowed(input),
            textures: self.batch_textures(textures)?,
            filter: None,
            clear_color: None,
            count_in_stats: true,
            readback: None,
            scissor,
        })
    }

    fn surface(&self, surface: &ResolvedSurfaceTexture) -> Result<Rc<Texture<A>>> {
        match *surface {
            ResolvedSurfaceTexture::TextureCache { texture } => self.source(texture),
            ResolvedSurfaceTexture::Native { id, .. } => self.native_targets.get(&id)
                .map(|target| target.texture.clone()).ok_or_else(|| "Native target is not acquired".into()),
        }
    }

    fn track_native_operations(&mut self, operations: &[crate::composite::NativeSurfaceOperation]) {
        use crate::composite::NativeSurfaceOperationDetails as Op;
        for operation in operations {
            match operation.details {
                Op::DestroySurface { id } => {
                    self.native_sizes.retain(|tile, _| tile.surface_id != id);
                    self.native_operations.retain(|op| match op.details {
                        Op::CreateSurface { id: surface, .. } | Op::CreateExternalSurface { id: surface, .. }
                        | Op::CreateBackdropSurface { id: surface, .. } | Op::AttachExternalImage { id: surface, .. } => surface != id,
                        Op::CreateTile { id: tile } => tile.surface_id != id,
                        _ => false,
                    });
                }
                Op::DestroyTile { id } => {
                    self.native_sizes.remove(&id);
                    self.native_operations.retain(|op| !matches!(op.details, Op::CreateTile { id: tile } if tile == id));
                }
                Op::AttachExternalImage { id, .. } => {
                    self.native_operations.retain(|op| !matches!(op.details, Op::AttachExternalImage { id: surface, .. } if surface == id));
                    self.native_operations.push(operation.clone());
                }
                _ => self.native_operations.push(operation.clone()),
            }
        }
    }

    fn update(&mut self, updates: ResourceUpdateList) -> Result<()> {
        if !updates.native_surface_updates.is_empty() {
            let device = self.external_image_device();
            match &mut self.compositor {
                CompositorConfig::Native { compositor, .. } => compositor.update_surfaces(&device, &updates.native_surface_updates)?,
                _ => return Err("Native surface updates require a native compositor".into()),
            }
            self.track_native_operations(&updates.native_surface_updates);
        }
        let updates = updates.texture_updates;
        if !updates.allocations.is_empty() {
            self.descriptors.borrow_mut().clear();
        }
        for ((src, dst), copies) in updates.copies {
            let source = self
                .textures
                .get(&src)
                .cloned()
                .ok_or("Missing HAL copy source")?;
            let destination = self
                .textures
                .get(&dst)
                .cloned()
                .ok_or("Missing HAL copy destination")?;
            for copy in copies {
                self.copy(&source, &destination, copy.src_rect, copy.dst_rect)?;
            }
            self.generate_mips(&destination)?;
        }
        for allocation in updates.allocations {
            match allocation.kind {
                TextureCacheAllocationKind::Alloc(info)
                | TextureCacheAllocationKind::Reset(info) => {
                    if info.target != ImageBufferKind::Texture2D {
                        return Err("Unsupported HAL texture target".into());
                    }
                    let texture = Texture::new(
                        &self.owner,
                        info.width as u32,
                        info.height as u32,
                        texture_format(info.format)?,
                        info.filter,
                        matches!(
                            info.format,
                            api::ImageFormat::RGBA8
                                | api::ImageFormat::BGRA8
                                | api::ImageFormat::R8
                        ),
                    )?;
                    self.textures.insert(allocation.id, texture);
                }
                TextureCacheAllocationKind::Free => {
                    self.textures.remove(&allocation.id);
                }
            }
        }
        for (id, updates) in updates.updates {
            let texture = self
                .textures
                .get(&id)
                .cloned()
                .ok_or("Updating unknown HAL texture")?;
            for update in updates {
                let uploaded = !matches!(update.source, TextureUpdateSource::DebugClear);
                let upload_bytes = update.rect.width() as u64 * update.rect.height() as u64
                    * super::resources::bytes_per_pixel(texture.format) as u64;
                match update.source {
                    TextureUpdateSource::Bytes { data } => texture.upload_recorded(
                        &self.owner,
                        &self.submissions,
                        update.rect,
                        &data,
                        update.stride,
                        update.offset,
                        update.format_override,
                    )?,
                    TextureUpdateSource::DebugClear => {
                        let c = crate::renderer::TEXTURE_CACHE_DBG_CLEAR_COLOR;
                        let draw = self.clear(update.rect, ColorF::new(c[0], c[1], c[2], c[3]));
                        self.draw_pass(
                            &texture,
                            &[draw],
                            &HashMap::new(),
                            &mut DrawStats::default(),
                        )?;
                    }
                    TextureUpdateSource::External { id, channel_index } => {
                        let owner = &self.owner;
                        let queue = &self.submissions;
                        drop(queue.recording()?);
                        let provider = self.external_provider.as_mut().ok_or("No HAL external-image provider is installed")?;
                        let (rect, stride, offset, format) = (update.rect, update.stride, update.offset, update.format_override);
                        super::external::upload_buffer(provider.as_mut(), id, channel_index, &self.releases, owner.metrics.as_ref(),
                            &mut |descriptor, data, opaque| {
                                texture.upload_recorded_with_alpha(owner, queue, rect, data, stride, offset,
                                    format.or(Some(descriptor.format)), opaque.then_some(descriptor))
                            })?;
                    }
                }
                if uploaded {
                    self.resource_upload_bytes += upload_bytes;
                    if let Some(metrics) = &self.owner.metrics {
                        metrics.add(RenderCounter::ResourceUploads, 1);
                        metrics.add(RenderCounter::ResourceUploadBytes, upload_bytes);
                    }
                }
            }
            self.generate_mips(&texture)?;
        }
        Ok(())
    }

    fn record_blit(
        &mut self,
        src: &Rc<Texture<A>>,
        dst: &Rc<Texture<A>>,
        src_rect: DeviceIntRect,
        dst_rect: DeviceIntRect,
        filter: TextureFilter,
        stats: &mut DrawStats,
    ) -> Result<()> {
        let Some(draw) = self.blit_draw(src, dst, src_rect, dst_rect, filter)? else {
            return Ok(());
        };
        if src.overlaps(dst) {
            let scratch =
                self.texture_pool
                    .acquire(src.size.width, src.size.height, src.format, false)?;
            {
                let mut commands = self.submissions.recording()?;
                scratch.invalidate(&mut commands);
            }
            let full = DeviceIntRect::from_size(DeviceIntSize::new(
                src.size.width as i32,
                src.size.height as i32,
            ));
            self.copy_native(src, &scratch, full, full)?;
            return self.record_blit(&scratch, dst, src_rect, dst_rect, filter, stats);
        }
        self.draw_pass(dst, &[draw], &HashMap::new(), stats)
    }

    fn queue_blit<'a>(
        &mut self,
        src: &Rc<Texture<A>>,
        dst: &Rc<Texture<A>>,
        src_rect: DeviceIntRect,
        dst_rect: DeviceIntRect,
        filter: TextureFilter,
        draws: &mut Vec<Draw<'a, A>>,
        stats: &mut DrawStats,
    ) -> Result<()> {
        if src.overlaps(dst) {
            self.draw_pass(dst, draws, &HashMap::new(), stats)?;
            draws.clear();
            self.record_blit(src, dst, src_rect, dst_rect, filter, stats)
        } else {
            if let Some(draw) = self.blit_draw(src, dst, src_rect, dst_rect, filter)? {
                draws.push(draw);
            }
            Ok(())
        }
    }

    fn blit_draw(
        &self,
        src: &Rc<Texture<A>>,
        dst: &Rc<Texture<A>>,
        src_rect: DeviceIntRect,
        dst_rect: DeviceIntRect,
        filter: TextureFilter,
    ) -> Result<Option<Draw<'static, A>>> {
        if !src.initialized() {
            return Err("Sampling uninitialized HAL blit source".into());
        }
        for format in [src.format, dst.format] {
            if !matches!(
                format,
                wgt::TextureFormat::Rgba8Unorm
                    | wgt::TextureFormat::Bgra8Unorm
                    | wgt::TextureFormat::R8Unorm
            ) {
                return Err(format!("Unsupported HAL blit conversion for {format:?}"));
            }
        }
        let mut source_rect = src_rect.to_f32();
        let mut target_rect = dst_rect.to_f32();
        if source_rect.is_empty() || target_rect.is_empty() {
            return Ok(None);
        }
        let clip = |s0: f32, s1: f32, d0: f32, d1: f32, sw: f32, dw: f32| {
            let lo = 0.0f32.max(-s0 / (s1 - s0)).max(-d0 / (d1 - d0));
            let hi = 1.0f32.min((sw - s0) / (s1 - s0)).min((dw - d0) / (d1 - d0));
            (
                s0 + lo * (s1 - s0),
                s0 + hi * (s1 - s0),
                d0 + lo * (d1 - d0),
                d0 + hi * (d1 - d0),
            )
        };
        (
            source_rect.min.x,
            source_rect.max.x,
            target_rect.min.x,
            target_rect.max.x,
        ) = clip(
            source_rect.min.x,
            source_rect.max.x,
            target_rect.min.x,
            target_rect.max.x,
            src.size.width as f32,
            dst.size.width as f32,
        );
        (
            source_rect.min.y,
            source_rect.max.y,
            target_rect.min.y,
            target_rect.max.y,
        ) = clip(
            source_rect.min.y,
            source_rect.max.y,
            target_rect.min.y,
            target_rect.max.y,
            src.size.height as f32,
            dst.size.height as f32,
        );
        if source_rect.is_empty() || target_rect.is_empty() {
            return Ok(None);
        }
        let instance = ScalingInstance::new(target_rect, source_rect, false);

        let draw = Draw {
            shader: Shader::Other("cs_scale", "TEXTURE_2D"),
            blend: 0,
            depth: 0,
            count: 1,
            instances: Instances::owned(bytes(&[instance])),
            textures: self.single_texture(src.clone()),
            filter: Some(filter),
            clear_color: None,
            count_in_stats: false,
            readback: None,
            scissor: dst_rect,
        };
        Ok(Some(draw))
    }

    fn generate_mips(&mut self, texture: &Rc<Texture<A>>) -> Result<()> {
        for level in 1..texture.mip_count {
            let source = texture.mip_view(level - 1)?;
            let target = texture.mip_view(level)?;
            self.record_blit(
                &source,
                &target,
                DeviceIntRect::from_size(DeviceIntSize::new(
                    source.size.width as i32,
                    source.size.height as i32,
                )),
                DeviceIntRect::from_size(DeviceIntSize::new(
                    target.size.width as i32,
                    target.size.height as i32,
                )),
                TextureFilter::Linear,
                &mut DrawStats::default(),
            )?;
        }
        Ok(())
    }

    fn copy(
        &mut self,
        src: &Rc<Texture<A>>,
        dst: &Rc<Texture<A>>,
        src_rect: DeviceIntRect,
        dst_rect: DeviceIntRect,
    ) -> Result<()> {
        if Rc::ptr_eq(&src.raw, &dst.raw) && src.base_mip == dst.base_mip {
            let scratch = self.texture_pool.acquire(
                src_rect.width() as u32,
                src_rect.height() as u32,
                src.format,
                false,
            )?;
            {
                let mut commands = self.submissions.recording()?;
                scratch.invalidate(&mut commands);
            }
            let rect = DeviceIntRect::from_size(src_rect.size());
            self.copy_native(src, &scratch, src_rect, rect)?;
            return self.copy(&scratch, dst, rect, dst_rect);
        }
        if src.format == dst.format && src_rect.size() == dst_rect.size() {
            self.copy_native(src, dst, src_rect, dst_rect)
        } else {
            self.record_blit(
                src,
                dst,
                src_rect,
                dst_rect,
                TextureFilter::Nearest,
                &mut DrawStats::default(),
            )
        }
    }

    fn copy_native(
        &self,
        src: &Rc<Texture<A>>,
        dst: &Rc<Texture<A>>,
        src_rect: DeviceIntRect,
        dst_rect: DeviceIntRect,
    ) -> Result<()> {
        if dst.copy_aspect() != hal::FormatAspects::COLOR {
            return Err("Cannot copy into a foreign video plane".into());
        }
        if src.format != dst.format || src_rect.size() != dst_rect.size() || Rc::ptr_eq(src, dst) {
            return Err("Unsupported HAL texture copy".into());
        }
        for (texture, rect) in [(src, src_rect), (dst, dst_rect)] {
            if rect.min.x < 0
                || rect.min.y < 0
                || rect.max.x as u32 > texture.size.width
                || rect.max.y as u32 > texture.size.height
                || rect.is_empty()
            {
                return Err("Invalid HAL texture copy bounds".into());
            }
        }
        if !src.initialized() {
            return Err("Copying uninitialized HAL texture contents".into());
        }
        if !dst.initialized()
            && dst_rect
                != DeviceIntRect::from_size(DeviceIntSize::new(
                    dst.size.width as i32,
                    dst.size.height as i32,
                ))
        {
            let size = (dst.size.width as usize)
                .checked_mul(dst.size.height as usize)
                .and_then(|n| n.checked_mul(super::resources::bytes_per_pixel(dst.format)))
                .ok_or("HAL initialization size overflow")?;
            if size as u64 > self.owner.capabilities.limits.max_buffer_size
                || size > isize::MAX as usize
            {
                return Err("HAL initialization exceeds buffer limits".into());
            }
            dst.upload_recorded(
                &self.owner,
                &self.submissions,
                DeviceIntRect::from_size(DeviceIntSize::new(
                    dst.size.width as i32,
                    dst.size.height as i32,
                )),
                &vec![0; size],
                None,
                0,
                None,
            )?;
        }
        let base = |rect: DeviceIntRect, mip_level, aspect| hal::TextureCopyBase {
            mip_level,
            array_layer: 0,
            origin: wgt::Origin3d {
                x: rect.min.x as u32,
                y: rect.min.y as u32,
                z: 0,
            },
            aspect,
        };
        let mut commands = self.submissions.recording()?;
        src.transition(&mut commands, wgt::TextureUses::COPY_SRC);
        dst.transition(&mut commands, wgt::TextureUses::COPY_DST);
        unsafe {
            commands.encoder().copy_texture_to_texture(
                &src.raw,
                wgt::TextureUses::COPY_SRC,
                &dst.raw,
                std::iter::once(hal::TextureCopy {
                    src_base: base(src_rect, src.base_mip, src.copy_aspect()),
                    dst_base: base(dst_rect, dst.base_mip, dst.copy_aspect()),
                    size: wgt::Extent3d {
                        width: src_rect.width() as u32,
                        height: src_rect.height() as u32,
                        depth_or_array_layers: 1,
                    }
                    .into(),
                }),
            );
        }
        src.transition(&mut commands, wgt::TextureUses::RESOURCE);
        dst.transition(&mut commands, wgt::TextureUses::RESOURCE);
        dst.initialize(&mut commands);
        Ok(())
    }

    fn data_texture<T: GpuData>(
        &self,
        name: &'static str,
        values: &[T],
        format: wgt::TextureFormat,
    ) -> Result<Rc<Texture<A>>> {
        let source = bytes(values);
        let width = MAX_VERTEX_TEXTURE_WIDTH;
        let height = source.len().div_ceil(width * 16).max(1);
        let height_u32 = u32::try_from(height).map_err(|_| "HAL data texture height overflow")?;
        self.owner.layout(width as u32, height_u32)?;
        let size = width
            .checked_mul(height)
            .and_then(|n| n.checked_mul(16))
            .ok_or("HAL data texture size overflow")?;
        if size > isize::MAX as usize
            || size as u64 > self.owner.capabilities.limits.max_buffer_size
        {
            return Err("HAL data texture exceeds buffer limits".into());
        }
        let mut cache = self.data_textures.borrow_mut();
        let texture = match cache.get(name) {
            Some(texture) if texture.size.height >= height_u32 && texture.format == format => {
                texture.clone()
            }
            _ => {
                let texture = Texture::new(
                    &self.owner,
                    width as u32,
                    height_u32.next_power_of_two(),
                    format,
                    TextureFilter::Nearest,
                    false,
                )?;
                self.descriptors.borrow_mut().clear();
                if u64::from(texture.size.width) * u64::from(texture.size.height) * 16
                    <= 16 * 1024 * 1024
                {
                    cache.insert(name, texture.clone());
                }
                texture
            }
        };
        drop(cache);
        texture.upload_zero_padded(
            &self.owner,
            &self.submissions,
            DeviceIntRect::from_size(DeviceIntSize::new(width as i32, height as i32)),
            source,
        )?;
        Ok(texture)
    }

    fn artifact(shader: Shader) -> &'static ShaderArtifact {
        let (name, features) = match shader {
            Shader::Quad => ("ps_quad_textured", "TEXTURE_2D"),
            Shader::Composite => ("composite", "TEXTURE_2D"),
            Shader::Clear => ("ps_clear", ""),
            Shader::Other(name, features) => (name, features),
            Shader::LegacyBrilinear(name, features) => (name, features),
        };
        shaders::SHADERS
            .iter()
            .find(|entry| entry.name == name && if matches!(shader, Shader::LegacyBrilinear(..)) {
                (features.is_empty() && entry.features == "HAL_LEGACY_BRILINEAR")
                    || entry.features.strip_suffix(",HAL_LEGACY_BRILINEAR") == Some(features)
            } else { entry.features == features })
            .unwrap()
    }

    fn descriptor(shader: Shader) -> &'static VertexDescriptor {
        match Self::artifact(shader).name {
            "cs_blur" => &desc::BLUR,
            "cs_scale" => &desc::SCALE,
            "cs_line_decoration" => &desc::LINE,
            "cs_border_segment" | "cs_border_solid" => &desc::BORDER,
            "cs_svg_filter_node" => &desc::SVG_FILTER_NODE,
            "ps_quad_mask" => &desc::MASK,
            "composite" => &desc::COMPOSITE,
            "ps_clear" => &desc::CLEAR,
            "ps_copy" => &desc::COPY,
            _ => &desc::PRIM_INSTANCES,
        }
    }

    fn shader_metadata(&self, shader: Shader) -> Result<Rc<ShaderMetadata>> {
        if let Some(metadata) = self.shader_metadata.borrow().get(&shader) {
            return Ok(metadata.clone());
        }
        use std::hash::{Hash, Hasher};
        let artifact = Self::artifact(shader);
        let layouts = vertex_layouts(Self::descriptor(shader), artifact)?;
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        layouts.hash(&mut hash);
        let metadata = Rc::new(ShaderMetadata {
            artifact,
            vertex: layouts.0,
            instances: layouts.1,
            stride: layouts.2,
            layout_digest: hash.finish(),
        });
        self.shader_metadata.borrow_mut().insert(shader, metadata.clone());
        Ok(metadata)
    }

    fn key(
        &self,
        shader: Shader,
        blend: u8,
        depth: u8,
        format: wgt::TextureFormat,
    ) -> Result<PipelineKey> {
        let metadata = self.shader_metadata(shader)?;
        Ok(self.key_with_metadata(shader, &metadata, blend, depth, format))
    }

    fn key_with_metadata(
        &self,
        shader: Shader,
        metadata: &ShaderMetadata,
        blend: u8,
        depth: u8,
        format: wgt::TextureFormat,
    ) -> PipelineKey {
        PipelineKey {
            device: self.owner.cache_id,
            backend: self.owner.info.backend,
            abi: PIPELINE_ABI,
            filtering: self.filtering,
            dual_source: self.owner.supports_dual_source_blending(),
            shader_input: self.shader_input,
            shader,
            blend,
            depth,
            format,
            shader_digest: metadata.artifact.digest,
            vertex_layout: metadata.layout_digest,
            samples: 1,
            depth_format: if depth == 0 {
                None
            } else {
                Some(wgt::TextureFormat::Depth32Float)
            },
        }
    }

    fn layouts(
        owner: &Rc<Device<A>>,
        artifact: &ShaderArtifact,
    ) -> Result<(Owned<A, A::PipelineLayout>, Owned<A, A::BindGroupLayout>)> {
        let native = &owner.open.device;
        let mut entries = Vec::new();
        if artifact.projection_stages != 0 {
            entries.push(wgt::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgt::ShaderStages::from_bits_retain(artifact.projection_stages),
                ty: wgt::BindingType::Buffer {
                    ty: wgt::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: std::num::NonZeroU64::new(64),
                },
                count: None,
            });
        }
        for binding in artifact.textures {
            let filtering = binding.name.starts_with("sColor");
            let sample_type = match binding.scalar {
                ScalarType::Float => wgt::TextureSampleType::Float {
                    filterable: filtering,
                },
                ScalarType::Sint => wgt::TextureSampleType::Sint,
                ScalarType::Uint => wgt::TextureSampleType::Uint,
            };
            entries.push(wgt::BindGroupLayoutEntry {
                binding: binding.binding,
                visibility: wgt::ShaderStages::from_bits_retain(binding.stages),
                ty: wgt::BindingType::Texture {
                    sample_type,
                    view_dimension: wgt::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            });
            if binding.sampler_stages != 0 {
                entries.push(wgt::BindGroupLayoutEntry {
                    binding: binding.binding + 1,
                    visibility: wgt::ShaderStages::from_bits_retain(binding.sampler_stages),
                    ty: wgt::BindingType::Sampler(if filtering {
                        wgt::SamplerBindingType::Filtering
                    } else {
                        wgt::SamplerBindingType::NonFiltering
                    }),
                    count: None,
                });
            }
        }
        let bindings = Owned::new(
            owner,
            unsafe {
                native.create_bind_group_layout(&hal::BindGroupLayoutDescriptor {
                    label: Some("WR bindings"),
                    flags: hal::BindGroupLayoutFlags::empty(),
                    entries: &entries,
                })
            }
            .map_err(|e| format!("Creating binding layout: {e:?}"))?,
            A::Device::destroy_bind_group_layout,
        );
        let layout = Owned::new(
            owner,
            unsafe {
                native.create_pipeline_layout(&hal::PipelineLayoutDescriptor {
                    label: Some("WR pipelines"),
                    flags: hal::PipelineLayoutFlags::empty(),
                    bind_group_layouts: &[Some(&*bindings)],
                    immediate_size: 0,
                })
            }
            .map_err(|e| format!("Creating pipeline layout: {e:?}"))?,
            A::Device::destroy_pipeline_layout,
        );
        Ok((layout, bindings))
    }

    fn pipeline(&mut self, key: PipelineKey) -> Result<Rc<Pipeline<A>>> {
        if key.device != self.owner.cache_id || key.backend != self.owner.info.backend {
            return Err("HAL pipeline key belongs to another device/backend".into());
        }
        if let Some(pipeline) = self.pipelines.get(&key) {
            return Ok(pipeline.clone());
        }
        if self.pipelines.len() >= 128 {
            self.pipelines.clear();
            self.descriptors.borrow_mut().clear();
        }
        let metadata = self.shader_metadata(key.shader)?;
        let artifact = metadata.artifact;
        if artifact.features.contains("DUAL_SOURCE_BLENDING")
            && !self
                .owner
                .features
                .contains(wgt::Features::DUAL_SOURCE_BLENDING)
        {
            return Err("HAL adapter has no dual-source blending support".into());
        }
        let (layout, bindings) = Self::layouts(&self.owner, artifact)?;

        let vertex_buffers = [
            Some(hal::VertexBufferLayout {
                array_stride: 4,
                step_mode: wgt::VertexStepMode::Vertex,
                attributes: &metadata.vertex,
            }),
            Some(hal::VertexBufferLayout {
                array_stride: metadata.stride,
                step_mode: wgt::VertexStepMode::Instance,
                attributes: &metadata.instances,
            }),
        ];
        let native = &self.owner.open.device;
        let module = |fragment| -> Result<_> {
            let raw = A::create_shader_module(native, artifact, fragment, self.shader_input,
                &mut self.shader_cache.borrow_mut())?;
            Ok(Owned::new(&self.owner, raw, A::Device::destroy_shader_module))
        };
        let vs = module(false)?;
        let fs = module(true)?;
        let constants = Default::default();
        let stage = |module| hal::ProgrammableStage {
            module,
            entry_point: "main",
            constants: &constants,
            zero_initialize_workgroup_memory: false,
        };
        let component = |src_factor, dst_factor| wgt::BlendComponent {
            src_factor,
            dst_factor,
            operation: wgt::BlendOperation::Add,
        };
        use wgt::BlendFactor as Factor;
        let blend = match key.blend {
            0 => None,
            1 => Some(wgt::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
            2 => Some(wgt::BlendState::ALPHA_BLENDING),
            3 => Some(wgt::BlendState {
                color: component(Factor::Zero, Factor::Src),
                alpha: component(Factor::Zero, Factor::SrcAlpha),
            }),
            4 => Some(wgt::BlendState {
                color: component(Factor::Zero, Factor::OneMinusSrcAlpha),
                alpha: component(Factor::Zero, Factor::OneMinusSrcAlpha),
            }),
            5 => Some(wgt::BlendState {
                color: component(Factor::One, Factor::OneMinusSrc1),
                alpha: component(Factor::One, Factor::OneMinusSrc1Alpha),
            }),
            6 => Some(wgt::BlendState {
                color: component(Factor::One, Factor::OneMinusSrc),
                alpha: component(Factor::One, Factor::OneMinusSrcAlpha),
            }),
            7 => Some(wgt::BlendState {
                color: component(Factor::OneMinusDst, Factor::OneMinusSrc),
                alpha: component(Factor::One, Factor::OneMinusSrcAlpha),
            }),
            8 => Some(wgt::BlendState {
                color: component(Factor::One, Factor::One),
                alpha: component(Factor::One, Factor::One),
            }),
            _ => unreachable!(),
        };
        let pipeline = unsafe {
            native.create_render_pipeline(&hal::RenderPipelineDescriptor {
                label: Some(artifact.name),
                layout: &layout,
                vertex_processor: hal::VertexProcessor::Standard {
                    vertex_buffers: &vertex_buffers,
                    vertex_stage: stage(&*vs),
                },
                fragment_stage: Some(stage(&*fs)),
                primitive: wgt::PrimitiveState {
                    topology: wgt::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: if key.depth == 0 {
                    None
                } else {
                    Some(wgt::DepthStencilState {
                        format: key.depth_format.unwrap(),
                        depth_write_enabled: Some(key.depth == 1),
                        depth_compare: Some(if key.depth == 3 {
                            wgt::CompareFunction::Always
                        } else {
                            wgt::CompareFunction::LessEqual
                        }),
                        stencil: Default::default(),
                        bias: Default::default(),
                    })
                },
                multisample: wgt::MultisampleState {
                    count: key.samples,
                    ..Default::default()
                },
                color_targets: &[Some(wgt::ColorTargetState {
                    format: key.format,
                    blend,
                    write_mask: wgt::ColorWrites::ALL,
                })],
                multiview_mask: None,
                cache: None,
            })
        }
        .map_err(|e| format!("Creating pipeline {key:?}: {e:?}"))?;
        if !super::diagnostics::quiet() {
            println!("HAL pipeline {:?} shader={:016x}", key, artifact.digest);
        }
        let pipeline = Rc::new(Pipeline {
            raw: Owned::new(&self.owner, pipeline, A::Device::destroy_render_pipeline),
            layout,
            bindings,
        });
        self.pipelines.insert(key, pipeline.clone());
        Ok(pipeline)
    }

    fn draw_pass(
        &mut self,
        target: &Rc<Texture<A>>,
        draws: &[Draw<'_, A>],
        data: &HashMap<&str, Rc<Texture<A>>>,
        stats: &mut DrawStats,
    ) -> Result<()> {
        self.draw_pass_at(target, draws, data, stats, DeviceIntPoint::zero())
    }

    fn draw_pass_at(
        &mut self,
        target: &Rc<Texture<A>>,
        draws: &[Draw<'_, A>],
        data: &HashMap<&str, Rc<Texture<A>>>,
        stats: &mut DrawStats,
        origin: DeviceIntPoint,
    ) -> Result<()> {
        if draws.is_empty() && target.initialized() {
            let mut commands = self.submissions.recording()?;
            target.transition(&mut commands, wgt::TextureUses::RESOURCE);
            return Ok(());
        }
        let size = target.size;
        let full_rect = DeviceIntRect::from_origin_and_size(
            origin,
            DeviceIntSize::new(size.width as i32, size.height as i32),
        );
        let load_clear = draws
            .first()
            .filter(|draw| draw.scissor == full_rect)
            .and_then(|draw| draw.clear_color);
        let draws = if load_clear.is_some() {
            &draws[1..]
        } else {
            draws
        };
        let has_depth = draws.iter().any(|draw| draw.depth != 0);
        let depth = if has_depth {
            let id = (target.allocation_id, target.base_mip);
            if !self.depths.contains_key(&id) {
                let depth = self.texture_pool.acquire(
                    size.width,
                    size.height,
                    wgt::TextureFormat::Depth32Float,
                    true,
                )?;
                let mut commands = self.submissions.recording()?;
                depth.invalidate(&mut commands);
                self.depths.insert(id, depth);
            }
            Some(self.depths[&id].clone())
        } else {
            None
        };
        // Convert GL's [-N, N-1] near/far planes to HAL's [0, 1] clip depth.
        let depth_ids = crate::renderer::hal::MAX_DEPTH_IDS as f32;
        let depth_span = 2.0 * depth_ids - 1.0;
        let matrix: [f32; 16] = [
            2.0 / size.width as f32,
            0.0,
            0.0,
            0.0,
            0.0,
            -2.0 / size.height as f32,
            0.0,
            0.0,
            0.0,
            0.0,
            -1.0 / depth_span,
            0.0,
            -1.0 - 2.0 * origin.x as f32 / size.width as f32,
            1.0 + 2.0 * origin.y as f32 / size.height as f32,
            depth_ids / depth_span,
            1.0,
        ];
        #[cfg(test)]
        let matrix = self.projection_override.unwrap_or(matrix);
        let matrix_key = matrix.map(f32::to_bits);
        let uniform = if let Some(buffer) = self.uniforms.get(&matrix_key) {
            buffer.clone()
        } else {
            let matrix_bytes = matrix.map(f32::to_ne_bytes);
            let buffer = Buffer::new(&self.owner, matrix_bytes.as_flattened(), wgt::BufferUses::UNIFORM)?;
            if self.uniforms.len() >= 64 {
                self.uniforms.clear();
            }
            self.uniforms.insert(matrix_key, buffer.clone());
            buffer
        };
        let submissions = self.submissions.clone();
        let mut commands = submissions.recording()?;
        let mut resources = Vec::new();
        let mut sampled = Vec::new();
        for draw in draws {
            let depth_mode = if has_depth && draw.depth == 0 {
                3
            } else {
                draw.depth
            };
            let shader = if self.filtering == Filtering::LegacyBrilinear
                && draw.textures.colors[0].mip_count > 1
                && draw.filter.unwrap_or(draw.textures.colors[0].filter) == TextureFilter::Trilinear {
                let artifact = Self::artifact(draw.shader);
                if !matches!(artifact.name, "ps_quad_textured" | "ps_quad_repeat" | "composite" | "cs_scale") {
                    return Err(format!("Legacy brilinear is not audited for {}", artifact.name));
                }
                Shader::LegacyBrilinear(artifact.name, artifact.features)
            } else { draw.shader };
            if self.filtering == Filtering::LegacyBrilinear && draw.textures.colors[1..].iter().any(|texture|
                texture.mip_count > 1 && draw.filter.unwrap_or(texture.filter) == TextureFilter::Trilinear) {
                return Err("Legacy brilinear requires mipmapped images in sColor0".into());
            }
            let metadata = self.shader_metadata(shader)?;
            let key = self.key_with_metadata(
                shader,
                &metadata,
                draw.blend,
                depth_mode,
                target.format,
            );
            let pipeline = self.pipeline(key)?;
            let input = draw.instances.bytes();
            let buffer = submissions.upload_in_recording(
                &commands, packed_instance_size(shader, input)?, wgt::BufferUses::VERTEX, |destination| {
                    pack_instances(shader, input, destination);
                    Ok(())
                },
            )?;
            let artifact = metadata.artifact;
            let mut identities = SmallVec::new();
            let mut resolved: SmallVec<[(&Rc<Texture<A>>, usize); 16]> = SmallVec::new();
            for binding in artifact.textures {
                let texture = match binding.name {
                    "sColor0" => &draw.textures.colors[0],
                    "sColor1" => &draw.textures.colors[1],
                    "sColor2" => &draw.textures.colors[2],
                    "sClipMask" => &draw.textures.clip,
                    name => data.get(name).ok_or_else(|| format!("Missing HAL binding {name}"))?,
                };
                if texture.overlaps(target) {
                    return Err("HAL attachment feedback is unsupported".into());
                }
                sampled.push((texture.clone(), binding.name, shader, draw.count));
                let filter = if binding.name.starts_with("sColor") {
                    draw.filter.unwrap_or(texture.filter)
                } else {
                    TextureFilter::Nearest
                };
                let index = match filter {
                    TextureFilter::Nearest => 0,
                    TextureFilter::Linear => 1,
                    TextureFilter::Trilinear => 2,
                };
                identities.push((texture.allocation_id, texture.base_mip,
                    texture.mip_count, texture.format, index as u8));
                resolved.push((texture, index));
            }
            let descriptor_key = DescriptorKey {
                pipeline: key,
                uniform: uniform.allocation_id,
                textures: identities,
            };
            let cached = self.descriptors.borrow().get(&descriptor_key).cloned();
            let group = if let Some(group) = cached {
                group
            } else {
                let mut entries = Vec::new();
                let mut textures = Vec::new();
                let mut texture_owners = Vec::new();
                let mut samplers = Vec::new();
                if artifact.projection_stages != 0 {
                    entries.push(hal::BindGroupEntry { binding: 0, resource_index: 0, count: 1 });
                }
                for (binding, &(texture, filter)) in artifact.textures.iter().zip(&resolved) {
                    entries.push(hal::BindGroupEntry {
                        binding: binding.binding, resource_index: textures.len() as u32, count: 1,
                    });
                    textures.push(hal::TextureBinding { view: &*texture.view, usage: wgt::TextureUses::RESOURCE });
                    texture_owners.push(texture.clone());
                    if binding.sampler_stages != 0 {
                        entries.push(hal::BindGroupEntry {
                            binding: binding.binding + 1, resource_index: samplers.len() as u32, count: 1,
                        });
                        samplers.push(&*self.samplers[filter]);
                    }
                }
                let raw = unsafe {
                    self.owner
                        .open
                        .device
                        .create_bind_group(&hal::BindGroupDescriptor {
                            label: Some("WR draw"),
                            layout: &pipeline.bindings,
                            buffers: &[uniform.binding()],
                            samplers: &samplers,
                            textures: &textures,
                            entries: &entries,
                            acceleration_structures: &[],
                            external_textures: &[],
                        })
                }
                .map_err(|e| format!("Creating draw bindings: {e:?}"))?;
                let cacheable = texture_owners
                    .iter()
                    .all(|texture| !texture.transient.get());
                let group = Rc::new(Descriptor {
                    raw: Owned::new(&self.owner, raw, A::Device::destroy_bind_group),
                    _uniform: uniform.clone(),
                    _textures: texture_owners,
                    _pipeline: pipeline.clone(),
                });
                if cacheable {
                    let mut cache = self.descriptors.borrow_mut();
                    if cache.len() >= 256 {
                        cache.clear();
                    }
                    cache.insert(descriptor_key, group.clone());
                }
                group
            };
            resources.push((pipeline, buffer, group));
        }
        let initialized = target.initialized() && load_clear.is_none();
        let clear = load_clear.unwrap_or(ColorF::TRANSPARENT);
        for (_, _, group) in &resources {
            commands.keep(group.clone());
        }
        for (texture, binding, shader, count) in sampled {
            if !texture.sample_initialized() {
                let cache: Vec<_> = self
                    .textures
                    .iter()
                    .filter(|(_, entry)| entry.allocation_id == texture.allocation_id)
                    .map(|(id, _)| id)
                    .collect();
                return Err(format!(
                    "Sampling uninitialized or invalidated HAL texture contents: shader={shader:?}, binding={binding}, count={count}, texture={}, cache={cache:?}, size={:?}, format={:?}, mip={}+{}, usage={:?}, target={}",
                    texture.allocation_id,
                    texture.size,
                    texture.format,
                    texture.base_mip,
                    texture.mip_count,
                    texture.current_usage(),
                    target.allocation_id,
                ));
            }
            texture.transition(&mut commands, wgt::TextureUses::RESOURCE);
        }
        self.quad.transition(&mut commands, wgt::BufferUses::VERTEX);
        uniform.transition(&mut commands, wgt::BufferUses::UNIFORM);
        for (_, buffer, _) in &resources {
            buffer.transition(&mut commands, wgt::BufferUses::VERTEX);
        }
        target.transition(&mut commands, wgt::TextureUses::COLOR_TARGET);
        if let Some(depth) = &depth {
            depth.transition(&mut commands, wgt::TextureUses::DEPTH_WRITE);
        }
        unsafe {
            commands
                .encoder()
                .begin_render_pass(&hal::RenderPassDescriptor {
                    label: Some("WR target"),
                    extent: size,
                    sample_count: 1,
                    color_attachments: &[Some(hal::ColorAttachment {
                        target: hal::Attachment {
                            view: target
                                .target
                                .as_ref()
                                .ok_or("HAL texture is not renderable")?,
                            usage: wgt::TextureUses::COLOR_TARGET,
                        },
                        depth_slice: None,
                        resolve_target: None,
                        clear_value: wgt::Color {
                            r: clear.r as f64,
                            g: clear.g as f64,
                            b: clear.b as f64,
                            a: clear.a as f64,
                        },
                        ops: (if initialized {
                            hal::AttachmentOps::LOAD
                        } else {
                            hal::AttachmentOps::LOAD_CLEAR
                        }) | hal::AttachmentOps::STORE,
                    })],
                    depth_stencil_attachment: depth.as_ref().map(|texture| {
                        hal::DepthStencilAttachment {
                            depth_read_only: false,
                            stencil_read_only: true,
                            target: hal::Attachment {
                                view: &*texture.view,
                                usage: wgt::TextureUses::DEPTH_WRITE,
                            },
                            depth_ops: (if texture.initialized() {
                                hal::AttachmentOps::LOAD
                            } else {
                                hal::AttachmentOps::LOAD_CLEAR
                            }) | hal::AttachmentOps::STORE,
                            stencil_ops: hal::AttachmentOps::LOAD_DONT_CARE
                                | hal::AttachmentOps::STORE_DISCARD,
                            clear_value: (1.0, 0),
                        }
                    }),
                    multiview_mask: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                })
                .map_err(|e| format!("Beginning target: {e:?}"))?;
            commands.encoder().set_viewport(
                &hal::Rect {
                    x: 0.0,
                    y: 0.0,
                    w: size.width as f32,
                    h: size.height as f32,
                },
                0.0..1.0,
            );
            commands.encoder().set_vertex_buffer(0, self.quad.binding());
            let mut last_scissor = None;
            let mut last_pipeline: Option<&Rc<Pipeline<A>>> = None;
            let mut last_group: Option<&Rc<Descriptor<A>>> = None;
            for (draw, (pipeline, buffer, group)) in draws.iter().zip(&resources) {
                let full = full_rect;
                let Some(rect) = draw.scissor.intersection(&full) else {
                    continue;
                };
                if last_scissor != Some(rect) {
                    commands.encoder().set_scissor_rect(&hal::Rect {
                        x: (rect.min.x - origin.x) as u32,
                        y: (rect.min.y - origin.y) as u32,
                        w: rect.width() as u32,
                        h: rect.height() as u32,
                    });
                    last_scissor = Some(rect);
                }
                let pipeline_changed = last_pipeline.map_or(true, |last| !Rc::ptr_eq(last, pipeline));
                if pipeline_changed {
                    commands.encoder().set_render_pipeline(&pipeline.raw);
                    last_pipeline = Some(pipeline);
                }
                if pipeline_changed || last_group.map_or(true, |last| !Rc::ptr_eq(last, group)) {
                    commands.encoder().set_bind_group(&pipeline.layout, 0, &group.raw, &[]);
                    last_group = Some(group);
                }
                commands.encoder().set_vertex_buffer(1, buffer.binding());
                commands.encoder().draw(0, 4, 0, draw.count);
                stats.draw_calls += 1;
                stats.wr_draw_calls += usize::from(draw.count_in_stats);
                match draw.shader {
                    Shader::Quad => stats.primitive_instances += draw.count as usize,
                    Shader::Composite => stats.composite_tiles += draw.count as usize,
                    Shader::Clear => {}
                    Shader::Other(name, _) | Shader::LegacyBrilinear(name, _) => {
                        if name.starts_with("ps_quad")
                            || name == "ps_text_run"
                            || name == "ps_split_composite"
                        {
                            stats.primitive_instances += draw.count as usize;
                        }
                    }
                }
            }
            commands.encoder().end_render_pass();
        }
        target.transition(&mut commands, wgt::TextureUses::RESOURCE);
        target.initialize(&mut commands);
        if let Some(depth) = &depth {
            depth.initialize(&mut commands);
        }
        stats.native_passes += 1;
        Ok(())
    }

    fn clear(&self, rect: DeviceIntRect, color: ColorF) -> Draw<'static, A> {
        let instance = ClearInstance {
            rect: [
                rect.min.x as f32,
                rect.min.y as f32,
                rect.max.x as f32,
                rect.max.y as f32,
            ],
            color: [color.r, color.g, color.b, color.a],
        };
        Draw {
            shader: Shader::Clear,
            blend: 0,
            depth: 0,
            count: 1,
            instances: Instances::owned(bytes(&[instance])),
            textures: self.single_texture(self.dummy.clone()),
            filter: None,
            clear_color: Some(color),
            count_in_stats: false,
            readback: None,
            scissor: rect,
        }
    }

    fn batches<'a>(
        &self,
        container: &'a AlphaBatchContainer,
        rect: DeviceIntRect,
        draws: &mut Vec<Draw<'a, A>>,
    ) -> Result<()> {
        let has_depth = !container.opaque_batches.is_empty();
        let Some(scissor) = container
            .task_scissor_rect
            .unwrap_or(rect)
            .intersection(&rect)
        else {
            return Ok(());
        };
        for (opaque, batches) in [
            (true, &container.opaque_batches),
            (false, &container.alpha_batches),
        ] {
            for index in 0..batches.len() {
                let batch = &batches[if opaque {
                    batches.len() - 1 - index
                } else {
                    index
                }];
                if batch.instances.is_empty() {
                    continue;
                }
                let shader = match batch.key.kind {
                    BatchKind::Quad(pattern) => self.quad_shader(pattern)?,
                    BatchKind::SplitComposite => Shader::Other("ps_split_composite", ""),
                    BatchKind::TextRun(format) => {
                        let transformed = matches!(
                            format,
                            glyph_rasterizer::GlyphFormat::TransformedAlpha
                                | glyph_rasterizer::GlyphFormat::TransformedSubpixel
                        );
                        let features = match (
                            transformed,
                            batch.key.blend_mode == BlendMode::SubpixelDualSource,
                        ) {
                            (false, false) => "ALPHA_PASS,TEXTURE_2D",
                            (true, false) => "ALPHA_PASS,GLYPH_TRANSFORM,TEXTURE_2D",
                            (false, true) => "ALPHA_PASS,DUAL_SOURCE_BLENDING,TEXTURE_2D",
                            (true, true) => {
                                "ALPHA_PASS,DUAL_SOURCE_BLENDING,GLYPH_TRANSFORM,TEXTURE_2D"
                            }
                        };
                        Shader::Other("ps_text_run", features)
                    }
                };
                let blend = match batch.key.blend_mode {
                    BlendMode::None => 0,
                    BlendMode::PremultipliedAlpha => 1,
                    BlendMode::Alpha => 2,
                    BlendMode::Multiply => 3,
                    BlendMode::PremultipliedDestOut => 4,
                    BlendMode::SubpixelDualSource => 5,
                    BlendMode::Screen => 6,
                    BlendMode::Exclusion => 7,
                    BlendMode::PlusLighter => 8,
                    mode => return Err(format!("Unsupported HAL blend {mode:?}")),
                };
                let input = bytes(&batch.instances);
                let size = packed_instance_size(shader, input)?;
                draws.push(Draw {
                    shader,
                    blend,
                    depth: if !has_depth {
                        0
                    } else if opaque {
                        1
                    } else {
                        2
                    },
                    count: u32::try_from(size / 16).map_err(|_| "Too many HAL instances")?,
                    instances: Instances::Borrowed(input),
                    textures: self.batch_textures(&batch.key.textures)?,
                    filter: None,
                    clear_color: None,
                    count_in_stats: true,
                    readback: batch.readback,
                    scissor,
                });
            }
        }
        Ok(())
    }

    fn draw_batches(
        &mut self,
        target: &Rc<Texture<A>>,
        draws: &[Draw<'_, A>],
        data: &HashMap<&str, Rc<Texture<A>>>,
        tasks: &crate::render_task_graph::RenderTaskGraph,
        stats: &mut DrawStats,
    ) -> Result<()> {
        let mut start = 0;
        for (index, draw) in draws.iter().enumerate() {
            if let Some(readback) = draw.readback {
                self.draw_pass(target, &draws[start..index], data, stats)?;
                let source = &tasks[readback.src_task_id];
                let destination = &tasks[readback.readback_task_id];
                if let Some((source_rect, destination_rect)) =
                    crate::renderer::geometry::readback_rects(source, destination)
                {
                    let texture = self.source(destination.get_texture_source())?;
                    self.record_blit(
                        target,
                        &texture,
                        source_rect,
                        destination_rect,
                        TextureFilter::Linear,
                        stats,
                    )?;
                }
                start = index;
            }
        }
        self.draw_pass(target, &draws[start..], data, stats)
    }

    fn masks<'a>(
        &self,
        masks: &'a ClipMaskInstanceList,
        rect: DeviceIntRect,
        draws: &mut Vec<Draw<'a, A>>,
    ) -> Result<()> {
        let mut group = |features,
                         instances: &'a [crate::gpu_types::MaskInstance],
                         scissored: &'a crate::internal_types::FastHashMap<
            DeviceIntRect,
            crate::internal_types::FrameVec<crate::gpu_types::MaskInstance>,
        >|
         -> Result<()> {
            let shader = Shader::Other("ps_quad_mask", features);
            if !instances.is_empty() {
                draws.push(self.task_draw(shader, 3, instances, &BatchTextures::empty(), rect)?);
            }
            for (scissor, instances) in scissored {
                draws.push(self.task_draw(
                    shader,
                    3,
                    instances,
                    &BatchTextures::empty(),
                    *scissor,
                )?);
            }
            Ok(())
        };
        group(
            "FAST_PATH",
            &masks.mask_instances_fast,
            &masks.mask_instances_fast_with_scissor,
        )?;
        group(
            "SUPERELLIPSE",
            &masks.mask_instances_superellipse,
            &masks.mask_instances_superellipse_with_scissor,
        )?;
        for (source, instances) in &masks.image_mask_instances {
            draws.push(self.task_draw(
                Shader::Quad,
                3,
                instances,
                &BatchTextures::composite_rgb(*source),
                rect,
            )?);
        }
        for ((scissor, source), instances) in &masks.image_mask_instances_with_scissor {
            draws.push(self.task_draw(
                Shader::Quad,
                3,
                instances,
                &BatchTextures::composite_rgb(*source),
                *scissor,
            )?);
        }
        let shader = Shader::Other("ps_quad_mask", "");
        if !masks.mask_instances_slow.is_empty() {
            draws.push(self.task_draw(
                shader,
                3,
                &masks.mask_instances_slow,
                &BatchTextures::empty(),
                rect,
            )?);
        }
        for (scissor, instances) in &masks.mask_instances_slow_with_scissor {
            draws.push(self.task_draw(shader, 3, instances, &BatchTextures::empty(), *scissor)?);
        }
        Ok(())
    }

    fn target(
        &mut self,
        target: &RenderTarget,
        data: &HashMap<&str, Rc<Texture<A>>>,
        tasks: &crate::render_task_graph::RenderTaskGraph,
        stats: &mut DrawStats,
    ) -> Result<()> {
        let texture = self
            .textures
            .get(&target.texture_id)
            .cloned()
            .ok_or("Missing HAL render target")?;
        let rect = target
            .used_rect
            .unwrap_or(DeviceIntRect::from_size(DeviceIntSize::new(
                texture.size.width as i32,
                texture.size.height as i32,
            )));
        let mut draws = Vec::new();
        if !target.cached {
            if let Some(color) = target.clear_color {
                draws.push(self.clear(rect, color));
            }
        }
        let clears: Vec<_> = target
            .clears
            .iter()
            .filter(|(_, color)| target.cached || target.clear_color != Some(*color))
            .map(|(rect, color)| ClearInstance {
                rect: [
                    rect.min.x as f32,
                    rect.min.y as f32,
                    rect.max.x as f32,
                    rect.max.y as f32,
                ],
                color: color.to_array(),
            })
            .collect();
        if !clears.is_empty() {
            let full = DeviceIntRect::from_size(DeviceIntSize::new(
                texture.size.width as i32,
                texture.size.height as i32,
            ));
            draws.push(self.task_draw(Shader::Clear, 0, &clears, &BatchTextures::empty(), full)?);
        }
        if !target.resolve_ops.is_empty() || !target.blits.is_empty() {
            self.draw_pass(&texture, &draws, data, stats)?;
            draws.clear();
            for resolve in &target.resolve_ops {
                for id in &resolve.src_task_ids {
                    let source_task = &tasks[*id];
                    let destination_task = &tasks[resolve.dest_task_id];
                    if let Some((source_rect, destination_rect)) =
                        crate::renderer::geometry::resolve_rects(
                            source_task,
                            destination_task,
                            &resolve.dest_to_src_raster,
                        )
                    {
                        let source = self.source(source_task.get_texture_source())?;
                        self.queue_blit(
                            &source,
                            &texture,
                            source_rect,
                            destination_rect,
                            TextureFilter::Linear,
                            &mut draws,
                            stats,
                        )?;
                    }
                }
            }
            for blit in &target.blits {
                let task = &tasks[blit.source];
                let source = self.source(task.get_texture_source())?;
                let source_rect = blit
                    .source_rect
                    .translate(task.get_target_rect().min.to_vector());
                self.queue_blit(
                    &source,
                    &texture,
                    source_rect,
                    blit.target_rect,
                    TextureFilter::Linear,
                    &mut draws,
                    stats,
                )?;
            }
            self.draw_pass(&texture, &draws, data, stats)?;
            draws.clear();
        }
        for (name, features, instances) in [
            ("cs_border_solid", "", &target.border_segments_solid),
            ("cs_border_segment", "", &target.border_segments_complex),
            (
                "cs_border_solid",
                "SUPERELLIPSE",
                &target.border_segments_solid_superellipse,
            ),
            (
                "cs_border_segment",
                "SUPERELLIPSE",
                &target.border_segments_complex_superellipse,
            ),
        ] {
            if !instances.is_empty() {
                draws.push(self.task_draw(
                    Shader::Other(name, features),
                    1,
                    instances,
                    &BatchTextures::empty(),
                    rect,
                )?);
            }
        }
        if !target.line_decorations.is_empty() {
            draws.push(self.task_draw(
                Shader::Other("cs_line_decoration", ""),
                1,
                &target.line_decorations,
                &BatchTextures::empty(),
                rect,
            )?);
        }
        for blurs in [&target.vertical_blurs, &target.horizontal_blurs] {
            for (source, instances) in blurs {
                draws.push(self.task_draw(
                    Shader::Other("cs_blur", "COLOR_TARGET"),
                    0,
                    instances,
                    &BatchTextures::composite_rgb(*source),
                    rect,
                )?);
            }
        }
        for (source, values) in &target.scalings {
            let instances = if let TextureSource::External(source) = source {
                let image = self.external_images.get(&source.index).ok_or("Missing external scaling image")?;
                let adjusted: Vec<_> = values.iter().map(|instance| ScalingInstance::new(
                    instance.target_rect, DeviceRect::new(image.uv.uv0, image.uv.uv1), false,
                )).collect();
                Instances::owned(bytes(&adjusted))
            } else { Instances::Borrowed(bytes(values)) };
            draws.push(Draw {
                shader: Shader::Other("cs_scale", "TEXTURE_2D"),
                blend: 0,
                depth: 0,
                count: values.len() as u32,
                instances,
                textures: self.single_texture(self.source(*source)?),
                filter: None,
                clear_color: None,
                count_in_stats: true,
                readback: None,
                scissor: rect,
            });
        }
        for (textures, instances) in &target.svg_nodes {
            if !instances.is_empty() {
                draws.push(self.task_draw(
                    Shader::Other("cs_svg_filter_node", ""),
                    0,
                    instances,
                    textures,
                    rect,
                )?);
            }
        }
        for container in &target.alpha_batch_containers {
            self.batches(container, rect, &mut draws)?;
        }
        for (index, batches) in target.prim_instances.iter().enumerate() {
            if batches.is_empty() {
                continue;
            }
            let shader = self.quad_shader(PatternKind::from_u32(index as u32))?;
            for (textures, instances) in batches {
                draws.push(self.task_draw(
                    shader,
                    0,
                    instances,
                    &BatchTextures {
                        input: *textures,
                        clip_mask: TextureSource::Invalid,
                    },
                    rect,
                )?);
            }
        }
        for ((scissor, pattern), batches) in &target.prim_instances_with_scissor {
            let shader = self.quad_shader(*pattern)?;
            for (textures, instances) in batches {
                draws.push(self.task_draw(
                    shader,
                    1,
                    instances,
                    &BatchTextures {
                        input: *textures,
                        clip_mask: TextureSource::Invalid,
                    },
                    *scissor,
                )?);
            }
        }
        self.masks(&target.clip_masks, rect, &mut draws)?;
        self.draw_batches(&texture, &draws, data, tasks, stats)
    }

    pub fn update_resources(&mut self, updates: Vec<ResourceUpdateList>) -> Result<()> {
        if self.is_failed() {
            return Err("HAL renderer must be recreated after an execution failure".into());
        }
        self.failed.set(true);
        let result = (|| {
            for update in updates { self.update(update)?; }
            self.submissions.submit()
        })();
        if result.is_err() { self.abort(); } else { self.failed.set(false); }
        result
    }

    pub fn render(
        &mut self,
        frame: &mut Frame,
        updates: Vec<ResourceUpdateList>,
        clear: ColorF,
    ) -> Result<RenderedFrame<A>> {
        self.execute(frame, updates, clear, frame.present, None)
    }

    pub fn render_retained(
        &mut self, frame: &mut Frame, clear: ColorF,
        previous: &RenderedFrame<A>, damage: DeviceIntRect,
    ) -> Result<RenderedFrame<A>> {
        if !self.has_owned_output(previous) || previous.origin != frame.device_rect.min
            || previous.size != [frame.device_rect.width() as u32, frame.device_rect.height() as u32]
            || damage.is_empty() || !frame.device_rect.contains_box(&damage) {
            return Err("Invalid retained HAL composition target or damage".into());
        }
        self.execute(frame, Vec::new(), clear, frame.present, Some((previous, damage)))
    }

    pub fn render_offscreen(&mut self, frame: &mut Frame) -> Result<()> {
        self.execute(frame, Vec::new(), ColorF::TRANSPARENT, false, None)
            .map(|_| ())
    }

    fn execute(
        &mut self,
        frame: &mut Frame,
        updates: Vec<ResourceUpdateList>,
        clear: ColorF,
        composite: bool,
        retained: Option<(&RenderedFrame<A>, DeviceIntRect)>,
    ) -> Result<RenderedFrame<A>> {
        if self.is_failed() {
            return Err("HAL renderer must be recreated after an execution failure".into());
        }
        self.poll()?;
        self.failed.set(true);
        let result = self.render_inner(frame, updates, clear, composite, retained);
        if result.is_err() { self.abort(); }
        self.external_images.clear();
        self.native_targets.clear();
        self.layer_targets.clear();
        dispatch_releases(&self.releases);
        if let Ok(output) = &result {
            if output.texture.is_some() {
                let damage = retained.map_or(frame.device_rect, |(_, damage)| damage);
                let local = DeviceIntRect::new(
                    DeviceIntPoint::new(damage.min.x - output.origin.x, damage.min.y - output.origin.y),
                    DeviceIntPoint::new(damage.max.x - output.origin.x, damage.max.y - output.origin.y),
                );
                self.output_history.record(output.serial, output.size, local);
            }
            self.failed.set(false);
        }
        result
    }

    fn render_inner(
        &mut self,
        frame: &mut Frame,
        updates: Vec<ResourceUpdateList>,
        clear: ColorF,
        composite: bool,
        retained: Option<(&RenderedFrame<A>, DeviceIntRect)>,
    ) -> Result<RenderedFrame<A>> {
        self.count(RenderCounter::Executions, 1);
        if !composite { self.count(RenderCounter::OffscreenExecutions, 1); }
        self.depths.clear();
        for updates in updates {
            self.update(updates)?;
        }
        self.resolve_external_images(frame)?;
        let query = self.queries.borrow_mut().begin(&self.submissions)?;
        let mut data = HashMap::from([
            (
                "sPrimitiveHeadersF",
                self.data_texture(
                    "sPrimitiveHeadersF",
                    &frame.prim_headers.headers_float,
                    wgt::TextureFormat::Rgba32Float,
                )?,
            ),
            (
                "sPrimitiveHeadersI",
                self.data_texture(
                    "sPrimitiveHeadersI",
                    &frame.prim_headers.headers_int,
                    wgt::TextureFormat::Rgba32Sint,
                )?,
            ),
            (
                "sGpuBufferF",
                self.data_texture(
                    "sGpuBufferF",
                    &frame.gpu_buffer_f.data,
                    wgt::TextureFormat::Rgba32Float,
                )?,
            ),
            (
                "sGpuBufferI",
                self.data_texture(
                    "sGpuBufferI",
                    &frame.gpu_buffer_i.data,
                    wgt::TextureFormat::Rgba32Sint,
                )?,
            ),
            (
                "sTransformPalette",
                self.data_texture(
                    "sTransformPalette",
                    &frame.transform_palette,
                    wgt::TextureFormat::Rgba32Float,
                )?,
            ),
            (
                "sRenderTasks",
                self.data_texture(
                    "sRenderTasks",
                    &frame.render_tasks.task_data,
                    wgt::TextureFormat::Rgba32Float,
                )?,
            ),
        ]);
        if let Some(dither) = &self.dither {
            data.insert("sDither", dither.clone());
        }
        let mut stats = DrawStats::default();
        for pass in &frame.passes {
            if !frame.has_been_rendered {
                for target in pass.texture_cache.values() {
                    self.target(target, &data, &frame.render_tasks, &mut stats)?;
                }
                for target in &pass.picture_cache {
                    self.count(RenderCounter::RasterizedTiles, 1);
                    stats.color_targets += 1;
                    let native_id = match target.surface {
                        ResolvedSurfaceTexture::Native { id, size } => {
                            self.bind_native_tile(id, size, target.dirty_rect, target.valid_rect)?;
                            Some(id)
                        }
                        _ => None,
                    };
                    let texture = if let Some(id) = native_id {
                        let native = &self.native_targets[&id];
                        let texture = self.texture_pool.acquire(native.size.width as u32, native.size.height as u32, native.texture.format, true)?;
                        {
                            let mut commands = self.submissions.recording()?;
                            texture.invalidate(&mut commands);
                        }
                        if native.texture.initialized() {
                            let source = native.texture.clone();
                            let rect = DeviceIntRect::from_origin_and_size(native.origin, native.size);
                            self.copy_native(&source, &texture, rect, DeviceIntRect::from_size(rect.size()))?;
                        } else {
                            self.draw_pass(&texture, &[self.clear(DeviceIntRect::from_size(native.size), ColorF::TRANSPARENT)], &data, &mut stats)?;
                        }
                        texture
                    } else { self.surface(&target.surface)? };
                    match &target.kind {
                        PictureCacheTargetKind::Draw {
                            alpha_batch_container,
                        } => {
                            let mut draws = Vec::new();
                            if let Some(color) = target.clear_color {
                                draws.push(self.clear(target.dirty_rect, color));
                            }
                            self.batches(alpha_batch_container, target.dirty_rect, &mut draws)?;
                            self.draw_batches(
                                &texture,
                                &draws,
                                &data,
                                &frame.render_tasks,
                                &mut stats,
                            )?;
                        }
                        PictureCacheTargetKind::Blit {
                            task_id,
                            sub_rect_offset,
                        } => {
                            let task = &frame.render_tasks[*task_id];
                            let source = self.source(task.get_texture_source())?;
                            let source_rect = DeviceIntRect::from_origin_and_size(
                                task.get_target_rect().min + *sub_rect_offset,
                                target.dirty_rect.size(),
                            );
                            if let Some(color) = target.clear_color {
                                self.draw_pass(
                                    &texture,
                                    &[self.clear(target.dirty_rect, color)],
                                    &data,
                                    &mut stats,
                                )?;
                            }
                            self.copy(&source, &texture, source_rect, target.dirty_rect)?;
                        }
                    }
                    if let Some(id) = native_id {
                        let native = &self.native_targets[&id];
                        let destination = native.texture.clone();
                        let destination_rect = target.dirty_rect.translate(native.origin.to_vector());
                        self.copy_native(&texture, &destination, target.dirty_rect, destination_rect)?;
                    }
                }
            }
            for target in &pass.alpha.targets {
                stats.alpha_targets += 1;
                self.target(target, &data, &frame.render_tasks, &mut stats)?;
            }
            for target in &pass.color.targets {
                stats.color_targets += 1;
                self.target(target, &data, &frame.render_tasks, &mut stats)?;
            }
            for id in &pass.textures_to_invalidate {
                if let Some(texture) = self.textures.get(id) {
                    let mut commands = self.submissions.recording()?;
                    texture.invalidate(&mut commands);
                }
            }
        }
        self.update_native_external_surfaces(frame, &data, &mut stats)?;
        let size = frame.device_rect.size();
        if !composite || size.is_empty() {
            self.restore_external_images()?;
            let serial = self.queries.borrow_mut().finish(&self.submissions, query)?;
            return Ok(RenderedFrame {
                size: [0, 0],
                origin: frame.device_rect.min,
                stats,
                serial,
                texture: None,
            });
        }
        stats.color_targets += 1;
        self.acquire_composite_tiles(frame)?;
        let (output, damage) = if let Some((previous, damage)) = retained {
            self.count(RenderCounter::PartialCompositions, 1);
            (previous.texture.as_ref().unwrap().clone(), damage)
        } else {
            self.count(RenderCounter::FullCompositions, 1);
            let output = self.texture_pool.acquire(
                size.width as u32, size.height as u32, wgt::TextureFormat::Rgba8Unorm, true,
            )?;
            let mut commands = self.submissions.recording()?;
            output.invalidate(&mut commands);
            (output, frame.device_rect)
        };
        self.count(RenderCounter::ComposedPixels, damage.width() as u64 * damage.height() as u64);
        let mut draws = vec![self.clear(damage, clear)];
        let mut layer_rects = Vec::new();
        let state = &frame.composite_state;
        let optimize_composite = matches!(self.compositor, CompositorConfig::Draw);
        for region in composite::regions(state, frame.device_rect, damage, optimize_composite) {
            let tile = &state.tiles[region.tile_index];
            let rect = state.get_device_rect(&tile.local_rect, tile.transform_index);
            let clip_rect = region.rect;
            layer_rects.push(clip_rect.round_out().to_i32());
            let transform = state.get_device_transform(tile.transform_index);
            let flip = (transform.scale.x < 0.0, transform.scale.y < 0.0);
            let clip = tile
                .clip_index
                .filter(|_| region.needs_mask)
                .map(|index| state.get_compositor_clip(index));
            let (instance, textures, shader) = match tile.surface {
                CompositeTileSurface::Color { color } => (
                    CompositeInstance::new(rect, clip_rect, color.premultiplied(), flip, clip),
                    self.single_texture(self.dummy.clone()),
                    Shader::Composite,
                ),
                CompositeTileSurface::Texture { ref surface } => {
                    let instance = match surface {
                        ResolvedSurfaceTexture::Native { id, .. } => {
                            let target = &self.native_targets[id];
                            CompositeInstance::new_rgb(rect, clip_rect, PremultipliedColorF::WHITE,
                                DeviceIntRect::from_origin_and_size(target.origin, target.size).into(), false, flip, clip)
                        }
                        _ => CompositeInstance::new(rect, clip_rect, PremultipliedColorF::WHITE, flip, clip),
                    };
                    (instance, self.single_texture(self.surface(surface)?), Shader::Composite)
                },
                CompositeTileSurface::ExternalSurface {
                    external_surface_index,
                } => {
                    let surface = &state.external_surfaces[external_surface_index.0];
                    if matches!(self.compositor, CompositorConfig::Native { .. }) && surface.external_image_id.is_none() {
                        let surface_id = surface.native_surface_id.ok_or("Missing native external surface identity")?;
                        let target = &self.native_targets[&NativeTileId { surface_id, x: 0, y: 0 }];
                        (CompositeInstance::new_rgb(rect, clip_rect, PremultipliedColorF::WHITE,
                            DeviceIntRect::from_origin_and_size(target.origin, target.size).into(), false, flip, clip),
                         self.single_texture(target.texture.clone()), Shader::Composite)
                    } else {
                        self.external_composite(surface, rect, clip_rect, flip, clip)?
                    }
                }
            };
            let draw = Draw {
                shader,
                blend: region.blend,
                depth: 0,
                count: 1,
                instances: Instances::owned(bytes(&[instance])),
                textures,
                filter: None,
                clear_color: None,
                count_in_stats: true,
                readback: None,
                scissor: damage,
            };
            if optimize_composite {
                composite::push_draw(&mut draws, draw);
            } else {
                draws.push(draw);
            }
        }
        if matches!(self.compositor, CompositorConfig::Layer { .. }) {
            let input_layers: Vec<_> = layer_rects.iter().map(|rect| crate::composite::CompositorInputLayer {
                offset: rect.min, clip_rect: *rect, usage: crate::composite::CompositorSurfaceUsage::Content,
                is_opaque: false, rounded_clip_rect: *rect, rounded_clip_radii: crate::composite::ClipRadius::EMPTY,
            }).collect();
            let device = self.external_image_device();
            if let CompositorConfig::Layer { compositor } = &mut self.compositor {
                compositor.begin_frame(&device, &crate::composite::CompositorInputConfig { enable_screenshot: true, layers: &input_layers })?;
            }
            let mut composites = vec![self.clear(frame.device_rect, clear)];
            for (index, (draw, rect)) in draws.into_iter().skip(1).zip(layer_rects).enumerate() {
                let dirty = DeviceIntRect::from_size(rect.size());
                let target = match &mut self.compositor {
                    CompositorConfig::Layer { compositor } => compositor.bind_layer(index, &[dirty])?,
                    _ => unreachable!(),
                };
                let target = self.acquired_target(target, rect.size())?;
                let origin = rect.min - target.origin.to_vector();
                let clear = self.clear(rect, ColorF::TRANSPARENT);
                self.draw_pass_at(&target.texture, &[clear, draw], &data, &mut stats, origin)?;
                stats.color_targets += 1;
                let instance = CompositeInstance::new_rgb(rect.to_f32(), rect.to_f32(), PremultipliedColorF::WHITE,
                    DeviceIntRect::from_origin_and_size(target.origin, target.size).into(), false, (false, false), None);
                composites.push(Draw {
                    shader: Shader::Composite, blend: 1, depth: 0, count: 1, instances: Instances::owned(bytes(&[instance])),
                    textures: self.single_texture(target.texture.clone()), filter: None, clear_color: None,
                    count_in_stats: false, readback: None, scissor: frame.device_rect,
                });
                self.layer_targets.push(target);
            }
            self.draw_pass_at(&output, &composites, &data, &mut stats, frame.device_rect.min)?;
        } else {
            self.draw_pass_at(&output, &draws, &data, &mut stats, frame.device_rect.min)?;
        }
        self.restore_external_images()?;
        let serial = self.queries.borrow_mut().finish(&self.submissions, query)?;
        Ok(RenderedFrame {
            size: [output.size.width, output.size.height],
            origin: frame.device_rect.min,
            stats,
            serial,
            texture: Some(output),
        })
    }

    pub fn readback_bytes(&self, width: u32, height: u32) -> Result<u64> {
        Ok(self.owner.layout(width, height)?.size)
    }

    pub fn release_capture_buffers(&mut self) {
        self.capture_pool.clear();
        self.readback_pool.borrow_mut().clear();
    }

    fn readback_buffer(&self, layout: &ReadbackLayout) -> Result<Rc<Buffer<A>>> {
        self.submissions.poll()?;
        let mut pool = self.readback_pool.borrow_mut();
        if let Some(buffer) = pool.iter().find(|buffer| Rc::strong_count(buffer) == 1 && buffer.size >= layout.size) {
            return Ok(buffer.clone());
        }
        let buffer = Buffer::readback(&self.owner, layout)?;
        let mut bytes: u64 = pool.iter().map(|buffer| buffer.size).sum();
        let mut count = pool.len();
        pool.retain(|old| {
            if (bytes + buffer.size > 64 << 20 || count >= 8) && Rc::strong_count(old) == 1 {
                bytes -= old.size; count -= 1; false
            } else { true }
        });
        if bytes + buffer.size <= 64 << 20 && pool.len() < 8 { pool.push(buffer.clone()); }
        Ok(buffer)
    }

    pub fn scaled_readback(&mut self, frame: &RenderedFrame<A>, rect: DeviceIntRect, size: DeviceIntSize) -> Result<PendingReadback<A>> {
        if self.is_failed() { return Err("HAL renderer requires recreation".into()); }
        let mut source = frame.texture.as_ref().ok_or("No HAL output to capture")?.clone();
        if rect.is_empty() || size.is_empty() || !DeviceIntRect::from_size(DeviceIntSize::new(frame.size[0] as i32, frame.size[1] as i32)).contains_box(&rect) {
            return Err("Invalid HAL screenshot rectangle or size".into());
        }
        let mut levels = vec![size];
        while rect.width() > levels.last().unwrap().width.saturating_mul(2) {
            let previous = *levels.last().unwrap();
            let next = DeviceIntSize::new(previous.width.saturating_mul(2), previous.height.saturating_mul(2));
            if next.width > self.owner.max_texture_size() || next.height > self.owner.max_texture_size() { break; }
            levels.push(next);
        }
        self.failed.set(true);
        let mut source_rect = rect;
        for level in levels.into_iter().rev() {
            let target = self.capture_pool.acquire(level.width as u32, level.height as u32, wgt::TextureFormat::Rgba8Unorm, true)?;
            {
                let mut commands = self.submissions.recording()?;
                target.invalidate(&mut commands);
            }
            let target_rect = DeviceIntRect::from_size(level);
            self.record_blit(&source, &target, source_rect, target_rect, TextureFilter::Linear, &mut DrawStats::default())?;
            source = target;
            source_rect = target_rect;
        }
        let serial = self.submissions.submit_serial()?;
        self.failed.set(false);
        self.start_readback(&RenderedFrame { size: [size.width as u32, size.height as u32], origin: DeviceIntPoint::zero(),
            stats: DrawStats::default(), serial, texture: Some(source) }, DeviceIntRect::from_size(size))
    }

    pub fn poll_completion(&self, serial: u64) -> Result<bool> {
        if self.is_failed() { return Err("HAL renderer requires recreation".into()); }
        let result = self.submissions.poll();
        dispatch_releases(&self.releases);
        let result = result.and_then(|completed| self.external_device.poll().map(|_| completed));
        match result {
            Ok(completed) => Ok(completed >= serial),
            Err(error) => { self.failed.set(true); Err(error) }
        }
    }

    pub fn start_readback(&self, frame: &RenderedFrame<A>, rect: DeviceIntRect) -> Result<PendingReadback<A>> {
        if self.is_failed() {
            return Err("HAL renderer must be recreated after an execution failure".into());
        }
        let output = frame.texture.as_ref().ok_or("HAL frame has no output target")?;
        if rect.is_empty() || rect.min.x < 0 || rect.min.y < 0
            || rect.max.x as u32 > frame.size[0] || rect.max.y as u32 > frame.size[1] {
            return Err("Invalid HAL readback rectangle".into());
        }
        self.count(RenderCounter::Readbacks, 1);
        let size = [rect.width() as u32, rect.height() as u32];
        let layout = self.owner.layout(size[0], size[1])?;
        let buffer = self.readback_buffer(&layout)?;
        self.failed.set(true);
        let mut commands = self.submissions.recording()?;
        buffer.transition(&mut commands, wgt::BufferUses::COPY_DST);
        output.transition(&mut commands, wgt::TextureUses::COPY_SRC);
        unsafe {
            commands.encoder().copy_texture_to_buffer(
                &output.raw,
                wgt::TextureUses::COPY_SRC,
                &buffer.raw,
                std::iter::once(hal::BufferTextureCopy {
                    buffer_layout: wgt::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(layout.pitch),
                        rows_per_image: Some(size[1]),
                    },
                    texture_base: hal::TextureCopyBase {
                        mip_level: output.base_mip,
                        array_layer: 0,
                        origin: wgt::Origin3d { x: rect.min.x as u32, y: rect.min.y as u32, z: 0 },
                        aspect: output.copy_aspect(),
                    },
                    size: wgt::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 }.into(),
                }),
            );
        }
        buffer.transition(&mut commands, wgt::BufferUses::MAP_READ);
        drop(commands);
        let serial = self.submissions.submit_serial()?;
        self.failed.set(false);
        Ok(PendingReadback { buffer, layout, serial, size })
    }

    pub fn poll_readback(&self, readback: &PendingReadback<A>, wait: bool) -> Result<Option<Vec<u8>>> {
        if self.is_failed() { return Err("HAL renderer requires recreation".into()); }
        let result = (|| {
            if wait {
                self.submissions.wait_for(readback.serial)?;
            } else if self.submissions.poll()? < readback.serial {
                return Ok(None);
            }
            self.owner.map_readback(&readback.buffer.raw, &readback.layout).map(Some)
        })();
        if result.is_err() { self.failed.set(true); }
        dispatch_releases(&self.releases);
        result
    }
}

impl<A: BackendApi> Drop for FrameRenderer<A> {
    fn drop(&mut self) {
        if !self.is_failed() {
            if let Some(acquired) = self.surface.as_ref().and_then(|surface| surface.acquired.as_ref()) {
                if self.submissions.recording().is_ok() {
                    let _ = self.submissions.submit_surfaces(&[&acquired.texture]);
                }
            }
        }
        self.submissions.shutdown();
        if let Some(surface) = &mut self.surface { surface.discard(); }
        self.external_images.clear();
        self.native_targets.clear();
        self.layer_targets.clear();
        dispatch_releases(&self.releases);
        let _ = self.external_device.finish();
    }
}

fn vertex_layouts(
    descriptor: &VertexDescriptor,
    shader: &ShaderArtifact,
) -> Result<(Vec<wgt::VertexAttribute>, Vec<wgt::VertexAttribute>, u64)> {
    let mut output = [Vec::new(), Vec::new()];
    let mut stride = 0;
    for (slot, attributes) in [descriptor.vertex_attributes, descriptor.instance_attributes]
        .iter()
        .enumerate()
    {
        let mut offset = 0;
        for attribute in *attributes {
            let (scalar, format, size) = match (&attribute.kind, attribute.count) {
                (VertexAttributeKind::U8Norm, 2) => {
                    (ScalarType::Float, wgt::VertexFormat::Unorm8x2, 2)
                }
                (VertexAttributeKind::F32, 2) => {
                    (ScalarType::Float, wgt::VertexFormat::Float32x2, 8)
                }
                (VertexAttributeKind::F32, 4) => {
                    (ScalarType::Float, wgt::VertexFormat::Float32x4, 16)
                }
                (VertexAttributeKind::I32, 4) => {
                    (ScalarType::Sint, wgt::VertexFormat::Sint32x4, 16)
                }
                (VertexAttributeKind::F32, 1) => (ScalarType::Float, wgt::VertexFormat::Float32, 4),
                (VertexAttributeKind::F32, 3) => {
                    (ScalarType::Float, wgt::VertexFormat::Float32x3, 12)
                }
                (VertexAttributeKind::I32, 1) => (ScalarType::Sint, wgt::VertexFormat::Sint32, 4),
                (VertexAttributeKind::I32, 2) => (ScalarType::Sint, wgt::VertexFormat::Sint32x2, 8),
                (VertexAttributeKind::I32, 3) => {
                    (ScalarType::Sint, wgt::VertexFormat::Sint32x3, 12)
                }
                (VertexAttributeKind::U16, 2) => (ScalarType::Uint, wgt::VertexFormat::Uint16x2, 4),
                (VertexAttributeKind::U16, 4) => (ScalarType::Uint, wgt::VertexFormat::Uint16x4, 8),
                (VertexAttributeKind::U8Norm, 4) => {
                    (ScalarType::Float, wgt::VertexFormat::Unorm8x4, 4)
                }
                (VertexAttributeKind::U16Norm, 2) => {
                    (ScalarType::Float, wgt::VertexFormat::Unorm16x2, 4)
                }
                (VertexAttributeKind::U16Norm, 4) => {
                    (ScalarType::Float, wgt::VertexFormat::Unorm16x4, 8)
                }
                _ => return Err(format!("Unsupported HAL vertex attribute {attribute:?}")),
            };
            if let Some(input) = shader
                .inputs
                .iter()
                .find(|input| input.name == attribute.name)
            {
                if input.scalar != scalar || input.components != attribute.count {
                    return Err(format!("HAL vertex interface mismatch for {}", input.name));
                }
                output[slot].push(wgt::VertexAttribute {
                    format,
                    offset,
                    shader_location: input.location,
                });
            }
            offset += size;
        }
        if slot == 1 {
            stride = offset;
        }
    }
    if output[0].len() + output[1].len() != shader.inputs.len() {
        return Err("Missing HAL shader vertex attributes".into());
    }
    let [vertex, instances] = output;
    Ok((vertex, instances, stride))
}

#[cfg(all(test, wr_hal_vulkan))]
mod shader_tests {
    use super::*;

    #[test]
    fn staged_quad_instances_preserve_aa_join_order() {
        for part in [0u32, 1, 2, 3] {
            for edges in [0u32, 2, 8, 10] {
                for aa in [0u32, 8] {
                    let word = (aa << 24) | (edges << 16) | (part << 8) | 17;
                    let words = [13u32, 23, word, 47].map(u32::to_ne_bytes);
                    let source = words.as_flattened();
                    let mut output = vec![0; packed_instance_size(Shader::Quad, source).unwrap()];
                    pack_instances(Shader::Quad, source, &mut output);
                    let mut expected = Vec::new();
                    if aa != 0 && (part == 1 || part == 3) && edges & 2 != 0 {
                        expected.push(if part == 1 { 6 } else { 8 });
                    }
                    expected.push(part);
                    if aa != 0 && (part == 1 || part == 3) && edges & 8 != 0 {
                        expected.push(if part == 1 { 7 } else { 9 });
                    }
                    assert_eq!(output.len(), expected.len() * 16);
                    for (instance, part) in output.chunks_exact(16).zip(expected) {
                        assert_eq!(&instance[..8], &source[..8]);
                        assert_eq!(&instance[12..], &source[12..]);
                        assert_eq!(u32::from_ne_bytes(instance[8..12].try_into().unwrap()),
                            (word & !0xff00) | (part << 8));
                    }
                    let mut unmodified = vec![0; source.len()];
                    pack_instances(Shader::Other("ps_quad_mask", ""), source, &mut unmodified);
                    assert_eq!(unmodified, source);
                }
            }
        }
    }

    #[test]
    #[ignore = "Requires Vulkan"]
    fn pipeline_device_and_cache_isolation() {
        let owner = create_vulkan_device(&Options { validation: true, ..Default::default() }).unwrap();
        let mut first = FrameRenderer::new(owner).unwrap();
        let shader = Shader::Other("ps_clear", "");
        let format = wgt::TextureFormat::Rgba8Unorm;
        let key = first.key(shader, 0, 0, format).unwrap();
        let metadata = first.shader_metadata(shader).unwrap();
        assert!(Rc::ptr_eq(&metadata, &first.shader_metadata(shader).unwrap()));
        first.pipeline(key).unwrap();
        let warm = first.pipelines[&key].clone();
        first.pipeline(key).unwrap();
        assert!(Rc::ptr_eq(&warm, &first.pipelines[&key]));
        first.pipelines.clear();
        first.pipeline(key).unwrap();
        assert!(!Rc::ptr_eq(&warm, &first.pipelines[&key]));
        first.filtering = Filtering::LegacyBrilinear;
        assert_ne!(key, first.key(shader, 0, 0, format).unwrap());
        first.filtering = Filtering::Standard;
        assert_ne!(key, first.key(shader, 0, 1, format).unwrap());
        let other_owner = create_vulkan_device(&Options { validation: true, ..Default::default() }).unwrap();
        let mut other = FrameRenderer::new(other_owner).unwrap();
        let other_key = other.key(shader, 0, 0, format).unwrap();
        assert_ne!(key, other_key);
        assert!(other.pipeline(key).err().unwrap().contains("another device/backend"));
        assert!(other.pipelines.is_empty());
        other.pipeline(other_key).unwrap();
        drop(first);
        other.pipeline(other_key).unwrap();
        assert_eq!(other.pipelines.len(), 1);
    }

    #[test]
    fn all_shader_vertex_interfaces_match_wr_descriptors() {
        for artifact in shaders::SHADERS {
            let shader = Shader::Other(artifact.name, artifact.features);
            vertex_layouts(
                FrameRenderer::<hal::api::Vulkan>::descriptor(shader),
                artifact,
            )
            .unwrap_or_else(|error| panic!("{} {}: {}", artifact.name, artifact.features, error));
        }
    }

    #[test]
    #[ignore = "Requires Vulkan"]
    fn all_shader_pipelines() {
        let owner = create_vulkan_device(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap();
        let mut renderer = FrameRenderer::new(owner).unwrap();
        let mut count = 0;
        for artifact in shaders::SHADERS {
            if artifact.features.contains("DUAL_SOURCE_BLENDING")
                && !renderer
                    .owner
                    .features
                    .contains(wgt::Features::DUAL_SOURCE_BLENDING)
            {
                continue;
            }
            let shader = Shader::Other(artifact.name, artifact.features);
            let format = if artifact.name == "ps_quad_mask" || artifact.features == "ALPHA_TARGET" {
                wgt::TextureFormat::R8Unorm
            } else {
                wgt::TextureFormat::Rgba8Unorm
            };
            let key = renderer.key(shader, 0, 0, format).unwrap();
            renderer.pipeline(key).unwrap();
            count += 1;
        }
        println!("Created {count} current HAL shader pipelines");
    }
    #[cfg(all(feature = "hal-naga", feature = "capture", feature = "replay"))]
    #[test]
    #[ignore = "Requires Vulkan"]
    fn capture_shader_input_identity() {
        use crate::capture::CaptureConfig;
        use crate::render_api::CaptureBits;
        let owner = create_vulkan_device(&Options { validation: true, ..Default::default() }).unwrap();
        let mut renderer = FrameRenderer::new(owner).unwrap();
        let root = std::env::temp_dir().join(format!("wr-shader-identity-{}", std::process::id()));
        let config = || CaptureConfig::new(root.clone(), CaptureBits::all());
        let marker = config().resource_root().join("hal-shader-input.txt");
        for mode in [ShaderInputMode::Native, ShaderInputMode::Naga] {
            renderer.shader_input = mode;
            renderer.save_capture(config(), Vec::new(), None).unwrap();
            assert_eq!(std::fs::read_to_string(&marker).unwrap(), mode.name());
            renderer.load_capture(config(), Vec::new()).unwrap();
            std::fs::write(&marker, "incompatible-translator").unwrap();
            assert!(renderer.load_capture(config(), Vec::new()).unwrap_err().contains("Capture shader input"));
            std::fs::remove_file(&marker).unwrap();
            let legacy = renderer.load_capture(config(), Vec::new());
            assert_eq!(legacy.is_ok(), mode == ShaderInputMode::Native);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    pub(super) fn pixels(
        renderer: &FrameRenderer<hal::api::Vulkan>,
        texture: &Rc<Texture<hal::api::Vulkan>>,
    ) -> Vec<u8> {
        let layout = ReadbackLayout::with_pixel_size(texture.size.width, texture.size.height,
            renderer.owner.capabilities.alignments.buffer_copy_pitch.get(),
            super::super::resources::bytes_per_pixel(texture.format) as u32).unwrap();
        let buffer = Buffer::readback(&renderer.owner, &layout).unwrap();
        let mut commands = renderer.submissions.recording().unwrap();
        commands.keep(buffer.clone());
        texture.transition(&mut commands, wgt::TextureUses::COPY_SRC);
        unsafe {
            commands.encoder().copy_texture_to_buffer(
                &texture.raw,
                wgt::TextureUses::COPY_SRC,
                &buffer.raw,
                std::iter::once(hal::BufferTextureCopy {
                    buffer_layout: wgt::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(layout.pitch),
                        rows_per_image: Some(texture.size.height),
                    },
                    texture_base: hal::TextureCopyBase {
                        mip_level: texture.base_mip,
                        array_layer: 0,
                        origin: wgt::Origin3d::ZERO,
                        aspect: texture.copy_aspect(),
                    },
                    size: texture.size.into(),
                }),
            );
        }
        buffer.transition(&mut commands, wgt::BufferUses::MAP_READ);
        drop(commands);
        renderer.submissions.wait().unwrap();
        renderer.owner.map_readback(&buffer.raw, &layout).unwrap()
    }

    #[test]
    #[ignore = "Requires Vulkan"]
    fn data_staging_preserves_padding_and_empty_updates() {
        let owner = create_vulkan_device(&Options { validation: true, ..Options::default() }).unwrap();
        let renderer = FrameRenderer::new(owner).unwrap();
        for format in [wgt::TextureFormat::Rgba32Float, wgt::TextureFormat::Rgba32Sint] {
            let texture = Texture::new(&renderer.owner, 3, 4, format, TextureFilter::Nearest, false).unwrap();
            let mut expected = vec![0; 3 * 4 * 16];
            for (height, data) in [(2, vec![23u8; 80]), (1, vec![47; 13]), (1, Vec::new())] {
                expected[..3 * height as usize * 16].fill(0);
                expected[..data.len()].copy_from_slice(&data);
                texture.upload_zero_padded(&renderer.owner, &renderer.submissions,
                    DeviceIntRect::from_size(DeviceIntSize::new(3, height)), &data).unwrap();
                assert_eq!(pixels(&renderer, &texture), expected);
            }
        }
    }

    #[test]
    #[ignore = "Requires Vulkan"]
    fn empty_passes_preserve_initialization_and_clear_effects() {
        let owner = create_vulkan_device(&Options { validation: true, ..Options::default() }).unwrap();
        let mut renderer = FrameRenderer::new(owner).unwrap();
        let target = Texture::new(&renderer.owner, 4, 4, wgt::TextureFormat::Rgba8Unorm,
            TextureFilter::Nearest, true).unwrap();
        let rect = DeviceIntRect::from_size(DeviceIntSize::new(4, 4));
        let mut stats = DrawStats::default();
        renderer.draw_pass(&target, &[], &HashMap::new(), &mut stats).unwrap();
        assert_eq!(stats.native_passes, 1);
        assert_eq!(pixels(&renderer, &target), vec![0; 4 * 4 * 4]);
        renderer.draw_pass(&target, &[], &HashMap::new(), &mut stats).unwrap();
        assert_eq!(stats.native_passes, 1);
        assert_eq!(target.current_usage(), wgt::TextureUses::RESOURCE);
        let clear = renderer.clear(rect, ColorF::WHITE);
        renderer.draw_pass(&target, &[clear], &HashMap::new(), &mut stats).unwrap();
        assert_eq!(stats.native_passes, 2);
        renderer.draw_pass(&target, &[], &HashMap::new(), &mut stats).unwrap();
        assert_eq!(stats.native_passes, 2);
        assert_eq!(pixels(&renderer, &target), vec![255; 4 * 4 * 4]);
    }

    #[test]
    #[ignore = "Requires Vulkan"]
    fn batched_blits_preserve_order_across_alias_copies() {
        let owner = create_vulkan_device(&Options { validation: true, ..Options::default() }).unwrap();
        let mut renderer = FrameRenderer::new(owner).unwrap();
        let rect = DeviceIntRect::from_size(DeviceIntSize::new(4, 4));
        let left = DeviceIntRect::from_size(DeviceIntSize::new(2, 4));
        let right = left.translate(DeviceIntVector2D::new(2, 0));
        let source = Texture::new(&renderer.owner, 4, 4, wgt::TextureFormat::Rgba8Unorm,
            TextureFilter::Nearest, false).unwrap();
        let source_pixels: Vec<u8> = (0..4).flat_map(|y| {
            (0..4).flat_map(move |x| [23 + x * 47, 17 + y * 53, 89, 255])
        }).collect();
        source.upload_recorded(&renderer.owner, &renderer.submissions, rect,
            &source_pixels, None, 0, None).unwrap();
        let mut results = Vec::new();
        for batch in [false, true] {
            let target = Texture::new(&renderer.owner, 4, 4, wgt::TextureFormat::Rgba8Unorm,
                TextureFilter::Nearest, true).unwrap();
            let mut stats = DrawStats::default();
            let mut draws = Vec::new();
            for (image, from, to) in [
                (&source, left, left),
                (&source, right, right),
                (&target, left, right),
                (&source, rect, left),
            ] {
                if batch {
                    renderer.queue_blit(image, &target, from, to, TextureFilter::Nearest,
                        &mut draws, &mut stats).unwrap();
                } else {
                    renderer.record_blit(image, &target, from, to, TextureFilter::Nearest, &mut stats).unwrap();
                }
            }
            renderer.draw_pass(&target, &draws, &HashMap::new(), &mut stats).unwrap();
            results.push((pixels(&renderer, &target), stats.native_passes));
        }
        assert_eq!(results[0].0, results[1].0);
        assert!(results[1].1 < results[0].1);
    }

    #[test]
    #[ignore = "Requires Vulkan"]
    fn blend_store_precision() {
        let owner = create_vulkan_device(&Options { validation: true, ..Options::default() }).unwrap();
        let mut renderer = FrameRenderer::new(owner).unwrap();
        let target = Texture::new(&renderer.owner, 4, 4, wgt::TextureFormat::Rgba8Unorm,
            TextureFilter::Nearest, true).unwrap();
        let rect = DeviceIntRect::from_size(DeviceIntSize::new(4, 4));
        for color in [
            ColorF::new(0.5627743005752563, 0.21779930591583252, 0.17092502117156982, 0.8750019073486328),
            ColorF::new(0.5627743005752563, 0.21779930591583252, 0.17092502117156982, 0.8750019669532776),
            ColorF::new(0.5, 0.5, 0.5, 0.5),
        ] {
            for blend in [0, 1] {
                let clear = renderer.clear(rect, ColorF::WHITE);
                renderer.draw_pass(&target, &[clear], &HashMap::new(), &mut DrawStats::default()).unwrap();
                let mut draw = renderer.clear(rect, color);
                draw.clear_color = None;
                draw.blend = blend;
                renderer.draw_pass(&target, &[draw], &HashMap::new(), &mut DrawStats::default()).unwrap();
                let data = pixels(&renderer, &target);
                for (actual, channel) in data[..3].iter().zip([color.r, color.g, color.b]) {
                    let ideal = (channel + if blend == 1 { 1.0 - color.a } else { 0.0 }).clamp(0.0, 1.0) * 255.0;
                    assert!((*actual as f32 - ideal).abs() <= 2.0);
                }
                println!("BLEND_STORE {:?} {} {:?}", color, blend, &data[..4]);
            }
        }
    }

    #[test]
    #[ignore = "Requires Vulkan"]
    fn blend_input_precision() {
        let owner = create_vulkan_device(&Options { validation: true, ..Options::default() }).unwrap();
        let mut renderer = FrameRenderer::new(owner).unwrap();
        let target = Texture::new(&renderer.owner, 4, 4, wgt::TextureFormat::Rgba8Unorm,
            TextureFilter::Nearest, true).unwrap();
        let rect = DeviceIntRect::from_size(DeviceIntSize::new(4, 4));
        for (color, background) in [
            (ColorF::new(0.10018382221460342, 0.06511948257684708, 0.00834865216165781, 0.10686274617910385), [251u8, 212, 46, 255]),
            (ColorF::new(0.3471840023994446, 0.23284552991390228, 0.04814836010336876, 0.36237290501594543), [253, 221, 48, 255]),
            (ColorF::new(0.17377522587776184, 0.11590474098920822, 0.02215271070599556, 0.18144430220127106), [253, 224, 48, 255]),
            (ColorF::new(0.13054342567920685, 0.08800264447927475, 0.01832490786910057, 0.1356818675994873), [253, 224, 48, 255]),
            (ColorF::new(0.5, 0.24901962280273438, 0.5, 0.5), [0, 128, 0, 255]),
            (ColorF::new(0.5, 0.24901960790157318, 0.5, 0.5), [0, 128, 0, 255]),
        ] {
            target.upload_recorded(&renderer.owner, &renderer.submissions, rect, &background.repeat(16), None, 0, None).unwrap();
            let mut draw = renderer.clear(rect, color);
            draw.clear_color = None;
            draw.blend = 1;
            renderer.draw_pass(&target, &[draw], &HashMap::new(), &mut DrawStats::default()).unwrap();
            let data = pixels(&renderer, &target);
            for ((actual, channel), backdrop) in data[..3].iter().zip([color.r, color.g, color.b]).zip(background) {
                let ideal = (channel * 255.0 + (1.0 - color.a) * backdrop as f32).clamp(0.0, 255.0);
                assert!((*actual as f32 - ideal).abs() <= 2.0);
            }
            println!("BLEND_INPUT {:?} {:?} {:?}", color, background, &data[..4]);
        }
    }

    #[test]
    #[ignore = "Requires Vulkan"]
    fn unorm_sample_precision() {
        let owner = create_vulkan_device(&Options { validation: true, ..Options::default() }).unwrap();
        let mut renderer = FrameRenderer::new(owner).unwrap();
        let source = Texture::new(&renderer.owner, 4, 4, wgt::TextureFormat::Rgba8Unorm,
            TextureFilter::Linear, false).unwrap();
        let target = Texture::new(&renderer.owner, 4, 4, wgt::TextureFormat::Rgba32Float,
            TextureFilter::Nearest, true).unwrap();
        let rect = DeviceIntRect::from_size(DeviceIntSize::new(4, 4));
        for color in [[144, 48, 24, 191], [16, 48, 96, 128], [1, 64, 127, 255]] {
            source.upload_recorded(&renderer.owner, &renderer.submissions, rect, &color.repeat(16), None, 0, None).unwrap();
            for filter in [TextureFilter::Nearest, TextureFilter::Linear] {
                let draw = Draw {
                    shader: Shader::Other("cs_scale", "TEXTURE_2D"), blend: 0, depth: 0, count: 1,
                    instances: Instances::owned(bytes(&[ScalingInstance::new(rect.to_f32(), rect.to_f32(), false)])),
                    textures: renderer.single_texture(source.clone()), filter: Some(filter), clear_color: None,
                    count_in_stats: false, readback: None, scissor: rect,
                };
                renderer.draw_pass(&target, &[draw], &HashMap::new(), &mut DrawStats::default()).unwrap();
                let data = pixels(&renderer, &target);
                let values: Vec<_> = data[..16].chunks_exact(4).map(|v| f32::from_le_bytes(v.try_into().unwrap())).collect();
                for (actual, expected) in values.iter().zip(color) {
                    assert!((actual - expected as f32 / 255.0).abs() < 1.0 / 255.0);
                }
                println!("UNORM_SAMPLE {:?} {:?} {:?}", color, filter, values);
            }
        }
    }

    #[test]
    #[ignore = "Requires Vulkan"]
    fn sampler_lod_and_mip_probes() {
        sampler_probes(Filtering::Standard);
    }

    #[test]
    #[ignore = "Requires Vulkan"]
    fn legacy_brilinear_lod_and_mip_probes() {
        sampler_probes(Filtering::LegacyBrilinear);
    }

    fn sampler_probes(filtering: Filtering) {
        let owner = create_vulkan_device(&Options { validation: true, ..Options::default() }).unwrap();
        let mut renderer = FrameRenderer::new(owner).unwrap();
        renderer.filtering = filtering;
        let rect = |w, h| DeviceIntRect::from_size(DeviceIntSize::new(w, h));
        let source = Texture::new(&renderer.owner, 256, 256, wgt::TextureFormat::Rgba8Unorm,
                                  TextureFilter::Trilinear, true).unwrap();
        for level in 0..source.mip_count {
            let view = source.mip_view(level).unwrap();
            let color = [(level * 28) as u8, 0, 0, 255];
            view.upload_recorded(&renderer.owner, &renderer.submissions,
                rect(view.size.width as i32, view.size.height as i32),
                &color.repeat((view.size.width * view.size.height) as usize), None, 0, None).unwrap();
            assert_eq!(pixels(&renderer, &view), color.repeat((view.size.width * view.size.height) as usize));
        }
        let target = Texture::new(&renderer.owner, 32, 32, wgt::TextureFormat::Rgba8Unorm,
                                  TextureFilter::Nearest, true).unwrap();
        let diagnostic = std::env::var_os("WR_HAL_SAMPLER_PROBES");
        for extent in (16..=128).step_by(2).chain([192, 224, 254, 256]) {
            renderer.record_blit(&source, &target, rect(extent, extent), rect(32, 32),
                TextureFilter::Trilinear, &mut DrawStats::default()).unwrap();
            let data = pixels(&renderer, &target);
            let actual = data[(16 * 32 + 16) * 4];
            let expected = ((extent as f32 / 32.0).log2().max(0.0) * 28.0).round() as i32;
            println!("SAMPLER_LOD {extent} {actual} {expected}");
            if diagnostic.is_none() && filtering == Filtering::Standard {
                // Mesa's linear log2 approximation errs by at most 0.087 LOD, plus UNORM rounding.
                assert!((actual as i32 - expected).abs() <= 4, "extent {}: {} vs {}", extent, actual, expected);
                if (extent as u32).is_power_of_two() {
                    assert_eq!(actual as i32, expected);
                }
            }
            if filtering == Filtering::LegacyBrilinear {
                let anchors = [(16, 0), (32, 0), (38, 0), (40, 3), (48, 20),
                    (52, 28), (56, 28), (64, 28), (80, 31), (96, 48), (128, 56), (192, 76), (256, 84)];
                if let Some((_, expected)) = anchors.iter().find(|(size, _)| *size == extent) {
                    assert!((actual as i32 - expected).abs() <= 1, "legacy footprint {}: {} vs {}", extent, actual, expected);
                }
            }
        }
        for level in [0, 2, 5, 8] {
            let view = source.mip_view(level).unwrap();
            renderer.record_blit(&view, &target, rect(view.size.width as i32, view.size.height as i32),
                rect(32, 32), TextureFilter::Trilinear, &mut DrawStats::default()).unwrap();
            assert_eq!(pixels(&renderer, &target), [(level * 28) as u8, 0, 0, 255].repeat(1024));
        }
        for (width, height) in [(17, 9), (1, 17), (17, 1), (16, 16)] {
            for format in [wgt::TextureFormat::Rgba8Unorm, wgt::TextureFormat::Bgra8Unorm, wgt::TextureFormat::R8Unorm] {
                let texture = Texture::new(&renderer.owner, width, height, format,
                    TextureFilter::Trilinear, true).unwrap();
                let mut data = Vec::new();
                for y in 0..height {
                    for x in 0..width {
                        let alpha = if (x + y) % 2 == 0 { 255 } else { 64 };
                        if format == wgt::TextureFormat::R8Unorm {
                            data.push(alpha as u8);
                        } else {
                            data.extend_from_slice(&[(x * 7).min(alpha) as u8, (y * 7).min(alpha) as u8, 0, alpha as u8]);
                        }
                    }
                }
                texture.upload_recorded(&renderer.owner, &renderer.submissions,
                    rect(width as i32, height as i32), &data, None, 0, None).unwrap();
                assert_eq!(pixels(&renderer, &texture.mip_view(0).unwrap()), data);
                renderer.generate_mips(&texture).unwrap();
                for level in 0..texture.mip_count {
                    let view = texture.mip_view(level).unwrap();
                    let data = pixels(&renderer, &view);
                    if let Some(directory) = &diagnostic {
                        let path = std::path::Path::new(directory);
                        std::fs::create_dir_all(path).unwrap();
                        std::fs::write(path.join(format!("mip-{width}x{height}-{format:?}-{level}-{}x{}.bin",
                            view.size.width, view.size.height)), data).unwrap();
                    }
                }
            }
        }
        for format in [wgt::TextureFormat::Rg8Unorm, wgt::TextureFormat::R16Unorm, wgt::TextureFormat::Rg16Unorm] {
            let texture = Texture::new(&renderer.owner, 7, 5, format, TextureFilter::Linear, false).unwrap();
            let bpp = super::super::resources::bytes_per_pixel(format);
            let data: Vec<_> = (0..35 * bpp).map(|n| (n * 29) as u8).collect();
            texture.upload_recorded(&renderer.owner, &renderer.submissions, rect(7, 5), &data, None, 0, None).unwrap();
            assert_eq!(pixels(&renderer, &texture), data);
            assert_eq!(texture.mip_count, 1);
        }
    }

    #[test]
    #[ignore = "Requires Vulkan"]
    fn transformed_sampler_profiles() {
        let owner = create_vulkan_device(&Options { validation: true, ..Options::default() }).unwrap();
        let mut renderer = FrameRenderer::new(owner).unwrap();
        let source = Texture::new(&renderer.owner, 256, 256, wgt::TextureFormat::Rgba8Unorm,
            TextureFilter::Trilinear, true).unwrap();
        for level in 0..source.mip_count {
            let view = source.mip_view(level).unwrap();
            let rect = DeviceIntRect::from_size(DeviceIntSize::new(view.size.width as i32, view.size.height as i32));
            view.upload_recorded(&renderer.owner, &renderer.submissions, rect,
                &[(level * 28) as u8, 0, 0, 255].repeat((view.size.width * view.size.height) as usize), None, 0, None).unwrap();
        }
        let target = Texture::new(&renderer.owner, 64, 64, wgt::TextureFormat::Rgba8Unorm,
            TextureFilter::Nearest, true).unwrap();
        let full = DeviceIntRect::from_size(DeviceIntSize::new(64, 64));
        let target_rect = DeviceRect::new(DevicePoint::new(16.0, 16.0), DevicePoint::new(48.0, 48.0));
        let c = std::f32::consts::FRAC_1_SQRT_2;
        for policy in [Filtering::Standard, Filtering::LegacyBrilinear] {
            renderer.filtering = policy;
            for (name, transform, uv, legacy) in [
                ("identity", [1.0, 0.0, 0.0, 1.0], [0.0, 0.0, 64.0, 64.0], 28),
                ("rotate45", [c, -c, c, c], [0.0, 0.0, 64.0, 64.0], 14),
                ("rotate90", [0.0, -1.0, 1.0, 0.0], [0.0, 0.0, 64.0, 64.0], 28),
                ("shear", [1.0, 1.0, 0.0, 1.0], [0.0, 0.0, 64.0, 64.0], 28),
                ("anisotropic", [0.5, 0.0, 0.0, 2.0], [0.0, 0.0, 64.0, 64.0], 56),
                ("cropped", [1.0, 0.0, 0.0, 1.0], [70.0, 30.0, 134.0, 94.0], 28),
                ("flipped", [1.0, 0.0, 0.0, 1.0], [96.0, 96.0, 32.0, 32.0], 28),
                ("nonsquare", [1.0, 0.0, 0.0, 1.0], [0.0, 0.0, 128.0, 64.0], 56),
                ("transition", [c, -c, c, c], [0.0, 0.0, 80.0, 80.0], 28),
            ] {
                let [a, b, c, d] = transform;
                let matrix = [a/32.0, -c/32.0, 0.0, 0.0, b/32.0, -d/32.0, 0.0, 0.0,
                    0.0, 0.0, 0.0, 0.0, -a-b, c+d, 0.0, 1.0];
                let source_rect = DeviceRect::new(DevicePoint::new(uv[0], uv[1]), DevicePoint::new(uv[2], uv[3]));
                for filter in [TextureFilter::Trilinear, TextureFilter::Linear, TextureFilter::Nearest] {
                    renderer.projection_override = None;
                    let clear = renderer.clear(full, ColorF::new(1.0, 0.0, 1.0, 1.0));
                    renderer.draw_pass(&target, &[clear], &HashMap::new(), &mut DrawStats::default()).unwrap();
                    renderer.projection_override = Some(matrix);
                    let draw = Draw { shader: Shader::Other("cs_scale", "TEXTURE_2D"), blend: 0, depth: 0, count: 1,
                        instances: Instances::owned(bytes(&[ScalingInstance::new(target_rect, source_rect, false)])),
                        textures: renderer.single_texture(source.clone()), filter: Some(filter), clear_color: None,
                        count_in_stats: false, readback: None, scissor: full };
                    renderer.draw_pass(&target, &[draw], &HashMap::new(), &mut DrawStats::default()).unwrap();
                    let data = pixels(&renderer, &target);
                    let pixel = &data[(32*64+32)*4..(32*64+33)*4];
                    assert_eq!(&pixel[1..], &[0, 0, 255]);
                    if filter != TextureFilter::Trilinear {
                        assert_eq!(pixel[0], 0);
                    } else if policy == Filtering::LegacyBrilinear {
                        assert!((pixel[0] as i32-legacy).abs()<=1, "{}: {} vs {}", name, pixel[0], legacy);
                    } else {
                        let determinant = a*d-b*c;
                        let ex = (uv[2]-uv[0])/32.0;
                        let ey = (uv[3]-uv[1])/32.0;
                        let j = [ex*d/determinant, -ex*b/determinant, -ey*c/determinant, ey*a/determinant];
                        let lower = j.iter().map(|v| v.abs()).fold(0.0f32, f32::max);
                        let upper = std::f32::consts::SQRT_2*(j[0].abs()+j[2].abs()).max(j[1].abs()+j[3].abs());
                        let low = 28.0*lower.log2().max(0.0)-4.0;
                        let high = 28.0*upper.log2().max(0.0)+4.0;
                        assert!((low..=high).contains(&(pixel[0] as f32)), "{}: {} outside {}..{}", name, pixel[0], low, high);
                    }
                    println!("TRANSFORMED_SAMPLER {:?} {} {:?} {:?}", policy, name, filter, pixel);
                }
            }
            for level in [0, 2, 5, 8] {
                renderer.projection_override = None;
                let clear = renderer.clear(full, ColorF::new(1.0, 0.0, 1.0, 1.0));
                renderer.draw_pass(&target, &[clear], &HashMap::new(), &mut DrawStats::default()).unwrap();
                renderer.projection_override = Some([c/32.0, -c/32.0, 0.0, 0.0, -c/32.0, -c/32.0, 0.0, 0.0,
                    0.0, 0.0, 0.0, 0.0, 0.0, 2.0*c, 0.0, 1.0]);
                let view = source.mip_view(level).unwrap();
                let source_rect = DeviceRect::from_size(DeviceSize::new(view.size.width as f32, view.size.height as f32));
                let draw = Draw { shader: Shader::Other("cs_scale", "TEXTURE_2D"), blend: 0, depth: 0, count: 1,
                    instances: Instances::owned(bytes(&[ScalingInstance::new(target_rect, source_rect, false)])),
                    textures: renderer.single_texture(view), filter: Some(TextureFilter::Trilinear), clear_color: None,
                    count_in_stats: false, readback: None, scissor: full };
                renderer.draw_pass(&target, &[draw], &HashMap::new(), &mut DrawStats::default()).unwrap();
                let data = pixels(&renderer, &target);
                assert_eq!(&data[(32*64+32)*4..(32*64+33)*4], &[(level*28) as u8, 0, 0, 255]);
            }
        }
    }

    #[test]
    #[ignore = "Requires Vulkan"]
    fn transfers_mips_and_preserved_regions() {
        let device = create_vulkan_device(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap();
        let mut renderer = FrameRenderer::new(device).unwrap();
        let source = Texture::new(
            &renderer.owner,
            7,
            5,
            wgt::TextureFormat::Rgba8Unorm,
            TextureFilter::Nearest,
            false,
        )
        .unwrap();
        let full = DeviceIntRect::from_size(DeviceIntSize::new(7, 5));
        source
            .upload_recorded(
                &renderer.owner,
                &renderer.submissions,
                full,
                &[255, 0, 0, 255].repeat(35),
                None,
                0,
                None,
            )
            .unwrap();
        let target = Texture::new(
            &renderer.owner,
            9,
            7,
            wgt::TextureFormat::Bgra8Unorm,
            TextureFilter::Nearest,
            true,
        )
        .unwrap();
        let rect = |x, y, w, h| {
            DeviceIntRect::from_origin_and_size(DeviceIntPoint::new(x, y), DeviceIntSize::new(w, h))
        };
        renderer
            .copy(&source, &target, rect(1, 1, 3, 2), rect(4, 3, 3, 2))
            .unwrap();
        renderer
            .copy(&target, &target, rect(4, 3, 3, 2), rect(3, 3, 3, 2))
            .unwrap();
        let output = pixels(&renderer, &target);
        for y in 0..7 {
            for x in 0..9 {
                assert_eq!(
                    &output[(y * 9 + x) * 4..(y * 9 + x + 1) * 4],
                    if (3..7).contains(&x) && (3..5).contains(&y) {
                        &[0, 0, 255, 255]
                    } else {
                        &[0; 4]
                    }
                );
            }
        }
        let mipmapped = Texture::new(
            &renderer.owner,
            17,
            9,
            wgt::TextureFormat::Rgba8Unorm,
            TextureFilter::Trilinear,
            true,
        )
        .unwrap();
        for color in [[0, 255, 0, 255], [0, 0, 255, 255]] {
            mipmapped
                .upload_recorded(
                    &renderer.owner,
                    &renderer.submissions,
                    rect(0, 0, 17, 9),
                    &color.repeat(153),
                    None,
                    0,
                    None,
                )
                .unwrap();
            renderer.generate_mips(&mipmapped).unwrap();
            for level in 0..mipmapped.mip_count {
                let view = mipmapped.mip_view(level).unwrap();
                assert_eq!(
                    pixels(&renderer, &view),
                    color.repeat((view.size.width * view.size.height) as usize)
                );
            }
        }
    }
    #[test]
    #[ignore = "Requires Vulkan"]
    fn encoding_error_releases_pooled_resources() {
        use crate::internal_types::{TextureUpdateList, TextureCacheUpdate};
        let device = create_vulkan_device(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap();
        let mut renderer = FrameRenderer::new(device).unwrap();
        let owner = renderer.owner.clone();
        let texture = Texture::new(
            &owner,
            4,
            4,
            wgt::TextureFormat::Rgba8Unorm,
            TextureFilter::Nearest,
            true,
        )
        .unwrap();
        let id = CacheTextureId(777);
        renderer.textures.insert(id, texture);
        let mut updates = TextureUpdateList::new();
        for rect in [
            DeviceIntRect::from_size(DeviceIntSize::new(4, 4)),
            DeviceIntRect::from_origin_and_size(
                DeviceIntPoint::new(-1, 0),
                DeviceIntSize::new(4, 4),
            ),
        ] {
            updates.push_update(
                id,
                TextureCacheUpdate {
                    rect,
                    stride: None,
                    offset: 0,
                    format_override: None,
                    source: TextureUpdateSource::Bytes {
                        data: std::sync::Arc::new(vec![255; 64]),
                    },
                },
            );
        }
        assert!(renderer
            .update_resources(vec![ResourceUpdateList {
                native_surface_updates: Vec::new(),
                texture_updates: updates
            }])
            .is_err());
        assert!(renderer
            .update_resources(Vec::new())
            .unwrap_err()
            .contains("recreated"));
        drop(renderer);
        assert_eq!(owner.memory.get().buffers, 0);
        assert_eq!(owner.memory.get().textures, 0);
    }
}
