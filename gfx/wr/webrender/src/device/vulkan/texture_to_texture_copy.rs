/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::super::{hal, wgt, Recording};
use super::Texture;
use api::units::{DeviceIntPoint, DeviceIntRect};
use ash::vk;
use std::rc::Rc;
use wgpu_hal::CommandEncoder as _;

impl Texture {
    pub(super) fn prepare_copy_destination(
        self: &Rc<Self>,
        commands: &mut Recording<'_>,
        destination_rect: DeviceIntRect,
    ) -> Result<(), String> {
        self.transition(commands, wgt::TextureUses::COPY_DST)?;
        let full_destination = destination_rect.min == DeviceIntPoint::zero()
            && destination_rect.max.x as u32 == self.size.width
            && destination_rect.max.y as u32 == self.size.height;
        let encoder = commands.encoder();
        if !self.initialized() && !full_destination {
            unsafe {
                self.raw
                    .owner
                    .open
                    .device
                    .raw_device()
                    .cmd_clear_color_image(
                        encoder.raw_handle(),
                        self.raw_texture().raw_handle(),
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &vk::ClearColorValue { uint32: [0; 4] },
                        &[vk::ImageSubresourceRange {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            base_mip_level: self.base_mip,
                            level_count: 1,
                            base_array_layer: 0,
                            layer_count: 1,
                        }],
                    );
                // Order the clear before the copy even though the layout is unchanged.
                encoder.transition_textures(std::iter::once(hal::TextureBarrier {
                    queue_family_ownership_transfer: None,
                    texture: self.raw_texture(),
                    range: wgt::ImageSubresourceRange {
                        base_mip_level: self.base_mip,
                        mip_level_count: Some(1),
                        array_layer_count: Some(1),
                        ..Default::default()
                    },
                    usage: hal::StateTransition {
                        from: wgt::TextureUses::COPY_DST,
                        to: wgt::TextureUses::COPY_DST,
                    },
                }));
            }
        }
        Ok(())
    }

    pub fn copy_from_texture(
        self: &Rc<Self>,
        commands: &mut Recording<'_>,
        source: &Rc<Self>,
        source_rect: DeviceIntRect,
        destination_rect: DeviceIntRect,
    ) -> Result<(), String> {
        let recording = commands.recording_id(&self.raw.owner)?;
        commands.recording_id(&source.raw.owner)?;
        if self.format != source.format
            || self.format == wgt::TextureFormat::Depth32Float
            || Rc::ptr_eq(&self.raw, &source.raw)
        {
            return Err("Unsupported Vulkan texture copy".into());
        }
        for (texture, rect) in [(source, source_rect), (self, destination_rect)] {
            if rect.min.x < 0
                || rect.min.y < 0
                || rect.max.x <= rect.min.x
                || rect.max.y <= rect.min.y
                || rect.max.x as u32 > texture.size.width
                || rect.max.y as u32 > texture.size.height
            {
                return Err("Invalid Vulkan texture copy bounds".into());
            }
            for state in texture.states() {
                state.check_recording(&recording)?;
            }
        }
        if source_rect.size() != destination_rect.size() {
            return Err("Vulkan texture copies cannot scale".into());
        }
        if !source.initialized() {
            return Err("Copying uninitialized Vulkan texture contents".into());
        }
        source.transition(commands, wgt::TextureUses::COPY_SRC)?;
        self.prepare_copy_destination(commands, destination_rect)?;
        let encoder = commands.encoder();
        let base = |rect: DeviceIntRect, mip_level| hal::TextureCopyBase {
            mip_level,
            array_layer: 0,
            origin: wgt::Origin3d {
                x: rect.min.x as u32,
                y: rect.min.y as u32,
                z: 0,
            },
            aspect: hal::FormatAspects::COLOR,
        };
        unsafe {
            encoder.copy_texture_to_texture(
                source.raw_texture(),
                wgt::TextureUses::COPY_SRC,
                self.raw_texture(),
                std::iter::once(hal::TextureCopy {
                    src_base: base(source_rect, source.base_mip),
                    dst_base: base(destination_rect, self.base_mip),
                    size: wgt::Extent3d {
                        width: source_rect.width() as u32,
                        height: source_rect.height() as u32,
                        depth_or_array_layers: 1,
                    }
                    .into(),
                }),
            );
        }
        source.transition(commands, wgt::TextureUses::RESOURCE)?;
        self.transition(commands, wgt::TextureUses::RESOURCE)?;
        self.initialize(commands)
    }
}
