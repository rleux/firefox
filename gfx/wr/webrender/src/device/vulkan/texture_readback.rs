/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::super::resources::Owned;
use super::super::{hal, wgt, Submission};
use super::Texture;
use api::units::DeviceIntRect;
use std::convert::TryFrom;
use std::rc::Rc;
use wgpu_hal::{CommandEncoder as _, Device as _};

struct ReadbackLayout {
    row_bytes: u32,
    pitch: u32,
    size: u64,
}

impl ReadbackLayout {
    fn new(width: u32, height: u32, bpp: u32, alignment: u64, limit: u64) -> Result<Self, String> {
        if width == 0 || height == 0 || bpp == 0 || alignment == 0 {
            return Err("Invalid Vulkan readback dimensions or alignment".into());
        }
        let row_bytes = width.checked_mul(bpp).ok_or("Readback row overflow")?;
        let pitch = u64::from(row_bytes)
            .checked_add(alignment - 1)
            .ok_or("Readback pitch overflow")?
            / alignment
            * alignment;
        let pitch = u32::try_from(pitch).map_err(|_| "Readback pitch exceeds u32")?;
        let size = u64::from(pitch) * u64::from(height);
        if size > limit || size > isize::MAX as u64 {
            return Err("Readback exceeds buffer or address-space limit".into());
        }
        Ok(Self {
            row_bytes,
            pitch,
            size,
        })
    }
}

pub struct PendingReadback {
    submission: Submission,
    buffer: Rc<Owned<hal::vulkan::Buffer>>,
    layout: ReadbackLayout,
}

impl Texture {
    /// Submit a base-level color readback, preserving native channel order and row order.
    pub fn readback(self: &Rc<Self>, rect: DeviceIntRect) -> Result<PendingReadback, String> {
        if rect.min.x < 0
            || rect.min.y < 0
            || rect.max.x <= rect.min.x
            || rect.max.y <= rect.min.y
            || rect.max.x as u32 > self.size.width
            || rect.max.y as u32 > self.size.height
        {
            return Err("Invalid Vulkan readback rectangle".into());
        }
        let bpp = match self.format {
            wgt::TextureFormat::R8Unorm => 1,
            wgt::TextureFormat::Rg8Unorm | wgt::TextureFormat::R16Unorm => 2,
            wgt::TextureFormat::Rgba8Unorm
            | wgt::TextureFormat::Bgra8Unorm
            | wgt::TextureFormat::Rg16Unorm => 4,
            wgt::TextureFormat::Rgba32Float | wgt::TextureFormat::Rgba32Sint => 16,
            _ => return Err("Vulkan readback requires a color texture".into()),
        };
        if !self.initialized() {
            return Err("Reading uninitialized Vulkan texture contents".into());
        }
        let owner = &self.raw.owner;
        let mut submission = Submission::new(owner)?;
        let mut commands = submission.recording()?;
        let recording = commands.recording_id(owner)?;
        for state in &self.states {
            state.check_recording(&recording)?;
        }
        let layout = ReadbackLayout::new(
            rect.width() as u32,
            rect.height() as u32,
            bpp,
            owner.capabilities.alignments.buffer_copy_pitch.get(),
            owner.capabilities.limits.max_buffer_size,
        )?;
        let raw = unsafe {
            owner.open.device.create_buffer(&hal::BufferDescriptor {
                label: Some("WR Vulkan texture readback"),
                size: layout.size,
                usage: wgt::BufferUses::COPY_DST | wgt::BufferUses::MAP_READ,
                memory_flags: hal::MemoryFlags::PREFER_COHERENT,
            })
        }
        .map_err(|error| format!("Creating readback buffer: {error:?}"))?;
        let buffer = Rc::new(Owned::new(owner, raw, hal::vulkan::Device::destroy_buffer));
        commands.keep(buffer.clone());
        let previous = self.current_usage();
        self.transition(&mut commands, wgt::TextureUses::COPY_SRC)?;
        unsafe {
            let encoder = commands.encoder();
            encoder.copy_texture_to_buffer(
                self.raw_texture(),
                wgt::TextureUses::COPY_SRC,
                &buffer,
                std::iter::once(hal::BufferTextureCopy {
                    buffer_layout: wgt::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(layout.pitch),
                        rows_per_image: Some(rect.height() as u32),
                    },
                    texture_base: hal::TextureCopyBase {
                        mip_level: 0,
                        array_layer: 0,
                        origin: wgt::Origin3d {
                            x: rect.min.x as u32,
                            y: rect.min.y as u32,
                            z: 0,
                        },
                        aspect: hal::FormatAspects::COLOR,
                    },
                    size: wgt::Extent3d {
                        width: rect.width() as u32,
                        height: rect.height() as u32,
                        depth_or_array_layers: 1,
                    }
                    .into(),
                }),
            );
            encoder.transition_buffers(std::iter::once(hal::BufferBarrier {
                buffer: &**buffer,
                usage: hal::StateTransition {
                    from: wgt::BufferUses::COPY_DST,
                    to: wgt::BufferUses::MAP_READ,
                },
            }));
        }
        self.transition(&mut commands, previous)?;
        drop(commands);
        submission.submit()?;
        Ok(PendingReadback {
            submission,
            buffer,
            layout,
        })
    }
}

impl PendingReadback {
    pub fn poll(&mut self) -> Result<Option<Vec<u8>>, String> {
        if !self.submission.poll()? {
            return Ok(None);
        }
        self.read_pixels().map(Some)
    }

    pub fn wait(&mut self) -> Result<Vec<u8>, String> {
        if !self.submission.wait(None)? {
            return Err("Vulkan readback did not complete".into());
        }
        self.read_pixels()
    }

    fn read_pixels(&self) -> Result<Vec<u8>, String> {
        let owner = &self.buffer.owner;
        if owner.is_lost() {
            return Err("Vulkan device requires recreation".into());
        }
        let device = &owner.open.device;
        let layout = &self.layout;
        let mut pixels = Vec::with_capacity(
            layout.row_bytes as usize * (layout.size / u64::from(layout.pitch)) as usize,
        );
        unsafe {
            let mapping = device
                .map_buffer(&self.buffer, 0..layout.size)
                .map_err(|error| {
                    owner.lost.set(true);
                    format!("Mapping readback: {error:?}")
                })?;
            if !mapping.is_coherent {
                device.invalidate_mapped_ranges(&self.buffer, std::iter::once(0..layout.size));
            }
            for y in 0..layout.size / u64::from(layout.pitch) {
                pixels.extend_from_slice(std::slice::from_raw_parts(
                    mapping
                        .ptr
                        .as_ptr()
                        .add((y * u64::from(layout.pitch)) as usize),
                    layout.row_bytes as usize,
                ));
            }
            device.unmap_buffer(&self.buffer);
        }
        Ok(pixels)
    }
}

#[test]
fn readback_layout_limits_and_padding() {
    let layout = ReadbackLayout::new(3, 2, 4, 256, 512).unwrap();
    assert_eq!(
        (layout.row_bytes, layout.pitch, layout.size),
        (12, 256, 512)
    );
    for (width, height, bpp, alignment, limit) in [
        (0, 1, 4, 256, u64::MAX),
        (1, 0, 4, 256, u64::MAX),
        (1, 1, 0, 256, u64::MAX),
        (1, 1, 4, 0, u64::MAX),
        (u32::MAX, 1, 4, 256, u64::MAX),
        (1, 1, 4, u64::MAX, u64::MAX),
        (u32::MAX, 1, 1, 256, u64::MAX),
        (u32::MAX, u32::MAX, 1, 1, u64::MAX),
        (3, 2, 4, 256, 511),
    ] {
        assert!(ReadbackLayout::new(width, height, bpp, alignment, limit).is_err());
    }
}
