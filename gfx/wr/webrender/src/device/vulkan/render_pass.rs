/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::draw::DrawPass;
use super::texture_store::TextureStore;
use super::{Recording, Texture};
use api::units::{DeviceIntPoint, DeviceIntRect, FramebufferIntRect};
use crate::device::{LoadOp, RenderPassDescriptor, StoreOp};
use std::cell::Cell;
use std::rc::Rc;

struct ActivePass {
    color: Rc<Texture>,
    depth: Option<Rc<Texture>>,
    bounds: DeviceIntRect,
    viewport: Option<DeviceIntRect>,
}

impl ActivePass {
    fn draw_pass(&self) -> DrawPass<'_> {
        DrawPass {
            target: &self.color,
            origin: DeviceIntPoint::zero(),
            viewport: self.viewport,
            depth: self.depth.as_ref(),
            clear_color: None,
            clear_depth: None,
            depth_range: 0.0..1.0,
        }
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
    ) -> Result<(), String> {
        if self.active.is_some() {
            return Err("A Vulkan render pass is already active".into());
        }
        if descriptor.target.is_default() && matches!(descriptor.depth_load, LoadOp::Clear(_)) {
            return Err("Vulkan default target has no depth attachment".into());
        }
        let (color, depth, viewport) = textures.draw_target(descriptor.target)?;
        let dimensions = descriptor.target.dimensions();
        let active = ActivePass {
            color,
            depth,
            bounds: DeviceIntRect::from_size(dimensions),
            viewport,
        };
        let pass = active.draw_pass();
        pass.validate(commands)?;
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
                active.color.invalidate(commands)?;
            }
            if descriptor.depth_load == LoadOp::DontCare {
                if let Some(depth) = &active.depth {
                    depth.invalidate(commands)?;
                }
            }
        }
        if color_clear.is_some() || depth_clear.is_some() {
            pass.clear_rect(commands, active.bounds, color_clear, depth_clear)?;
        }
        textures.clear_color_bindings();
        self.scissor_enabled.set(false);
        self.active = Some(active);
        Ok(())
    }

    pub fn draw_pass(&self) -> Result<DrawPass<'_>, String> {
        Ok(self
            .active
            .as_ref()
            .ok_or("No Vulkan render pass is active")?
            .draw_pass())
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
    ) -> Result<(), String> {
        let rect = match rect {
            Some(rect) => rect.cast_unit(),
            None => self.scissor_rect()?,
        };
        self.draw_pass()?.clear_rect(commands, rect, color, depth)
    }

    pub fn end(
        &mut self,
        commands: &mut Recording<'_>,
        depth_store: StoreOp,
    ) -> Result<(), String> {
        let active = self
            .active
            .as_ref()
            .ok_or("No Vulkan render pass is active")?;
        active.draw_pass().validate(commands)?;
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
