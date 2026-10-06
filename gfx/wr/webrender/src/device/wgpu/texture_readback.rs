/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::super::resources::Owned;
use super::super::{hal, wgt, Submission};
use super::Texture;
use api::units::DeviceIntRect;
use std::convert::TryFrom;
use std::rc::Rc;

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
    buffer: Rc<Owned<dyn hal::DynBuffer>>,
    layout: ReadbackLayout,
    capacity: u64,
}

impl Texture {
    /// Submit a base-level color readback, preserving native channel order and row order.
    pub fn readback(self: &Rc<Self>, rect: DeviceIntRect) -> Result<PendingReadback, String> {
        self.readback_reusing(rect, None)
    }

    pub(in crate::device::wgpu) fn readback_reusing(
        self: &Rc<Self>, rect: DeviceIntRect, previous: Option<PendingReadback>,
    ) -> Result<PendingReadback, String> {
        let source = self.copy_source()?;
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
        let layout = ReadbackLayout::new(
            rect.width() as u32,
            rect.height() as u32,
            bpp,
            owner.capabilities.alignments.buffer_copy_pitch.get(),
            owner.capabilities.limits.max_buffer_size,
        )?;
        let (mut submission, buffer, capacity, reused) = if let Some(mut previous) = previous.filter(|previous|
            Rc::ptr_eq(&previous.buffer.owner, owner) && previous.capacity >= layout.size
        ) {
            previous.submission.restart_completed()?;
            (previous.submission, previous.buffer, previous.capacity, true)
        } else {
        let (raw, capacity) = unsafe {
            owner.open.device.create_buffer(&hal::BufferDescriptor {
                label: Some("WR Vulkan texture readback"),
                size: layout.size,
                usage: wgt::BufferUses::COPY_DST | wgt::BufferUses::MAP_READ,
                memory_flags: hal::MemoryFlags::PREFER_COHERENT,
            })
        }
        .map_err(|error| format!("Creating readback buffer: {error:?}"))?;
        let buffer = Rc::new(Owned::new(owner, raw, <dyn hal::DynDevice>::destroy_buffer));
            (Submission::new(owner)?, buffer, capacity, false)
        };
        let mut commands = submission.recording()?;
        let recording = commands.recording_id(owner)?;
        for state in self.states() {
            state.check_recording(&recording)?;
        }
        if reused {
            unsafe {
                commands.encoder().transition_buffers(&[hal::BufferBarrier {
                    buffer: &**buffer,
                    usage: hal::StateTransition { from: wgt::BufferUses::MAP_READ, to: wgt::BufferUses::COPY_DST },
                }]);
            }
        }
        commands.keep(&buffer);
        let previous = self.current_usage();
        source.prepare(&mut commands)?;
        unsafe {
            let encoder = commands.encoder();
            encoder.copy_texture_to_buffer(
                self.raw_texture(),
                wgt::TextureUses::COPY_SRC,
                &**buffer,
                &[hal::BufferTextureCopy {
                    buffer_layout: wgt::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(layout.pitch),
                        rows_per_image: None,
                    },
                    texture_base: hal::TextureCopyBase {
                        mip_level: self.base_mip,
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
                }],
            );
            encoder.transition_buffers(&[hal::BufferBarrier {
                buffer: &**buffer,
                usage: hal::StateTransition {
                    from: wgt::BufferUses::COPY_DST,
                    to: wgt::BufferUses::MAP_READ,
                },
            }]);
        }
        self.transition(&mut commands, previous)?;
        drop(commands);
        submission.submit()?;
        Ok(PendingReadback {
            submission,
            buffer,
            layout,
            capacity,
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

    pub fn wait_into(&mut self, pixels: &mut [u8]) -> Result<(), String> {
        if pixels.len() != self.pixel_length() {
            return Err("Vulkan readback output size does not match the rectangle".into());
        }
        if !self.submission.wait(None)? {
            return Err("Vulkan readback did not complete".into());
        }
        self.read_pixels_into(pixels)
    }

    fn pixel_length(&self) -> usize {
        self.layout.row_bytes as usize * (self.layout.size / u64::from(self.layout.pitch)) as usize
    }

    fn read_pixels(&self) -> Result<Vec<u8>, String> {
        let mut pixels = vec![0; self.pixel_length()];
        self.read_pixels_into(&mut pixels)?;
        Ok(pixels)
    }

    fn read_pixels_into(&self, pixels: &mut [u8]) -> Result<(), String> {
        let owner = &self.buffer.owner;
        if owner.is_lost() {
            return Err("Vulkan device requires recreation".into());
        }
        let device = owner.open.device.as_ref();
        let layout = &self.layout;
        unsafe {
            let mapping = device
                .map_buffer(&**self.buffer, 0..layout.size)
                .map_err(|error| {
                    owner.lost.set(true);
                    format!("Mapping readback: {error:?}")
                })?;
            if !mapping.is_coherent {
                device.invalidate_mapped_ranges(&**self.buffer, &[0..layout.size]);
            }
            for y in 0..layout.size / u64::from(layout.pitch) {
                let start = y as usize * layout.row_bytes as usize;
                pixels[start..start + layout.row_bytes as usize].copy_from_slice(std::slice::from_raw_parts(
                    mapping
                        .ptr
                        .as_ptr()
                        .add((y * u64::from(layout.pitch)) as usize),
                    layout.row_bytes as usize,
                ));
            }
            device.unmap_buffer(&**self.buffer);
        }
        Ok(())
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

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn completed_readback_reuses_storage_without_stale_rows() {
    use crate::device::wgpu::{BufferPool, Device, Options, SubmissionQueue, TextureFilter};
    use crate::device::wgpu::tests::{validation_logging, ERRORS};
    use api::units::{DeviceIntPoint, DeviceIntSize};
    use std::sync::atomic::Ordering;
    validation_logging();
    let owner = Rc::new(Device::new(&Options { validation: true, ..Default::default() }).unwrap());
    let queue = SubmissionQueue::new(&Rc::new(BufferPool::new(&owner)), 2).unwrap();
    let texture = Texture::new(&owner, 3, 2, wgt::TextureFormat::Rgba8Unorm, TextureFilter::Nearest, false).unwrap();
    let full = DeviceIntRect::from_size(DeviceIntSize::new(3, 2));
    texture.upload(&queue, full, &[1; 24], None, 0, None).unwrap();
    queue.wait().unwrap();
    let mut pending = texture.readback(full).unwrap();
    assert_eq!(pending.wait().unwrap(), [1; 24]);
    let allocation = Rc::as_ptr(&pending.buffer);
    for byte in [3, 7, 19] {
        texture.upload(&queue, full, &[byte; 24], None, 0, None).unwrap();
        queue.wait().unwrap();
        let crop = DeviceIntRect::from_origin_and_size(DeviceIntPoint::new(1, 0), DeviceIntSize::new(2, 2));
        pending = texture.readback_reusing(crop, Some(pending)).unwrap();
        assert_eq!(Rc::as_ptr(&pending.buffer), allocation);
        let mut wrong = [17; 15];
        assert!(pending.wait_into(&mut wrong).is_err());
        assert_eq!(wrong, [17; 15]);
        let mut pixels = [0; 16];
        pending.wait_into(&mut pixels).unwrap();
        assert_eq!(pixels, [byte; 16]);
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
