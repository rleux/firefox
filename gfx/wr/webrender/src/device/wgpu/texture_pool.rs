/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{wgt, Device, Texture, TextureFilter};
use std::rc::Rc;

const BYTE_LIMIT: u64 = 64 * 1024 * 1024;
const COUNT_LIMIT: usize = 128;

pub struct TexturePool {
    owner: Rc<Device>,
    textures: Vec<Rc<Texture>>,
    bytes: u64,
}

impl TexturePool {
    pub fn clear(&mut self) {
        self.textures.clear();
        self.bytes = 0;
    }

    pub fn new(owner: &Rc<Device>) -> Self {
        Self {
            owner: owner.clone(),
            textures: Vec::new(),
            bytes: 0,
        }
    }

    pub fn acquire(
        &mut self,
        width: u32,
        height: u32,
        format: wgt::TextureFormat,
        renderable: bool,
    ) -> Result<Rc<Texture>, String> {
        if self.owner.is_lost() {
            return Err("Vulkan device requires recreation".into());
        }
        if let Some(texture) = self.textures.iter().find(|texture| {
            Rc::strong_count(texture) == 1
                && texture.size().width == width
                && texture.size().height == height
                && texture.format() == format
                && texture.target_view().is_some() == renderable
        }) {
            return Ok(texture.clone());
        }
        let texture = Texture::new(
            &self.owner,
            width,
            height,
            format,
            TextureFilter::Nearest,
            renderable,
        )?;
        let bytes = Self::size(&texture);
        let mut size = self.bytes;
        let mut count = self.textures.len();
        if size + bytes > BYTE_LIMIT || count >= COUNT_LIMIT {
            self.textures.retain(|old| {
                if (size + bytes > BYTE_LIMIT || count >= COUNT_LIMIT) && Rc::strong_count(old) == 1 {
                    size -= Self::size(old);
                    count -= 1;
                    false
                } else {
                    true
                }
            });
        }
        if size + bytes <= BYTE_LIMIT && self.textures.len() < COUNT_LIMIT {
            size += bytes;
            self.textures.push(texture.clone());
        }
        self.bytes = size;
        Ok(texture)
    }

    fn size(texture: &Texture) -> u64 {
        u64::from(texture.size().width)
            * u64::from(texture.size().height)
            * match texture.format() {
                wgt::TextureFormat::R8Unorm => 1,
                wgt::TextureFormat::Rg8Unorm | wgt::TextureFormat::R16Unorm => 2,
                wgt::TextureFormat::Rgba8Unorm
                | wgt::TextureFormat::Bgra8Unorm
                | wgt::TextureFormat::Rg16Unorm
                | wgt::TextureFormat::Depth32Float => 4,
                wgt::TextureFormat::Rgba32Float | wgt::TextureFormat::Rgba32Sint => 16,
                _ => unreachable!("Unsupported owned texture format"),
            }
    }
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}
