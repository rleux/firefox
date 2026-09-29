/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::super::{hal, wgt, Recording};
use super::{access::CopyDestination, Texture};
use api::units::{DeviceIntPoint, DeviceIntRect};
use std::rc::Rc;

impl CopyDestination<'_> {
    pub(super) fn prepare_copy_destination(
        &self,
        commands: &mut Recording<'_>,
        destination_rect: DeviceIntRect,
    ) -> Result<(), String> {
        let texture = self.texture();
        self.transition(commands)?;
        let full_destination = destination_rect.min == DeviceIntPoint::zero()
            && destination_rect.max.x as u32 == texture.size.width
            && destination_rect.max.y as u32 == texture.size.height;
        if !texture.initialized() && !full_destination {
            unsafe {
                texture.raw.owner.clear_color_copy_destination(commands.encoder(), texture.raw_texture(), 0);
            }
            self.transition(commands)?;
        }
        Ok(())
    }
}

impl Texture {
    pub fn copy_from_texture(
        self: &Rc<Self>,
        commands: &mut Recording<'_>,
        source: &Rc<Self>,
        source_rect: DeviceIntRect,
        destination_rect: DeviceIntRect,
    ) -> Result<(), String> {
        let destination = self.copy_destination()?;
        let copy_source = source.copy_source()?;
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
            for state in &texture.states {
                state.check_recording(&recording)?;
            }
        }
        if source_rect.size() != destination_rect.size() {
            return Err("Vulkan texture copies cannot scale".into());
        }
        if !source.initialized() {
            return Err("Copying uninitialized Vulkan texture contents".into());
        }
        copy_source.prepare(commands)?;
        destination.prepare_copy_destination(commands, destination_rect)?;
        let encoder = commands.encoder();
        let base = |rect: DeviceIntRect| hal::TextureCopyBase {
            mip_level: 0,
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
                &[hal::TextureCopy {
                    src_base: base(source_rect),
                    dst_base: base(destination_rect),
                    size: wgt::Extent3d {
                        width: source_rect.width() as u32,
                        height: source_rect.height() as u32,
                        depth_or_array_layers: 1,
                    }
                    .into(),
                }],
            );
        }
        destination.initialize(commands)
    }
}
