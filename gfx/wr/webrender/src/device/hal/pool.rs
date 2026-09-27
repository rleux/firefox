/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::resources::{Buffer, Texture, bytes_per_pixel};
use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    rc::Rc,
};

const BUFFER_BUDGET: u64 = 64 * 1024 * 1024;
const TEXTURE_BUDGET: u64 = 64 * 1024 * 1024;

pub(super) struct BufferPool<A: hal::Api> {
    owner: Rc<Device<A>>,
    idle: Rc<RefCell<IdleBuffers<A>>>,
}

struct IdleBuffers<A: hal::Api> {
    buckets: HashMap<(u16, u32), VecDeque<Rc<Buffer<A>>>>,
    bytes: u64,
    count: usize,
    epoch: u64,
}

pub(super) struct BufferRecycle<A: hal::Api> {
    idle: Rc<RefCell<IdleBuffers<A>>>,
    buffer: Rc<Buffer<A>>,
    epoch: u64,
}

impl<A: hal::Api> BufferRecycle<A> {
    pub fn recycle(self) {
        BufferPool::<A>::cache_buffer(&self.idle, self.buffer, self.epoch);
    }
}

impl<A: hal::Api> BufferPool<A> {
    pub fn clear(&self) {
        let mut idle = self.idle.borrow_mut();
        idle.buckets.clear();
        idle.bytes = 0;
        idle.count = 0;
        idle.epoch = idle.epoch.wrapping_add(1);
    }

    pub fn new(owner: &Rc<Device<A>>) -> Self {
        Self {
            owner: owner.clone(),
            idle: Rc::new(RefCell::new(IdleBuffers {
                buckets: HashMap::new(),
                bytes: 0,
                count: 0,
                epoch: 0,
            })),
        }
    }

    #[cfg(test)]
    pub fn upload(&self, bytes: &[u8], usage: wgt::BufferUses) -> Result<Rc<Buffer<A>>> {
        let buffer = self.upload_with(bytes.len(), usage, |destination| {
            destination.copy_from_slice(bytes);
            Ok(())
        })?;
        let epoch = self.idle.borrow().epoch;
        Self::cache_buffer(&self.idle, buffer.clone(), epoch);
        Ok(buffer)
    }

    pub fn upload_with(
        &self,
        length: usize,
        usage: wgt::BufferUses,
        write: impl FnOnce(&mut [u8]) -> Result<()>,
    ) -> Result<Rc<Buffer<A>>> {
        let required_size = (length as u64)
            .max(4)
            .checked_next_power_of_two()
            .ok_or("HAL buffer size overflow")?;
        let usage = usage | wgt::BufferUses::MAP_WRITE;
        let mut idle = self.idle.borrow_mut();
        for size_class in required_size.trailing_zeros()..64 {
            let key = (usage.bits(), size_class);
            let attempts = idle.buckets.get(&key).map_or(0, VecDeque::len);
            for _ in 0..attempts {
                let mut buffer = idle.buckets.get_mut(&key).unwrap().pop_front().unwrap();
                let buffer_size = buffer.size;
                if let Some(buffer_mut) = Rc::get_mut(&mut buffer) {
                    idle.count -= 1;
                    idle.bytes -= buffer_size;
                    buffer_mut.write_with(length, write)?;
                    return Ok(buffer);
                }
                idle.buckets.get_mut(&key).unwrap().push_back(buffer);
            }
        }
        drop(idle);
        Buffer::new_with(&self.owner, length, usage, write)
    }

    pub fn recycle_after(&self, recording: &mut super::submission::Submission<A>, buffer: Rc<Buffer<A>>) {
        let epoch = self.idle.borrow().epoch;
        recording.recycle_buffer(BufferRecycle { idle: self.idle.clone(), buffer, epoch });
    }

    fn cache_buffer(idle: &Rc<RefCell<IdleBuffers<A>>>, buffer: Rc<Buffer<A>>, epoch: u64) {
        let mut idle = idle.borrow_mut();
        if idle.epoch != epoch || buffer.size > BUFFER_BUDGET {
            return;
        }
        while idle.bytes + buffer.size > BUFFER_BUDGET || idle.count >= 256 {
            let old = idle.buckets.values_mut().find_map(VecDeque::pop_front);
            let Some(old) = old else { break };
            idle.bytes -= old.size;
            idle.count -= 1;
        }
        if idle.bytes + buffer.size <= BUFFER_BUDGET && idle.count < 256 {
            let key = (buffer.usage.bits(), buffer.size.trailing_zeros());
            idle.bytes += buffer.size;
            idle.count += 1;
            idle.buckets.entry(key).or_default().push_back(buffer);
        }
    }

    pub fn bytes(&self) -> u64 { self.idle.borrow().bytes }
}

pub(super) struct TexturePool<A: hal::Api> {
    owner: Rc<Device<A>>,
    textures: Vec<Rc<Texture<A>>>,
}

impl<A: hal::Api> TexturePool<A> {
    pub fn clear(&mut self) { self.textures.clear(); }

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
