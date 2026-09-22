/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

pub(super) struct Registry<A: BackendApi> {
    textures: HashMap<u32, Rc<Texture<A>>>,
    next_id: u32,
    opaque: std::collections::HashSet<(api::ExternalImageId, u8)>,
    normalized: HashMap<(api::ExternalImageId, u8), DeviceIntSize>,
}

impl<A: BackendApi> Default for Registry<A> {
    fn default() -> Self {
        Self {
            textures: HashMap::new(),
            next_id: 1 << 31,
            normalized: HashMap::new(),
            opaque: Default::default(),
        }
    }
}

struct Locked<A: BackendApi> {
    lease: ExternalImageLease,
    texture: Option<(u32, Rc<Texture<A>>, wgt::TextureUses)>,
}

#[derive(Clone)]
struct SharedProvider(Rc<RefCell<Box<dyn ExternalImageProvider>>>);
impl ExternalImageProvider for SharedProvider {
    fn acquire(
        &mut self,
        id: api::ExternalImageId,
        channel: u8,
        composited: bool,
    ) -> Result<ExternalImageLease> {
        self.0.borrow_mut().acquire(id, channel, composited)
    }
    fn with_buffer(
        &mut self,
        id: api::ExternalImageId,
        channel: u8,
        upload: &mut dyn FnMut(crate::device::hal::ExternalImageBuffer<'_>) -> Result<()>,
    ) -> Result<()> {
        self.0.borrow_mut().with_buffer(id, channel, upload)
    }
}

struct Handler<A: BackendApi> {
    registry: Rc<RefCell<Registry<A>>>,
    owner: Rc<Device<A>>,
    submissions: Rc<SubmissionQueue<A>>,
    releases: ReleaseQueue,
    provider: Box<dyn ExternalImageProvider>,
    locked: HashMap<(api::ExternalImageId, u8), Vec<Locked<A>>>,
    failure: Rc<RefCell<Option<String>>>,
}

impl<A: BackendApi> HalGpuBackend<A> {
    pub(crate) fn configure_external_images(&self, frame: &Frame) {
        let mut registry = self.external.borrow_mut();
        registry.normalized.clear();
        registry.opaque.clear();
        for resolve in &frame.deferred_resolves {
            if let Some(external) = resolve.image_properties.external_image {
                if resolve
                    .image_properties
                    .descriptor
                    .flags
                    .contains(api::ImageDescriptorFlags::IS_OPAQUE)
                {
                    registry
                        .opaque
                        .insert((external.id, external.channel_index));
                }
                if external.normalized_uvs {
                    registry.normalized.insert(
                        (external.id, external.channel_index),
                        resolve.image_properties.descriptor.size,
                    );
                }
            }
        }
    }

    pub(crate) fn external_handler(
        &mut self,
        provider: Box<dyn ExternalImageProvider>,
    ) -> Box<dyn api::ExternalImageHandler> {
        let provider = SharedProvider(Rc::new(RefCell::new(provider)));
        self.gpu.external_provider = Some(Box::new(provider.clone()));
        Box::new(Handler {
            registry: self.external.clone(),
            owner: self.gpu.owner.clone(),
            submissions: self.gpu.submissions.clone(),
            releases: self.gpu.releases.clone(),
            provider: Box::new(provider),
            locked: HashMap::new(),
            failure: self.failure.clone(),
        })
    }

    pub(super) fn texture_handle(&self, id: u32) -> Option<Rc<Texture<A>>> {
        self.textures
            .get(&id)
            .cloned()
            .or_else(|| self.external.borrow().textures.get(&id).cloned())
    }
}

impl<A: BackendApi> Handler<A> {
    fn acquire(&mut self, id: api::ExternalImageId, channel: u8, composited: bool) -> Result<()> {
        let key = (id, channel);
        let lease = self.provider.acquire(id, channel, composited)?;
        lease.attach_releases(&self.releases);
        lease.attach_metrics(self.owner.metrics.as_ref());
        let texture = match &lease.source {
            ExternalImageSource::Native(native) => {
                let texture = native.texture(&self.owner)?;
                if !texture.sample_initialized() {
                    return Err("External image contents are uninitialized".into());
                }
                let usage = texture.current_usage();
                let texture = texture.with_lease(
                    lease.state.clone(),
                    TextureFilter::Linear,
                    self.registry.borrow().opaque.contains(&key)
                        || lease
                            .descriptor
                            .flags
                            .contains(api::ImageDescriptorFlags::IS_OPAQUE),
                )?;
                let mut registry = self.registry.borrow_mut();
                let handle = registry.next_id;
                registry.next_id = handle
                    .checked_add(1)
                    .ok_or("External image handle exhaustion")?;
                registry.textures.insert(handle, texture.clone());
                Some((handle, texture, usage))
            }
            ExternalImageSource::Buffer(_) => None,
        };
        self.locked
            .entry(key)
            .or_default()
            .push(Locked { lease, texture });
        Ok(())
    }
}

impl<A: BackendApi> api::ExternalImageHandler for Handler<A> {
    fn lock(
        &mut self,
        id: api::ExternalImageId,
        channel: u8,
        composited: bool,
    ) -> api::ExternalImage<'_> {
        if let Err(error) = self.acquire(id, channel, composited) {
            self.owner.lost.set(true);
            self.failure
                .borrow_mut()
                .get_or_insert_with(|| error.clone());
            log::error!("HAL external image acquisition failed: {error}");
            return api::ExternalImage {
                uv: TexelRect::new(0.0, 0.0, 1.0, 1.0),
                source: api::ExternalImageSource::Invalid,
            };
        }
        let locked = self.locked[&(id, channel)].last().unwrap();
        let source = match &locked.lease.source {
            ExternalImageSource::Buffer(bytes) => api::ExternalImageSource::RawData(bytes),
            ExternalImageSource::Native(_) => api::ExternalImageSource::NativeTexture(
                ExternalTextureHandle(locked.texture.as_ref().unwrap().0 as u64),
            ),
        };
        let mut uv = locked.lease.uv;
        if let Some(size) = self.registry.borrow().normalized.get(&(id, channel)) {
            uv.uv0.x /= size.width as f32;
            uv.uv1.x /= size.width as f32;
            uv.uv0.y /= size.height as f32;
            uv.uv1.y /= size.height as f32;
        }
        api::ExternalImage { uv, source }
    }

    fn unlock(&mut self, id: api::ExternalImageId, channel: u8) {
        if let Some(locked) = self
            .locked
            .get_mut(&(id, channel))
            .and_then(|locks| locks.pop())
        {
            if let Some((handle, texture, usage)) = locked.texture {
                match self.submissions.recording() {
                    Ok(mut commands) => texture.transition(&mut commands, usage),
                    Err(error) => {
                        self.owner.lost.set(true);
                        log::error!("HAL external image release failed: {error}");
                    }
                }
                self.registry.borrow_mut().textures.remove(&handle);
            } else {
                locked.lease.complete_cpu_copy();
            }
        }
        self.locked.retain(|_, locks| !locks.is_empty());
        dispatch_releases(&self.releases);
    }
}
