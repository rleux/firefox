/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{hal, wgt, Recording, Texture, TextureState};
use std::rc::Rc;

pub(in crate::device::wgpu) struct WritableTexture<'a> {
    texture: &'a Rc<Texture>,
}

pub(in crate::device::wgpu) struct CopyDestination<'a> {
    writable: WritableTexture<'a>,
}

impl Texture {
    pub(in crate::device::wgpu) fn writable(self: &Rc<Self>) -> Result<WritableTexture<'_>, String> {
        Ok(WritableTexture { texture: self })
    }

    pub(in crate::device::wgpu) fn copy_destination(self: &Rc<Self>) -> Result<CopyDestination<'_>, String> {
        if !self.usage.contains(wgt::TextureUses::COPY_DST) {
            return Err("Texture does not support copy-destination access".into());
        }
        Ok(CopyDestination { writable: self.writable()? })
    }
}

impl<'a> CopyDestination<'a> {
    pub(super) fn texture(&self) -> &'a Rc<Texture> {
        self.writable.texture
    }

    pub(super) fn transition(&self, commands: &mut Recording<'_>) -> Result<(), String> {
        self.texture().transition_validated(commands, wgt::TextureUses::COPY_DST)
    }

    pub(super) fn initialize(&self, commands: &mut Recording<'_>) -> Result<(), String> {
        self.writable.initialize(commands)
    }
}

impl WritableTexture<'_> {
    pub(in crate::device::wgpu) fn initialize(&self, commands: &mut Recording<'_>) -> Result<(), String> {
        let texture = self.texture;
        let recording = commands.recording_id(&texture.raw.owner)?;
        let (_, first) = texture.states[0].prepare(
            &recording,
            TextureState {
                initialized: true,
                ..texture.states[0].current()
            },
        )?;
        if first {
            let resource = texture.clone();
            commands.commit(move || resource.states[0].commit());
        }
        commands.keep(texture);
        Ok(())
    }
}

