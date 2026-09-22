/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::{cell::RefCell, collections::HashMap, rc::Rc};
use webrender::api::{ImageDescriptor, ImageDescriptorFlags, ImageFormat};
use webrender::api::units::*;
use webrender::{CompositorCapabilities, NativeSurfaceId, NativeTileId};
use webrender::hal::{self, NativeSurfaceOperationDetails as Operation};

#[derive(Default)]
pub struct Trace {
    pub updates: usize,
    pub binds: Vec<(NativeTileId, DeviceIntRect, DeviceIntRect)>,
    pub commits: usize,
    pub releases: usize,
    pub layers: usize,
    pub abandoned: usize,
}

pub struct Layer {
    images: Vec<hal::NativeImage>,
    trace: Rc<RefCell<Trace>>,
    offset: DeviceIntPoint,
}

impl Layer {
    pub fn config(offset: DeviceIntPoint) -> (hal::CompositorConfig, Rc<RefCell<Trace>>) {
        let trace = Rc::new(RefCell::new(Trace::default()));
        (hal::CompositorConfig::Layer { compositor: Box::new(Self { images: Vec::new(), trace: trace.clone(), offset }) }, trace)
    }
}

impl hal::LayerCompositor for Layer {
    fn begin_frame(&mut self, device: &hal::ExternalImageDevice, config: &webrender::CompositorInputConfig) -> Result<(), String> {
        self.trace.borrow_mut().layers += config.layers.len();
        self.images.truncate(config.layers.len());
        for (index, layer) in config.layers.iter().enumerate() {
            let size = layer.clip_rect.size();
            let descriptor = ImageDescriptor::new(size.width + self.offset.x + 3, size.height + self.offset.y + 3,
                ImageFormat::RGBA8, ImageDescriptorFlags::empty());
            if self.images.get(index).map_or(true, |image| image.descriptor().size != descriptor.size) {
                let image = device.create_target(descriptor)?;
                if index == self.images.len() { self.images.push(image); } else { self.images[index] = image; }
            }
        }
        Ok(())
    }

    fn bind_layer(&mut self, index: usize, _: &[DeviceIntRect]) -> Result<hal::CompositorTarget, String> {
        let image = self.images.get(index).cloned().ok_or("Unknown compositor layer")?;
        let descriptor = image.descriptor();
        let trace = self.trace.clone();
        let lease = hal::ExternalImageLease::new(descriptor,
            TexelRect::new(0.0, 0.0, descriptor.size.width as f32, descriptor.size.height as f32),
            image.generation(), hal::ExternalImageSource::Native(image), move |status| {
                let mut trace = trace.borrow_mut();
                trace.releases += 1;
                if status == hal::ExternalImageRelease::Abandoned { trace.abandoned += 1; }
            })?;
        Ok(hal::CompositorTarget { image: lease, origin: self.offset,
            size: DeviceIntSize::new(descriptor.size.width - self.offset.x - 3, descriptor.size.height - self.offset.y - 3) })
    }

    fn end_frame(&mut self, _: hal::FrameCompletion) -> Result<(), String> {
        self.trace.borrow_mut().commits += 1;
        Ok(())
    }
}

struct Surface {
    size: Option<DeviceIntSize>,
    opaque: bool,
}

pub struct Native {
    surfaces: HashMap<NativeSurfaceId, Surface>,
    tiles: HashMap<NativeTileId, hal::NativeImage>,
    device: Option<hal::ExternalImageDevice>,
    trace: Rc<RefCell<Trace>>,
    offset: DeviceIntPoint,
}

impl Native {
    pub fn config(offset: DeviceIntPoint) -> (hal::CompositorConfig, Rc<RefCell<Trace>>) {
        let trace = Rc::new(RefCell::new(Trace::default()));
        let compositor = Self { surfaces: HashMap::new(), tiles: HashMap::new(), device: None, trace: trace.clone(), offset };
        (hal::CompositorConfig::Native {
            capabilities: CompositorCapabilities { max_update_rects: 8, supports_surface_for_backdrop: true, ..Default::default() },
            compositor: Box::new(compositor),
        }, trace)
    }

    fn create_tile(&mut self, id: NativeTileId, size: DeviceIntSize) -> Result<(), String> {
        let surface = self.surfaces.get(&id.surface_id).ok_or_else(|| format!("Unknown native surface {:?} while creating {id:?}", id.surface_id))?;
        let descriptor = ImageDescriptor::new(size.width + self.offset.x + 3, size.height + self.offset.y + 3,
            ImageFormat::RGBA8, if surface.opaque { ImageDescriptorFlags::IS_OPAQUE } else { ImageDescriptorFlags::empty() });
        let image = self.device.as_ref().ok_or("Native compositor device is missing")?.create_target(descriptor)?;
        self.tiles.insert(id, image);
        Ok(())
    }

    fn acquire(&self, id: NativeTileId) -> Result<hal::CompositorTarget, String> {
        let image = self.tiles.get(&id).cloned().ok_or("Unknown native tile")?;
        let descriptor = image.descriptor();
        let trace = self.trace.clone();
        let lease = hal::ExternalImageLease::new(descriptor,
            TexelRect::new(0.0, 0.0, descriptor.size.width as f32, descriptor.size.height as f32),
            image.generation(), hal::ExternalImageSource::Native(image), move |status| {
                let mut trace = trace.borrow_mut();
                trace.releases += 1;
                if status == hal::ExternalImageRelease::Abandoned { trace.abandoned += 1; }
            })?;
        Ok(hal::CompositorTarget { image: lease, origin: self.offset,
            size: DeviceIntSize::new(descriptor.size.width - self.offset.x - 3, descriptor.size.height - self.offset.y - 3) })
    }
}

impl hal::NativeCompositor for Native {
    fn update_surfaces(&mut self, device: &hal::ExternalImageDevice, operations: &[hal::NativeSurfaceOperation]) -> Result<(), String> {
        self.device = Some(device.clone());
        for operation in operations {
            log::debug!("HAL native operation {:?}", operation.details);
            self.trace.borrow_mut().updates += 1;
            match operation.details {
                Operation::CreateSurface { id, tile_size, is_opaque, .. } => {
                    self.surfaces.insert(id, Surface { size: Some(tile_size), opaque: is_opaque });
                }
                Operation::CreateExternalSurface { id, is_opaque } => {
                    self.surfaces.insert(id, Surface { size: None, opaque: is_opaque });
                }
                Operation::CreateBackdropSurface { id, .. } => {
                    self.surfaces.insert(id, Surface { size: None, opaque: true });
                }
                Operation::DestroySurface { id } => {
                    self.tiles.retain(|tile, _| tile.surface_id != id);
                    self.surfaces.remove(&id);
                }
                Operation::CreateTile { id } => {
                    let size = self.surfaces.get(&id.surface_id).and_then(|surface| surface.size).ok_or("Missing native tile dimensions")?;
                    self.create_tile(id, size)?;
                }
                Operation::DestroyTile { id } => { self.tiles.remove(&id); }
                Operation::AttachExternalImage { id, .. } => {
                    if !self.surfaces.contains_key(&id) { return Err("External attachment has no native surface".into()); }
                }
            }
        }
        Ok(())
    }

    fn bind_tile(&mut self, id: NativeTileId, dirty: DeviceIntRect, valid: DeviceIntRect) -> Result<hal::CompositorTarget, String> {
        if !self.tiles.contains_key(&id) { self.create_tile(id, valid.max.to_vector().to_size())?; }
        self.trace.borrow_mut().binds.push((id, dirty, valid));
        self.acquire(id)
    }

    fn read_tile(&mut self, id: NativeTileId) -> Result<hal::CompositorTarget, String> { self.acquire(id) }

    fn end_frame(&mut self, descriptor: &hal::CompositeDescriptor, _: hal::FrameCompletion) -> Result<(), String> {
        for surface in &descriptor.surfaces {
            if let Some(id) = surface.surface_id {
                if !self.surfaces.contains_key(&id) { return Err("Compositing unknown native surface".into()); }
            }
        }
        self.trace.borrow_mut().commits += 1;
        Ok(())
    }
}
