/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::hal::surface::{SurfaceSetup, SurfaceState};
use ash::vk;
use std::borrow::Borrow;

impl FrameRenderer<hal::api::Vulkan> {
    pub fn attach_surface(&mut self, setup: SurfaceSetup<hal::api::Vulkan>, size: [u32; 2], options: SurfaceOptions) -> Result<()> {
        let mut surface = SurfaceState::new(&self.owner, setup, options);
        surface.configure(size)?;
        self.surface = Some(surface);
        Ok(())
    }

    pub fn surface_info(&self) -> Option<SurfaceInfo> { self.surface.as_ref().map(|surface| surface.info.clone()) }

    pub fn resize_surface(&mut self, size: [u32; 2]) -> Result<()> {
        if self.failed.get() { return Err("Cannot configure a failed renderer".into()); }
        self.discard_surface()?;
        self.submissions.wait()?;
        self.surface.as_mut().ok_or("Renderer has no window surface")?.configure(size)
    }

    pub fn acquire_surface(&mut self) -> Result<PresentationStatus> {
        if self.failed.get() { return Err("Cannot acquire on a failed renderer".into()); }
        let surface = self.surface.as_mut().ok_or("Renderer has no window surface")?;
        if surface.dirty {
            self.submissions.wait()?;
            surface.configure(surface.info.size)?;
        }
        let fence = self.submissions.fence()?;
        let result = surface.acquire(&fence);
        if result.is_err() && surface.acquired.is_none() { self.failed.set(true); }
        result
    }

    pub fn discard_surface(&mut self) -> Result<()> {
        if self.failed.get() { return Err("Cannot discard on a failed renderer; destroy it".into()); }
        let surface = self.surface.as_mut().ok_or("Renderer has no window surface")?;
        if let Some(acquired) = &surface.acquired {
            drop(self.submissions.recording()?);
            self.submissions.submit_surfaces(&[&acquired.texture])?;
            self.submissions.wait()?;
            surface.discard();
        }
        Ok(())
    }

    pub fn present_output(&mut self, output: &RenderedFrame<hal::api::Vulkan>) -> Result<PresentationStatus> {
        if self.failed.get() { return Err("Cannot present a failed renderer".into()); }
        if output.texture.is_none() { self.discard_surface()?; return Ok(PresentationStatus::Suspended); }
        if self.surface.as_ref().ok_or("Renderer has no window surface")?.acquired.is_none() {
            let status = self.acquire_surface()?;
            if status != PresentationStatus::Acquired { return Ok(status); }
        }
        let surface = self.surface.as_mut().unwrap();
        let acquired = surface.acquired.as_ref().unwrap();
        let config = surface.config.as_ref().unwrap();
        let source = output.texture.as_ref().unwrap();
        let target: &hal::vulkan::Texture = acquired.texture.borrow();
        let raw = self.owner.open.device.raw_device();
        let instance = self.owner.open.device.shared_instance().raw_instance();
        let format = match config.format {
            wgt::TextureFormat::Rgba8Unorm => vk::Format::R8G8B8A8_UNORM,
            wgt::TextureFormat::Bgra8Unorm => vk::Format::B8G8R8A8_UNORM,
            _ => unreachable!(),
        };
        for (format, usage) in [(vk::Format::R8G8B8A8_UNORM, vk::FormatFeatureFlags::BLIT_SRC), (format, vk::FormatFeatureFlags::BLIT_DST)] {
            let caps = unsafe { instance.get_physical_device_format_properties(self.owner.open.device.raw_physical_device(), format) };
            if !caps.optimal_tiling_features.contains(usage) { return Err("Surface format does not support GPU presentation blits".into()); }
        }
        self.failed.set(true);
        let mut commands = self.submissions.recording()?;
        let previous = source.current_usage();
        source.transition(&mut commands, wgt::TextureUses::COPY_SRC);
        let range = wgt::ImageSubresourceRange { aspect: wgt::TextureAspect::All, base_mip_level: 0,
            mip_level_count: Some(1), base_array_layer: 0, array_layer_count: Some(1) };
        unsafe {
            commands.encoder().transition_textures(std::iter::once(hal::TextureBarrier {
                texture: target, range: range.clone(), usage: hal::StateTransition { from: wgt::TextureUses::UNINITIALIZED, to: wgt::TextureUses::COPY_DST },
            }));
            let layers = vk::ImageSubresourceLayers::default().aspect_mask(vk::ImageAspectFlags::COLOR).layer_count(1);
            let blit = vk::ImageBlit::default().src_subresource(layers).dst_subresource(layers)
                .src_offsets([vk::Offset3D::default(), vk::Offset3D { x: output.size[0] as i32, y: output.size[1] as i32, z: 1 }])
                .dst_offsets([vk::Offset3D::default(), vk::Offset3D { x: config.extent.width as i32, y: config.extent.height as i32, z: 1 }]);
            raw.cmd_blit_image(commands.encoder().raw_handle(), source.raw.raw_handle(), vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                target.raw_handle(), vk::ImageLayout::TRANSFER_DST_OPTIMAL, &[blit], vk::Filter::NEAREST);
            commands.encoder().transition_textures(std::iter::once(hal::TextureBarrier {
                texture: target, range, usage: hal::StateTransition { from: wgt::TextureUses::COPY_DST, to: wgt::TextureUses::PRESENT },
            }));
        }
        source.transition(&mut commands, previous);
        drop(commands);
        self.submissions.submit_surfaces(&[&acquired.texture])?;
        let status = surface.present()?;
        self.failed.set(false);
        Ok(status)
    }
}
