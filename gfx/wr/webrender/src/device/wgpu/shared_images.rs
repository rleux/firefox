/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{core, hal, images::Image, wgt, CanvasPublication, Device, TextureFilter};
use std::{
    cell::{Cell, OnceCell},
    rc::Rc,
    sync::Arc,
};

pub struct SharedImage {
    owner: Arc<Device>,
    publication: Arc<CanvasPublication>,
    raw: OnceCell<Arc<core::resource::Texture>>,
    normal: OnceCell<Arc<Image>>,
    opaque: OnceCell<Arc<Image>>,
    failed: Cell<bool>,
}
impl SharedImage {
    pub(super) fn new(
        owner: &Arc<Device>,
        publication: Arc<CanvasPublication>,
        expected_owner: u64,
        expected_publication: u64,
    ) -> Result<Rc<Self>, String> {
        let context = owner
            .shared
            .as_ref()
            .ok_or("Renderer has no shared context")?;
        if !publication.matches(context, expected_owner, expected_publication) {
            return Err("Shared canvas owner, publication, or context generation mismatch".into());
        }
        Ok(Rc::new(Self {
            owner: owner.clone(),
            publication,
            raw: OnceCell::new(),
            normal: OnceCell::new(),
            opaque: OnceCell::new(),
            failed: Cell::new(false),
        }))
    }
    pub fn prepare(&self) -> Result<bool, String> {
        if self.failed.get() || self.publication.is_failed() {
            return Err("Shared canvas context is retired".into());
        }
        if self.raw.get().is_some() {
            return Ok(true);
        }
        let Some(raw) = self.publication.wrap(&self.owner.device)? else {
            return Ok(false);
        };
        if !self.publication.matches_texture(&raw) {
            self.failed.set(true);
            return Err("Shared canvas consumer changed VkImage".into());
        }
        {
            let device = self.owner.raw_device();
            let image = unsafe { raw.clone().as_hal::<hal::api::Vulkan>() }
                .ok_or("Expected Vulkan canvas")?;
            log::info!("WebGPU shared canvas consumer publication={} owner={} generation={} nonce={} device={:?} image={:?} same_image=true",
                self.publication.publication(), self.publication.owner(), self.publication.generation(),
                self.publication.nonce(), device.raw_device().handle(), unsafe { image.raw_handle() });
        }
        self.raw
            .set(raw)
            .map_err(|_| "Shared canvas wrapped twice")?;
        Ok(true)
    }
    pub(super) fn pending(&self) -> bool {
        self.raw.get().is_none() && !self.failed.get() && !self.publication.is_failed()
    }
    pub(super) fn poll(&self) -> bool {
        if !self.pending() {
            return false;
        }
        match self.prepare() {
            Ok(ready) => ready,
            Err(error) => {
                log::warn!("Shared canvas acquisition failed: {error}");
                self.failed.set(true);
                false
            }
        }
    }
    pub(super) fn image(&self, opaque: bool) -> Result<Arc<Image>, String> {
        if !self.prepare()? {
            return Err("Shared canvas producer is pending".into());
        }
        let cache = if opaque { &self.opaque } else { &self.normal };
        if let Some(image) = cache.get() {
            return Ok(image.clone());
        }
        if opaque
            && !self
                .owner
                .features()
                .contains(wgt::Features::TEXTURE_COMPONENT_SWIZZLE)
        {
            return Err("Opaque shared canvas sampling is unavailable".into());
        }
        let raw = self.raw.get().unwrap().clone();
        let view = raw.create_view(&core::resource::TextureViewDescriptor {
            swizzle: if opaque {
                wgt::TextureComponentSwizzle {
                    a: wgt::ComponentSwizzle::One,
                    ..Default::default()
                }
            } else {
                Default::default()
            },
            ..Default::default()
        });
        self.owner.check().map_err(|e| e.to_string())?;
        let image = Arc::new(Image {
            raw,
            sampled: view.clone(),
            levels: vec![view],
            filter: TextureFilter::Linear,
        });
        let _ = cache.set(image.clone());
        Ok(image)
    }
    pub(super) fn belongs_to(&self, owner: &Arc<Device>) -> bool {
        Arc::ptr_eq(&self.owner, owner)
    }
}
