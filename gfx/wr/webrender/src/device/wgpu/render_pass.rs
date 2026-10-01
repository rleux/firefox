/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::draw::{ColorAttachment, Draw, DrawPass, PassCommand};
use super::swapchain::{SurfacePassTarget, SurfaceView, Swapchain};
use super::texture_store::TextureStore;
use super::{hal, wgt, Buffer, Device, Recording, Texture};
use api::units::{DeviceIntPoint, DeviceIntRect, FramebufferIntRect};
use crate::device::{LoadOp, RenderPassDescriptor, StoreOp};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

struct ActivePass {
    target: ActiveTarget,
    depth: Option<Rc<Texture>>,
    bounds: DeviceIntRect,
    viewport: Option<DeviceIntRect>,
    descriptor: RenderPassDescriptor,
    started: Cell<bool>,
    pending: RefCell<Vec<PassCommand>>,
}

enum ActiveTarget {
    Texture(Rc<Texture>),
    Surface(SurfacePassTarget),
    Skipped,
}

pub(super) struct DrawingPass<'a> {
    pass: DrawPass<'a, PassTarget<'a>>,
    scissor: DeviceIntRect,
    pending: &'a RefCell<Vec<PassCommand>>,
}

impl DrawingPass<'_> {
    pub fn pass(&self) -> &DrawPass<'_, PassTarget<'_>> {
        &self.pass
    }

    pub fn scissor(&self) -> DeviceIntRect {
        self.scissor
    }

    pub fn push(&self, draw: Draw) {
        self.pending.borrow_mut().push(PassCommand::Draw(draw));
    }
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
    fn target_view(&self) -> Option<&dyn hal::DynTextureView> {
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
    fn draw_pass(&self) -> Option<DrawPass<'_, PassTarget<'_>>> {
        let target = match &self.target {
            ActiveTarget::Texture(color) => PassTarget::Texture(color),
            ActiveTarget::Surface(surface) => PassTarget::Surface(surface.view()),
            ActiveTarget::Skipped => return None,
        };
        Some(DrawPass {
            target,
            origin: DeviceIntPoint::zero(),
            viewport: self.viewport,
            depth: self.depth.as_ref(),
            clear_color: None,
            clear_depth: None,
            depth_range: 0.0..1.0,
        })
    }
}

pub(super) struct RenderPassState {
    active: Option<ActivePass>,
    spare_commands: Vec<PassCommand>,
    quad: Rc<Buffer>,
    scissor: Cell<Option<DeviceIntRect>>,
    scissor_enabled: Cell<bool>,
}

impl RenderPassState {
    pub fn new(quad: &Rc<Buffer>) -> Self {
        Self {
            active: None,
            spare_commands: Vec::new(),
            quad: quad.clone(),
            scissor: Cell::new(None),
            scissor_enabled: Cell::new(false),
        }
    }

    pub fn drawing_pass(&self) -> Result<Option<DrawingPass<'_>>, String> {
        let active = self.active.as_ref().ok_or("No Vulkan render pass is active")?;
        Ok(active.draw_pass().map(|pass| DrawingPass {
            pass,
            scissor: self.clip_scissor(active.bounds),
            pending: &active.pending,
        }))
    }

    pub fn discard(&mut self) {
        if let Some(active) = self.active.take() {
            self.spare_commands = active.pending.into_inner();
            self.spare_commands.clear();
        }
    }

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
        let (target, depth, viewport) = match swapchain.filter(|_| descriptor.target.is_default()) {
            Some(swapchain) => {
                let (_, viewport) = TextureStore::default_target_viewport(descriptor.target)?;
                let target = swapchain.pass_target().map_or(ActiveTarget::Skipped, ActiveTarget::Surface);
                (target, None, Some(viewport))
            }
            None => {
                let (color, depth, viewport) = textures.draw_target(descriptor.target)?;
                (ActiveTarget::Texture(color), depth, viewport)
            }
        };
        let dimensions = descriptor.target.dimensions();
        let active = ActivePass {
            target,
            depth,
            bounds: DeviceIntRect::from_size(dimensions),
            viewport,
            descriptor: *descriptor,
            started: Cell::new(false),
            pending: RefCell::new(std::mem::take(&mut self.spare_commands)),
        };
        let pass = active.draw_pass();
        if let Some(pass) = &pass {
            pass.validate(commands)?;
        }
        match descriptor.depth_load {
            LoadOp::Clear(value) if active.depth.is_some() && (0.0..=1.0).contains(&value) => {
                ()
            }
            LoadOp::Clear(_) => return Err("Invalid Vulkan render-pass depth clear".into()),
            _ => (),
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
        textures.clear_color_bindings();
        self.scissor_enabled.set(false);
        self.active = Some(active);
        Ok(())
    }

    pub fn draw_pass(&self) -> Result<Option<DrawPass<'_, PassTarget<'_>>>, String> {
        Ok(self.active
            .as_ref()
            .ok_or("No Vulkan render pass is active")?
            .draw_pass())
    }

    #[cfg(test)]
    pub fn scissor_rect(&self) -> Result<DeviceIntRect, String> {
        let bounds = self
            .active
            .as_ref()
            .ok_or("No Vulkan render pass is active")?
            .bounds;
        Ok(self.clip_scissor(bounds))
    }

    fn clip_scissor(&self, bounds: DeviceIntRect) -> DeviceIntRect {
        if self.scissor_enabled.get() {
            self.scissor.get().map_or(bounds, |rect| {
                rect.intersection(&bounds)
                    .unwrap_or_else(DeviceIntRect::zero)
            })
        } else {
            bounds
        }
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
        let active = self.active.as_ref().ok_or("No Vulkan render pass is active")?;
        let rect = match rect {
            Some(rect) => rect.cast_unit(),
            None => self.clip_scissor(active.bounds),
        };
        if depth.map_or(false, |value| !(0.0..=1.0).contains(&value)) {
            return Err("Invalid render-pass depth clear".into());
        }
        if let Some(pass) = active.draw_pass() {
            if depth.is_some() && pass.depth.is_none() {
                return Err("Depth clear requires an attachment".into());
            }
            let Some(rect) = rect.intersection(&pass.validate(commands)?.bounds) else { return Ok(()) };
            if color.is_some() || depth.is_some() {
                let clear = commands.clear(pass.target.owner(), pass.target.format(),
                    pass.depth.is_some(), rect, color, depth)?;
                active.pending.borrow_mut().push(PassCommand::Clear(clear));
            }
        }
        Ok(())
    }

    pub fn flush(
        &self,
        commands: &mut Recording<'_>,
        depth_store: StoreOp,
    ) -> Result<(), String> {
        let Some(active) = &self.active else { return Ok(()) };
        let Some(mut pass) = active.draw_pass() else {
            active.pending.borrow_mut().clear();
            return Ok(());
        };
        let full = active.viewport.map_or(true, |viewport| viewport == active.bounds)
            && active.bounds.width() as u32 == pass.target.size().width
            && active.bounds.height() as u32 == pass.target.size().height;
        let mut pending = active.pending.borrow_mut();
        if active.started.get() && pending.is_empty() && depth_store == StoreOp::Store {
            return Ok(());
        }
        let first = !active.started.get();
        let color_load = if first { active.descriptor.color_load } else { LoadOp::Load };
        let depth_load = if first { active.descriptor.depth_load } else { LoadOp::Load };
        if full {
            pass.clear_color = match color_load {
                LoadOp::Clear(color) => Some(wgt::Color { r: color[0] as f64, g: color[1] as f64, b: color[2] as f64, a: color[3] as f64 }),
                _ => None,
            };
            pass.clear_depth = match depth_load { LoadOp::Clear(value) => Some(value), _ => None };
        } else if first {
            let color = match color_load { LoadOp::Clear(value) => Some(value), _ => None };
            let depth = match depth_load { LoadOp::Clear(value) => Some(value), _ => None };
            if color.is_some() || depth.is_some() {
                let area = active.bounds;
                if let Some(rect) = area.intersection(&active.bounds) {
                    let clear = commands.clear(pass.target.owner(), pass.target.format(),
                        pass.depth.is_some(), rect, color, depth)?;
                    pending.insert(0, PassCommand::Clear(clear));
                }
            }
        }
        let load = |initialized: bool, clear: bool, dont_care: bool| {
            if clear { hal::AttachmentOps::LOAD_CLEAR }
            else if full && active.descriptor.render_area == Some(active.bounds) && dont_care { hal::AttachmentOps::LOAD_DONT_CARE }
            else if !initialized { hal::AttachmentOps::LOAD_CLEAR }
            else { hal::AttachmentOps::LOAD }
        };
        let color_ops = load(pass.target.initialized(), pass.clear_color.is_some(), color_load == LoadOp::DontCare)
            | hal::AttachmentOps::STORE;
        let depth_ops = load(pass.depth.map_or(false, |depth| depth.initialized()), pass.clear_depth.is_some(), depth_load == LoadOp::DontCare)
            | if depth_store == StoreOp::Discard { hal::AttachmentOps::STORE_DISCARD } else { hal::AttachmentOps::STORE };
        pass.record_commands(commands, &self.quad, &pending, Some((color_ops, depth_ops)))?;
        pending.clear();
        active.started.set(true);
        if depth_store == StoreOp::Discard {
            if let Some(depth) = &active.depth { depth.invalidate(commands)?; }
        }
        Ok(())
    }

    pub fn end(
        &mut self,
        commands: &mut Recording<'_>,
        depth_store: StoreOp,
    ) -> Result<(), String> {
        if self.active.is_none() { return Err("No render pass is active".into()); }
        self.flush(commands, depth_store)?;
        self.discard();
        Ok(())
    }
}

#[cfg(test)]
#[path = "render_pass_tests.rs"]
mod tests;
