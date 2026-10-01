/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::draw::{ColorAttachment, DrawPass};
use super::swapchain::{SurfaceView, Swapchain};
use super::texture_store::TextureStore;
use super::{hal, wgt, Device, Recording, Texture};
use api::units::{DeviceIntPoint, DeviceIntRect, FramebufferIntRect};
use crate::device::{LoadOp, RenderPassDescriptor, StoreOp};
use std::cell::Cell;
use std::rc::Rc;

struct ActivePass {
    color: Option<Rc<Texture>>,
    depth: Option<Rc<Texture>>,
    bounds: DeviceIntRect,
    viewport: Option<DeviceIntRect>,
}

pub(super) enum PassTarget<'a> {
    Texture(&'a Rc<Texture>),
    Surface(SurfaceView<'a>),
}

impl PassTarget<'_> {
    fn invalidate(&self, commands: &mut Recording<'_>) -> Result<(), String> {
        match self {
            Self::Texture(texture) => texture.invalidate(commands),
            Self::Surface(surface) => surface.invalidate(commands),
        }
    }
}

impl ColorAttachment for PassTarget<'_> {
    fn owner(&self) -> &Rc<Device> {
        match self {
            Self::Texture(texture) => ColorAttachment::owner(texture),
            Self::Surface(surface) => surface.owner(),
        }
    }
    fn size(&self) -> wgt::Extent3d {
        match self {
            Self::Texture(texture) => ColorAttachment::size(texture),
            Self::Surface(surface) => surface.size(),
        }
    }
    fn format(&self) -> wgt::TextureFormat {
        match self {
            Self::Texture(texture) => ColorAttachment::format(texture),
            Self::Surface(surface) => surface.format(),
        }
    }
    fn target_view(&self) -> Option<&hal::vulkan::TextureView> {
        match self {
            Self::Texture(texture) => ColorAttachment::target_view(texture),
            Self::Surface(surface) => surface.target_view(),
        }
    }
    fn initialized(&self) -> bool {
        match self {
            Self::Texture(texture) => ColorAttachment::initialized(texture),
            Self::Surface(surface) => surface.initialized(),
        }
    }
    fn validate_recording(&self, commands: &Recording<'_>) -> Result<(), String> {
        match self {
            Self::Texture(texture) => ColorAttachment::validate_recording(texture, commands),
            Self::Surface(surface) => surface.validate_recording(commands),
        }
    }
    fn prepare(&self, commands: &mut Recording<'_>) -> Result<(), String> {
        match self {
            Self::Texture(texture) => ColorAttachment::prepare(texture, commands),
            Self::Surface(surface) => surface.prepare(commands),
        }
    }
    fn initialize(&self, commands: &mut Recording<'_>) -> Result<(), String> {
        match self {
            Self::Texture(texture) => ColorAttachment::initialize(texture, commands),
            Self::Surface(surface) => surface.initialize(commands),
        }
    }
    fn is_sampled_by(&self, sample: &Texture) -> bool {
        match self {
            Self::Texture(texture) => ColorAttachment::is_sampled_by(texture, sample),
            Self::Surface(surface) => surface.is_sampled_by(sample),
        }
    }
}

impl ActivePass {
    fn draw_pass<'a>(
        &'a self,
        swapchain: Option<&'a Swapchain>,
    ) -> Result<Option<DrawPass<'a, PassTarget<'a>>>, String> {
        let target = match &self.color {
            Some(color) => PassTarget::Texture(color),
            None => {
                let surface = swapchain.ok_or("Window render pass has no swapchain")?;
                let Some(target) = surface.current_target() else {
                    return Ok(None);
                };
                PassTarget::Surface(target)
            }
        };
        Ok(Some(DrawPass {
            target,
            origin: DeviceIntPoint::zero(),
            viewport: self.viewport,
            depth: self.depth.as_ref(),
            clear_color: None,
            clear_depth: None,
            depth_range: 0.0..1.0,
        }))
    }
}

#[derive(Default)]
pub(super) struct RenderPassState {
    active: Option<ActivePass>,
    scissor: Cell<Option<DeviceIntRect>>,
    scissor_enabled: Cell<bool>,
}

impl RenderPassState {
    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    pub fn begin(
        &mut self,
        commands: &mut Recording<'_>,
        textures: &mut TextureStore,
        descriptor: &RenderPassDescriptor,
        swapchain: Option<&Swapchain>,
    ) -> Result<(), String> {
        if self.active.is_some() {
            return Err("A Vulkan render pass is already active".into());
        }
        if descriptor.target.is_default() && matches!(descriptor.depth_load, LoadOp::Clear(_)) {
            return Err("Vulkan default target has no depth attachment".into());
        }
        let (color, depth, viewport) = if descriptor.target.is_default() && swapchain.is_some() {
            let (_, viewport) = TextureStore::default_target_viewport(descriptor.target)?;
            (None, None, Some(viewport))
        } else {
            let (color, depth, viewport) = textures.draw_target(descriptor.target)?;
            (Some(color), depth, viewport)
        };
        let dimensions = descriptor.target.dimensions();
        let active = ActivePass {
            color,
            depth,
            bounds: DeviceIntRect::from_size(dimensions),
            viewport,
        };
        let pass = active.draw_pass(swapchain)?;
        if let Some(pass) = &pass {
            pass.validate(commands)?;
        }
        let color_clear = match descriptor.color_load {
            LoadOp::Clear(color) => Some(color),
            _ => None,
        };
        let depth_clear = match descriptor.depth_load {
            LoadOp::Clear(value) if active.depth.is_some() && (0.0..=1.0).contains(&value) => {
                Some(value)
            }
            LoadOp::Clear(_) => return Err("Invalid Vulkan render-pass depth clear".into()),
            _ => None,
        };
        if descriptor.render_area == Some(active.bounds)
            && active
                .viewport
                .map_or(true, |viewport| viewport == active.bounds)
        {
            if descriptor.color_load == LoadOp::DontCare {
                if let Some(pass) = &pass {
                    pass.target.invalidate(commands)?;
                }
            }
            if descriptor.depth_load == LoadOp::DontCare {
                if let Some(depth) = &active.depth {
                    depth.invalidate(commands)?;
                }
            }
        }
        if color_clear.is_some() || depth_clear.is_some() {
            if let Some(pass) = &pass {
                pass.clear_rect(commands, active.bounds, color_clear, depth_clear)?;
            }
        }
        textures.clear_color_bindings();
        self.scissor_enabled.set(false);
        self.active = Some(active);
        Ok(())
    }

    pub fn draw_pass<'a>(
        &'a self,
        swapchain: Option<&'a Swapchain>,
    ) -> Result<Option<DrawPass<'a, PassTarget<'a>>>, String> {
        self.active
            .as_ref()
            .ok_or("No Vulkan render pass is active")?
            .draw_pass(swapchain)
    }

    pub fn scissor_rect(&self) -> Result<DeviceIntRect, String> {
        let bounds = self
            .active
            .as_ref()
            .ok_or("No Vulkan render pass is active")?
            .bounds;
        Ok(if self.scissor_enabled.get() {
            self.scissor.get().map_or(bounds, |rect| {
                rect.intersection(&bounds)
                    .unwrap_or_else(DeviceIntRect::zero)
            })
        } else {
            bounds
        })
    }

    pub fn set_scissor_rect(&self, rect: FramebufferIntRect) {
        self.scissor.set(Some(rect.cast_unit()));
    }

    pub fn enable_scissor(&self) {
        self.scissor_enabled.set(true);
    }

    pub fn disable_scissor(&self) {
        self.scissor_enabled.set(false);
    }

    pub fn clear(
        &self,
        commands: &mut Recording<'_>,
        color: Option<[f32; 4]>,
        depth: Option<f32>,
        rect: Option<FramebufferIntRect>,
        swapchain: Option<&Swapchain>,
    ) -> Result<(), String> {
        let rect = match rect {
            Some(rect) => rect.cast_unit(),
            None => self.scissor_rect()?,
        };
        if let Some(pass) = self.draw_pass(swapchain)? {
            pass.clear_rect(commands, rect, color, depth)?;
        }
        Ok(())
    }

    pub fn end(
        &mut self,
        commands: &mut Recording<'_>,
        depth_store: StoreOp,
        swapchain: Option<&Swapchain>,
    ) -> Result<(), String> {
        let active = self
            .active
            .as_ref()
            .ok_or("No Vulkan render pass is active")?;
        if let Some(pass) = active.draw_pass(swapchain)? {
            pass.validate(commands)?;
            if active.color.is_none() && !pass.target.initialized() {
                pass.clear_rect(commands, active.bounds, Some([0.0; 4]), None)?;
            }
        }
        if depth_store == StoreOp::Discard {
            if let Some(depth) = &active.depth {
                depth.invalidate(commands)?;
            }
        }
        self.active = None;
        Ok(())
    }
}

#[cfg(test)]
#[path = "render_pass_tests.rs"]
mod tests;
