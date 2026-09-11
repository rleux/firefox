/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::resources::{Buffer, Texture, bytes_per_pixel};
use std::{cell::RefCell, rc::Rc};

const BUFFER_BUDGET: u64 = 64 * 1024 * 1024;
const TEXTURE_BUDGET: u64 = 64 * 1024 * 1024;

pub(super) struct BufferPool<A: hal::Api> {
    owner: Rc<Device<A>>,
    buffers: RefCell<Vec<Rc<Buffer<A>>>>,
}

impl<A: hal::Api> BufferPool<A> {
    pub fn new(owner: &Rc<Device<A>>) -> Self {
        Self {
            owner: owner.clone(),
            buffers: RefCell::new(Vec::new()),
        }
    }

    pub fn upload(&self, bytes: &[u8], usage: wgt::BufferUses) -> Result<Rc<Buffer<A>>> {
        let mut buffers = self.buffers.borrow_mut();
        // Submission references disappear only after an observed completion fence.
        if let Some(buffer) = buffers.iter().find(|buffer| {
            Rc::strong_count(buffer) == 1
                && buffer.usage == usage | wgt::BufferUses::MAP_WRITE
                && buffer.size >= (bytes.len() as u64).max(4)
        }) {
            buffer.write(bytes)?;
            return Ok(buffer.clone());
        }
        let buffer = Buffer::new(&self.owner, bytes, usage)?;
        let mut size: u64 = buffers.iter().map(|buffer| buffer.size).sum();
        let mut count = buffers.len();
        buffers.retain(|old| {
            if (size + buffer.size > BUFFER_BUDGET || count >= 256) && Rc::strong_count(old) == 1 {
                size -= old.size;
                count -= 1;
                false
            } else {
                true
            }
        });
        if size + buffer.size <= BUFFER_BUDGET && buffers.len() < 256 {
            buffers.push(buffer.clone());
        }
        Ok(buffer)
    }

    pub fn bytes(&self) -> u64 {
        self.buffers.borrow().iter().map(|buffer| buffer.size).sum()
    }
}

pub(super) struct TexturePool<A: hal::Api> {
    owner: Rc<Device<A>>,
    textures: Vec<Rc<Texture<A>>>,
}

impl<A: hal::Api> TexturePool<A> {
    pub fn new(owner: &Rc<Device<A>>) -> Self {
        Self {
            owner: owner.clone(),
            textures: Vec::new(),
        }
    }

    pub fn acquire(
        &mut self,
        width: u32,
        height: u32,
        format: wgt::TextureFormat,
        renderable: bool,
    ) -> Result<Rc<Texture<A>>> {
        if let Some(texture) = self.textures.iter().find(|texture| {
            Rc::strong_count(texture) == 1
                && texture.size.width == width
                && texture.size.height == height
                && texture.format == format
                && texture.target.is_some() == renderable
        }) {
            return Ok(texture.clone());
        }
        let texture = Texture::new(
            &self.owner,
            width,
            height,
            format,
            crate::device::TextureFilter::Nearest,
            renderable,
        )?;
        texture.transient.set(true);
        let bytes = Self::size(&texture);
        let mut size = self.bytes();
        self.textures.retain(|old| {
            if size + bytes > TEXTURE_BUDGET && Rc::strong_count(old) == 1 {
                size -= Self::size(old);
                false
            } else {
                true
            }
        });
        if size + bytes <= TEXTURE_BUDGET && self.textures.len() < 128 {
            self.textures.push(texture.clone());
        }
        Ok(texture)
    }

    fn size(texture: &Texture<A>) -> u64 {
        u64::from(texture.size.width)
            * u64::from(texture.size.height)
            * bytes_per_pixel(texture.format) as u64
    }
    pub fn bytes(&self) -> u64 {
        self.textures
            .iter()
            .map(|texture| Self::size(texture))
            .sum()
    }
}
