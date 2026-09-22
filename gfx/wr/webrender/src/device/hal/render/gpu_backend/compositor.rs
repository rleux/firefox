/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::composite as comp;
use comp::{
    NativeSurfaceId, NativeSurfaceInfo, NativeSurfaceHandle, NativeSurfaceOperationDetails as Op,
};

pub(super) struct Targets<A: BackendApi> {
    pub targets: HashMap<u64, BoundTarget<A>>,
    pub current: Option<u64>,
    operations: Vec<comp::NativeSurfaceOperation>,
    owner: Rc<Device<A>>,
    device: ExternalImageDevice,
    queue: Rc<SubmissionQueue<A>>,
    releases: ReleaseQueue,
    next: u64,
}

impl<A: BackendApi> Targets<A> {
    pub fn new(gpu: &FrameRenderer<A>) -> Self {
        Self {
            targets: HashMap::new(),
            current: None,
            operations: Vec::new(),
            owner: gpu.owner.clone(),
            device: gpu.external_image_device(),
            queue: gpu.submissions.clone(),
            releases: gpu.releases.clone(),
            next: 1,
        }
    }

    fn check<T>(&self, result: Result<T>) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                self.owner.lost.set(true);
                log::error!("HAL compositor failed: {error}");
                None
            }
        }
    }

    fn bind(&mut self, target: CompositorTarget) -> Result<NativeSurfaceInfo> {
        target.image.attach_releases(&self.releases);
        target.image.attach_metrics(self.owner.metrics.as_ref());
        let image = match &target.image.source {
            ExternalImageSource::Native(image) => image,
            _ => return Err("HAL compositor target must be a native image".into()),
        };
        let texture = image.texture(&self.owner)?;
        if texture.target.is_none()
            || target.origin.x < 0
            || target.origin.y < 0
            || target.size.is_empty()
            || i64::from(target.origin.x) + i64::from(target.size.width)
                > i64::from(texture.size.width)
            || i64::from(target.origin.y) + i64::from(target.size.height)
                > i64::from(texture.size.height)
        {
            return Err("Invalid HAL compositor target extent".into());
        }
        let return_usage = if texture.current_usage() == wgt::TextureUses::UNINITIALIZED {
            wgt::TextureUses::RESOURCE
        } else {
            texture.current_usage()
        };
        let texture =
            texture.with_lease(target.image.state.clone(), TextureFilter::Linear, false)?;
        let handle = self.next;
        self.next = handle
            .checked_add(1)
            .ok_or("HAL compositor handle exhaustion")?;
        self.targets.insert(
            handle,
            BoundTarget {
                texture,
                origin: target.origin,
                size: target.size,
                return_usage,
            },
        );
        self.current = Some(handle);
        Ok(NativeSurfaceInfo {
            origin: target.origin,
            handle: NativeSurfaceHandle(handle),
        })
    }

    fn unbind(&mut self) {
        if let Some(handle) = self.current.take() {
            if let Some(target) = self.targets.remove(&handle) {
                if let Some(mut commands) = self.check(self.queue.recording()) {
                    target
                        .texture
                        .transition(&mut commands, target.return_usage);
                }
            }
        }
    }
}

struct SharedNative(Rc<RefCell<Box<dyn NativeCompositor>>>);
impl NativeCompositor for SharedNative {
    fn update_surfaces(
        &mut self,
        device: &ExternalImageDevice,
        ops: &[comp::NativeSurfaceOperation],
    ) -> Result<()> {
        self.0.borrow_mut().update_surfaces(device, ops)
    }
    fn bind_tile(
        &mut self,
        id: NativeTileId,
        dirty: DeviceIntRect,
        valid: DeviceIntRect,
    ) -> Result<CompositorTarget> {
        self.0.borrow_mut().bind_tile(id, dirty, valid)
    }
    fn read_tile(&mut self, id: NativeTileId) -> Result<CompositorTarget> {
        self.0.borrow_mut().read_tile(id)
    }
    fn end_frame(
        &mut self,
        descriptor: &comp::CompositeDescriptor,
        completion: FrameCompletion,
    ) -> Result<()> {
        self.0.borrow_mut().end_frame(descriptor, completion)
    }
}

struct NativeBridge<A: BackendApi> {
    user: Rc<RefCell<Box<dyn NativeCompositor>>>,
    targets: Rc<RefCell<Targets<A>>>,
    capabilities: comp::CompositorCapabilities,
}
impl<A: BackendApi> NativeBridge<A> {
    fn operation(&mut self, details: Op) {
        let mut targets = self.targets.borrow_mut();
        let operation = comp::NativeSurfaceOperation { details };
        if targets
            .check(
                self.user
                    .borrow_mut()
                    .update_surfaces(&targets.device, std::slice::from_ref(&operation)),
            )
            .is_some()
        {
            targets.operations.push(operation);
        }
    }
}
impl<A: BackendApi> comp::Compositor for NativeBridge<A> {
    fn create_surface(
        &mut self,
        id: NativeSurfaceId,
        virtual_offset: DeviceIntPoint,
        tile_size: DeviceIntSize,
        is_opaque: bool,
    ) {
        self.operation(Op::CreateSurface {
            id,
            virtual_offset,
            tile_size,
            is_opaque,
        });
    }
    fn create_external_surface(&mut self, id: NativeSurfaceId, is_opaque: bool) {
        self.operation(Op::CreateExternalSurface { id, is_opaque });
    }
    fn create_backdrop_surface(&mut self, id: NativeSurfaceId, color: ColorF) {
        self.operation(Op::CreateBackdropSurface { id, color });
    }
    fn destroy_surface(&mut self, id: NativeSurfaceId) {
        self.operation(Op::DestroySurface { id });
    }
    fn create_tile(&mut self, id: NativeTileId) {
        self.operation(Op::CreateTile { id });
    }
    fn destroy_tile(&mut self, id: NativeTileId) {
        self.operation(Op::DestroyTile { id });
    }
    fn attach_external_image(&mut self, id: NativeSurfaceId, external_image: api::ExternalImageId) {
        self.operation(Op::AttachExternalImage { id, external_image });
    }
    fn bind(
        &mut self,
        id: NativeTileId,
        dirty: DeviceIntRect,
        valid: DeviceIntRect,
    ) -> NativeSurfaceInfo {
        let result = self
            .user
            .borrow_mut()
            .bind_tile(id, dirty, valid)
            .and_then(|target| self.targets.borrow_mut().bind(target));
        self.targets
            .borrow()
            .check(result)
            .unwrap_or(NativeSurfaceInfo {
                origin: DeviceIntPoint::zero(),
                handle: NativeSurfaceHandle::DEFAULT,
            })
    }
    fn unbind(&mut self) {
        self.targets.borrow_mut().unbind();
    }
    fn begin_frame(&mut self) {}
    fn add_surface(
        &mut self,
        _: NativeSurfaceId,
        _: comp::CompositorSurfaceTransform,
        _: DeviceIntRect,
        _: api::ImageRendering,
        _: DeviceIntRect,
        _: comp::ClipRadius,
    ) {
    }
    fn end_frame(&mut self) {}
    fn enable_native_compositor(&mut self, _: bool) {}
    fn deinit(&mut self) {
        self.targets.borrow_mut().unbind();
    }
    fn get_capabilities(&self) -> comp::CompositorCapabilities {
        self.capabilities
    }
    fn get_window_visibility(&self) -> comp::WindowVisibility {
        Default::default()
    }
}

struct SharedLayer(Rc<RefCell<Box<dyn LayerCompositor>>>);
impl LayerCompositor for SharedLayer {
    fn begin_frame(
        &mut self,
        device: &ExternalImageDevice,
        config: &comp::CompositorInputConfig,
    ) -> Result<()> {
        self.0.borrow_mut().begin_frame(device, config)
    }
    fn bind_layer(&mut self, index: usize, dirty: &[DeviceIntRect]) -> Result<CompositorTarget> {
        self.0.borrow_mut().bind_layer(index, dirty)
    }
    fn end_frame(&mut self, completion: FrameCompletion) -> Result<()> {
        self.0.borrow_mut().end_frame(completion)
    }
}

struct LayerBridge<A: BackendApi> {
    user: Rc<RefCell<Box<dyn LayerCompositor>>>,
    targets: Rc<RefCell<Targets<A>>>,
}
impl<A: BackendApi> comp::LayerCompositor for LayerBridge<A> {
    fn begin_frame(&mut self, config: &comp::CompositorInputConfig) -> bool {
        let targets = self.targets.borrow();
        targets.check(self.user.borrow_mut().begin_frame(&targets.device, config));
        true
    }
    fn bind_layer(&mut self, index: usize, dirty: &[DeviceIntRect]) {
        let result = self
            .user
            .borrow_mut()
            .bind_layer(index, dirty)
            .and_then(|target| self.targets.borrow_mut().bind(target));
        self.targets.borrow().check(result);
    }
    fn present_layer(&mut self, _: usize, _: &[DeviceIntRect]) {
        self.targets.borrow_mut().unbind();
    }
    fn add_surface(
        &mut self,
        _: usize,
        _: comp::CompositorSurfaceTransform,
        _: DeviceIntRect,
        _: api::ImageRendering,
        _: DeviceIntRect,
        _: comp::ClipRadius,
    ) {
    }
    fn end_frame(&mut self) {}
    fn get_window_properties(&self) -> comp::WindowProperties {
        Default::default()
    }
}

impl<A: BackendApi> HalGpuBackend<A> {
    pub(crate) fn compositor_config(&mut self) -> comp::CompositorConfig {
        match std::mem::take(&mut self.gpu.compositor) {
            CompositorConfig::Draw => comp::CompositorConfig::default(),
            CompositorConfig::Native {
                capabilities,
                compositor,
            } => {
                let user = Rc::new(RefCell::new(compositor));
                self.gpu.compositor = CompositorConfig::Native {
                    capabilities,
                    compositor: Box::new(SharedNative(user.clone())),
                };
                comp::CompositorConfig::Native {
                    compositor: Box::new(NativeBridge {
                        user,
                        targets: self.compositor_targets.clone(),
                        capabilities,
                    }),
                }
            }
            CompositorConfig::Layer { compositor } => {
                let user = Rc::new(RefCell::new(compositor));
                self.gpu.compositor = CompositorConfig::Layer {
                    compositor: Box::new(SharedLayer(user.clone())),
                };
                comp::CompositorConfig::Layer {
                    compositor: Box::new(LayerBridge {
                        user,
                        targets: self.compositor_targets.clone(),
                    }),
                }
            }
        }
    }

    pub(crate) fn compositor_output(
        &mut self,
        frame: &mut Frame,
        clear: ColorF,
    ) -> Result<RenderedFrame<A>> {
        let operations = std::mem::take(&mut self.compositor_targets.borrow_mut().operations);
        self.gpu.track_native_operations(&operations);
        for tile in &frame.composite_state.tiles {
            if let CompositeTileSurface::Texture {
                surface: ResolvedSurfaceTexture::Native { id, size },
            } = tile.surface
            {
                self.gpu.native_sizes.insert(id, size);
            }
        }
        let empty = allocator_api2::vec::Vec::new_in(frame.passes.allocator().clone());
        let passes = std::mem::replace(&mut frame.passes, empty);
        let layer = matches!(self.gpu.compositor, CompositorConfig::Layer { .. });
        let compositor = if layer {
            Some(std::mem::take(&mut self.gpu.compositor))
        } else {
            None
        };
        let result = self.gpu.render(frame, Vec::new(), clear);
        if let Some(compositor) = compositor {
            self.gpu.compositor = compositor;
        }
        frame.passes = passes;
        result
    }

    pub(crate) fn uses_native_compositor(&self) -> bool {
        !matches!(self.gpu.compositor, CompositorConfig::Draw)
    }

    pub(crate) fn set_compositor_textures(
        &mut self,
        textures: &[(crate::internal_types::CacheTextureId, &wr::Texture)],
    ) {
        self.gpu.textures = textures
            .iter()
            .filter_map(|&(key, texture)| {
                self.textures
                    .get(&texture.id)
                    .map(|texture| (key, texture.clone()))
            })
            .collect();
    }
}
