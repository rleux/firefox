/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::textures::texture_format;
use super::program::ShaderResource;
use super::{wgt, Device, Recording, Texture, TextureFilter};
use crate::device::{
    DrawTarget, GpuFrameId, Texture as TextureHandle, TextureFlags, TextureId, TextureSlot,
};
use crate::internal_types::RenderTargetInfo;
use api::{ImageBufferKind, ImageFormat, units::{DeviceIntRect, DeviceIntSize}};
use std::cell::Cell;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::rc::Rc;

struct Entry {
    color: Rc<Texture>,
    depth: Option<Rc<Texture>>,
    renderable: bool,
}

pub(super) struct TextureStore {
    owner: Rc<Device>,
    entries: HashMap<u32, Entry>,
    last_id: u32,
    frame: GpuFrameId,
    created: u32,
    deleted: u32,
    bound: [Option<u32>; 16],
}

impl TextureStore {
    pub fn new(owner: &Rc<Device>) -> Self {
        Self {
            owner: owner.clone(),
            entries: HashMap::new(),
            last_id: 0,
            frame: GpuFrameId::new(0),
            created: 0,
            deleted: 0,
            bound: [None; 16],
        }
    }

    pub fn begin_frame(&mut self, frame: GpuFrameId) {
        self.reset_bindings();
        self.frame = frame;
        self.created = 0;
        self.deleted = 0;
    }

    pub fn created(&self) -> u32 {
        self.created
    }
    pub fn deleted(&self) -> u32 {
        self.deleted
    }

    pub fn draw_target(
        &mut self,
        target: DrawTarget,
    ) -> Result<(Rc<Texture>, Option<Rc<Texture>>, Option<DeviceIntRect>), String> {
        let (color, depth, dimensions) = match target {
            DrawTarget::Texture {
                texture,
                with_depth,
                dimensions,
            } => {
                let (color, depth) = self.render_target(texture, with_depth)?;
                (color, depth, dimensions)
            }
            _ => return Err("Vulkan render pass requires an owned texture target".into()),
        };
        if dimensions.width != color.size().width as i32
            || dimensions.height != color.size().height as i32
        {
            return Err("Vulkan render target dimensions do not match its image".into());
        }
        Ok((color, depth, None))
    }

    pub fn create(
        &mut self,
        target: ImageBufferKind,
        format: ImageFormat,
        size: DeviceIntSize,
        filter: TextureFilter,
        render_target: Option<RenderTargetInfo>,
    ) -> Result<TextureHandle, String> {
        if !matches!(
            target,
            ImageBufferKind::Texture2D | ImageBufferKind::TextureRect
        ) || size.width <= 0
            || size.height <= 0
        {
            return Err("Invalid Vulkan texture kind or dimensions".into());
        }
        let id = self
            .last_id
            .checked_add(1)
            .ok_or("Vulkan texture identifier space exhausted")?;
        let color = Texture::new(
            &self.owner,
            size.width as u32,
            size.height as u32,
            texture_format(format),
            filter,
            render_target.is_some(),
        )?;
        let depth = if render_target.map_or(false, |info| info.has_depth) {
            Some(Texture::new(
                &self.owner,
                size.width as u32,
                size.height as u32,
                wgt::TextureFormat::Depth32Float,
                TextureFilter::Nearest,
                true,
            )?)
        } else {
            None
        };
        self.entries.insert(
            id,
            Entry {
                color,
                depth,
                renderable: render_target.is_some(),
            },
        );
        self.last_id = id;
        self.created += 1;
        Ok(TextureHandle {
            id,
            target_id: TextureId(u64::from(id)),
            target,
            format,
            size,
            filter,
            flags: TextureFlags::empty(),
            active_swizzle: Cell::default(),
            render_target,
            last_frame_used: self.frame,
        })
    }

    fn entry(&self, handle: &TextureHandle) -> Result<&Entry, String> {
        if handle.target_id.0 != u64::from(handle.id) {
            return Err("Invalid Vulkan texture handle".into());
        }
        self.entries
            .get(&handle.id)
            .ok_or_else(|| "Unknown Vulkan texture handle".into())
    }

    pub fn image(&self, handle: &TextureHandle) -> Result<Rc<Texture>, String> {
        Ok(self.entry(handle)?.color.clone())
    }

    pub fn bind(&mut self, slot: TextureSlot, handle: &TextureHandle) -> Result<(), String> {
        self.entry(handle)?;
        let binding = self
            .bound
            .get_mut(slot.0)
            .ok_or("Invalid Vulkan texture slot")?;
        *binding = Some(handle.id);
        Ok(())
    }

    pub fn reset_bindings(&mut self) {
        self.bound.fill(None);
    }

    pub fn clear_color_bindings(&mut self) {
        self.bound[..3].fill(None);
    }

    fn unbind_texture(&mut self, id: u32) {
        for slot in &mut self.bound {
            if *slot == Some(id) {
                *slot = None;
            }
        }
    }

    pub fn bindings(&self) -> [Option<ShaderResource>; 16] {
        self.bound.map(|id| {
            id.map(|id| ShaderResource::Texture {
                texture: self.entries[&id].color.clone(),
                filter: None,
            })
        })
    }

    pub fn invalidate_render_target(
        &mut self,
        handle: &TextureHandle,
        commands: &mut Recording<'_>,
    ) -> Result<(), String> {
        let entry = self.entry(handle)?;
        if !entry.renderable {
            return Ok(());
        }
        entry.color.invalidate(commands)?;
        if let Some(depth) = &entry.depth {
            depth.invalidate(commands)?;
        }
        self.unbind_texture(handle.id);
        Ok(())
    }

    pub fn render_target(
        &self,
        id: TextureId,
        with_depth: bool,
    ) -> Result<(Rc<Texture>, Option<Rc<Texture>>), String> {
        let id = u32::try_from(id.0).map_err(|_| "Invalid Vulkan render target identifier")?;
        let entry = self
            .entries
            .get(&id)
            .ok_or("Unknown Vulkan render target")?;
        if !entry.renderable {
            return Err("Vulkan texture is not a render target".into());
        }
        let depth = if with_depth {
            Some(
                entry
                    .depth
                    .as_ref()
                    .ok_or("Vulkan render target has no depth attachment")?
                    .clone(),
            )
        } else {
            None
        };
        Ok((entry.color.clone(), depth))
    }

    pub fn reuse_render_target(
        &mut self,
        handle: &mut TextureHandle,
        info: RenderTargetInfo,
    ) -> Result<(), String> {
        if !self.entry(handle)?.renderable {
            return Err("Vulkan texture is not a render target".into());
        }
        let entry = self.entries.get_mut(&handle.id).unwrap();
        if info.has_depth && entry.depth.is_none() {
            let size = entry.color.size();
            entry.depth = Some(Texture::new(
                &self.owner,
                size.width,
                size.height,
                wgt::TextureFormat::Depth32Float,
                TextureFilter::Nearest,
                true,
            )?);
        } else if !info.has_depth {
            entry.depth = None;
        }
        handle.render_target = Some(info);
        handle.last_frame_used = self.frame;
        Ok(())
    }

    pub fn delete(&mut self, handle: &mut TextureHandle) -> Result<(), String> {
        if handle.id == 0 {
            return Ok(());
        }
        self.entry(handle)?;
        self.unbind_texture(handle.id);
        self.entries.remove(&handle.id);
        handle.id = 0;
        handle.render_target = None;
        self.deleted += 1;
        Ok(())
    }

    pub fn depth_bytes(&self) -> usize {
        self.entries
            .values()
            .filter_map(|entry| entry.depth.as_ref())
            .fold(0usize, |total, depth| {
                let size = depth.size();
                let bytes = u64::from(size.width) * u64::from(size.height) * 4;
                total.saturating_add(usize::try_from(bytes).unwrap_or(usize::MAX))
            })
    }
}

#[cfg(test)]
#[path = "texture_store_tests.rs"]
mod tests;
